//! A workspace's export against a real PostgreSQL (`FERRUM_EXPORT_TEST_DATABASE_URL`; the database is reset first):
//! what a replica holds while it exports a large file (K9), that what it writes opens with ordinary tools, and that a
//! store failing, or a worker dying, part-way through a part leaves no object behind; and that records' child rows are
//! read a page at a time, not one record at a time (statements counted as every query sqlx runs, an event of target
//! `sqlx::query`). The tests share the database: each holds [`SERIAL`].
//!
//! ```text
//! cargo test -p ferrum-kernel --test export_it -- --nocapture --test-threads=1
//! ```

use async_trait::async_trait;
use ferrum_kernel::files::{FileStore, ObjectWriter};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use sha2::{Digest, Sha256};
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

/// Bytes allocated minus freed, by every thread, and the most that was at once since [`reset_peak`].
static NET: AtomicI64 = AtomicI64::new(0);
static PEAK: AtomicI64 = AtomicI64::new(0);

struct Counting;

fn add(n: i64) {
    let now = NET.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

// SAFETY: every call is the system allocator's own; only counters beside it change.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            add(l.size() as i64);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        add(-(l.size() as i64));
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            add(l.size() as i64);
        }
        p
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            add(new as i64 - l.size() as i64);
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

static SERIAL: Mutex<()> = Mutex::new(());

static COUNTING: AtomicBool = AtomicBool::new(false);
static STATEMENTS: AtomicU64 = AtomicU64::new(0);

struct Statements;

impl tracing::Subscriber for Statements {
    fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
        m.target() == "sqlx::query"
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        if COUNTING.load(Ordering::SeqCst) {
            STATEMENTS.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// The statements `f` runs.
async fn counted<T>(f: impl std::future::Future<Output = T>) -> (T, u64) {
    STATEMENTS.store(0, Ordering::SeqCst);
    COUNTING.store(true, Ordering::SeqCst);
    let out = f.await;
    COUNTING.store(false, Ordering::SeqCst);
    (out, STATEMENTS.load(Ordering::SeqCst))
}

fn reset_peak() -> i64 {
    let now = NET.load(Ordering::Relaxed);
    PEAK.store(now, Ordering::Relaxed);
    now
}

/// The large file's size: enough to tell a bound from a file held whole, small enough for a laptop.
const BIG: u64 = 300 << 20;

/// The large file's bytes from `at`, made as they are asked for (the same each time), so the test holds none of it.
fn generated(at: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 8);
    let mut word = at / 8;
    while out.len() < len + (at % 8) as usize {
        let mut z = word.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        out.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
        word += 1;
    }
    out.drain(..(at % 8) as usize);
    out.truncate(len);
    out
}

/// What the generated file's SHA-256 is, read 8 MiB at a time.
fn generated_sha() -> String {
    let mut h = Sha256::new();
    let mut at = 0;
    while at < BIG {
        let n = (8u64 << 20).min(BIG - at);
        h.update(generated(at, n as usize));
        at += n;
    }
    hex::encode(h.finalize())
}

/// An object store on a directory, as a single host's is; the large file is generated, not stored.
#[derive(Default)]
struct Store {
    dir: PathBuf,
    big: Mutex<String>,
    /// Bytes written through writers so far; past `fail_at` a write fails, past `hang_at` it never returns.
    written: AtomicU64,
    fail_at: AtomicU64,
    hang_at: AtomicU64,
    stuck: AtomicBool,
}

impl Store {
    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(key.replace('/', "_"))
    }

    fn is_big(&self, key: &str) -> bool {
        *self.big.lock().unwrap() == key
    }

    /// Objects on the disk that no file names: what an export left behind. Released objects go first.
    async fn left_over(&self, k: &Kernel) -> Vec<String> {
        while k.dispatch_outbox_once().await.unwrap() > 0 {}
        let mut tx = k.db.system_tx().await.unwrap();
        let named: Vec<String> = sqlx::query_scalar("select storage_key from file_object").fetch_all(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let named: Vec<String> = named.iter().map(|k| k.replace('/', "_")).collect();
        let mut left: Vec<String> = std::fs::read_dir(&self.dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        left.retain(|f| !named.contains(f));
        left
    }
}

/// Writes in place, as a disk store does.
struct DiskWriter<'a> {
    store: &'a Store,
    file: std::fs::File,
}

#[async_trait]
impl ObjectWriter for DiskWriter<'_> {
    async fn write(&mut self, piece: Vec<u8>) -> Result<()> {
        use std::io::Write;
        let s = self.store;
        let at = s.written.fetch_add(piece.len() as u64, Ordering::Relaxed) + piece.len() as u64;
        if at > s.fail_at.load(Ordering::Relaxed) {
            return Err(Error::Unavailable("storage: the store went away".into()));
        }
        self.file.write_all(&piece).map_err(Error::internal)?;
        if at > s.hang_at.load(Ordering::Relaxed) {
            s.stuck.store(true, Ordering::Relaxed);
            std::future::pending::<()>().await;
        }
        Ok(())
    }
    async fn close(self: Box<Self>) -> Result<()> {
        self.file.sync_all().map_err(Error::internal)
    }
}

#[async_trait]
impl FileStore for Store {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        std::fs::write(self.path(key), data).map_err(Error::internal)
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        if self.is_big(key) {
            return Ok(generated(0, BIG as usize));
        }
        std::fs::read(self.path(key)).map_err(|_| Error::NotFound("object".into()))
    }
    async fn get_range(&self, key: &str, range: std::ops::Range<u64>) -> Result<Vec<u8>> {
        if self.is_big(key) {
            let end = range.end.min(BIG);
            return Ok(generated(range.start, end.saturating_sub(range.start) as usize));
        }
        use std::io::{Read, Seek};
        let mut f = std::fs::File::open(self.path(key)).map_err(|_| Error::NotFound("object".into()))?;
        f.seek(std::io::SeekFrom::Start(range.start)).map_err(Error::internal)?;
        let mut out = Vec::new();
        f.take(range.end - range.start).read_to_end(&mut out).map_err(Error::internal)?;
        Ok(out)
    }
    async fn delete(&self, key: &str) -> Result<()> {
        let _ = std::fs::remove_file(self.path(key));
        Ok(())
    }
    async fn writer(&self, key: &str) -> Result<Box<dyn ObjectWriter + '_>> {
        Ok(Box::new(DiskWriter { store: self, file: std::fs::File::create(self.path(key)).map_err(Error::internal)? }))
    }
}

const PACKAGE: &str = "package: shipping\nversion: 0.1.0\nlabel: Shipping\ndepends: [core]\n";

const ENTITIES: &str = r#"
- name: ShipmentLine
  label: Shipment Line
  kind: child
  fields:
    - {name: item, type: data}
- name: ShipmentNote
  label: Shipment Note
  kind: child
  fields:
    - {name: note, type: data}
- name: Shipment
  label: Shipment
  kind: master
  title_field: title
  fields:
    - {name: title, type: data, required: true}
    - {name: lines, type: table, of: ShipmentLine}
    - {name: notes, type: table, of: ShipmentNote}
"#;

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-export-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("shipping/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("shipping/package.yaml"), PACKAGE).unwrap();
    std::fs::write(dir.join("shipping/entities/shipping.yaml"), ENTITIES).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

/// A kernel on the emptied database, its files in `store`, and a workspace's administrator.
async fn boot(store: Arc<Store>) -> (Arc<Kernel>, ferrum_kernel::Ctx) {
    let url = std::env::var("FERRUM_EXPORT_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_export_test".into());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let opener = ferrum_kernel::vault::Opener {
        database_url: url,
        file: "/nonexistent/root.key".into(),
        kdf: ferrum_kernel::vault::Kdf { m_kib: 64, t: 1, p: 1 },
        ..Default::default()
    };
    ferrum_kernel::vault::open(&db, opener, false).await.expect("the vault");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    k.set_file_store(store);
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["shipping".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    (k, admin)
}

fn run(scenario: impl std::future::Future<Output = ()> + Send + 'static) {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario).await.unwrap() });
}

/// The SHA-256 of what `r` reads.
fn sha_of(mut r: impl std::io::Read) -> String {
    let mut h = Sha256::new();
    std::io::copy(&mut r, &mut h).unwrap();
    hex::encode(h.finalize())
}

/// Exporting a workspace with one 300 MiB file holds a few chunks of it, not the file and not the part it goes into;
/// the parts open with the `zip` crate's reader and with `unzip -t`, each file as it was stored. A store that fails,
/// or a worker that dies, part-way through a part leaves no object no file names.
#[test]
fn large_file_bounded_memory() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    run(async {
        let dir = std::env::temp_dir().join(format!("ferrum-export-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store { dir: dir.clone(), fail_at: AtomicU64::new(u64::MAX), hang_at: AtomicU64::new(u64::MAX), ..Default::default() });
        let (k, admin) = boot(store.clone()).await;
        let big_id = Uuid::now_v7();
        let big_key = format!("tenants/{}/{big_id}", admin.tenant);
        *store.big.lock().unwrap() = big_key.clone();
        let mut tx = k.db.system_tenant_tx(admin.tenant).await.unwrap();
        // The large file, and one storage has lost.
        sqlx::query(
            "insert into file_object (tenant_id, id, file_name, content_type, size_bytes, storage_key, sha256, created_by) \
             values ($1, $2, 'big.bin', 'application/octet-stream', $3, $4, '', $5), ($1, $6, 'lost.bin', 'text/plain', 4, $7, '', $5)",
        )
        .bind(admin.tenant)
        .bind(big_id)
        .bind(BIG as i64)
        .bind(&big_key)
        .bind(admin.user)
        .bind(Uuid::now_v7())
        .bind(format!("tenants/{}/{}", admin.tenant, Uuid::now_v7()))
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let note = k.store_file(&admin, None, "note.txt", "text/plain", b"a small file").await.unwrap();

        // ── what it holds ──
        let job = k.export_workspace(&admin).await.expect("queued");
        let base = reset_peak();
        let started = std::time::Instant::now();
        assert_eq!(k.run_jobs_once(1).await.unwrap(), 1);
        let peak = PEAK.load(Ordering::Relaxed) - base;
        let v = k.job(&admin, job).await.unwrap();
        assert_eq!(v["status"], "done", "{v}");
        println!("export_peak_heap: {} MiB above the start for one {} MiB file ({} s)", peak >> 20, BIG >> 20, started.elapsed().as_secs());
        assert!(peak < 64 << 20, "an export holds a few chunks of a file, not the file: {} MiB", peak >> 20);

        // ── what it wrote opens with ordinary tools, each file as it was ──
        let result = &v["result"];
        assert_eq!(result["files_missing"], 1, "{result}");
        let parts = result["parts"].as_array().unwrap();
        assert!(parts.len() >= 2, "the large file has a part to itself: {result}");
        let unzip = std::process::Command::new("unzip").arg("-v").output().is_ok();
        if !unzip {
            println!("unzip is not installed: the parts are read with the zip crate only");
        }
        let mut found = std::collections::HashMap::new();
        for part in parts {
            let path = store.path(&format!("tenants/{}/{}", admin.tenant, part["id"].as_str().unwrap()));
            assert_eq!(part["sha256"].as_str().unwrap(), sha_of(std::fs::File::open(&path).unwrap()), "the part's digest is its bytes'");
            let mut zip = zip::ZipArchive::new(std::fs::File::open(&path).unwrap()).expect("a zip");
            for i in 0..zip.len() {
                let f = zip.by_index(i).unwrap();
                let name = f.name().unwrap().to_string();
                found.insert(name, sha_of(f));
            }
            if unzip {
                let tested = std::process::Command::new("unzip").arg("-tqq").arg(&path).output().unwrap();
                assert!(tested.status.success(), "unzip -t {}: {}", path.display(), String::from_utf8_lossy(&tested.stdout));
            }
        }
        assert_eq!(found.get(&format!("files/{big_id}")), Some(&generated_sha()), "the large file, byte for byte");
        assert_eq!(found.get(&format!("files/{}", note["id"].as_str().unwrap())), note.get("sha256").and_then(|s| s.as_str()).map(str::to_string).as_ref());
        assert!(found.contains_key("manifest.json") && found.keys().any(|n| n.starts_with("records/")));
        // ponytail: zip64 (an entry over 4 GiB) is not exercised: it needs 4 GiB written.

        // ── the store fails part-way through the large file's part: the job fails and is tried again; nothing half
        //    written stays, and no file names it ──
        store.fail_at.store(store.written.load(Ordering::Relaxed) + (50 << 20), Ordering::Relaxed);
        let job = k.export_workspace(&admin).await.expect("queued");
        assert_eq!(k.run_jobs_once(1).await.unwrap(), 1);
        let v = k.job(&admin, job).await.unwrap();
        assert_eq!((v["status"].as_str(), v["attempts"].as_i64()), (Some("queued"), Some(1)), "{v}");
        assert!(v["last_error"].as_str().unwrap_or_default().contains("went away"), "{v}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(store.left_over(&k).await, Vec::<String>::new(), "what was written of the failed part is gone");
        store.fail_at.store(u64::MAX, Ordering::Relaxed);
        let mut tx = k.db.system_tx().await.unwrap();
        sqlx::query("update job set run_at = now() where id = $1").bind(job).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(k.run_jobs_once(1).await.unwrap(), 1);
        assert_eq!(k.job(&admin, job).await.unwrap()["status"], "done");
        assert_eq!(store.left_over(&k).await, Vec::<String>::new(), "the failed attempt's finished part went with it");

        // ── its worker dies part-way through a part (nothing of it runs again): the next attempt deletes what it wrote ──
        store.hang_at.store(store.written.load(Ordering::Relaxed) + (50 << 20), Ordering::Relaxed);
        store.stuck.store(false, Ordering::Relaxed);
        let job = k.export_workspace(&admin).await.expect("queued");
        let mut dying = Box::pin(k.run_jobs_once(1));
        tokio::select! {
            _ = &mut dying => panic!("the worker was to stop part-way"),
            _ = async { while !store.stuck.load(Ordering::Relaxed) { tokio::time::sleep(Duration::from_millis(10)).await } } => {}
        }
        std::mem::forget(dying);
        let left = store.left_over(&k).await;
        assert_eq!(left.len(), 1, "the part it was writing: {left:?}");
        store.hang_at.store(u64::MAX, Ordering::Relaxed);
        let mut tx = k.db.system_tx().await.unwrap();
        sqlx::query("update job set lease_until = now() - interval '1 second' where id = $1").bind(job).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(k.run_jobs_once(1).await.unwrap(), 1);
        assert_eq!(k.job(&admin, job).await.unwrap()["status"], "done");
        assert_eq!(store.left_over(&k).await, Vec::<String>::new(), "the dead worker's part is gone");
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// An export reads the child rows of a page of records in one go: exporting 200 records with two child tables takes
/// no more statements than exporting 100, and each record comes out whole, its rows in order.
#[test]
fn children_one_query_per_table() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _ = tracing::subscriber::set_global_default(Statements);
    run(async {
        let dir = std::env::temp_dir().join(format!("ferrum-export-rows-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store { dir: dir.clone(), fail_at: AtomicU64::new(u64::MAX), hang_at: AtomicU64::new(u64::MAX), ..Default::default() });
        let (k, admin) = boot(store.clone()).await;
        let mut statements = Vec::new();
        let mut result = serde_json::Value::Null;
        for round in 0..2 {
            for i in 0..100 {
                let doc = serde_json::json!({
                    "title": format!("shipment {round}-{i:03}"),
                    "lines": [{"item": format!("first of {round}-{i:03}")}, {"item": format!("second of {round}-{i:03}")}],
                    "notes": [{"note": "fragile"}, {"note": format!("note of {round}-{i:03}")}],
                });
                k.insert(&admin, "Shipment", doc.as_object().unwrap().clone()).await.unwrap();
            }
            let job = k.export_workspace(&admin).await.expect("queued");
            let (ran, n) = counted(k.run_jobs_once(1)).await;
            assert_eq!(ran.unwrap(), 1);
            let v = k.job(&admin, job).await.unwrap();
            assert_eq!(v["status"], "done", "{v}");
            statements.push(n);
            result = v["result"].clone();
        }
        println!(
            "export_statements: {} for 100 records with two child tables, {} for 200: {} more",
            statements[0],
            statements[1],
            statements[1] as i64 - statements[0] as i64
        );
        let mut records = String::new();
        for part in result["parts"].as_array().unwrap() {
            let path = store.path(&format!("tenants/{}/{}", admin.tenant, part["id"].as_str().unwrap()));
            let mut zip = zip::ZipArchive::new(std::fs::File::open(&path).unwrap()).expect("a zip");
            for i in 0..zip.len() {
                let mut f = zip.by_index(i).unwrap();
                if f.name().unwrap().starts_with("records/Shipment/") {
                    std::io::Read::read_to_string(&mut f, &mut records).unwrap();
                }
            }
        }
        assert_eq!(records.lines().count(), 200);
        let one: serde_json::Value = serde_json::from_str(records.lines().find(|l| l.contains("shipment 1-042")).unwrap()).unwrap();
        let items: Vec<&str> = one["lines"].as_array().unwrap().iter().map(|r| r["item"].as_str().unwrap()).collect();
        let notes: Vec<&str> = one["notes"].as_array().unwrap().iter().map(|r| r["note"].as_str().unwrap()).collect();
        assert_eq!((items, notes), (vec!["first of 1-042", "second of 1-042"], vec!["fragile", "note of 1-042"]), "{one}");
        assert!(statements[1] <= statements[0] + 10, "child rows are read a page at a time, not a record at a time: {statements:?}");
        let _ = std::fs::remove_dir_all(&dir);
    });
}
