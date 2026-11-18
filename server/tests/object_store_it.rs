//! Object storage only (docs/plan/stateless.md, D6), against a real PostgreSQL (`FERRUM_FILES_TEST_DATABASE_URL`, reset
//! first), with a local directory standing for the object store (the deletions are the same on any store;
//! scripts/object_store_rehearsal.sh runs them on S3).
//!
//! * Where a start keeps files: a bucket is the store; without one, production is refused and development keeps a directory.
//! * Deletions that wait: kept until due, swept then (not while a restore is held), and a key written again while its
//!   deletion is being swept is there afterwards.

mod common;

use async_trait::async_trait;
use ferrum_kernel::files::FileStore;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use ferrum_server::mailer::StorageFiles;
use ferrum_server::objects::{Files, Objects};
use ferrum_server::state::Settings;
use ferrum_server::store::{self, Releases};
use loco_rs::storage::Storage;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// The kernel's file store, swapped as the scenario moves on (the kernel takes one at boot).
struct Swap(RwLock<Arc<dyn FileStore>>);

impl Swap {
    fn now(&self) -> Arc<dyn FileStore> {
        self.0.read().unwrap().clone()
    }
    fn set(&self, objects: &Arc<Objects>) {
        *self.0.write().unwrap() = files(objects);
    }
}

fn files(objects: &Arc<Objects>) -> Arc<dyn FileStore> {
    Arc::new(StorageFiles(Arc::new(Storage::single(Box::new(Files(objects.clone())))), objects.clone()))
}

#[async_trait]
impl FileStore for Swap {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        self.now().put(key, data).await
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        self.now().get(key).await
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.now().delete(key).await
    }
    async fn exists(&self, key: &str) -> Result<bool> {
        self.now().exists(key).await
    }
}

fn temp(what: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferrum-store-{what}-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn settings(files_dir: &Path) -> Settings {
    serde_json::from_value(json!({"files_dir": files_dir.to_str().unwrap()})).unwrap()
}

async fn sql(db: &Db, statement: &str) {
    let mut tx = db.system_tx().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(statement.to_string())).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

#[test]
fn where_files_are_kept_and_deletions_wait() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_FILES_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_files_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 12, ..Default::default() }).await.expect("connect");
    common::reset(&db.pool).await;
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages");
    let packages = ["core", "contacts", "parties"].map(|p| ferrum_meta::catalog::load_package_dir(&dir.join(p)).expect(p));
    let k = Kernel::boot(db, Catalog::from_packages(packages.to_vec()).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let db = k.db.clone();
    let (old, bucket) = (temp("directory"), temp("store"));
    let in_dir = Arc::new(Objects::local(old.to_str().unwrap()).unwrap());
    let swap = Arc::new(Swap(RwLock::new(files(&in_dir))));
    k.set_file_store(swap.clone());

    // ───────────── where a start keeps files ─────────────
    let why = store::decide(&settings(&old), false).err().expect("production without a bucket is refused");
    assert!(why.contains("FERRUM_S3_BUCKET") && why.contains("only development and tests"), "says what to set: {why}");
    assert!(store::decide(&settings(&old), true).is_ok(), "development keeps a directory");
    let mut with_bucket = settings(&old);
    with_bucket.s3_bucket = "files".into();
    with_bucket.s3_region = "eu-west-1".into();
    assert!(store::decide(&with_bucket, false).unwrap().direct_uploads(), "a bucket is the store");

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
        .unwrap();
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let store_file = |n: usize| {
        let (k, admin) = (k.clone(), admin.clone());
        async move {
            let data: Vec<u8> = format!("file {n} ").repeat(n % 7 + 1).into_bytes();
            let v = k.store_file(&admin, None, &format!("f{n}.txt"), "text/plain", &data).await.unwrap();
            v["id"].as_str().unwrap().parse::<uuid::Uuid>().unwrap()
        }
    };
    let key = |id: uuid::Uuid| format!("tenants/{tenant}/{id}");
    let in_store = Arc::new(Objects::local(bucket.to_str().unwrap()).unwrap());
    swap.set(&in_store);
    let first = store_file(0).await;
    assert_eq!(k.read_file(&admin, first).await.unwrap().2, b"file 0 ".to_vec());
    let mut differ = Vec::new();
    let read = store::verify_files(&db, &in_store, None, |line| differ.push(line)).await.unwrap();
    assert!(differ.is_empty() && read == 1, "{differ:?} {read}");

    // ───────────── deletions that wait ─────────────
    // On the defaults, a deleted file stays as long as the database's backups reach (D3).
    let defaults = settings(&old);
    let releases = Arc::new(Releases::new(db.clone(), defaults.files_delete_after_days));
    let waiting = Arc::new(Objects::local(bucket.to_str().unwrap()).unwrap().with_releases(releases.clone()));
    swap.set(&waiting);
    let id = store_file(500).await;
    k.remove_file(&admin, id).await.unwrap();
    assert!(bucket.join(key(id)).exists(), "on the defaults, kept while its deletion waits");
    let due: bool = sqlx::query_scalar("select due_at > now() + make_interval(days => $2) - interval '1 hour' from object_release where key = $1")
        .bind(key(id))
        .bind(defaults.backup_reach_days as i32)
        .fetch_one(&mut *db.system_tx().await.unwrap())
        .await
        .unwrap();
    assert!(due && defaults.backup_reach_days >= 30, "as long as the backups reach: {} days", defaults.backup_reach_days);
    // Not swept before its time, nor while a restore is held; swept once due.
    assert_eq!(store::sweep(&k, &waiting).await.unwrap(), 0);
    sql(&db, &format!("update object_release set due_at = now() - interval '1 minute' where key = '{}'", key(id))).await;
    sql(&db, "update platform_incarnation set held_since = now(), held_because = 'a test'").await;
    assert_eq!(store::sweep(&k, &waiting).await.unwrap(), 0, "a restored copy deletes nothing of a store it shares");
    assert!(bucket.join(key(id)).exists());
    sql(&db, "update platform_incarnation set held_since = null, held_because = null").await;
    assert_eq!(store::sweep(&k, &waiting).await.unwrap(), 1);
    assert!(!bucket.join(key(id)).exists(), "gone once due");

    // A key written again (an interface's file, named by its digest): not there while its deletion waits, so whoever asks
    // writes it again, which calls the deletion off.
    let storage = Storage::single(Box::new(Files(waiting.clone())));
    let ui = Path::new("ui/shell/0123abcd/index.js");
    storage.upload(ui, &bytes::Bytes::from_static(b"one")).await.unwrap();
    storage.delete(ui).await.unwrap();
    assert!(!storage.exists(ui).await.unwrap(), "going: written again by whoever needs it");
    storage.upload(ui, &bytes::Bytes::from_static(b"one")).await.unwrap();
    assert!(storage.exists(ui).await.unwrap());
    assert!(!releases.pending("ui/shell/0123abcd/index.js").await.unwrap());
    // Written again while a sweep is deleting it: the write waits for the sweep, and the file is there afterwards.
    storage.delete(ui).await.unwrap();
    sql(&db, "update object_release set due_at = now() - interval '1 minute'").await;
    let mut sweeping = db.system_tx().await.unwrap();
    let held: Vec<String> = sqlx::query_scalar("select key from object_release where due_at <= now() for update").fetch_all(&mut *sweeping).await.unwrap();
    assert_eq!(held, vec![ui.display().to_string()]);
    std::fs::remove_file(bucket.join(ui)).unwrap();
    let writer = {
        let waiting = waiting.clone();
        tokio::spawn(async move {
            Storage::single(Box::new(Files(waiting))).upload(Path::new("ui/shell/0123abcd/index.js"), &bytes::Bytes::from_static(b"one")).await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!writer.is_finished(), "the write waits for the sweep's row");
    sqlx::query("delete from object_release where key = any($1)").bind(&held).execute(&mut *sweeping).await.unwrap();
    sweeping.commit().await.unwrap();
    writer.await.unwrap().unwrap();
    assert_eq!(std::fs::read(bucket.join(ui)).unwrap(), b"one");
    // Uploads not yet recorded never wait.
    storage.upload(Path::new("uploads/x/y"), &bytes::Bytes::from_static(b"u")).await.unwrap();
    storage.delete(Path::new("uploads/x/y")).await.unwrap();
    assert!(!bucket.join("uploads/x/y").exists());

    for d in [old, bucket] {
        let _ = std::fs::remove_dir_all(d);
    }
}
