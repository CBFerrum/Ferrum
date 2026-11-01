//! Uploads and the malware scanner, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the
//! database is reset first) and a stand-in for ClamAV's daemon speaking its protocol on a local
//! port: what it finds is not stored, a scanner that cannot answer stops uploads, and a file past
//! its limit is stored and marked.

use ferrum_kernel::files::FileStore;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

const EICAR: &[u8] = b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*";
/// The stand-in's `StreamMaxLength`.
const LIMIT: usize = 64 * 1024;

#[derive(Default)]
struct MemFiles(Mutex<HashMap<String, Vec<u8>>>);

#[async_trait::async_trait]
impl FileStore for MemFiles {
    async fn put(&self, key: &str, data: &[u8]) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().insert(key.to_string(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> ferrum_kernel::Result<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned().ok_or_else(|| Error::NotFound("object".into()))
    }
    async fn delete(&self, key: &str) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

async fn clamd() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut command = [0u8; 10];
                if conn.read_exact(&mut command[..6]).await.is_err() {
                    return;
                }
                if &command[..6] == b"zPING\0" {
                    let _ = conn.write_all(b"PONG\0").await;
                    return;
                }
                conn.read_exact(&mut command[6..]).await.unwrap();
                assert_eq!(&command, b"zINSTREAM\0");
                let mut seen = Vec::new();
                loop {
                    let mut size = [0u8; 4];
                    if conn.read_exact(&mut size).await.is_err() {
                        return;
                    }
                    let size = u32::from_be_bytes(size) as usize;
                    if size == 0 {
                        break;
                    }
                    let start = seen.len();
                    seen.resize(start + size, 0);
                    conn.read_exact(&mut seen[start..]).await.unwrap();
                    assert!(seen.len() <= LIMIT, "sent past the limit it was told");
                }
                let found = seen.windows(EICAR.len()).any(|w| w == EICAR);
                let _ = conn.write_all(if found { b"stream: Win.Test.EICAR_HDB-1 FOUND\0" as &[u8] } else { b"stream: OK\0" }).await;
            });
        }
    });
    addr
}

#[test]
fn what_the_scanner_finds_is_not_stored() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let connect = || async { Db::connect(DbConfig { url: url.clone(), max_connections: 10, ..Default::default() }).await.expect("connect") };
    let db = connect().await;
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = || Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let settings = KernelSettings { clamd: Some(clamd().await), clamd_max_bytes: LIMIT as u64, ..Default::default() };
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), settings, None).await.expect("boot");
    let files = Arc::new(MemFiles::default());
    k.set_file_store(files.clone());
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let status = |id: Uuid| {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(tenant).await.unwrap();
            let s: String = sqlx::query_scalar("select scan_status from file_object where tenant_id = $1 and id = $2")
                .bind(tenant)
                .bind(id)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            s
        }
    };
    let id = |v: serde_json::Value| v["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    assert!(k.scanner_ok().await.expect("configured").is_ok());
    let clean = k.store_file(&admin, None, "note.txt", "text/plain", b"nothing wrong here").await.expect("stored");
    assert_eq!(status(id(clean)).await, "clean");

    let before = files.0.lock().unwrap().len();
    let refused = k.store_file(&admin, None, "invoice.pdf", "application/pdf", EICAR).await;
    assert!(matches!(&refused, Err(Error::Invalid(m)) if m.contains("EICAR")), "{refused:?}");
    assert_eq!(files.0.lock().unwrap().len(), before, "no bytes kept");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let (rows, audited): (i64, i64) = sqlx::query_as(
        "select (select count(*) from file_object where tenant_id = $1 and file_name = 'invoice.pdf'), (select count(*) from audit_event where tenant_id = $1 and action = 'malware_refused')",
    )
    .bind(tenant)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!((rows, audited), (0, 1), "no record of a file, and a record of the attempt");

    // Past the scanner's limit it is not sent at all, and says so.
    let long = vec![b'.'; LIMIT + 10];
    let stored = k.store_file(&admin, None, "big.bin", "application/octet-stream", &long).await.expect("past the scanner's limit");
    assert_eq!(status(id(stored)).await, "unscanned", "stored, and it says so");

    // Found in a later frame of the protocol too.
    let mut deep = vec![b'.'; LIMIT - EICAR.len() - 10];
    deep.extend_from_slice(EICAR);
    assert!(matches!(k.store_file(&admin, None, "deep.bin", "application/octet-stream", &deep).await, Err(Error::Invalid(_))));

    // The scanner goes away: an outage is no way in.
    let db = connect().await;
    let down = KernelSettings { clamd: Some("127.0.0.1:1".into()), ..Default::default() };
    let k2 = Kernel::boot(db, catalog(), HookRegistry::default(), down, None).await.expect("boot");
    k2.set_file_store(files.clone());
    let admin2 = k2.load_ctx(tenant, admin.user, None, "it".into()).await.unwrap();
    let before = files.0.lock().unwrap().len();
    assert!(matches!(k2.store_file(&admin2, None, "note.txt", "text/plain", b"fine").await, Err(Error::Unavailable(_))));
    assert_eq!(files.0.lock().unwrap().len(), before);
    assert!(k2.scanner_ok().await.expect("configured").is_err());

    // Nothing configured: stored, and marked.
    let db = connect().await;
    let k3 = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    k3.set_file_store(files.clone());
    let admin3 = k3.load_ctx(tenant, admin.user, None, "it".into()).await.unwrap();
    let plain = k3.store_file(&admin3, None, "note.txt", "text/plain", b"fine").await.expect("stored");
    assert_eq!(status(id(plain)).await, "unscanned");
    assert!(k3.scanner_ok().await.is_none());
}
