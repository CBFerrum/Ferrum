//! What Temporal keeps of Ferrum's processes is sealed with the platform's key for processes (ferrum_kernel::vault,
//! keyed.rs), against PostgreSQL alone (`FERRUM_PROCESS_TEST_DATABASE_URL`, reset first): a payload names the version
//! that sealed it and opens on another replica, and after the key rotates; what the previous build sealed with the old
//! key opens once the key is gone from the environment, from the vault's copy; with the vault closed and no old key, and
//! before the kernel has started, nothing is sealed.

use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::vault::{self, Kdf, Opener, Scope};
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_process::{Keys, Sealed};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use temporalio_common::data_converters::{PayloadCodec, SerializationContextData};
use temporalio_common::protos::temporal::api::common::v1::Payload;

/// What the codec binds every payload to (crates/process, `AAD`).
const AAD: &str = "ferrum.process.payload/1";

fn url() -> String {
    std::env::var("FERRUM_PROCESS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_process_test".into())
}

/// A replica: its kernel, and its codec ready with the platform's handle.
async fn replica(key: Option<&SecretKey>, open: bool) -> (Arc<Kernel>, Sealed) {
    let db = Db::connect(DbConfig { url: url(), max_connections: 4, ..Default::default() }).await.expect("connect");
    if open {
        let opener = Opener { database_url: url(), file: "/nonexistent/root.key".into(), kdf: Kdf { m_kib: 64, t: 1, p: 1 }, ..Default::default() };
        vault::open(&db, opener, false).await.expect("vault");
    }
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let mut hooks = HookRegistry::default();
    let platform = hooks.platform_vault();
    let settings = KernelSettings { secret_key: key.cloned(), ..Default::default() };
    let k = Kernel::boot(db, Catalog::from_packages(vec![core]).unwrap(), hooks, settings, None).await.expect("boot");
    let keys = Arc::new(Keys::default());
    keys.ready(&k, platform);
    (k, Sealed(keys))
}

fn payload(text: &str) -> Payload {
    Payload { metadata: HashMap::from([("encoding".to_string(), b"json/plain".to_vec())]), data: text.as_bytes().to_vec(), ..Default::default() }
}

/// What the previous build's codec made of `p`: sealed with the old key, and no version named.
fn previous_build(key: &SecretKey, p: &Payload) -> Payload {
    let inner = serde_json::to_string(&(&p.metadata, &p.data)).unwrap();
    Payload {
        metadata: HashMap::from([("encoding".to_string(), b"binary/ferrum-sealed".to_vec())]),
        data: key.seal(AAD, &inner).unwrap().into_bytes(),
        ..Default::default()
    }
}

#[tokio::test]
async fn payloads_are_sealed_with_the_platforms_key() {
    let ctx = SerializationContextData::None;
    let db = Db::connect(DbConfig { url: url(), max_connections: 2, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    db.pool.close().await;
    let plain = payload("{\"total\":\"Sealed-5000\"}");

    // Not ready (the kernel has not started), and the vault closed with no old key: nothing is sealed.
    let unready = Sealed(Arc::new(Keys::default()));
    assert!(unready.encode(&ctx, vec![plain.clone()]).await.is_err());
    let (_shut_kernel, shut) = replica(None, false).await;
    let refused = shut.encode(&ctx, vec![plain.clone()]).await.unwrap_err().to_string();
    assert!(refused.contains("needs the vault"), "the vault closed, no old key: {refused}");

    // A previous build sealed one with the old key; this build starts with the key set, then without it.
    let old = SecretKey::new([3u8; 32]);
    let before = previous_build(&old, &plain);
    let (_with_key_kernel, with_key) = replica(Some(&old), true).await;
    assert_eq!(with_key.decode(&ctx, vec![before.clone()]).await.unwrap(), vec![plain.clone()]);
    let window = with_key.encode(&ctx, vec![plain.clone()]).await.unwrap();
    assert!(window[0].metadata.get("ferrum-key").is_none(), "while older builds may run: sealed with the old key, as they read it");
    assert!(old.open(AAD, std::str::from_utf8(&window[0].data).unwrap()).is_ok(), "the previous build opens it");

    let (a, a_codec) = replica(None, true).await;
    let (_b, b_codec) = replica(None, true).await;
    assert_eq!(b_codec.decode(&ctx, vec![before.clone()]).await.unwrap(), vec![plain.clone()], "the old key's, once it is gone");
    let sealed = a_codec.encode(&ctx, vec![plain.clone()]).await.unwrap();
    assert_eq!(sealed[0].metadata["encoding"], b"binary/ferrum-sealed");
    assert_eq!(sealed[0].metadata["ferrum-key"], b"1", "names the version that sealed it");
    assert!(!String::from_utf8_lossy(&sealed[0].data).contains("Sealed-5000"));
    assert_eq!(b_codec.decode(&ctx, sealed.clone()).await.unwrap(), vec![plain.clone()], "on another replica");

    // The platform's key for processes rotated: what was sealed before opens; a replica started since seals with the new.
    a.rotate_owner_key(Scope::Installation, "platform").await.unwrap();
    let (_c, c_codec) = replica(None, true).await;
    assert_eq!(c_codec.decode(&ctx, sealed).await.unwrap(), vec![plain.clone()]);
    let newer = c_codec.encode(&ctx, vec![plain.clone()]).await.unwrap();
    assert_eq!(newer[0].metadata["ferrum-key"], b"2");
    assert_eq!(a_codec.decode(&ctx, newer).await.unwrap(), vec![plain.clone()], "where the old version was current");
    // What was never sealed passes as it is.
    assert_eq!(a_codec.decode(&ctx, vec![plain.clone()]).await.unwrap(), vec![plain]);
}
