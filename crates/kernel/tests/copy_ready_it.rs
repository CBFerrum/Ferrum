//! Copies of a workspace run as the server's own role, with no grant on session_replication_role (D7; docs/operations.md,
//! "Taking one workspace back"): against real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, reset first). A migrating start
//! makes every foreign key able to wait for the commit and every trigger stand aside under `ferrum.copy`, the schema
//! sync's and packages' included; nothing changes for anyone else; and a copy that would leave a reference to nothing
//! rolls back whole.

use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use std::path::Path;

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

#[test]
fn a_copy_runs_as_the_servers_own_role() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn count(db: &Db, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).fetch_one(&db.pool).await.unwrap()
}

async fn scenario() {
    let db = Db::connect(DbConfig { url: url(), max_connections: 6, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    // A trigger and a foreign key made by plain SQL before the start, as an older migration or a package's SQL makes them.
    sqlx::raw_sql(
        "create table pre_parent (id int primary key); \
         create table pre_child (id int primary key, parent int references pre_parent (id) on delete restrict); \
         create function pre_forbid() returns trigger language plpgsql as $$ begin raise exception 'pre: no'; end $$; \
         create trigger pre_no_delete before delete on pre_parent for each row execute function pre_forbid()",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let k = Kernel::boot(db, catalog, HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let db = &k.db;

    // Every foreign key may wait, every trigger stands aside: the kernel's migrations', the sync's (entity tables, their
    // held triggers) and what plain SQL made before the start.
    assert_eq!(count(db, "select count(*) from pg_constraint where contype = 'f' and not condeferrable and connamespace = 'public'::regnamespace").await, 0);
    assert_eq!(
        count(
            db,
            "select count(*) from pg_trigger t join pg_class c on c.oid = t.tgrelid where not t.tgisinternal and c.relnamespace = 'public'::regnamespace \
             and pg_get_triggerdef(t.oid) not like '%ferrum.copy%'"
        )
        .await,
        0
    );
    assert!(
        count(db, "select count(*) from pg_trigger where tgname = 'ferrum_held' and pg_get_triggerdef(oid) like '%ferrum.copy%'").await > 0,
        "the sync's own"
    );
    assert!(
        count(db, "select count(*) from pg_constraint where contype = 'f' and condeferrable and not condeferred and conname like 'fk\\_%'").await > 0,
        "the sync's links: deferrable, checked at once"
    );
    // A second start has nothing left to do.
    let again = ferrum_kernel::ddl::copy_ready(db).await.unwrap();
    assert!(again.is_empty(), "{again:?}");

    // Nobody else sees a difference: checked at once, triggers as before.
    sqlx::query("insert into pre_parent values (1)").execute(&db.pool).await.unwrap();
    let e = sqlx::query("insert into pre_child values (1, 99)").execute(&db.pool).await.unwrap_err().to_string();
    assert!(e.contains("pre_child_parent_fkey"), "refused at once: {e}");
    let e = sqlx::query("delete from pre_parent").execute(&db.pool).await.unwrap_err().to_string();
    assert!(e.contains("pre: no"), "{e}");
    let mut tx = db.system_tx().await.unwrap();
    let e = sqlx::query("truncate audit_event").execute(&mut *tx).await.unwrap_err().to_string();
    assert!(e.contains("append-only"), "the audit trail is still append-only: {e}");
    tx.rollback().await.unwrap();

    // In a copy's transaction: the triggers stand aside, rows go in whatever the order, and are checked at the commit.
    let mut tx = db.system_tx().await.unwrap();
    sqlx::raw_sql("select set_config('ferrum.copy', 'on', true); set constraints all deferred").execute(&mut *tx).await.unwrap();
    sqlx::query("insert into pre_child values (2, 2)").execute(&mut *tx).await.unwrap();
    sqlx::query("insert into pre_parent values (2)").execute(&mut *tx).await.unwrap();
    sqlx::query("delete from pre_parent where id = 1").execute(&mut *tx).await.expect("the trigger stood aside");
    tx.commit().await.expect("every reference holds at the commit");
    assert_eq!(count(db, "select count(*) from pre_child").await, 1);

    // One that would point at nothing rolls back whole, at the commit, naming the reference.
    let mut tx = db.system_tx().await.unwrap();
    sqlx::raw_sql("select set_config('ferrum.copy', 'on', true); set constraints all deferred").execute(&mut *tx).await.unwrap();
    sqlx::query("insert into pre_parent values (3)").execute(&mut *tx).await.unwrap();
    sqlx::query("insert into pre_child values (3, 404)").execute(&mut *tx).await.unwrap();
    let e = tx.commit().await.unwrap_err().to_string();
    assert!(e.contains("pre_child_parent_fkey"), "{e}");
    assert_eq!(count(db, "select count(*) from pre_parent where id = 3").await, 0, "nothing of it stayed");

    // A trigger made afterwards without the condition is named, and a copy refuses until a start puts it right.
    sqlx::raw_sql("create trigger pre_late before update on pre_parent for each row execute function pre_forbid()").execute(&db.pool).await.unwrap();
    let mut conn = db.pool.acquire().await.unwrap();
    let (keys, triggers) = ferrum_kernel::ddl::not_ready(&mut conn).await.unwrap();
    drop(conn);
    assert!(keys.is_empty() && triggers == vec!["pre_late on pre_parent".to_string()], "{triggers:?}");
    k.db.close().await;
}
