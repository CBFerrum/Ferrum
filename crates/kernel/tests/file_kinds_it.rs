//! Kinds of file an app keeps for its own work (crates/kernel/src/files.rs, `FileKind`; docs/app-contract.md), against a
//! real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, reset first): Cloud's built artifacts and databases' backups are the
//! workspace's files, counted and deleted as any, but not its content to hand out. An administrator cannot download,
//! copy, delete or tie one by its id, nor find it among a record's files; the workspace's export leaves them out and says
//! so, kind by kind, its records as before; the app that declared the kind reads and lets go of them, and another app
//! cannot; a kind no app declares any more (its app left the build) is nobody's to reach all the same.

use async_trait::async_trait;
use ferrum_kernel::files::{FileKind, FileStore};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const PACKAGE: &str = "package: notes\nversion: 0.1.0\ndepends: [core]\n";
const NOTE: &str = "name: Note\nlabel: Note\nkind: master\nfields:\n  - {name: title, type: data}\n  - {name: file, type: attach}\n";

/// Stored files' bytes, for every kernel of the test.
#[derive(Default, Clone)]
struct Memory(Arc<Mutex<HashMap<String, Vec<u8>>>>);

#[async_trait]
impl FileStore for Memory {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        self.0.lock().unwrap().insert(key.into(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned().ok_or_else(|| Error::NotFound(key.into()))
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

fn catalog() -> Catalog {
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> = [("package.yaml", PACKAGE), ("entities/note.yaml", NOTE)].into_iter().map(|(k, v)| (k.into(), v.into())).collect();
    Catalog::from_packages(vec![core, ferrum_meta::catalog::load_package_files(&files).expect("notes")]).unwrap()
}

/// A kernel with the apps `cloud` (its kind `cloud.backup`, and the package `notes`) and `hr` (its kind `hr.payslip`)
/// registered as the server registers apps, or with neither (`apps` false: Cloud left the build).
async fn kernel(store: &Memory, apps: bool) -> (Arc<Kernel>, Option<(FileKind, FileKind)>) {
    let db = Db::connect(DbConfig { url: url(), max_connections: 8, ..Default::default() }).await.expect("connect");
    let mut hooks = HookRegistry::default();
    let kinds = apps.then(|| {
        let (mut backup, mut payslip) = (None, None);
        hooks.app("cloud", &[], |h| backup = Some(h.file_kind("backup"))).own("cloud", &["notes".into()]);
        hooks.app("hr", &[], |h| payslip = Some(h.file_kind("payslip")));
        (backup.unwrap(), payslip.unwrap())
    });
    let k = Kernel::boot(db, catalog(), hooks, KernelSettings::default(), None).await.expect("boot");
    k.set_file_store(Arc::new(store.clone()));
    (k, kinds)
}

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

fn not_found<T: std::fmt::Debug>(r: Result<T>) -> bool {
    matches!(r, Err(Error::NotFound(_)))
}

/// The manifest and the names of what an export holds, as an administrator downloads it.
async fn export(k: &Kernel, admin: &Ctx) -> (Value, HashMap<String, Vec<u8>>) {
    let job = k.export_workspace(admin).await.expect("queued");
    while k.run_jobs_once(4).await.unwrap() > 0 {}
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    let (status, result): (String, Option<Value>) = sqlx::query_as("select status, result from job where id = $1").bind(job).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(status, "done", "{result:?}");
    let part: Uuid = result.unwrap()["parts"][0]["id"].as_str().unwrap().parse().unwrap();
    let (_, bytes) = k.export_part(admin, job, part).await.expect("an administrator's copy");
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).expect("a zip");
    let mut contents = HashMap::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).unwrap();
        let mut out = Vec::new();
        f.read_to_end(&mut out).unwrap();
        contents.insert(f.name().unwrap().to_string(), out);
    }
    let manifest = serde_json::from_slice(&contents["manifest.json"]).unwrap();
    (manifest, contents)
}

#[test]
fn a_kind_of_file_an_app_keeps_for_its_own_work() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let db = Db::connect(DbConfig { url: url(), max_connections: 2, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    db.pool.close().await;
    let store = Memory::default();
    let (k, kinds) = kernel(&store, true).await;
    let (backup, payslip) = kinds.unwrap();
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["notes".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();

    // ── Cloud keeps a project's database backup: a file of the workspace's, tied to its kind and the project ──
    let project = Uuid::now_v7();
    let kept = backup.keep(&k, tenant, project, "db.dump.0", "application/octet-stream", b"the project's backup").await.unwrap();
    let file: Uuid = kept["id"].as_str().unwrap().parse().unwrap();
    assert!(kept.get("url").is_none(), "no address of the platform's serves it: {kept}");
    assert_eq!(backup.kind(), "cloud.backup");
    assert_eq!(backup.read(&k, tenant, file).await.unwrap().2, b"the project's backup", "its app reads it");
    assert_eq!(backup.info(&k, tenant, file).await.unwrap().size, 20);

    // ── an administrator who has its id reaches it through none of the platform's ways ──
    assert!(not_found(k.file_info(&admin, file).await), "downloaded");
    assert!(not_found(k.read_file(&admin, file).await), "read");
    assert!(not_found(k.copy_file(&admin, file, None).await), "copied");
    assert!(not_found(k.delete_file(&admin, file).await), "deleted");
    let note = k.insert(&admin, "Note", obj(json!({"title": "a note", "file": format!("/files/{file}")}))).await.expect("saved, as with an id that is no file");
    let note: Uuid = note["id"].as_str().unwrap().parse().unwrap();
    assert!(not_found(k.file_info(&admin, file).await), "not tied to the note: still nobody's");
    assert_eq!(backup.read(&k, tenant, file).await.unwrap().2, b"the project's backup", "and still its app's");
    let attached = k.store_file(&admin, Some(("Note", note)), "minutes.txt", "text/plain", b"the workspace's own file").await.unwrap();
    let listed = k.attachments(&admin, "Note", note).await.unwrap();
    assert_eq!(listed.iter().map(|f| f["id"].clone()).collect::<Vec<_>>(), [attached["id"].clone()], "the note's files, not the backup");

    // ── another app's handle reaches none of it; nor does a handle another server gave out ──
    assert!(not_found(payslip.read(&k, tenant, file).await), "another app's kind");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert_eq!(payslip.release(&k, &mut tx, tenant, &[file]).await.unwrap(), 0, "another app lets go of nothing of it");
    tx.commit().await.unwrap();

    // ── the workspace's export: its records and files as before, the backup left out and said so ──
    let (manifest, contents) = export(&k, &admin).await;
    assert_eq!(
        manifest["files_left_out"],
        json!([{"kind": "cloud.backup", "files": 1, "bytes": 20, "why": "its app keeps them for its own work and hands them out its own way"}]),
        "{manifest}"
    );
    assert_eq!(manifest["files"], json!(1), "the note's file: {manifest}");
    assert!(contents.contains_key(&format!("files/{}", attached["id"].as_str().unwrap())));
    assert!(!contents.contains_key(&format!("files/{file}")) && !String::from_utf8_lossy(&contents["files/index.jsonl"]).contains(&file.to_string()));
    let notes: String = contents.iter().filter(|(n, _)| n.starts_with("records/Note/")).map(|(_, b)| String::from_utf8_lossy(b).to_string()).collect();
    assert!(notes.contains("a note"), "the record as before: {notes}");

    // ── Cloud left the build: its kind is still nobody's through the platform's ways, and still left out ──
    let (bare, _) = kernel(&store, false).await;
    let admin = bare.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    assert!(not_found(bare.file_info(&admin, file).await), "downloaded with no app declaring the kind");
    assert!(not_found(bare.delete_file(&admin, file).await), "deleted with no app declaring the kind");
    assert!(matches!(backup.read(&bare, tenant, file).await, Err(Error::Forbidden(_))), "a handle this server did not give out");
    let (manifest, _) = export(&bare, &admin).await;
    assert_eq!(manifest["files_left_out"][0]["kind"], json!("cloud.backup"), "{manifest}");

    // ── its app lets it go: the row now, the bytes after the deletion lag ──
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert_eq!(backup.release(&k, &mut tx, tenant, &[file]).await.unwrap(), 1);
    tx.commit().await.unwrap();
    assert!(not_found(backup.read(&k, tenant, file).await), "gone");
}
