//! FLC-CAP-001 (AT-CAP-001): the builder's endpoints, an agent's tools and the kernel describe each
//! capability alike (version, effect, rights, input and output, completion, digest), over ERP's real
//! packages, against a real PostgreSQL (`FERRUM_STUDIO_TEST_DATABASE_URL`; the database is reset first).
//! A receipt made through one path answers the others.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, HookRegistry, Kernel, KernelSettings, Site, SiteRequest};
use ferrum_meta::Catalog;
use serde_json::{Map, Value, json};
use std::time::Instant;

#[test]
fn one_catalog_whoever_asks() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn site(k: &Kernel, ctx: &Ctx, method: &str, path: &str, query: &[(&str, &str)], headers: &[(&str, &str)], body: Value) -> Value {
    let req = SiteRequest {
        tenant: ctx.tenant,
        user: Some(ctx.clone()),
        method: method.into(),
        path: path.into(),
        query: query.iter().map(|(k, v)| (k.to_string(), json!(v))).collect(),
        headers: headers.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<Map<String, Value>>(),
        body: if body.is_null() { vec![] } else { body.to_string().into_bytes() },
        client_ip: "127.0.0.1".into(),
    };
    let resp = ferrum_studio::site::StudioSite.handle(k, req).await.expect("studio answers");
    serde_json::from_slice(&resp.body).expect("json")
}

async fn tool(k: &Kernel, ctx: &Ctx, name: &str, arguments: Value) -> Value {
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": arguments}});
    let out = ferrum_mcp::handle(k, ctx, msg).await.expect("an answer");
    assert_eq!(out["result"]["isError"], json!(false), "{out}");
    out["result"]["structuredContent"].clone()
}

async fn scenario() {
    let url = std::env::var("FERRUM_STUDIO_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_studio_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("platform packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    packages.extend(ferrum_studio::packages().expect("studio packages"));
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    ferrum_studio::register(&mut hooks);
    let k = Kernel::boot(db, Catalog::from_packages(packages).expect("catalog"), hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "cat".into(),
            name: "Catalog Traders".into(),
            admin_email: "admin@cat.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["selling".into(), "buying".into(), "studio".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("cat", "admin@cat.test", "Correct-Horse-9").await.expect("login");
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");

    let begun = Instant::now();
    let kernel = k.catalog(&admin);
    let took = begun.elapsed();
    println!("catalog: {} capabilities over ERP's selling and buying in {took:?}", kernel.len());
    assert!(kernel.len() > 100, "every installed entity offers its operations: {}", kernel.len());

    // The builder's endpoint and the agent's tool list the same descriptors, digest for digest.
    let listed = site(&k, &admin, "GET", "/catalog", &[], &[], Value::Null).await;
    let builder: Vec<Value> = listed["capabilities"].as_array().unwrap().clone();
    let from_kernel: Vec<Value> = kernel.iter().map(|c| serde_json::to_value(c).unwrap()).collect();
    assert_eq!(builder, from_kernel, "the builder sees what the kernel offers");
    let agent = tool(&k, &admin, "list_capabilities", json!({"q": "sales_order", "limit": 200})).await;
    let agent: Vec<Value> = agent["capabilities"].as_array().unwrap().clone();
    assert!(!agent.is_empty());
    for c in &agent {
        let same = from_kernel.iter().find(|k| k["id"] == c["id"]).expect("the agent lists nothing the kernel does not");
        assert_eq!(c, same, "{}", c["id"]);
    }
    let one = site(&k, &admin, "GET", "/catalog/selling.sales_order.submit", &[], &[], Value::Null).await;
    assert_eq!(one["effect"], json!("transactional_action"));
    assert_eq!(one["idempotency"], json!("native_receipt"));
    assert!(listed["events"].as_array().unwrap().iter().any(|e| e["id"] == json!("selling.sales_order.submitted")), "events are listed beside capabilities");

    // A receipt made through the builder's endpoint answers the agent's call under the same key.
    let customer = json!({"values": {"customer_name": "Pia Garage"}});
    let first =
        site(&k, &admin, "POST", "/catalog/parties.customer.create/invoke", &[], &[("idempotency-key", "onboard-pia")], json!({"input": customer})).await;
    assert_eq!(first["repeated"], json!(false), "{first}");
    let again = tool(&k, &admin, "invoke_capability", json!({"capability": "parties.customer.create", "input": customer, "effect_key": "onboard-pia"})).await;
    assert_eq!(again["repeated"], json!(true), "{again}");
    assert_eq!(again["result"]["id"], first["result"]["id"]);
}
