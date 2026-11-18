//! The schema sync's index and constraint builds on a table that has rows and writers (K7), against a real PostgreSQL
//! (`FERRUM_DDL_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_ddl_test`; the database is
//! reset first): a start that adds an index or a foreign key to a large table does not stall the inserts into it, a
//! build that failed leaves nothing invalid and is made again, rows that repeat a value fail the start naming the
//! index, and two replicas starting at once build once.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings, ddl};
use ferrum_meta::Catalog;
use sqlx::{AssertSqlSafe, Connection, PgConnection};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// The tests share one database: one at a time.
static ONE: Mutex<()> = Mutex::new(());

const ROWS: i64 = 300_000;

fn url() -> String {
    std::env::var("FERRUM_DDL_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_ddl_test".into())
}

fn run(f: impl std::future::Future<Output = ()> + Send + 'static) {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(f).await.unwrap() });
}

fn catalog() -> Catalog {
    Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog")
}

/// A started kernel on an empty database, and what the tests build on: the exchange rates' table (two links to
/// currencies, a unique group), a workspace, two currencies.
struct World {
    k: Arc<Kernel>,
    table: String,
    tenant: Uuid,
    from: Uuid,
    to: Uuid,
}

async fn sql(db: &Db, s: String) {
    sqlx::raw_sql(AssertSqlSafe(s.clone())).execute(&db.pool).await.unwrap_or_else(|e| panic!("{s}: {e}"));
}

async fn world() -> World {
    let db = Db::connect(DbConfig { url: url(), max_connections: 12, ..Default::default() }).await.expect("connect");
    sql(&db, "drop schema if exists public cascade; drop schema if exists vault cascade; create schema public".into()).await;
    let catalog = catalog();
    let schema = ddl::cluster_schema(&catalog).unwrap();
    let (table, currency) = (schema.entity("ExchangeRate").unwrap().table.clone(), schema.entity("Currency").unwrap().table.clone());
    let k = Kernel::boot(db, catalog, HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
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
    let (from, to) = (Uuid::new_v4(), Uuid::new_v4());
    let mut tx = k.db.system_tx().await.unwrap();
    for id in [from, to] {
        sqlx::query(AssertSqlSafe(format!("insert into {currency} (tenant_id, id) values ($1, $2)"))).bind(tenant).bind(id).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    World { k, table, tenant, from, to }
}

impl World {
    fn db(&self) -> &Db {
        &self.k.db
    }

    /// `n` rows of the table, and the table without what the sync makes of it (its indexes but the primary key, its
    /// links' foreign keys): so that the next sync builds them over the rows.
    async fn fill_and_strip(&self, n: i64) {
        let t = &self.table;
        // No statement timeout: the fill of a large table is not what is measured.
        let mut conn = system_conn().await;
        sqlx::query(AssertSqlSafe(format!(
            "insert into {t} (tenant_id, id, from_currency, to_currency, valid_from) \
             select $1, gen_random_uuid(), $2, $3, date '2000-01-01' + g::int from generate_series(1, $4) g"
        )))
        .bind(self.tenant)
        .bind(self.from)
        .bind(self.to)
        .bind(n)
        .execute(&mut conn)
        .await
        .unwrap();
        self.strip().await;
    }

    async fn strip(&self) {
        let t = &self.table;
        let keys: Vec<String> =
            sqlx::query_scalar("select conname::text from pg_constraint where conrelid = $1::regclass and contype = 'f' and conname like 'fk\\_%'")
                .bind(t)
                .fetch_all(&self.db().pool)
                .await
                .unwrap();
        for k in keys {
            sql(self.db(), format!("alter table {t} drop constraint \"{k}\"")).await;
        }
        for i in self.indexes().await {
            if !i.ends_with("_pkey") {
                sql(self.db(), format!("drop index \"{i}\"")).await;
            }
        }
        // So that the next sync is not skipped.
        sql(self.db(), "delete from cluster_state where key = 'catalog_fingerprint'".into()).await;
    }

    async fn indexes(&self) -> Vec<String> {
        sqlx::query_scalar("select indexname::text from pg_indexes where schemaname = 'public' and tablename = $1 order by 1")
            .bind(&self.table)
            .fetch_all(&self.db().pool)
            .await
            .unwrap()
    }

    async fn invalid(&self) -> Vec<String> {
        sqlx::query_scalar("select c.relname::text from pg_index i join pg_class c on c.oid = i.indexrelid where not i.indisvalid order by 1")
            .fetch_all(&self.db().pool)
            .await
            .unwrap()
    }

    async fn unvalidated(&self) -> Vec<String> {
        sqlx::query_scalar("select conname::text from pg_constraint where not convalidated order by 1").fetch_all(&self.db().pool).await.unwrap()
    }

    async fn unique_index(&self) -> String {
        sqlx::query_scalar("select indexname::text from pg_indexes where tablename = $1 and indexdef like 'CREATE UNIQUE%' and indexdef like '%valid_from%'")
            .bind(&self.table)
            .fetch_one(&self.db().pool)
            .await
            .unwrap()
    }

    fn sync(&self) -> tokio::task::JoinHandle<ferrum_kernel::Result<ddl::SyncReport>> {
        let (db, catalog) = (self.db().clone(), self.k.registry.catalog());
        tokio::spawn(async move { ddl::sync(&db, &catalog, false).await })
    }
}

/// A connection of its own, working as the server does for itself (row-level security lets it through).
async fn system_conn() -> PgConnection {
    let mut conn = PgConnection::connect(&url()).await.unwrap();
    sqlx::query("select set_config('ferrum.system', 'on', false)").execute(&mut conn).await.unwrap();
    conn
}

/// Inserts into the table one row at a time, as a workspace's users do, until told to stop: the slowest one. `done` counts
/// the inserts as they complete.
async fn inserter(w: &World, stop: Arc<AtomicBool>, done: Arc<AtomicI64>) -> tokio::task::JoinHandle<(Duration, i64)> {
    let mut conn = system_conn().await;
    let (t, tenant, from, to) = (w.table.clone(), w.tenant, w.from, w.to);
    tokio::spawn(async move {
        let (mut slowest, mut n) = (Duration::ZERO, 0);
        while !stop.load(Ordering::Relaxed) {
            let started = Instant::now();
            sqlx::query(AssertSqlSafe(format!(
                "insert into {t} (tenant_id, id, from_currency, to_currency, valid_from) values ($1, gen_random_uuid(), $2, $3, date '1900-01-01' + $4::int)"
            )))
            .bind(tenant)
            .bind(from)
            .bind(to)
            .bind(n as i32)
            .execute(&mut conn)
            .await
            .unwrap();
            slowest = slowest.max(started.elapsed());
            n += 1;
            done.fetch_add(1, Ordering::Relaxed);
        }
        (slowest, n)
    })
}

/// How long a plain `create unique index` over the rows takes, the sync's longest build here: what a start used to hold
/// the table for.
async fn plain_build(w: &World) -> Duration {
    let started = Instant::now();
    sql(w.db(), format!("create unique index ref_build on {} (tenant_id, from_currency, to_currency, valid_from)", w.table)).await;
    let took = started.elapsed();
    sql(w.db(), "drop index ref_build".into()).await;
    took
}

/// While `building` is set, polls `pg_stat_activity` every few milliseconds for a writer's insert that waits on a lock
/// another backend holds: what a plain `create index` makes (SHARE conflicts with the insert's ROW EXCLUSIVE; the
/// concurrent build's SHARE UPDATE EXCLUSIVE does not). Returns the samples taken and those that saw one.
async fn sampler(table: String, building: Arc<AtomicBool>, stop: Arc<AtomicBool>) -> tokio::task::JoinHandle<(u64, u64)> {
    let mut conn = PgConnection::connect(&url()).await.unwrap();
    tokio::spawn(async move {
        let (mut samples, mut blocked) = (0, 0);
        while !stop.load(Ordering::Relaxed) {
            if building.load(Ordering::Relaxed) {
                let waiting: i64 = sqlx::query_scalar(
                    "select count(*) from pg_stat_activity where datname = current_database() and query like $1 \
                     and wait_event_type = 'Lock' and cardinality(pg_blocking_pids(pid)) > 0",
                )
                .bind(format!("insert into {table}%"))
                .fetch_one(&mut conn)
                .await
                .unwrap();
                samples += 1;
                blocked += (waiting > 0) as u64;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
        (samples, blocked)
    })
}

/// The measuring test: while the sync adds its link indexes and foreign keys to a table of 300,000 rows that writers
/// insert into, no writer is ever seen waiting on a lock the sync holds. Before (a plain `create index`) the sampler sees
/// one for the whole build. The verdict is the sampler's, not a time: the times are printed.
#[test]
fn link_index_does_not_block_inserts() {
    run(async {
        let w = world().await;
        w.fill_and_strip(ROWS).await;
        let build = plain_build(&w).await;

        let (stop, building, done) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)), Arc::new(AtomicI64::new(0)));
        let writer = inserter(&w, stop.clone(), done.clone()).await;
        let watcher = sampler(w.table.clone(), building.clone(), stop.clone()).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (before, started) = (done.load(Ordering::Relaxed), Instant::now());
        building.store(true, Ordering::Relaxed);
        let report = w.sync().await.unwrap().expect("the sync");
        building.store(false, Ordering::Relaxed);
        let (during, synced) = (done.load(Ordering::Relaxed) - before, started.elapsed());
        tokio::time::sleep(Duration::from_millis(300)).await;
        stop.store(true, Ordering::Relaxed);
        let (slowest, inserts) = writer.await.unwrap();
        let (samples, blocked) = watcher.await.unwrap();
        println!(
            "K7: {ROWS} rows; a plain unique index build over them {build:?}; the sync ({} statements) {synced:?}; {inserts} inserts, {during} of them during the sync, the slowest {slowest:?}; \
             {samples} samples of the writers' locks during the sync, {blocked} saw a writer blocked",
            report.statements
        );

        assert!(!report.skipped);
        assert!(w.invalid().await.is_empty() && w.unvalidated().await.is_empty(), "nothing is left invalid");
        assert!(w.indexes().await.iter().any(|i| i.contains("from_currency")), "the link's index is built again");
        assert!(during > 0, "the writers did not run during the sync: nothing was observed");
        assert!(samples >= 20, "not observed: {samples} samples in {synced:?}, a build too fast to sample");
        assert_eq!(blocked, 0, "a writer was blocked by the sync in {blocked} of {samples} samples");
        assert!(slowest < Duration::from_secs(1), "an insert waited {slowest:?}");
        // What the sync set for its own waits is given back: the pool's 10 s.
        let mut conn = w.db().direct.acquire().await.unwrap();
        let t: String = sqlx::query_scalar("show lock_timeout").fetch_one(&mut *conn).await.unwrap();
        assert_eq!(t, "10s");
    });
}

/// A build that failed midway leaves an invalid index of its name (PostgreSQL skips `if not exists` on it): the next
/// start drops it and builds it again.
#[test]
fn invalid_index_is_rebuilt() {
    run(async {
        let w = world().await;
        w.fill_and_strip(1_000).await;
        let t = &w.table;
        // The unique index the sync makes (to learn its name), then a build of it that fails on two rows repeating a value.
        w.sync().await.unwrap().expect("the sync");
        let name = w.unique_index().await;
        sql(w.db(), format!("drop index \"{name}\"")).await;
        let mut tx = w.db().system_tx().await.unwrap();
        sqlx::query(AssertSqlSafe(format!("update {t} set valid_from = date '2000-01-02' where valid_from = date '2000-01-03'")))
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let failed =
            sqlx::raw_sql(AssertSqlSafe(format!("create unique index concurrently \"{name}\" on {t} (tenant_id, from_currency, to_currency, valid_from)")))
                .execute(&w.db().pool)
                .await;
        assert!(failed.is_err());
        assert_eq!(w.invalid().await, vec![name.clone()], "the failed build is left invalid");
        let mut tx = w.db().system_tx().await.unwrap();
        sqlx::query(AssertSqlSafe(format!(
            "update {t} set valid_from = date '2100-01-01' where id = (select id from {t} where valid_from = date '2000-01-02' limit 1)"
        )))
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        sql(w.db(), "delete from cluster_state where key = 'catalog_fingerprint'".into()).await;

        w.sync().await.unwrap().expect("the start builds it again");
        assert!(w.invalid().await.is_empty(), "{:?}", w.invalid().await);
        assert_eq!(w.unique_index().await, name);
    });
}

/// Rows that repeat a value fail the start, naming the index and the repeated key; nothing invalid is left; once the
/// rows are changed, the next start builds it.
#[test]
fn duplicates_fail_the_start_and_leave_nothing_invalid() {
    run(async {
        let w = world().await;
        w.fill_and_strip(1_000).await;
        let t = &w.table;
        let mut tx = w.db().system_tx().await.unwrap();
        sqlx::query(AssertSqlSafe(format!("update {t} set valid_from = date '2000-01-02' where valid_from = date '2000-01-03'")))
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let said = w.sync().await.unwrap().expect_err("the build cannot finish").to_string();
        println!("K7: {said}");
        assert!(said.contains(t.as_str()) && said.contains("valid_from") && said.contains("repeat"), "{said}");
        assert!(w.invalid().await.is_empty(), "left invalid: {:?}", w.invalid().await);

        let mut tx = w.db().system_tx().await.unwrap();
        sqlx::query(AssertSqlSafe(format!(
            "update {t} set valid_from = date '2100-01-01' where id = (select id from {t} where valid_from = date '2000-01-02' limit 1)"
        )))
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        w.sync().await.unwrap().expect("with the rows changed the start builds it");
        assert!(w.invalid().await.is_empty());
        w.unique_index().await;
    });
}

/// Two replicas starting at once: one builds, the other waits for it and finds nothing to do. One that waits for the
/// other's build longer than a request waits for a lock (10 s) is not refused.
#[test]
fn two_replicas_build_once() {
    run(async {
        let w = world().await;
        w.fill_and_strip(1_000).await;
        let (a, b) = (w.sync(), w.sync());
        let (a, b) = (a.await.unwrap().expect("first"), b.await.unwrap().expect("second"));
        assert_ne!(a.skipped, b.skipped, "one built and one found it built: {a:?} {b:?}");
        assert!(w.invalid().await.is_empty());

        // The sync's lock held by another replica for longer than the 10 s every other statement waits.
        sql(w.db(), "delete from cluster_state where key = 'catalog_fingerprint'".into()).await;
        let mut holder = PgConnection::connect(&url()).await.unwrap();
        sqlx::query("select pg_advisory_lock($1)").bind(0x0FE2_2077_i64).execute(&mut holder).await.unwrap();
        let waiting = w.sync();
        tokio::time::sleep(Duration::from_secs(12)).await;
        assert!(!waiting.is_finished(), "the sync waits for the lock");
        sqlx::query("select pg_advisory_unlock($1)").bind(0x0FE2_2077_i64).execute(&mut holder).await.unwrap();
        waiting.await.unwrap().expect("a start that waited 12 s for another replica's sync");
    });
}

/// A transaction that is open when an index build starts holds the build back (PostgreSQL waits for it before the
/// build's phases), not the inserts into the table; when it ends, the build finishes.
#[test]
fn build_waits_for_an_open_transaction_not_the_writers() {
    run(async {
        let w = world().await;
        w.fill_and_strip(1_000).await;
        let mut old = PgConnection::connect(&url()).await.unwrap();
        sqlx::raw_sql("begin isolation level repeatable read; select 1").execute(&mut old).await.unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let writer = inserter(&w, stop.clone(), Arc::new(AtomicI64::new(0))).await;
        let syncing = w.sync();
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!syncing.is_finished(), "the build waits for the transaction opened before it");
        sqlx::raw_sql("commit").execute(&mut old).await.unwrap();
        syncing.await.unwrap().expect("the build finishes once the transaction has");
        stop.store(true, Ordering::Relaxed);
        let (slowest, inserts) = writer.await.unwrap();
        println!("K7: {inserts} inserts while a build waited 3 s for an old transaction, the slowest {slowest:?}");
        assert!(slowest < Duration::from_secs(1));
        assert!(w.invalid().await.is_empty());
    });
}

/// A foreign key is added `not valid` and validated apart: one that a start left unvalidated (its validation failed or
/// the start ended first) is validated by the next, and is not added a second time.
#[test]
fn an_unvalidated_key_is_validated_at_the_next_start() {
    run(async {
        let w = world().await;
        w.fill_and_strip(1_000).await;
        let t = &w.table;
        w.sync().await.unwrap().expect("the sync");
        let (key, def): (String, String) = sqlx::query_as(
            "select conname::text, pg_get_constraintdef(oid) from pg_constraint where conrelid = $1::regclass and conname like 'fk\\_%from\\_currency'",
        )
        .bind(t)
        .fetch_one(&w.db().pool)
        .await
        .unwrap();
        sql(w.db(), format!("alter table {t} drop constraint \"{key}\"")).await;
        sql(w.db(), format!("alter table {t} add constraint \"{key}\" {def} not valid")).await;
        sql(w.db(), "delete from cluster_state where key = 'catalog_fingerprint'".into()).await;
        assert_eq!(w.unvalidated().await, vec![key.clone()]);

        w.sync().await.unwrap().expect("the next start");
        assert!(w.unvalidated().await.is_empty(), "{:?}", w.unvalidated().await);
    });
}
