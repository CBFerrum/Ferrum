//! A plugin that is a connector, against a real PostgreSQL (`FERRUM_PLUGINS_TEST_DATABASE_URL`, reset
//! first) and a provider that keeps what it is sent. The module is built from
//! plugins/examples/connector-demo. What this proves is the seam: the kernel's machinery driving a
//! sandboxed guest, with a connection's token held by the host. It proves nothing about any provider.

use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_plugins::connect::PluginConnectors;
use ferrum_plugins::{NewPlugin, PluginEvents, PluginHost};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const WASM: &[u8] = include_bytes!("fixtures/connector_demo.wasm");
const MANIFEST: &str = include_str!("fixtures/connector_demo.json");
const TOKEN: &str = "tok_live_never_in_the_guest";

const ENTITIES: &str = r#"
- name: Contact
  label: Contact
  kind: master
  module: cases
  title_field: full_name
  fields:
    - {name: full_name, type: data, required: true, in_list: true, max_length: 40}
    - {name: city, type: data, in_list: true}
"#;

#[derive(Default)]
struct Provider {
    /// Every request as it arrived, lower-cased.
    seen: Vec<String>,
    /// Answers 429 this many times.
    limited: usize,
    /// Keys of changes it took, and how often.
    pushed: BTreeMap<String, usize>,
}

async fn provider() -> (String, Arc<Mutex<Provider>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let state: Arc<Mutex<Provider>> = Arc::default();
    let world = state.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let raw = String::from_utf8_lossy(&buf[..n]).to_string();
            let (status, extra, body) = {
                let mut w = world.lock().unwrap();
                w.seen.push(raw.to_lowercase());
                let line = raw.lines().next().unwrap_or_default().to_string();
                if !raw.to_lowercase().contains(&format!("authorization: bearer {}", TOKEN.to_lowercase())) {
                    (401, "", json!({}).to_string())
                } else if w.limited > 0 {
                    w.limited -= 1;
                    (429, "retry-after: 120\r\n", json!({}).to_string())
                } else if line.contains("/me") {
                    (200, "", json!({"account": "Acme"}).to_string())
                } else if line.starts_with("POST /people") {
                    let key = raw.split("\"key\":\"").nth(1).and_then(|r| r.split('"').next()).unwrap_or_default().to_string();
                    *w.pushed.entry(key).or_insert(0) += 1;
                    (200, "", json!({"id": "p-9"}).to_string())
                } else if line.contains("after=0") {
                    (200, "", json!({"records": [{"id": "1", "version": "1", "fields": {"name": "Asha", "city": "Pune"}}, {"id": "2", "version": "1", "fields": {"name": "Ravi", "city": "Goa"}}], "next": "2"}).to_string())
                } else {
                    (200, "", json!({"records": [], "next": null}).to_string())
                }
            };
            let reply =
                format!("HTTP/1.1 {status} X\r\n{extra}content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            let _ = socket.write_all(reply.as_bytes()).await;
        }
    });
    (url, state)
}

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-connector-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("cases/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("cases/package.yaml"), "package: cases\nversion: 0.1.0\nlabel: Cases\ndepends: [core]\n").unwrap();
    std::fs::write(dir.join("cases/entities/cases.yaml"), ENTITIES).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

#[test]
fn a_plugin_can_be_a_connector() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_PLUGINS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_plugins_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let host = PluginHost::new(true).unwrap();
    let mut hooks = HookRegistry::default();
    hooks.add_event_sink(Arc::new(PluginEvents(host.clone())));
    hooks.set_connector_source(Arc::new(PluginConnectors(host.clone())));
    let settings = KernelSettings { secret_key: Some(SecretKey::new([7u8; 32])), ..Default::default() };
    let k = Kernel::boot(db, catalog(), hooks, settings, None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["cases".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let manifest: NewPlugin = serde_json::from_str(MANIFEST).expect("manifest");
    host.install(&k, &admin, manifest, WASM).await.expect("install");

    // ───────────── it says what it is ─────────────
    let offered = k.connectors(&admin).await.unwrap();
    assert_eq!((offered.len(), offered[0].provider.as_str(), offered[0].api_version.as_str()), (1, "connector-demo", "2026-01"));
    assert_eq!(offered[0].capabilities, ["read_people", "write_people"]);

    // ───────────── it signs in with a token only the host holds ─────────────
    let (base, world) = provider().await;
    let id = k
        .save_connection(
            &admin,
            None,
            serde_json::from_value(json!({
                "provider": "connector-demo", "name": "Acme's account", "config": {"base_url": base},
                "credentials": {"token": TOKEN, "webhook_secret": "whsec"},
                "mapping": {"people": {"entity": "Contact", "fields": {"name": "full_name", "city": "city"}, "conflict": "source", "push": true}},
            }))
            .unwrap(),
        )
        .await
        .expect("saved");
    assert_eq!(k.test_connection(&admin, id).await.expect("tested"), "Acme");
    let schema = k.connection_schema(&admin, id).await.expect("schema");
    assert_eq!((schema[0].stream.as_str(), schema[0].fields.len()), ("people", 2));

    // ───────────── a page comes in, after the provider made it wait ─────────────
    world.lock().unwrap().limited = 1;
    let run = k.sync_connection(&admin, id, false).await.unwrap();
    k.run_jobs_once(4).await.unwrap();
    let v = k.job(&admin, run).await.unwrap();
    assert_eq!((v["status"].as_str(), v["attempts"].as_i64()), (Some("queued"), Some(0)), "told to wait, it waits at no cost: {v}");
    assert!(v["last_error"].as_str().unwrap().contains("rate limit"), "{v}");
    let quota = k.connections(&admin).await.unwrap()[0]["health"]["quota"].clone();
    assert!(quota["waiting_until"].is_string(), "{quota}");
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update job set run_at = now()").execute(&mut *tx).await.unwrap();
    sqlx::query("update connection set waiting_until = now()").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    k.run_jobs_once(4).await.unwrap();
    assert_eq!(k.job(&admin, run).await.unwrap()["result"]["created"], json!(2));

    // ───────────── a change goes out, once however often it is retried ─────────────
    let page = k.list(&admin, "Contact", &ferrum_kernel::ListQuery { limit: Some(10), ..Default::default() }).await.unwrap();
    let asha = page.rows.iter().find(|r| r["full_name"] == json!("Asha")).unwrap();
    let asha = asha["id"].as_str().unwrap().parse().unwrap();
    k.update(&admin, "Contact", asha, json!({"city": "Mumbai"}).as_object().cloned().unwrap(), None).await.expect("changed here");
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    k.run_jobs_once(4).await.unwrap();
    assert_eq!(world.lock().unwrap().pushed.values().copied().collect::<Vec<_>>(), [1], "one change out");

    // ───────────── a webhook counts only if it is signed ─────────────
    let body = br#"{"event":"e1","record":{"id":"3","version":"1","fields":{"name":"Dia","city":"Agra"}}}"#;
    let unsigned = BTreeMap::from([("x-demo-signature".to_string(), "00".to_string())]);
    assert!(matches!(k.connection_webhook(id, &unsigned, body).await, Err(Error::Invalid(ref m)) if m.contains("not signed")));
    let signature = {
        use hmac::{Hmac, Mac};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(b"whsec").unwrap();
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    };
    let signed = BTreeMap::from([("x-demo-signature".to_string(), signature)]);
    assert_eq!(k.connection_webhook(id, &signed, body).await.expect("signed"), 1);
    assert_eq!(k.connection_webhook(id, &signed, body).await.expect("repeated"), 0, "one event, however often it arrives");
    k.run_jobs_once(4).await.unwrap();
    let page = k.list(&admin, "Contact", &ferrum_kernel::ListQuery { limit: Some(10), ..Default::default() }).await.unwrap();
    assert!(page.rows.iter().any(|r| r["full_name"] == json!("Dia")), "{:?}", page.rows);

    // ───────────── nothing but the host ever held the token ─────────────
    let view: Value = json!(k.connections(&admin).await.unwrap());
    assert!(!view.to_string().contains(TOKEN), "never read back");
    let reachable = host.plugin(&k.db, tenant, "connector-demo").await.unwrap();
    assert!(!reachable.config.to_string().contains(TOKEN), "and not in the plugin's configuration");
    assert!(world.lock().unwrap().seen.iter().all(|r| r.contains("bearer tok_live")), "every call signed in");
}
