//! A plugin's secret in the vault goes on its requests and to nobody else (crates/kernel/src/vault, `Vault::carry`),
//! against a real PostgreSQL (`FERRUM_PLUGINS_TEST_DATABASE_URL`; reset first, the vault's schema too): the host adds it
//! for the hosts the secret names, the plugin's own answer never holds it, another plugin cannot use it, and a server
//! that gives its plugins no vault refuses.

use ferrum_kernel::controls::Proof;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::vault::{self, Kdf, New, Opener, Rule, Zeroizing};
use ferrum_kernel::{Ctx, Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_plugins::{Loaded, Manifest, PluginHost};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

/// A provider that keeps what it is sent.
async fn provider() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let kept = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            kept.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).to_lowercase());
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}").await;
        }
    });
    (url, seen)
}

fn plugin(name: &str) -> Loaded {
    let manifest: Manifest = serde_json::from_value(json!({"http_hosts": ["127.0.0.1"]})).unwrap();
    Loaded {
        id: Uuid::now_v7(),
        name: name.into(),
        sha: String::new(),
        manifest: Arc::new(manifest),
        config: Arc::new(json!({})),
        credentials: Arc::new(Map::new()),
        user_id: None,
        fuel: 0,
        memory_mb: 0,
    }
}

#[test]
fn a_plugins_secret_goes_only_on_its_requests() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_PLUGINS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_plugins_test".into());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 8, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists vault cascade; drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let opener = Opener { database_url: url, file: "/nonexistent/root.key".into(), kdf: Kdf { m_kib: 64, t: 1, p: 1 }, ..Default::default() };
    vault::open(&db, opener, false).await.unwrap();
    let mut hooks = HookRegistry::default();
    let platform = hooks.platform_vault();
    let catalog = ferrum_meta::Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    let s = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin =
        Ctx { proof: Proof::Person { at: None, staff: true }, ..k.load_ctx(tenant, s.user, None, "it".into()).await.unwrap() }.proved(Some(chrono::Utc::now()));
    let value = |v: &str| New::Value(Zeroizing::new(v.as_bytes().to_vec()));
    k.vault_set(&admin, "plugin:pay", "key", value("Pk-4321-goes-on-requests"), Rule { hosts: vec!["127.0.0.1".into()], ..Default::default() }).await.unwrap();
    k.vault_set(&admin, "plugin:pay", "elsewhere", value("Pk-8765-another-host"), Rule { hosts: vec!["api.other.example".into()], ..Default::default() })
        .await
        .unwrap();

    let host = PluginHost::new(true).unwrap();
    let (url, seen) = provider().await;
    let ask = |auth: &str| json!({"url": url, "auth": auth});
    let refused = host.fetch_as(&plugin("pay"), &ask("vault:key"), Some((&k, tenant))).await.unwrap_err();
    assert!(refused.to_string().contains("no vault"), "a server that gives its plugins no vault: {refused}");
    host.use_vault(platform);
    let answer: Value = host.fetch_as(&plugin("pay"), &ask("vault:key"), Some((&k, tenant))).await.unwrap();
    assert_eq!(answer["status"], json!(200));
    assert!(seen.lock().unwrap().iter().any(|r| r.contains("authorization: bearer pk-4321-goes-on-requests")), "the host added it");
    assert!(!answer.to_string().to_lowercase().contains("pk-4321"), "the plugin's answer does not hold it");
    let to_another = host.fetch_as(&plugin("pay"), &ask("vault:elsewhere"), Some((&k, tenant))).await.unwrap_err();
    assert!(to_another.to_string().contains("may not be sent"), "{to_another}");
    let other = host.fetch_as(&plugin("other"), &ask("vault:key"), Some((&k, tenant))).await.unwrap_err();
    assert!(matches!(other, ferrum_kernel::Error::NotFound(_)), "another plugin's handle finds none of it: {other}");
    let no_workspace = host.fetch_as(&plugin("pay"), &ask("vault:key"), None).await.unwrap_err();
    assert!(no_workspace.to_string().contains("workspace's call"), "{no_workspace}");
    assert_eq!(seen.lock().unwrap().len(), 1, "nothing else went out");
}
