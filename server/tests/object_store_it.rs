//! Object storage only (docs/plan/stateless.md, D6), against a real PostgreSQL (`FERRUM_FILES_TEST_DATABASE_URL`, reset
//! first), with two local directories: the old files directory and one standing for the object store (the move and the
//! deletions are the same on any store; scripts/object_store_rehearsal.sh runs them on S3).
//!
//! * Where a start keeps files without a bucket: a new database is refused, an installation whose newest file is in
//!   the directory keeps it, a wrong directory and a database whose files moved are refused, development keeps any.
//! * `files_move`: files written and deleted while it runs are neither lost nor brought back; a crash in the middle goes
//!   on from where it stopped; a copy that does not read back the same, or a replica that may still write to the
//!   directory, stops the confirmation; a second run at once copies nothing; after it, the store alone holds every
//!   file the database names, each as its row says.
//! * Deletions that wait: kept until due, swept then (not while a restore is held), and a key written again while its
//!   deletion is being swept is there afterwards.

mod common;

use async_trait::async_trait;
use ferrum_kernel::files::FileStore;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use ferrum_server::mailer::StorageFiles;
use ferrum_server::objects::{Files, Objects};
use ferrum_server::state::Settings;
use ferrum_server::store::{self, Found, Releases};
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

fn settings(files_dir: &Path, require: bool) -> Settings {
    serde_json::from_value(json!({"files_dir": files_dir.to_str().unwrap(), "require_object_storage": require})).unwrap()
}

async fn sql(db: &Db, statement: &str) {
    let mut tx = db.system_tx().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(statement.to_string())).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

#[test]
fn files_move_to_object_storage_and_deletions_wait() {
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

    // ───────────── a start without a bucket ─────────────
    let refused = |r: std::result::Result<(Objects, Found), String>| r.err().expect("refused");
    let why = refused(store::decide(&db, &settings(&old, false), false, false).await);
    assert!(why.contains("a new installation does not keep files in a local directory"), "{why}");
    assert!(why.contains("FERRUM_S3_BUCKET"), "says what to set: {why}");
    let (_, found) = store::decide(&db, &settings(&old, false), true, false).await.expect("development keeps a directory");
    assert_eq!(found, Found::Directory { kept: false });
    assert!(refused(store::decide(&db, &settings(&old, true), true, false).await).contains("object storage is required"), "as before");

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
    let store_file = {
        let (k, admin) = (k.clone(), admin.clone());
        move |n: usize| {
            let (k, admin) = (k.clone(), admin.clone());
            async move {
                let data: Vec<u8> = format!("file {n} ").repeat(n % 7 + 1).into_bytes();
                let v = k.store_file(&admin, None, &format!("f{n}.txt"), "text/plain", &data).await.unwrap();
                v["id"].as_str().unwrap().parse::<uuid::Uuid>().unwrap()
            }
        }
    };
    // An installation from before this release, its files in the directory.
    let mut ids = Vec::new();
    for n in 0..60 {
        ids.push(store_file(n).await);
    }
    let (_, found) = store::decide(&db, &settings(&old, false), false, false).await.expect("an installation that keeps its files there");
    assert_eq!(found, Found::Directory { kept: true });
    let wrong = temp("wrong");
    let why = refused(store::decide(&db, &settings(&wrong, false), false, false).await);
    assert!(why.contains("does not hold the newest file this database names"), "a directory that is not this installation's: {why}");

    // ───────────── files move to the store ─────────────
    sql(&db, "insert into file_store (state) values ('moving')").await;
    let why = refused(store::decide(&db, &settings(&old, false), false, false).await);
    assert!(why.contains("keeps its files in object storage"), "no going back to the directory: {why}");
    let moving = Arc::new(Objects::local(bucket.to_str().unwrap()).unwrap().reading_directory(old.to_str().unwrap()).unwrap());
    swap.set(&moving);
    // Read from the directory while the store lacks them.
    let (_, _, back) = k.read_file(&admin, ids[0]).await.unwrap();
    assert_eq!(back, b"file 0 ".to_vec());

    // A copy that does not read back the same stops the confirmation, and is never written over.
    let key = |id: uuid::Uuid| format!("tenants/{tenant}/{id}");
    let bad = bucket.join(key(ids[5]));
    std::fs::create_dir_all(bad.parent().unwrap()).unwrap();
    std::fs::write(&bad, b"other bytes").unwrap();
    let moved = store::move_files(&db, &moving, old.to_str().unwrap(), None).await.unwrap();
    assert!(!moved.confirmed && moved.failed.len() == 1 && moved.failed[0].contains(&ids[5].to_string()), "{moved:?}");
    assert_eq!(std::fs::read(&bad).unwrap(), b"other bytes");
    std::fs::remove_file(&bad).unwrap();
    sql(&db, "delete from file_move").await;

    // A crash in the middle: the next run goes on from where it stopped.
    let crashed = store::move_files(&db, &moving, old.to_str().unwrap(), Some(10)).await.unwrap();
    assert!(crashed.copied >= 10 && crashed.copied < 60 && !crashed.confirmed, "{crashed:?}");

    // Another run at the same moment, anywhere, stops at once.
    let mut holder = db.direct.acquire().await.unwrap().detach();
    let _: bool = sqlx::query_scalar("select pg_try_advisory_lock($1)").bind(0x0FE2_2101i64).fetch_one(&mut holder).await.unwrap();
    assert!(matches!(store::move_files(&db, &moving, old.to_str().unwrap(), None).await, Err(Error::Conflict(_))));
    let _: bool = sqlx::query_scalar("select pg_advisory_unlock($1)").bind(0x0FE2_2101i64).fetch_one(&mut holder).await.unwrap();

    // A replica that started before files began to move may still write to the directory: no confirmation while it runs.
    sql(&db, "insert into replica (id, pool, build_date, started_at) values (gen_random_uuid(), 'default', now(), now() - interval '1 hour')").await;
    // Meanwhile: files stored through the server go to the store, files deleted go from both, and the old replica keeps
    // writing to the directory.
    let traffic = {
        let (k, admin, old) = (k.clone(), admin.clone(), old.clone());
        let (gone, more) = (ids[20..30].to_vec(), store_file.clone());
        tokio::spawn(async move {
            let mut new = Vec::new();
            for (i, id) in gone.iter().enumerate() {
                new.push(more(100 + i).await);
                k.remove_file(&admin, *id).await.unwrap();
                let late = old.join(format!("tenants/{tenant}/late-{i}"));
                std::fs::write(late, format!("late {i}")).unwrap();
            }
            new
        })
    };
    let moved = store::move_files(&db, &moving, old.to_str().unwrap(), None).await.unwrap();
    let written = traffic.await.unwrap();
    assert!(!moved.confirmed && moved.failed.is_empty() && moved.writers.len() == 1, "{moved:?}");
    // What the crash left copied is not copied again (but for what was deleted meanwhile).
    assert!(moved.already + 10 >= crashed.copied && moved.already > 0, "{moved:?} after {crashed:?}");
    sql(&db, "delete from replica").await;
    let moved = store::move_files(&db, &moving, old.to_str().unwrap(), None).await.unwrap();
    assert!(moved.confirmed && moved.failed.is_empty(), "{moved:?}");
    let state: String = sqlx::query_scalar("select state from file_store").fetch_one(&mut *db.system_tx().await.unwrap()).await.unwrap();
    assert_eq!(state, "store");
    // Safe to run again: nothing to copy.
    let again = store::move_files(&db, &moving, old.to_str().unwrap(), None).await.unwrap();
    assert!(again.confirmed && again.copied == 0, "{again:?}");

    // The store alone now: every file the database names, as its row says; what was deleted is not brought back; what
    // the old replica wrote was moved too; the directory is as it was, less what was deleted.
    let in_store = Arc::new(Objects::local(bucket.to_str().unwrap()).unwrap());
    let mut differ = Vec::new();
    let read = store::verify_files(&db, &in_store, None, |line| differ.push(line)).await.unwrap();
    assert!(differ.is_empty(), "{differ:?}");
    assert_eq!(read, 60 - 10 + written.len() as u64);
    for id in &ids[20..30] {
        assert!(!bucket.join(key(*id)).exists() && !old.join(key(*id)).exists(), "deleted during the move, gone from both");
    }
    for i in 0..10 {
        assert_eq!(std::fs::read(bucket.join(format!("tenants/{tenant}/late-{i}"))).unwrap(), format!("late {i}").into_bytes());
    }
    assert!(old.join(key(ids[0])).exists(), "the move never deletes from the directory");
    swap.set(&in_store);
    assert_eq!(k.read_file(&admin, ids[0]).await.unwrap().2, b"file 0 ".to_vec());

    // ───────────── an older build writes to the directory after the move ─────────────
    // A rollback to a build that keeps files in the directory, after the move was confirmed: no replica row to see it.
    swap.set(&in_dir);
    let late = store_file(600).await;
    // This build again, the old directory still mounted: the file is read at once from there.
    let mounted = Arc::new(Objects::local(bucket.to_str().unwrap()).unwrap().reading_directory(old.to_str().unwrap()).unwrap());
    swap.set(&mounted);
    assert_eq!(k.read_file(&admin, late).await.unwrap().2, b"file 600 ".repeat(6), "readable at once");
    assert!(!bucket.join(key(late)).exists());
    let counted = store::rescue(&db, &mounted, None, false).await.unwrap().unwrap();
    assert_eq!((counted.found, counted.copied), (1, 0), "the doctor's count, nothing copied");
    // The upkeep copies it over, once across replicas (another one holding the move's lock skips its turn).
    let mut holder = db.direct.acquire().await.unwrap().detach();
    let _: bool = sqlx::query_scalar("select pg_try_advisory_lock($1)").bind(0x0FE2_2101i64).fetch_one(&mut holder).await.unwrap();
    assert!(store::rescue(&db, &mounted, None, true).await.unwrap().is_none(), "another replica does it");
    let _: bool = sqlx::query_scalar("select pg_advisory_unlock($1)").bind(0x0FE2_2101i64).fetch_one(&mut holder).await.unwrap();
    let rescued = store::rescue(&db, &mounted, None, true).await.unwrap().unwrap();
    assert!(rescued.found == 1 && rescued.copied == 1 && rescued.failed.is_empty(), "{rescued:?}");
    assert_eq!(std::fs::read(bucket.join(key(late))).unwrap(), b"file 600 ".repeat(6), "in the store after the upkeep");
    assert_eq!(store::rescue(&db, &mounted, None, false).await.unwrap().unwrap().found, 0);
    // With the directory gone, nothing looks for a file left there.
    swap.set(&in_dir);
    let left = store_file(601).await;
    swap.set(&in_store);
    assert!(matches!(k.read_file(&admin, left).await, Err(Error::NotFound(_))), "not looked for without the directory");
    assert_eq!(store::rescue(&db, &in_store, None, true).await.unwrap().unwrap().found, 0);
    k.remove_file(&admin, left).await.unwrap();

    // ───────────── deletions that wait ─────────────
    // On the defaults, a deleted file stays as long as the database's backups reach (D3).
    let defaults = settings(&old, true);
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

    for d in [old, bucket, wrong] {
        let _ = std::fs::remove_dir_all(d);
    }
}
