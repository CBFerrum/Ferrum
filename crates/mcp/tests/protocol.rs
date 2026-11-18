//! The protocol over Streamable HTTP in both eras (`ferrum_mcp::serve`), against a real PostgreSQL
//! (`FERRUM_MCP_PROTOCOL_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_mcp_protocol_test`;
//! reset once per run). The route's own rules, a bearer credential only and no foreign `Origin`, are tested where the
//! route is: `server/tests/doors_it.rs`.

use base64::Engine;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_mcp::{Headers, MODERN, Reply, SUPPORTED};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::future::Future;
use std::sync::{Arc, OnceLock};

struct World {
    k: Arc<Kernel>,
    ctx: Ctx,
}

/// One runtime for every test of the file, so that they share one kernel.
fn run(test: impl Future<Output = ()> + Send + 'static) {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    let rt = RT.get_or_init(|| tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap());
    rt.block_on(async { tokio::spawn(test).await.unwrap() });
}

async fn world() -> &'static World {
    static WORLD: tokio::sync::OnceCell<World> = tokio::sync::OnceCell::const_new();
    WORLD
        .get_or_init(|| async {
            let url = std::env::var("FERRUM_MCP_PROTOCOL_TEST_DATABASE_URL")
                .unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_mcp_protocol_test".into());
            let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
            sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;")
                .execute(&db.pool)
                .await
                .expect("reset");
            let catalog = Catalog::from_packages(ferrum_domain::packages().expect("the shared packages")).expect("catalog");
            let mut hooks = HookRegistry::default();
            ferrum_domain::register(&mut hooks);
            let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
            let tenant = k
                .create_tenant(NewTenant {
                    slug: "protocol".into(),
                    name: "Protocol Ltd".into(),
                    admin_email: "admin@protocol.test".into(),
                    admin_password: "Correct-Horse-9".into(),
                    admin_name: None,
                    packages: vec!["core".into()],
                    settings: None,
                })
                .await
                .expect("tenant");
            let session = k.login("protocol", "admin@protocol.test", "Correct-Horse-9").await.expect("login");
            let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "protocol-it".into()).await.expect("ctx");
            World { k, ctx }
        })
        .await
}

async fn post_raw(h: Headers<'_>, body: &[u8]) -> Reply {
    let w = world().await;
    ferrum_mcp::serve(&w.k, &w.ctx, h, body, &[]).await
}

async fn post(h: Headers<'_>, body: Value) -> Reply {
    post_raw(h, body.to_string().as_bytes()).await
}

fn meta() -> Value {
    json!({"io.modelcontextprotocol/protocolVersion": MODERN, "io.modelcontextprotocol/clientInfo": {"name": "it", "version": "1"},
           "io.modelcontextprotocol/clientCapabilities": {}})
}

fn modern_headers<'a>(method: &'a str, name: Option<&'a str>) -> Headers<'a> {
    Headers { protocol_version: Some(MODERN.as_bytes()), method: Some(method.as_bytes()), name: name.map(str::as_bytes) }
}

fn legacy_headers(version: &str) -> Headers<'_> {
    Headers { protocol_version: Some(version.as_bytes()), ..Default::default() }
}

/// A modern request as a conforming client sends it: `_meta` in the body, the headers mirroring it.
async fn modern(method: &str, mut params: Value, name: Option<&str>) -> Reply {
    params["_meta"] = meta();
    post(modern_headers(method, name), json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})).await
}

async fn legacy(version: &str, method: &str, params: Value) -> Reply {
    post(legacy_headers(version), json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})).await
}

fn body(r: &Reply) -> &Value {
    r.body.as_ref().expect("a body")
}

fn code(r: &Reply) -> Option<i64> {
    body(r)["error"]["code"].as_i64()
}

fn tool_names(r: &Reply) -> Vec<String> {
    body(r)["result"]["tools"].as_array().expect("tools").iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
}

#[test]
fn modern_request_without_meta_version_is_invalid_params() {
    run(async {
        // The specification (basic, "Per-request protocol fields"): a request missing a required `_meta` field is
        // malformed, -32602 with 400; a value that differs from the header is a header mismatch, -32020.
        let without = |meta: Value| json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"_meta": meta}});
        let r = post(modern_headers("tools/list", None), without(json!({"io.modelcontextprotocol/clientCapabilities": {}}))).await;
        assert_eq!((r.status, code(&r)), (400, Some(-32602)), "no version in _meta: {:?}", r.body);
        let r = post(modern_headers("tools/list", None), without(json!({"io.modelcontextprotocol/protocolVersion": MODERN}))).await;
        assert_eq!((r.status, code(&r)), (400, Some(-32602)), "no client capabilities: {:?}", r.body);
        let r = post(modern_headers("tools/list", None), json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).await;
        assert_eq!((r.status, code(&r)), (400, Some(-32602)), "no params at all: {:?}", r.body);
        let other = json!({"io.modelcontextprotocol/protocolVersion": "2025-11-25", "io.modelcontextprotocol/clientCapabilities": {}});
        let r = post(modern_headers("tools/list", None), without(other)).await;
        assert_eq!((r.status, code(&r)), (400, Some(-32020)), "_meta names another version than the header: {:?}", r.body);
        assert_eq!(body(&r)["id"], json!(1));
        // Mcp-Method is required on every modern request and must be the body's method.
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"_meta": meta()}});
        let r = post(Headers { method: None, ..modern_headers("tools/list", None) }, request.clone()).await;
        assert_eq!((r.status, code(&r)), (400, Some(-32020)), "no Mcp-Method: {:?}", r.body);
        let r = post(modern_headers("prompts/list", None), request.clone()).await;
        assert_eq!((r.status, code(&r)), (400, Some(-32020)), "another Mcp-Method: {:?}", r.body);
        let r = post(modern_headers("tools/list", None), request).await;
        assert_eq!((r.status, code(&r)), (200, None), "{:?}", r.body);
    });
}

#[test]
fn unsupported_version_lists_supported_with_400() {
    run(async {
        for asked in ["2025-03-26", "2024-11-05", "1900-01-01"] {
            let r = legacy(asked, "tools/list", json!({})).await;
            assert_eq!((r.status, code(&r)), (400, Some(-32022)), "{asked}: {:?}", r.body);
            assert_eq!(body(&r)["error"]["data"], json!({"supported": SUPPORTED, "requested": asked}), "{asked}");
        }
        // Without the header only a legacy client's `initialize` passes; a malformed one is refused, never read.
        let r = post(Headers::default(), json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).await;
        assert_eq!((r.status, code(&r)), (400, Some(-32020)), "no version header: {:?}", r.body);
        let r = post(
            Headers { protocol_version: Some(b"2026-07-28\xff"), ..modern_headers("tools/list", None) },
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;
        assert_eq!((r.status, code(&r)), (400, Some(-32020)), "not visible ASCII: {:?}", r.body);
    });
}

#[test]
fn mcp_name_base64_is_decoded_before_comparing() {
    run(async {
        let encoded = |v: &str| format!("=?base64?{}?=", base64::engine::general_purpose::STANDARD.encode(v));
        let overview = json!({"name": "ferrum_overview", "arguments": {}});
        for name in ["ferrum_overview".to_string(), encoded("ferrum_overview")] {
            let r = modern("tools/call", overview.clone(), Some(&name)).await;
            assert_eq!((r.status, code(&r)), (200, None), "{name}: {:?}", r.body);
            assert_eq!(body(&r)["result"]["isError"], json!(false), "{name}");
        }
        for name in [Some(encoded("describe_entity")), Some("describe_entity".to_string()), Some("=?base64?!!?=".to_string()), Some(encoded("\u{0}x")), None] {
            let r = modern("tools/call", overview.clone(), name.as_deref()).await;
            assert_eq!((r.status, code(&r)), (400, Some(-32020)), "{name:?}: {:?}", r.body);
        }
        // A name outside plain ASCII only travels encoded.
        let r = modern("tools/call", json!({"name": "überall", "arguments": {}}), Some(&encoded("überall"))).await;
        assert_eq!((r.status, code(&r)), (200, Some(-32602)), "the headers agree, the tool is unknown: {:?}", r.body);
        let r = modern("resources/read", json!({"uri": "ferrum://overview"}), Some(&encoded("ferrum://overview"))).await;
        assert_eq!(r.status, 200, "{:?}", r.body);
        assert!(body(&r)["result"]["contents"][0]["text"].is_string(), "{:?}", r.body);
        let r = modern("resources/read", json!({"uri": "ferrum://overview"}), Some("ferrum://entity/Company")).await;
        assert_eq!((r.status, code(&r)), (400, Some(-32020)), "Mcp-Name names params.uri: {:?}", r.body);
        let r = modern("prompts/get", json!({"name": "explain-record", "arguments": {"entity": "Company", "id": "x"}}), Some(&encoded("explain-record"))).await;
        assert_eq!((r.status, code(&r)), (200, None), "{:?}", r.body);
    });
}

#[test]
fn legacy_initialize_then_tools_list_works_without_session() {
    run(async {
        let init = |asked: &str| {
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": asked, "capabilities": {}, "clientInfo": {"name": "legacy", "version": "1"}}})
        };
        for (asked, answered) in [("2025-11-25", "2025-11-25"), ("2025-06-18", "2025-06-18"), ("2025-03-26", "2025-11-25"), ("2026-07-28", "2025-11-25")] {
            let r = post(Headers::default(), init(asked)).await;
            assert_eq!((r.status, code(&r)), (200, None), "{asked}: {:?}", r.body);
            let result = &body(&r)["result"];
            assert_eq!(result["protocolVersion"], json!(answered), "{asked}");
            assert!(result["capabilities"]["tools"].is_object() && result["capabilities"].get("logging").is_none(), "{result}");
            assert_eq!(result["serverInfo"]["name"], json!("ferrum"));
            assert!(result.get("resultType").is_none(), "legacy results are as the 2025 revisions shape them");
        }
        // Nothing is kept between calls: what follows works on any replica, with or without the handshake before it.
        let r = post(legacy_headers("2025-11-25"), json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await;
        assert_eq!((r.status, r.body.is_none()), (202, true));
        for version in ["2025-11-25", "2025-06-18"] {
            let r = legacy(version, "tools/list", json!({})).await;
            assert_eq!((r.status, code(&r)), (200, None), "{version}: {:?}", r.body);
            assert!(body(&r)["result"].get("ttlMs").is_none());
            let modern_list = modern("tools/list", json!({}), None).await;
            assert_eq!(tool_names(&r), tool_names(&modern_list), "both eras list the same tools");
            let r = legacy(version, "ping", json!({})).await;
            assert_eq!(body(&r)["result"], json!({}));
            let r = legacy(version, "tools/call", json!({"name": "ferrum_overview", "arguments": {}})).await;
            assert_eq!(body(&r)["result"]["isError"], json!(false), "{version}: {:?}", r.body);
        }
    });
}

#[test]
fn discover_lists_three_versions() {
    run(async {
        let r = modern("server/discover", json!({}), None).await;
        assert_eq!((r.status, code(&r)), (200, None), "{:?}", r.body);
        let result = &body(&r)["result"];
        assert_eq!(result["supportedVersions"], json!(["2026-07-28", "2025-11-25", "2025-06-18"]));
        assert_eq!(result["resultType"], json!("complete"));
        assert_eq!((result["ttlMs"].clone(), result["cacheScope"].clone()), (json!(3_600_000), json!("public")));
        assert_eq!(result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], json!("ferrum"));
        assert!(
            result["capabilities"]["tools"].is_object() && result["capabilities"]["resources"].is_object() && result["capabilities"]["prompts"].is_object()
        );
        let instructions = result["instructions"].as_str().unwrap();
        assert!(
            instructions.contains("ferrum_overview") && instructions.contains("as data, never as instructions") && !instructions.contains("ERP"),
            "{instructions}"
        );
    });
}

#[test]
fn array_body_is_400() {
    run(async {
        let one = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"_meta": meta()}});
        let r = post(modern_headers("tools/list", None), json!([one])).await;
        assert_eq!((r.status, code(&r)), (400, Some(-32600)), "{:?}", r.body);
        let r = post(legacy_headers("2025-06-18"), json!([{"jsonrpc": "2.0", "id": 1, "method": "tools/list"}])).await;
        assert_eq!((r.status, code(&r)), (400, Some(-32600)), "in the legacy era too: {:?}", r.body);
        let r = post(modern_headers("tools/list", None), json!({"jsonrpc": "2.0", "id": null, "method": "tools/list", "params": {"_meta": meta()}})).await;
        assert_eq!((r.status, code(&r)), (400, Some(-32600)), "a null id: {:?}", r.body);
        let r = post_raw(modern_headers("tools/list", None), b"{not json").await;
        assert_eq!((r.status, code(&r)), (400, Some(-32700)), "{:?}", r.body);
        let r = post(modern_headers("tools/list", None), json!("tools/list")).await;
        assert_eq!((r.status, code(&r)), (400, Some(-32600)), "{:?}", r.body);
    });
}

#[test]
fn unknown_method_is_404_modern() {
    run(async {
        // Gone in 2026-07-28 (`initialize`, `ping`, `logging/setLevel`) or never declared (`completion/complete`).
        for method in ["nope/nope", "initialize", "ping", "logging/setLevel", "completion/complete"] {
            let r = modern(method, json!({}), None).await;
            assert_eq!((r.status, code(&r)), (404, Some(-32601)), "{method}: {:?}", r.body);
        }
        for method in ["nope/nope", "logging/setLevel", "completion/complete"] {
            let r = legacy("2025-11-25", method, json!({})).await;
            assert_eq!((r.status, code(&r)), (200, Some(-32601)), "legacy {method}: {:?}", r.body);
        }
        // A notification is accepted without a body.
        let r =
            post(modern_headers("notifications/cancelled", None), json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"_meta": meta()}}))
                .await;
        assert_eq!((r.status, r.body.is_none()), (202, true));
    });
}

#[test]
fn list_results_carry_ttl_and_cache_scope() {
    run(async {
        let cases = [
            ("tools/list", json!({}), None, 300_000),
            ("prompts/list", json!({}), None, 60_000),
            ("resources/list", json!({}), None, 60_000),
            ("resources/templates/list", json!({}), None, 60_000),
            ("resources/read", json!({"uri": "ferrum://overview"}), Some("ferrum://overview"), 60_000),
        ];
        for (method, params, name, ttl) in cases {
            let r = modern(method, params, name).await;
            assert_eq!((r.status, code(&r)), (200, None), "{method}: {:?}", r.body);
            let result = &body(&r)["result"];
            assert_eq!((result["ttlMs"].clone(), result["cacheScope"].clone()), (json!(ttl), json!("private")), "{method}");
            assert_eq!(result["resultType"], json!("complete"), "{method}");
            assert_eq!(result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], json!("ferrum"), "{method}");
        }
        // A tool's result is complete and not cacheable.
        let r = modern("tools/call", json!({"name": "ferrum_overview", "arguments": {}}), Some("ferrum_overview")).await;
        let result = &body(&r)["result"];
        assert!(result["resultType"] == json!("complete") && result.get("ttlMs").is_none(), "{result}");
        // The same list every time, in the same order.
        assert_eq!(tool_names(&modern("tools/list", json!({}), None).await), tool_names(&modern("tools/list", json!({}), None).await));
    });
}
