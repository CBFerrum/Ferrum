//! MCP server against a real PostgreSQL (`FERRUM_MCP_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_mcp_test`). The database is reset first.
//!
//! An agent runs a whole sale through MCP tools alone: company setup, masters, an idempotent
//! invoice, preview, submit, payment, a report and the audit trail; then protocol errors and the
//! limits of a non-admin user.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

async fn fresh_kernel() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_MCP_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_mcp_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot")
}

struct Agent<'a> {
    k: &'a Kernel,
    ctx: Ctx,
    next: i64,
}

impl Agent<'_> {
    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        ferrum_mcp::handle(self.k, &self.ctx, json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params})).await.expect("a response")
    }

    /// Calls a tool and returns its structured result, panicking on errors.
    async fn tool(&mut self, name: &str, args: Value) -> Value {
        let r = self.rpc("tools/call", json!({"name": name, "arguments": args})).await;
        assert!(r.get("error").is_none(), "{name}: protocol error {r}");
        let result = &r["result"];
        assert_eq!(result["isError"], json!(false), "{name} failed: {}", result["content"][0]["text"]);
        result["structuredContent"].clone()
    }

    /// Calls a tool expected to fail and returns the error payload.
    async fn tool_err(&mut self, name: &str, args: Value) -> Value {
        let r = self.rpc("tools/call", json!({"name": name, "arguments": args})).await;
        assert_eq!(r["result"]["isError"], json!(true), "{name} should fail: {r}");
        r["result"]["structuredContent"].clone()
    }

    async fn first_id(&mut self, entity: &str, filters: Value) -> String {
        let page = self.tool("search_records", json!({"entity": entity, "filters": filters})).await;
        page["rows"][0]["id"].as_str().unwrap_or_else(|| panic!("no {entity} for {filters}")).to_string()
    }
}

#[test]
fn agent_runs_a_sale_through_mcp() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let k = fresh_kernel().await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "agents".into(),
            name: "Agents Ltd".into(),
            admin_email: "admin@agents.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying"].iter().map(|s| s.to_string()).collect(),
            settings: Some(json!({"fiscal_year_start_month": 4})),
        })
        .await
        .expect("tenant");
    let session = k.login("agents", "admin@agents.test", "Correct-Horse-9").await.expect("login");
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "mcp-it".into()).await.expect("ctx");
    let mut a = Agent { k: &k, ctx, next: 0 };

    // ───────────── handshake ─────────────
    let init = a.rpc("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}})).await;
    assert_eq!(init["result"]["protocolVersion"], json!("2025-06-18"));
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert!(init["result"]["instructions"].as_str().unwrap().contains("ferrum_overview"));
    let newer = a.rpc("initialize", json!({"protocolVersion": "2099-01-01"})).await;
    assert_eq!(newer["result"]["protocolVersion"], json!(ferrum_mcp::PROTOCOL_VERSIONS[0]), "unknown versions get our newest");
    assert!(ferrum_mcp::handle(&k, &a.ctx, json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await.is_none());
    assert_eq!(a.rpc("ping", json!({})).await["result"], json!({}));

    let listed = a.rpc("tools/list", json!({})).await;
    let names: Vec<&str> = listed["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in ["ferrum_overview", "describe_entity", "search_records", "create_record", "submit_record", "run_action", "run_report", "customize_entity"] {
        assert!(names.contains(&expected), "missing tool {expected}");
    }
    let submit = listed["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "submit_record").unwrap();
    assert_eq!(submit["annotations"]["destructiveHint"], json!(true), "clients should confirm postings");

    // ───────────── discover ─────────────
    let overview = a.tool("ferrum_overview", json!({})).await;
    assert!(overview["entities"].as_array().unwrap().iter().any(|e| e["name"] == "SalesInvoice"));
    assert!(overview["reports"].as_array().unwrap().iter().any(|r| r["name"] == "trial_balance"));
    let invoice_schema = a.tool("describe_entity", json!({"entity": "SalesInvoice"})).await;
    let required: Vec<&str> = invoice_schema["json_schema"]["required"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(required.contains(&"customer") && required.contains(&"items"), "{required:?}");
    assert!(!required.contains(&"company"), "company has a default");
    assert_eq!(invoice_schema["json_schema"]["properties"]["items"]["type"], json!("array"));

    // ───────────── set up and sell ─────────────
    let inr = a.first_id("Currency", json!({"code": "INR"})).await;
    let company = a.tool("create_record", json!({"entity": "Company", "data": {"company_name": "Agents Ltd", "abbr": "AL", "default_currency": inr}})).await;
    let company_id = company["record"]["id"].as_str().unwrap().to_string();
    let accounts = a.tool("search_records", json!({"entity": "Account", "filters": {"company": company_id}, "limit": 200})).await;
    assert!(accounts["rows"].as_array().unwrap().len() > 50, "chart of accounts created by the company hook");
    let customer = a.tool("create_record", json!({"entity": "Customer", "data": {"customer_name": "Initech"}})).await["record"]["id"].clone();
    let group = a.first_id("ItemGroup", json!({"group_name": "Products"})).await;
    let uom = a.first_id("Uom", json!({"uom_name": "Nos"})).await;
    let item = a
        .tool(
            "create_record",
            json!({"entity": "Item", "data": {"item_code": "STAPLER", "item_name": "Red stapler", "item_group": group, "stock_uom": uom, "is_stock_item": false, "standard_rate": "250"}}),
        )
        .await["record"]["id"]
        .clone();
    let draft = json!({"company": company_id, "customer": customer, "items": [{"item": item, "qty": "4"}]});
    let preview = a.tool("preview_record", json!({"entity": "SalesInvoice", "data": draft})).await;
    assert_eq!(preview["grand_total"], json!("1000"));
    let created = a.tool("create_record", json!({"entity": "SalesInvoice", "data": draft, "idempotency_key": "agent-sale-0001"})).await;
    let replay = a.tool("create_record", json!({"entity": "SalesInvoice", "data": draft, "idempotency_key": "agent-sale-0001"})).await;
    assert_eq!(replay["replayed"], json!(true));
    assert_eq!(replay["record"]["id"], created["record"]["id"], "a retried create returns the same invoice");
    let invoice_id = created["record"]["id"].as_str().unwrap().to_string();
    let submitted = a.tool("submit_record", json!({"entity": "SalesInvoice", "id": invoice_id})).await;
    assert_eq!(submitted["docstatus"], json!(1));
    assert_eq!(submitted["status"], json!("Unpaid"));
    let payment = a.tool("run_action", json!({"entity": "SalesInvoice", "id": invoice_id, "action": "make_payment"})).await;
    assert_eq!(payment["entity"], json!("PaymentEntry"));
    a.tool("submit_record", json!({"entity": "PaymentEntry", "id": payment["id"]})).await;
    let invoice = a.tool("get_record", json!({"entity": "SalesInvoice", "id": invoice_id})).await;
    assert_eq!(invoice["status"], json!("Paid"));
    let tb = a.tool("run_report", json!({"name": "trial_balance", "params": {"company": company_id}})).await;
    assert_eq!(tb["meta"]["balanced"], json!(true));
    let trail = a.tool("get_audit_trail", json!({"entity": "SalesInvoice", "id": invoice_id})).await;
    assert_eq!(trail["chain_valid"], json!(true));
    assert!(trail["events"].as_array().unwrap().len() >= 2);

    // Customization by an administrator agent.
    a.tool("customize_entity", json!({"entity": "Customer", "op": {"op": "add_field", "field": {"name": "x_segment", "type": "select", "options": ["SMB", "Enterprise"]}}, "note": "agent test"})).await;
    // Every HTTP call loads a fresh context, which carries the tenant's new schema.
    a.ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "mcp-it".into()).await.unwrap();
    let customer_schema = a.tool("describe_entity", json!({"entity": "Customer"})).await;
    assert_eq!(customer_schema["json_schema"]["properties"]["x_segment"]["enum"], json!(["SMB", "Enterprise"]));

    // ───────────── errors come back in a form a model can act on ─────────────
    let invalid = a.tool_err("create_record", json!({"entity": "SalesInvoice", "data": {"company": company_id}})).await;
    assert_eq!(invalid["error"], json!("validation_failed"));
    assert!(invalid["fields"].as_array().unwrap().iter().any(|f| f["field"] == "customer"), "{invalid}");
    let unknown_tool = a.rpc("tools/call", json!({"name": "drop_database", "arguments": {}})).await;
    assert_eq!(unknown_tool["error"]["code"], json!(-32602));
    assert_eq!(a.rpc("nope/nope", json!({})).await["error"]["code"], json!(-32601));
    let bad = ferrum_mcp::handle(&k, &a.ctx, json!({"id": 1, "method": "ping"})).await.unwrap();
    assert_eq!(bad["error"]["code"], json!(-32600), "jsonrpc 2.0 is required");
    let missing = a.rpc("tools/call", json!({"name": "get_record", "arguments": {"entity": "SalesInvoice"}})).await;
    assert_eq!(missing["error"]["code"], json!(-32602));

    // ───────────── resources and prompts ─────────────
    let res = a.rpc("resources/list", json!({})).await;
    assert!(res["result"]["resources"].as_array().unwrap().iter().any(|r| r["uri"] == "ferrum://entity/SalesInvoice"));
    let read = a.rpc("resources/read", json!({"uri": format!("ferrum://record/SalesInvoice/{invoice_id}")})).await;
    assert!(read["result"]["contents"][0]["text"].as_str().unwrap().contains("Initech"));
    let prompt = a.rpc("prompts/get", json!({"name": "record-sale", "arguments": {"customer": "Initech", "items": "2 staplers"}})).await;
    assert!(prompt["result"]["messages"][0]["content"]["text"].as_str().unwrap().contains("Initech"));

    // ───────────── OAuth for hosted agents ─────────────
    let refused = k.oauth_register(Some("Evil".into()), vec!["http://evil.example/cb".into()]).await;
    assert!(refused.is_err(), "only https, loopback or app-scheme redirects");
    let client = k.oauth_register(Some("Claude".into()), vec!["https://claude.ai/api/mcp/auth_callback".into()]).await.unwrap();
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    let redirect = "https://claude.ai/api/mcp/auth_callback";
    assert!(k.oauth_issue_code(&a.ctx, &client.id, "https://claude.ai/other", challenge, "S256").await.is_err(), "unregistered redirect");
    assert!(k.oauth_issue_code(&a.ctx, &client.id, redirect, challenge, "plain").await.is_err(), "S256 only");
    let code = k.oauth_issue_code(&a.ctx, &client.id, redirect, challenge, "S256").await.unwrap();
    assert!(k.oauth_redeem_code(&client.id, &code, redirect, "wrong-verifier-wrong-verifier-wrong-verifier-x").await.is_err(), "PKCE");
    let grant = k.oauth_redeem_code(&client.id, &code, redirect, verifier).await.expect("redeem");
    assert_eq!((grant.tenant, grant.user), (tenant, session.user));
    assert!(k.oauth_redeem_code(&client.id, &code, redirect, verifier).await.is_err(), "codes are single-use");
    // The replay revoked the refresh token it had produced.
    assert!(k.oauth_refresh(&client.id, &grant.refresh_token).await.is_err(), "a replayed code revokes its tokens");
    let code2 = k.oauth_issue_code(&a.ctx, &client.id, redirect, challenge, "S256").await.unwrap();
    let g2 = k.oauth_redeem_code(&client.id, &code2, redirect, verifier).await.unwrap();
    let g3 = k.oauth_refresh(&client.id, &g2.refresh_token).await.expect("rotation");
    assert_ne!(g2.refresh_token, g3.refresh_token);
    assert!(k.oauth_refresh(&client.id, &g2.refresh_token).await.is_err(), "rotated tokens cannot be reused");
    assert!(k.oauth_refresh(&client.id, &g3.refresh_token).await.is_err(), "reuse revokes the whole family");
    let code3 = k.oauth_issue_code(&a.ctx, &client.id, redirect, challenge, "S256").await.unwrap();
    let g4 = k.oauth_redeem_code(&client.id, &code3, redirect, verifier).await.unwrap();
    k.oauth_revoke(&g4.refresh_token).await.unwrap();
    assert!(k.oauth_refresh(&client.id, &g4.refresh_token).await.is_err(), "revoked");
    assert!(k.oauth_refresh("fc_someone_else", &g4.refresh_token).await.is_err(), "bound to its client");
    // Signing out everywhere ends the agent's access too: its refresh token is refused, and a new
    // consent (under the new token version) works again.
    let code4 = k.oauth_issue_code(&a.ctx, &client.id, redirect, challenge, "S256").await.unwrap();
    let g5 = k.oauth_redeem_code(&client.id, &code4, redirect, verifier).await.unwrap();
    k.logout_everywhere(&a.ctx).await.unwrap();
    assert!(k.oauth_refresh(&client.id, &g5.refresh_token).await.is_err(), "issued before signing out everywhere");
    let fresh = k.load_ctx(tenant, session.user, None, "mcp-it".into()).await.unwrap();
    let code5 = k.oauth_issue_code(&fresh, &client.id, redirect, challenge, "S256").await.unwrap();
    let g6 = k.oauth_redeem_code(&client.id, &code5, redirect, verifier).await.unwrap();
    let g7 = k.oauth_refresh(&client.id, &g6.refresh_token).await.expect("a grant after signing out everywhere");
    assert!(g7.token_version > g5.token_version, "{} after {}", g7.token_version, g5.token_version);
    a.ctx = fresh;

    // ───────────── a sales clerk's agent has the clerk's limits ─────────────
    let clerk = k.create_user(&a.ctx, "clerk@agents.test", "Clerk", Some("Correct-Horse-9"), &["Sales User".into()], false).await.unwrap();
    let clerk_ctx = k.load_ctx(tenant, clerk, None, "mcp-it".into()).await.unwrap();
    let mut c = Agent { k: &k, ctx: clerk_ctx, next: 0 };
    let clerk_tools = c.rpc("tools/list", json!({})).await;
    assert!(!clerk_tools["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "customize_entity"));
    let denied = c.rpc("tools/call", json!({"name": "customize_entity", "arguments": {"entity": "Customer", "op": {}}})).await;
    assert_eq!(denied["error"]["code"], json!(-32602), "admin tools do not exist for non-admins");
    let forbidden = c.tool_err("run_report", json!({"name": "trial_balance"})).await;
    assert_eq!(forbidden["error"], json!("forbidden"));
    let cannot_submit = c.tool_err("submit_record", json!({"entity": "SalesInvoice", "id": invoice_id})).await;
    assert!(cannot_submit["error"] == json!("forbidden") || cannot_submit["error"] == json!("invalid"), "{cannot_submit}");
}
