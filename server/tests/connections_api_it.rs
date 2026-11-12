//! Connections over HTTP and in packs (BD-29 to BD-33), against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, reset
//! first): the inbound address on the server's own handler (accepted once, every refusal the same 401), the callback
//! page (a state not started here goes nowhere), a package's connection offered turned off, and a pack carrying a
//! connection's description but never its secret, installed turned off.

mod common;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use ferrum_kernel::connections::ConnectionInput;
use ferrum_kernel::pack::ExportOptions;
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_server::controllers::connections::{Back, callback_page, inbound_answer};
use ferrum_server::state::RateLimiter;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path as FsPath;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const TOKEN: &str = "tok-Hh31-inbound-token";
const PACKAGE: &str = "package: alerts\nversion: 0.1.0\ndepends: [core]\nconnections:\n  - name: pager\n    label: Pager\n    description:\n      test: true\n      operations:\n        page: {method: POST, body: '{}', changes: false}\n";
const ENTITIES: &str = "name: Alert\nlabel: Alert\nkind: master\nfields:\n  - {name: external_id, type: data}\n  - {name: title, type: data}\n";

type S = (Arc<Kernel>, Arc<RateLimiter>, Ctx);

async fn serve(k: Arc<Kernel>, admin: Ctx) -> String {
    let state: S = (k, Arc::new(RateLimiter::local_only("public", Duration::from_secs(60), 100)), admin);
    let app = Router::new()
        .route(
            "/api/connections/inbound/{id}",
            post(|State(s): State<S>, Path(id): Path<Uuid>, h: HeaderMap, body: Bytes| async move {
                inbound_answer(&s.0, &s.1, &h, id, &body).await.map_err(|e| e.into_response())
            }),
        )
        .route(
            "/api/connections/callback",
            get(|State(s): State<S>, Query(b): Query<BTreeMap<String, String>>| async move {
                let back = Back { state: b.get("state").cloned(), code: b.get("code").cloned(), error: None };
                callback_page(&s.0, &s.2, Some(Uuid::now_v7()), &back).await.map_err(|e| e.into_response())
            }),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn workspace(k: &Kernel, slug: &str) -> Ctx {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["alerts".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap()
}

#[test]
fn connections_are_called_and_carried() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    common::reset(&db.pool).await;
    let core = ferrum_meta::catalog::load_package_dir(&FsPath::new(env!("CARGO_MANIFEST_DIR")).join("../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/alert.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let alerts = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let settings = KernelSettings { secret_key: Some(SecretKey::new([7u8; 32])), public_url: Some("https://ferrum.example.test".into()), ..Default::default() };
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, alerts]).unwrap(), HookRegistry::default(), settings, None).await.expect("boot");
    let admin = workspace(&k, "acme").await;

    // ── a package's connection is offered turned off; saving it takes it up ──
    let listed = k.connection_defs(&admin).await.unwrap();
    let offered = listed.iter().find(|c| c["name"] == "pager").expect("offered");
    assert_eq!((offered["id"].clone(), offered["enabled"].clone(), offered["source"].clone()), (Value::Null, json!(false), json!("package:alerts")));

    // ── the inbound address, over HTTP ──
    let desc = json!({"test": true, "inbound": {"verify": {"type": "token", "header": "x-token", "secret": "token"}, "delivery_header": "x-delivery",
        "map": {"entity": "Alert", "by": {"external_id": "item.id"}, "set": {"title": "item.title"}}}});
    let input: ConnectionInput =
        serde_json::from_value(json!({"name": "monitor", "label": "Monitor", "description": desc, "secrets": {"token": TOKEN}, "enabled": true})).unwrap();
    let id = k.save_connection_def(&admin, None, &input).await.unwrap();
    let base = serve(k.clone(), admin.clone()).await;
    let http = reqwest::Client::new();
    let post = |to: Uuid, token: &'static str, delivery: &'static str| {
        let (http, base) = (http.clone(), base.clone());
        async move {
            let r = http
                .post(format!("{base}/api/connections/inbound/{to}"))
                .header("x-token", token)
                .header("x-delivery", delivery)
                .json(&json!({"id": "A-1", "title": "Disk full"}))
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }
    };
    assert_eq!(post(id, TOKEN, "d-1").await, (200, json!({"accepted": true}).to_string()));
    let refused = post(id, TOKEN, "d-1").await;
    assert_eq!(refused.0, 401, "a replay");
    assert_eq!(post(id, "tok-wrong", "d-2").await, refused, "a wrong token: the same answer");
    assert_eq!(post(Uuid::now_v7(), TOKEN, "d-3").await, refused, "an unknown address: the same answer");

    // ── the callback goes nowhere: a page says how it went ──
    let page = http.get(format!("{base}/api/connections/callback?state=forged&code=x")).send().await.unwrap();
    assert_eq!(page.status().as_u16(), 401);
    assert_eq!(page.headers()["cache-control"], "no-store");
    assert!(page.headers().get("location").is_none());
    assert_eq!(http.get(format!("{base}/api/connections/callback")).send().await.unwrap().status().as_u16(), 400);

    // ── a pack carries the description, never the secret; installed turned off ──
    let pack = k.export_pack(&admin, &ExportOptions { name: "conns".into(), label: "Connections".into(), version: 1, records: vec![] }).await.unwrap();
    let carried = pack["connections"].as_array().unwrap();
    assert!(carried.iter().any(|c| c["name"] == "monitor"));
    assert!(!pack.to_string().contains(TOKEN), "no secret travels");
    let other = workspace(&k, "other").await;
    let plan = k.plan_pack(&other, &pack).await.unwrap();
    k.install_pack(&other, &pack, plan["digest"].as_str().unwrap()).await.unwrap();
    let there = k.connection_defs(&other).await.unwrap();
    let monitor = there.iter().find(|c| c["name"] == "monitor").unwrap();
    assert_eq!((monitor["enabled"].clone(), monitor["secrets"].clone(), monitor["source"].clone()), (json!(false), json!([]), json!("pack:conns")));
    let plan = k.plan_pack(&other, &pack).await.unwrap();
    k.install_pack(&other, &pack, plan["digest"].as_str().unwrap()).await.unwrap();
    assert_eq!(k.connection_defs(&other).await.unwrap().iter().filter(|c| c["name"] == "monitor").count(), 1, "installed again, unchanged");
}
