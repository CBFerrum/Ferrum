//! An app whose code runs on its publisher's servers (`runtime: service`), against a real
//! PostgreSQL (`FERRUM_APPS_TEST_DATABASE_URL`, reset first) and a stand-in for the service on a
//! local port: the consent names where it runs; installing gives it an account of its own with the
//! records its manifest names and a key it is told of; it hears which customer changed, signed and
//! without the record, and reads it with its key; turning the app off stops both; removing it tells
//! the service and revokes the key; a service that refuses its installation leaves nothing behind.

mod common;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use bytes::Bytes;
use ferrum_apps::Bundle;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_plugins::PluginHost;
use ferrum_server::appmgr::AppManager;
use ferrum_server::state::{AppsSettings, Uploads};
use loco_rs::storage::{Storage, drivers::mem};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path as FsPath;
use std::sync::{Arc, Mutex};

/// What the service was sent: path, headers, body.
#[derive(Clone, Default)]
struct Service {
    calls: Arc<Mutex<Vec<(String, HeaderMap, Bytes)>>>,
}

async fn call(State(s): State<Service>, Path(path): Path<String>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let refused = path == "install" && serde_json::from_slice::<Value>(&body).is_ok_and(|v| v["app"] == "refused");
    s.calls.lock().unwrap().push((path, headers, body));
    if refused { StatusCode::INTERNAL_SERVER_ERROR } else { StatusCode::OK }
}

fn app(id: &str, url: &str) -> Bytes {
    let yaml = format!(
        "id: {id}\nname: {id}\nversion: 1.0.0\npublisher: acme\nsummary: Hears of customers\ncategories: [crm]\nruntime: service\nplatform: \"^0.3\"\n\
         access:\n  - {{entity: Customer, read: true}}\nservice:\n  url: {url}/ferrum\n  events: [Customer.*]\n"
    );
    let files: BTreeMap<String, Vec<u8>> = [("app.yaml".to_string(), yaml.into_bytes())].into();
    Bytes::from(Bundle::from_files(files).unwrap().write().unwrap())
}

fn signed(secret: &str, headers: &HeaderMap, body: &[u8]) -> bool {
    let sig = headers.get("x-ferrum-signature").and_then(|v| v.to_str().ok()).unwrap_or_default();
    let t = sig.strip_prefix("t=").and_then(|s| s.split_once(',')).and_then(|(t, _)| t.parse().ok()).unwrap_or_default();
    ferrum_kernel::outbox::signature(secret, t, body).is_ok_and(|expected| expected == sig)
}

#[test]
fn an_app_runs_on_its_publishers_servers_as_an_account_of_its_own() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_APPS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_apps_api_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    common::reset(&db.pool).await;
    let dir = FsPath::new(env!("CARGO_MANIFEST_DIR")).join("../packages");
    let packages = ["core", "contacts", "parties"].map(|p| ferrum_meta::catalog::load_package_dir(&dir.join(p)).expect(p));
    let settings = KernelSettings { webhooks_allow_private: true, public_url: Some("http://acme.example".into()), ..Default::default() };
    let k = Kernel::boot(db, Catalog::from_packages(packages.to_vec()).unwrap(), HookRegistry::default(), settings, None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into(), "contacts".into(), "parties".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();

    let service = Service::default();
    let router = Router::new().route("/ferrum/{path}", post(call)).with_state(service.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let apps = AppsSettings { registries: String::new(), trusted_keys: String::new(), uploads: Uploads::Cluster, allow_unsigned_uploads: true };
    let manager = AppManager::new(k.clone(), PluginHost::new(false).unwrap(), Arc::new(Storage::single(mem::new())), &apps, vec![]).unwrap();

    // ── the consent says where it runs, as what, and what it hears of ──
    let (_, plan) = manager.upload(&admin, app("svc", &base)).await.unwrap();
    let p = serde_json::to_value(&plan.permissions).unwrap();
    assert_eq!((&p["services"], &p["entities"], &p["subscriptions"]), (&json!(["127.0.0.1"]), &json!({"Customer": ["read"]}), &json!(["Customer.*"])), "{p}");
    manager.install(&admin, "svc", None, &plan.digest, false).await.unwrap();

    // ── it was told of its installation, with a key and a secret ──
    let calls = service.calls.lock().unwrap().clone();
    assert_eq!(calls.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), ["install"]);
    let told: Value = serde_json::from_slice(&calls[0].2).unwrap();
    assert_eq!((&told["event"], &told["app"], &told["api"]), (&json!("installed"), &json!("svc"), &json!("http://acme.example/api")));
    let (key, secret) = (told["key"].as_str().unwrap().to_string(), told["secret"].as_str().unwrap().to_string());

    // ── its key is an account of its own, with the records its manifest names and no seat ──
    let (s, _) = k.verify_api_key(&key).await.unwrap();
    let svc = k.load_ctx(tenant, s.user, Some(s.token_version), "svc".into()).await.unwrap();
    let mut tx = k.db.system_tx().await.unwrap();
    let kind: String = sqlx::query_scalar("select kind from app_user where id = $1").bind(s.user).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(kind, "service", "not among people, and no seat");
    let globex = k.insert(&admin, "Customer", json!({"customer_name": "Globex"}).as_object().cloned().unwrap()).await.unwrap();
    let id = globex["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(k.get(&svc, "Customer", id, false).await.unwrap()["customer_name"], "Globex", "it reads customers");
    let write = k.update(&svc, "Customer", id, json!({"customer_name": "Initech"}).as_object().cloned().unwrap(), None).await;
    assert!(matches!(write, Err(Error::Forbidden(_))), "and changes none: {write:?}");
    let contact = k.insert(&svc, "Contact", json!({"first_name": "Pat"}).as_object().cloned().unwrap()).await;
    assert!(matches!(contact, Err(Error::Forbidden(_))), "nor anything else: {contact:?}");

    // ── it hears which customer changed, signed, without the record ──
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    while k.deliver_webhooks_once().await.unwrap() > 0 {}
    let calls = service.calls.lock().unwrap().clone();
    let (path, headers, body) = calls.iter().find(|c| c.0 == "events").expect("an event was delivered").clone();
    assert_eq!(path, "events");
    let event: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!((&event["entity"], &event["id"], &event["event"]), (&json!("Customer"), &globex["id"], &json!("created")), "{event}");
    assert!(event.get("doc").is_none(), "the record stays behind: {event}");
    assert!(signed(&secret, &headers, &body), "signed with its secret");

    // ── turned off, its key and its deliveries stop; on, both work again ──
    manager.set_enabled(&admin, "svc", false).await.unwrap();
    assert!(k.verify_api_key(&key).await.is_err(), "the key stops");
    k.insert(&admin, "Customer", json!({"customer_name": "Hooli"}).as_object().cloned().unwrap()).await.unwrap();
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    while k.deliver_webhooks_once().await.unwrap() > 0 {}
    assert_eq!(service.calls.lock().unwrap().iter().filter(|c| c.0 == "events").count(), 1, "nothing delivered while off");
    manager.set_enabled(&admin, "svc", true).await.unwrap();
    let (again, _) = k.verify_api_key(&key).await.unwrap();
    assert_eq!(again.user, s.user, "the same key works again");

    // ── removed: the service is told, signed, and its key is revoked ──
    manager.uninstall(&admin, "svc", false).await.unwrap();
    let calls = service.calls.lock().unwrap().clone();
    let (_, headers, body) = calls.last().unwrap().clone();
    assert_eq!(calls.last().unwrap().0, "uninstall");
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["event"], "uninstalled");
    assert!(signed(&secret, &headers, &body), "the service can tell it is genuine");
    assert!(k.verify_api_key(&key).await.is_err(), "the key is revoked");
    let mut tx = k.db.system_tx().await.unwrap();
    let left: i64 = sqlx::query_scalar("select count(*) from webhook where app = 'svc'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(left, 0);

    // ── a service that refuses its installation leaves nothing behind ──
    let (_, plan) = manager.upload(&admin, app("refused", &base)).await.unwrap();
    let refused = manager.install(&admin, "refused", None, &plan.digest, false).await;
    assert!(format!("{refused:?}").contains("did not accept its installation"), "{refused:?}");
    let mut tx = k.db.system_tx().await.unwrap();
    let (hooks, active): (i64, i64) = sqlx::query_as(
        "select (select count(*) from webhook where app = 'refused'), \
         (select count(*) from app_user where email = 'refused@services.ferrum.invalid' and is_active)",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!((hooks, active), (0, 0));
    let overview = manager.overview(&admin).await.unwrap();
    assert!(!overview["installed"].as_array().unwrap().iter().any(|a| a["id"] == "refused"), "not installed");
}
