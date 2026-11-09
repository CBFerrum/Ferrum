//! A workspace closed, kept, and then deleted by an operator, against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first). AT-CLOUD-001: closing destroys nothing, and
//! deleting waits for its date, for every app to have let go of what it holds elsewhere, and for its files.

use async_trait::async_trait;
use chrono::{Duration, Utc};
use ferrum_kernel::account::Leaving;
use ferrum_kernel::files::FileStore;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings, Result, Tx};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// An app with something of the workspace's at a provider, for as long as `0` says so.
struct Holding(Arc<AtomicBool>);

#[async_trait]
impl Leaving for Holding {
    async fn held_outside(&self, _: &mut Tx, _: Uuid) -> Result<Vec<String>> {
        Ok(if self.0.load(Ordering::SeqCst) { vec!["1 database at a provider".into()] } else { vec![] })
    }
}

/// Object storage that can be made to fail, as a real one sometimes does.
#[derive(Default)]
struct Store {
    files: Mutex<HashMap<String, Vec<u8>>>,
    failing: AtomicBool,
}

#[async_trait]
impl FileStore for Store {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        self.files.lock().unwrap().insert(key.into(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        self.files.lock().unwrap().get(key).cloned().ok_or_else(|| Error::NotFound(key.into()))
    }
    async fn delete(&self, key: &str) -> Result<()> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(Error::Unavailable("the store did not answer".into()));
        }
        self.files.lock().unwrap().remove(key);
        Ok(())
    }
}

fn superuser(address: &str) -> String {
    let base = std::env::var("FERRUM_TEST_SUPERUSER").unwrap_or_else(|_| format!("postgres://{}@localhost:5432", std::env::var("USER").unwrap_or_default()));
    format!("{base}/{}", address.rsplit('/').next().unwrap())
}

async fn as_superuser(address: &str, sql: &'static str) {
    let db = Db::connect(DbConfig { url: superuser(address), max_connections: 1, ..Default::default() }).await.expect("a superuser");
    sqlx::raw_sql(sql).execute(&db.pool).await.unwrap();
    db.pool.close().await;
}

async fn workspace(k: &Kernel, slug: &str) -> (Uuid, Ctx) {
    let email = format!("admin@{slug}.test");
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: email.clone(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login(slug, &email, "Correct-Horse-9").await.unwrap();
    (tenant, k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap())
}

/// What is left of a workspace in any table, by any column that names one.
async fn left_of(k: &Kernel, tenant: Uuid) -> i64 {
    let tables: Vec<String> = sqlx::query_scalar(
        "select c.table_name::text from information_schema.columns c join information_schema.tables t using (table_schema, table_name) \
         where c.table_schema = 'public' and c.column_name = 'tenant_id' and t.table_type = 'BASE TABLE'",
    )
    .fetch_all(&k.db.pool)
    .await
    .unwrap();
    let mut tx = k.db.system_tx().await.unwrap();
    let mut n = 0;
    for t in tables {
        n += sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!("select count(*) from \"{t}\" where tenant_id = $1")))
            .bind(tenant)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    }
    n += sqlx::query_scalar::<_, i64>("select count(*) from tenant where id = $1").bind(tenant).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

#[test]
fn a_closed_workspace_is_kept_and_then_deleted_alone() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let holding = Arc::new(AtomicBool::new(false));
    let mut hooks = HookRegistry::default();
    hooks.add_leaving(Arc::new(Holding(holding.clone())));
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    let store = Arc::new(Store::default());
    k.set_file_store(store.clone());
    let (acme, admin) = workspace(&k, "acme").await;
    let (bravo, theirs) = workspace(&k, "bravo").await;
    let mine = k.store_file(&admin, None, "a.txt", "text/plain", b"acme's").await.unwrap();
    let kept = k.store_file(&theirs, None, "b.txt", "text/plain", b"bravo's").await.unwrap();
    k.create_user(&admin, "clerk@acme.test", "Clerk", Some("Correct-Horse-9"), &[], false).await.unwrap();
    // An app uploaded for this workspace alone names it by another column.
    sqlx::query("insert into app_release (app_id, version, runtime, manifest, digest, source, private_tenant) values ('their-tool', '1.0.0', 'wasm', '{}', 'sha256:0', 'upload', $1)")
        .bind(acme)
        .execute(&k.db.pool)
        .await
        .unwrap();
    let bravo_before = left_of(&k, bravo).await;

    let deleting = async |confirm: &str, apply: bool| k.delete_tenant("acme", confirm, apply, "the customer left, and the retention passed").await;
    assert!(matches!(deleting("acme", false).await, Err(Error::Conflict(m)) if m.contains("only a closed")));

    // A legal hold in force keeps the workspace from deletion until it is released (FEO-007).
    let hold = k.place_legal_hold(&admin, Some("clerk@acme.test"), None, "Matter 9").await.unwrap();

    // Closed: nothing is destroyed, everyone may still read, and nobody may change anything.
    let until = Utc::now().date_naive() + Duration::days(30);
    assert!(k.close_account(acme, Utc::now().date_naive(), "the customer left").await.is_err(), "a closed workspace is kept a while");
    k.close_account(acme, until, "the customer left").await.unwrap();
    let admin = k.load_ctx(acme, admin.user, None, "it".into()).await.unwrap();
    assert!(admin.read_only, "closed is read-only");
    assert_eq!(k.read_file(&admin, Uuid::parse_str(mine["id"].as_str().unwrap()).unwrap()).await.unwrap().2, b"acme's", "and still readable");
    assert!(matches!(deleting("acme", false).await, Err(Error::Conflict(m)) if m.contains("kept until")));

    // Its time is up. The slug has to be typed again, and what an app holds elsewhere has to go first.
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update tenant set retain_until = current_date - 1 where id = $1").bind(acme).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(deleting("acm", true).await, Err(Error::Validation(_))));
    holding.store(true, Ordering::SeqCst);
    let said = deleting("acme", false).await.expect("what would go");
    assert_eq!(said["held_outside"], json!(["1 database at a provider"]), "{said}");
    assert!(said["rows"]["app_user"].as_i64().unwrap() >= 2 && said["files"] == json!(1), "{said}");
    assert!(matches!(deleting("acme", true).await, Err(Error::Conflict(m)) if m.contains("outside this database")));
    holding.store(false, Ordering::SeqCst);
    assert_eq!(said["legal_holds"], json!(1), "{said}");
    assert!(matches!(deleting("acme", true).await, Err(Error::Conflict(m)) if m.contains("legal holds are in force")), "a hold in force stops it");
    // Releasing it is its own act, recorded: the workspace is opened for it, then closed again.
    k.set_account_state(acme, "active", Some("to release Matter 9")).await.unwrap();
    let admin = k.load_ctx(acme, admin.user, None, "it".into()).await.unwrap();
    k.release_legal_hold(&admin, hold).await.unwrap();
    k.close_account(acme, until, "the customer left").await.unwrap();
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update tenant set retain_until = current_date - 1 where id = $1").bind(acme).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(deleting("acme", false).await.unwrap()["legal_holds"], json!(0));

    // The store fails part way: no row goes, and running it again finishes the job.
    as_superuser(&url, "grant set on parameter session_replication_role to ferrum").await;
    store.failing.store(true, Ordering::SeqCst);
    assert!(matches!(deleting("acme", true).await, Err(Error::Unavailable(m)) if m.contains("no row was")));
    assert!(left_of(&k, acme).await > 0, "everything is still there");
    store.failing.store(false, Ordering::SeqCst);
    let done = deleting("acme", true).await.expect("deleted");
    as_superuser(&url, "revoke set on parameter session_replication_role from ferrum").await;
    assert_eq!(done["deleted"], json!(true), "{done}");

    // Gone, all of it, and nothing of anyone else's.
    assert_eq!(left_of(&k, acme).await, 0);
    let private: i64 = sqlx::query_scalar("select count(*) from app_release where private_tenant = $1").bind(acme).fetch_one(&k.db.pool).await.unwrap();
    assert_eq!(private, 0, "nor anything that names it some other way");
    assert!(store.files.lock().unwrap().len() == 1, "only bravo's file is left");
    assert_eq!(left_of(&k, bravo).await, bravo_before);
    let theirs = k.load_ctx(bravo, theirs.user, None, "it".into()).await.unwrap();
    assert_eq!(k.read_file(&theirs, Uuid::parse_str(kept["id"].as_str().unwrap()).unwrap()).await.unwrap().2, b"bravo's");
    let record: Value = sqlx::query_scalar("select jsonb_build_object('slug', slug, 'files', files, 'note', note) from tenant_deletion where id = $1")
        .bind(acme)
        .fetch_one(&k.db.pool)
        .await
        .unwrap();
    assert_eq!((&record["slug"], &record["files"]), (&json!("acme"), &json!(1)), "and a record that it happened");
    assert!(matches!(deleting("acme", false).await, Err(Error::NotFound(_))));
}
