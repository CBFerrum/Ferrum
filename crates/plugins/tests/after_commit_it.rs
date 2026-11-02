//! Nothing leaves the system from inside a document's transaction, against a real PostgreSQL
//! (`FERRUM_PLUGINS_TEST_DATABASE_URL`, reset first) and a provider that counts its calls.
//!
//! The fixture is built from plugins/examples/after-commit. The route is the control: the same
//! guest and the same call do reach the provider from there, so a silent count proves the refusal.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_plugins::http::HttpCall;
use ferrum_plugins::{NewPlugin, PluginEvents, PluginHook, PluginHost};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const WASM: &[u8] = include_bytes!("fixtures/after_commit.wasm");
const MANIFEST: &str = include_str!("fixtures/after_commit.json");

fn obj(v: Value) -> ferrum_kernel::Doc {
    v.as_object().cloned().unwrap()
}

async fn provider() -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/pay", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (counted, seen) = (calls.clone(), requests.clone());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            seen.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).to_lowercase());
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}").await;
        }
    });
    (url, calls, requests)
}

/// A request's header, as the provider saw it.
fn header<'a>(request: &'a str, name: &str) -> &'a str {
    request.lines().find_map(|l| l.strip_prefix(name)?.strip_prefix(": ")).unwrap_or_default().trim()
}

async fn boot(host: &Arc<PluginHost>) -> Arc<Kernel> {
    let url = std::env::var("FERRUM_PLUGINS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_plugins_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let mut hooks = HookRegistry::default();
    hooks.register("*", Arc::new(PluginHook(host.clone())));
    hooks.add_event_sink(Arc::new(PluginEvents(host.clone())));
    Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot")
}

async fn pending(k: &Kernel, topic: &str) -> i64 {
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query_scalar("select count(*) from outbox where topic = $1 and processed_at is null").bind(topic).fetch_one(&mut *tx).await.unwrap()
}

#[test]
fn what_leaves_the_system_waits_for_the_commit() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let host = PluginHost::new(true).unwrap();
    let k = boot(&host).await;
    let (url, calls, requests) = provider().await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "shop".into(),
            name: "Shop Ltd".into(),
            admin_email: "admin@shop.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["parties".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("shop", "admin@shop.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let mut manifest: NewPlugin = serde_json::from_str(MANIFEST).expect("manifest");
    manifest.config = Some(obj(json!({"provider_url": url})));
    host.install(&k, &admin, manifest, WASM).await.expect("install");
    let topic = "plugin.after-commit.provider";

    // The control: from a route the guest's call reaches the provider.
    let route = HttpCall { method: "GET".into(), path: "/call".into(), query: Map::new(), headers: Map::new(), body: String::new() };
    assert_eq!(host.handle_http(&k, tenant, "after-commit", route, None).await.expect("route").status, 200);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the provider is reachable");

    // From a hook the same call is refused, and the provider hears nothing.
    let refused = k.insert(&admin, "Customer", obj(json!({"customer_name": "Called from the hook"}))).await;
    assert!(matches!(refused, Err(Error::Invalid(ref m)) if m.contains("cannot run in a hook")), "{refused:?}");
    for op in ["sockets.send", "sockets.close"] {
        let refused = k.insert(&admin, "Customer", obj(json!({"customer_name": format!("{op} in the hook")}))).await;
        assert!(matches!(refused, Err(Error::Invalid(ref m)) if m.contains(op) && m.contains("cannot run in a hook")), "{refused:?}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "nothing left the transaction");

    // A document that asks for the provider and then rolls back leaves nothing to deliver.
    let rolled_back = k.insert(&admin, "Customer", obj(json!({"customer_name": "after commit, but rolled back"}))).await;
    assert!(matches!(rolled_back, Err(Error::Validation(_))), "{rolled_back:?}");
    assert_eq!(pending(&k, topic).await, 0, "the event rolled back with the document");
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    assert_eq!(calls.load(Ordering::SeqCst), 1, "and the provider was never called");

    // A document that commits calls the provider afterwards, once.
    k.insert(&admin, "Customer", obj(json!({"customer_name": "Paid after commit"}))).await.expect("commits");
    assert_eq!(pending(&k, topic).await, 1, "waiting for the dispatcher");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "nothing was sent while the document was being saved");
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    assert_eq!(calls.load(Ordering::SeqCst), 2, "the provider is called after the commit");
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    assert_eq!(calls.load(Ordering::SeqCst), 2, "and only once");

    // The crash between the provider's answer and the host's record of it. The run looks unfinished
    // and the event comes back: it cannot be avoided, so the retry says what it is, and the
    // provider sees the same key.
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update plugin_event_run set finished_at = null").execute(&mut *tx).await.unwrap();
    sqlx::query("update outbox set processed_at = null, available_at = now() where topic = $1").bind(topic).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    assert_eq!(calls.load(Ordering::SeqCst), 3, "repeated, as at-least-once allows");
    let seen = requests.lock().unwrap().clone();
    let (first, retry) = (&seen[1], &seen[2]);
    assert!(!header(first, "idempotency-key").is_empty(), "{first}");
    assert_eq!(header(first, "idempotency-key"), header(retry, "idempotency-key"), "the provider can tell it is the same operation");
    assert_eq!((header(first, "x-attempt"), header(retry, "x-attempt")), ("1", "2"), "and so can the plugin");

    // Once finished, it stays finished.
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update outbox set processed_at = null, available_at = now() where topic = $1").bind(topic).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    assert_eq!(calls.load(Ordering::SeqCst), 3, "a finished run is not repeated");
}
