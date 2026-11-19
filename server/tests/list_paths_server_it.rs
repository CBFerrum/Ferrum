//! The outer paths of the one read path (S9 K0-1), against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is
//! reset by each test, which take their turn): the server as the binary boots it, with an app of this test's compiled
//! in, behind its own router. An AI client at `/mcp` asking a list to filter or sort by a field the caller cannot read,
//! the form preview route asked for a record outside the caller's reach, and a plugin's `data.query` in a hook filtering
//! by a hidden field are each refused: the hidden values cannot be recovered a character at a time through them.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, HookRegistry, Kernel};
use ferrum_server::app::App;
use ferrum_server::extensions::{self, Extension};
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use serde_json::{Value, json};
use serial_test::serial;
use std::collections::BTreeMap;
use std::sync::{Arc, Once};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "Correct-Horse-9";
const HOST: &str = "erp.paths.test";

const MANIFEST: &str = "id: lps
name: Paths
version: 0.1.0
publisher: ferrum
summary: Records with a hidden field
categories: [developer]
license: AGPL-3.0-or-later
runtime: native
platform: \">=0.4, <0.5\"
packages: [lps]
";

const PACKAGE: &str = "package: lps\nversion: 0.1.0\ndepends: [core]\nroles:\n  - name: LpsClerk\n    permissions:\n      - {entity: LpsStaff, read: true, write: true, create: true}\n      - {entity: LpsCase, read: true, write: true, create: true, if_owner: true}\n";
const ENTITIES: &str = r#"
- name: LpsStaff
  label: Staff
  kind: master
  title_field: staff_name
  fields:
    - {name: staff_name, type: data, required: true}
    - {name: bank_account, type: data, perm_group: bank}
- name: LpsCase
  label: Case
  kind: master
  title_field: subject
  fields:
    - {name: subject, type: data, required: true}
"#;

fn packages() -> Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/lps.yaml", ENTITIES)].into_iter().map(|(p, d)| (p.to_string(), d.to_string())).collect();
    Ok(vec![ferrum_meta::catalog::load_package_files(&files)?])
}

fn register(_: &mut HookRegistry) {}

fn extend() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        extensions::extend(vec![Extension {
            id: "lps",
            packages,
            register,
            reports: Vec::new,
            maintenance: |_, _| Box::pin(async { Ok(()) }),
            manifest: MANIFEST,
            ui: &[],
            tasks: |_| {},
            metrics: |_| Box::pin(async { Ok(String::new()) }),
        }]);
    });
}

struct World {
    st: AppState,
    k: Arc<Kernel>,
    r: axum::Router,
    admin: Ctx,
    clerk: Ctx,
    /// The clerk's API key, for an AI client.
    key: String,
    /// A staff record whose bank account starts with 1.
    asha: Uuid,
}

async fn world() -> World {
    extend();
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    {
        let pool = sqlx::PgPool::connect(&url).await.expect("connect");
        common::reset(&pool).await;
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut config = Environment::Test.load_from_folder(&dir.join("config")).expect("config/test.yaml");
    ferrum_server::settings::set_database_url(&mut config, url);
    if let Some(s) = config.settings.as_mut() {
        s["packages_dir"] = json!(dir.join("../packages").to_string_lossy());
    }
    let base = <App as Hooks>::after_context(AppContext::builder(Environment::Test, config).build()).await.expect("boot");
    let st = base.shared_store.get::<AppState>().expect("state");
    let k = st.kernel.clone();
    let router = App::routes(&base).to_router::<App>(base.clone(), axum::Router::new()).expect("router");
    let r = App::after_routes(router, &base).await.expect("layers");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "paths".into(),
            name: "Paths".into(),
            admin_email: "admin@paths.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec!["lps".into()],
            settings: None,
        })
        .await
        .unwrap();
    k.add_operator_host("paths", HOST, "shell", None).await.unwrap();
    let s = k.login("paths", "admin@paths.test", PASSWORD).await.unwrap();
    let admin = k.load_ctx(tenant, s.user, None, "it".into()).await.unwrap();
    let staff = |name: &str, bank: &str| json!({"staff_name": name, "bank_account": bank}).as_object().cloned().unwrap();
    let asha = k.insert(&admin, "LpsStaff", staff("Asha", "1111-2222")).await.unwrap();
    k.insert(&admin, "LpsStaff", staff("Bo", "9999-8888")).await.unwrap();
    let clerk_id = k.create_user(&admin, "clerk@paths.test", "Clerk", Some(PASSWORD), &["LpsClerk".into()], false).await.unwrap();
    let clerk = k.load_ctx(tenant, clerk_id, None, "it".into()).await.unwrap();
    let key = k.create_api_key(&admin, clerk_id, "it", None, None).await.unwrap().1;
    World { st: st.clone(), k, r, admin, clerk, key, asha: asha["id"].as_str().unwrap().parse().unwrap() }
}

fn run<F: std::future::Future<Output = ()> + Send + 'static>(f: impl FnOnce() -> F) {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(f()).await.unwrap() });
}

async fn post(r: &axum::Router, path: &str, key: &str, extra: &[(&str, &str)], body: Value) -> (StatusCode, String) {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::HOST, HOST)
        .header("x-forwarded-for", "203.0.113.7")
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header(header::CONTENT_TYPE, "application/json");
    for (n, v) in extra {
        req = req.header(*n, *v);
    }
    let res = r.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = res.status();
    (status, String::from_utf8_lossy(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).into_owned())
}

/// An MCP `invoke_capability` of `lps.lps_staff.list` with `input`, as an AI client holding the clerk's key asks it.
async fn invoke_list(w: &World, input: Value) -> Value {
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "invoke_capability", "arguments": {"capability": "lps.lps_staff.list", "input": input}}});
    let (status, body) = post(&w.r, "/mcp", &w.key, &[("mcp-protocol-version", "2025-11-25"), ("accept", "application/json, text/event-stream")], call).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let line = body.lines().find(|l| l.trim_start().starts_with('{') || l.starts_with("data:")).unwrap_or(&body);
    serde_json::from_str(line.trim_start_matches("data:").trim()).unwrap_or_else(|_| panic!("{body}"))
}

#[test]
#[serial]
fn invoke_list_cannot_narrow_on_bank_numbers() {
    run(|| async {
        let w = world().await;
        // A filter a character at a time would recover the number; a sort would order by it: each refused.
        for input in [json!({"filters": [["bank_account", "like", "1%"]]}), json!({"sort": "bank_account asc"})] {
            let out = invoke_list(&w, input.clone()).await;
            let text = out.to_string();
            assert!(out["result"]["isError"] == json!(true) || out.get("error").is_some(), "refused: {input} gave {text}");
            assert!(!text.contains("Asha"), "nothing of the rows: {text}");
        }
        // A filter on a field they read answers.
        let out = invoke_list(&w, json!({"filters": [["staff_name", "=", "Asha"]]})).await;
        assert_eq!(out["result"]["isError"], json!(false), "{out}");
        assert!(out.to_string().contains("Asha") && !out.to_string().contains("1111-2222"), "{out}");
    });
}

#[test]
#[serial]
fn preview_route_answers_not_found_outside_reach() {
    run(|| async {
        let w = world().await;
        let theirs = w.k.insert(&w.admin, "LpsCase", json!({"subject": "The administrator's"}).as_object().cloned().unwrap()).await.unwrap();
        let mine = w.k.insert(&w.clerk, "LpsCase", json!({"subject": "Mine"}).as_object().cloned().unwrap()).await.unwrap();
        let (status, body) = post(&w.r, "/api/v1/LpsCase/_calc", &w.key, &[], json!({"id": theirs["id"]})).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(!body.contains("The administrator's"), "{body}");
        let (status, body) = post(&w.r, "/api/v1/LpsCase/_calc", &w.key, &[], json!({"id": mine["id"]})).await;
        assert!(status == StatusCode::OK && body.contains("Mine"), "{status} {body}");
    });
}

/// A plugin, in WebAssembly text, whose hook asks the host `request` and answers with the host's reply: an error from
/// the host is the hook's veto.
fn querying_plugin(request: &str) -> String {
    let escaped: String = request.bytes().map(|b| format!("\\{b:02x}")).collect();
    format!(
        r#"(module
  (import "ferrum" "host_call" (func $host (param i32 i32) (result i64)))
  (memory (export "memory") 2)
  (global $next (mut i32) (i32.const 4096))
  (func (export "ferrum_alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $next))
    (global.set $next (i32.add (global.get $next) (local.get $n)))
    (local.get $p))
  (data (i32.const 16) "{escaped}")
  (func (export "ferrum_hook") (param i32 i32) (result i64)
    (call $host (i32.const 16) (i32.const {len}))))"#,
        len = request.len()
    )
}

#[test]
#[serial]
fn a_plugin_query_refuses_a_hidden_filter() {
    run(|| async {
        let w = world().await;
        let request = json!({"op": "data.query", "args": {"entity": "LpsStaff", "filters": [["bank_account", "like", "1%"]]}}).to_string();
        let plugin: ferrum_plugins::NewPlugin = serde_json::from_value(json!({
            "name": "probe",
            "hooks": [{"entity": "LpsCase", "stage": "validate"}],
            "permissions": [{"entity": "LpsStaff", "read": true}]
        }))
        .unwrap();
        w.st.plugins.install(&w.k, &w.admin, plugin, querying_plugin(&request).as_bytes()).await.unwrap();
        // The clerk's save runs the hook as the clerk: the query by their hidden field is refused, and so the save.
        let e = w.k.insert(&w.clerk, "LpsCase", json!({"subject": "Probe"}).as_object().cloned().unwrap()).await.unwrap_err();
        assert!(e.to_string().contains("bank_account"), "{e}");
        let _ = w.asha;
        // The administrator, who reads the field, saves: the same query answers them.
        w.k.insert(&w.admin, "LpsCase", json!({"subject": "Probe"}).as_object().cloned().unwrap()).await.expect("the administrator's query answers");
    });
}
