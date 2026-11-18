//! Why the sync builds no trigram indexes (K13), against a real PostgreSQL (`FERRUM_SEARCH_INDEX_TEST_DATABASE_URL`,
//! default `postgres://ferrum:ferrum@localhost:5432/ferrum_search_index_test`; the database is reset first).
//!
//! THE FACT THIS FILE RESTS ON: every entity table has a row-level-security policy, and PostgreSQL lets a condition
//! be an index condition on such a table only if its function is leakproof. `texticlike` (`ilike`), `textlike`
//! (`like`) and `ts_match_vq` (full text) are not, so no `ilike` or full-text condition of a search or a filter can
//! use any index there, a trigram one included: a search reads the workspace's rows. To re-check on a PostgreSQL:
//!
//!     select proname, proleakproof from pg_proc where proname in ('texticlike','textlike','ts_match_vq');
//!
//! If a release of PostgreSQL makes any of them leakproof, `trigram_indexes_serve_no_search` and
//! `trigram_indexes_are_never_scanned` fail: then a trigram index on the searched columns is worth measuring again
//! (`trigram_indexes_cost_every_write` prints what it costs a write).

use ferrum_kernel::query::{Filter, ListQuery, Op};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, HookRegistry, Kernel, KernelSettings, ddl};
use ferrum_meta::Catalog;
use sqlx::{AssertSqlSafe, Connection, PgConnection};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static ONE: Mutex<()> = Mutex::new(());

const ROWS: i64 = 100_000;
/// The columns a list of customers searches: the title and the search fields.
const SEARCHED: [&str; 4] = ["customer_name", "email", "phone", "tax_id"];

fn url() -> String {
    std::env::var("FERRUM_SEARCH_INDEX_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_search_index_test".into())
}

fn run(f: impl std::future::Future<Output = ()> + Send + 'static) {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(f).await.unwrap() });
}

fn catalog() -> Catalog {
    Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog")
}

async fn exec(conn: &mut PgConnection, sql: String) {
    sqlx::raw_sql(AssertSqlSafe(sql.clone())).execute(&mut *conn).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn plan(conn: &mut PgConnection, sql: &str) -> String {
    let rows: Vec<String> = sqlx::query_scalar(AssertSqlSafe(format!("explain (analyze, costs off) {sql}"))).fetch_all(&mut *conn).await.unwrap();
    rows.join("\n")
}

fn millis(plan: &str) -> String {
    plan.lines().find(|l| l.contains("Execution Time")).unwrap_or("?").trim().to_string()
}

struct World {
    k: Arc<Kernel>,
    table: String,
    tenant: uuid::Uuid,
    admin: Ctx,
    conn: PgConnection,
}

/// A started kernel on an empty database, a workspace with the customers' package, and a connection that works as the
/// server does for itself (row-level security lets it through).
async fn world() -> World {
    let db = Db::connect(DbConfig { url: url(), max_connections: 8, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public").execute(&db.pool).await.unwrap();
    let catalog = catalog();
    let table = ddl::cluster_schema(&catalog).unwrap().entity("Customer").unwrap().table.clone();
    let k = Kernel::boot(db, catalog, HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into(), "contacts".into(), "parties".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let mut conn = PgConnection::connect(&url()).await.unwrap();
    exec(&mut conn, "select set_config('ferrum.system', 'on', false)".into()).await;
    World { k, table, tenant, admin, conn }
}

/// What a customer of the reference data looks like: a title of a few words, an address, a phone, a tax number, a
/// number from the entity's numbering.
const ROW: &str = "'CUST-' || lpad(g::text, 6, '0'), \
    (array['Northwind','Contoso','Acme','Globex','Initech','Umbrella','Hooli','Stark','Wayne','Wonka'])[1 + g % 10] || ' ' \
      || (array['Trading','Logistics','Foods','Systems','Holdings','Partners'])[1 + (g / 10) % 6] || ' ' \
      || (array['Ltd','LLC','GmbH','Inc'])[1 + (g / 60) % 4] || ' ' || g, \
    'accounts' || g || '@' || lower((array['northwind','contoso','acme','globex'])[1 + g % 4]) || '.example.com', \
    '+1' || lpad((g * 7919 % 10000000000)::text, 10, '0'), 'TX' || lpad(g::text, 9, '0')";

/// Customers numbered `first` to `first + n - 1`.
async fn fill(conn: &mut PgConnection, table: &str, tenant: uuid::Uuid, first: i64, n: i64) {
    exec(
        conn,
        format!(
            "insert into {table} (tenant_id, id, number, customer_name, email, phone, tax_id) \
             select '{tenant}', gen_random_uuid(), {ROW} from generate_series({first}, {}) g",
            first + n - 1
        ),
    )
    .await;
}

/// Drops every trigram index of a table, so that a fill of it is not slowed by them.
async fn drop_gin(conn: &mut PgConnection, table: &str) {
    let gin: Vec<String> = sqlx::query_scalar("select indexname::text from pg_indexes where tablename = $1 and indexdef like '% gin %'")
        .bind(table)
        .fetch_all(&mut *conn)
        .await
        .unwrap();
    for name in gin {
        exec(conn, format!("drop index \"{name}\"")).await;
    }
}

/// The trigram indexes the sync used to make for the customers (the title and the search fields), named as it named
/// them, and the one on `number` that K13 proposed.
async fn trgm(conn: &mut PgConnection, table: &str) {
    for c in SEARCHED {
        exec(conn, format!("create index \"trgm_{table}_{c}\" on {table} using gin ({c} gin_trgm_ops)")).await;
    }
    exec(conn, format!("create index \"trgm_{table}_number\" on {table} using gin (number gin_trgm_ops) where number is not null")).await;
    exec(conn, format!("analyze {table}")).await;
}

/// The sync makes no trigram index: nothing reads one.
#[test]
fn the_sync_builds_no_trigram_index() {
    run(async {
        let mut w = world().await;
        let n: i64 = sqlx::query_scalar("select count(*) from pg_indexes where indexname like 'trgm\\_%'").fetch_one(&mut w.conn).await.unwrap();
        assert_eq!(n, 0, "trigram indexes: they cost every write and no search uses them");
    });
}

/// The leakproof fact, and its effect on a plan: under the policy the search reads every row of the workspace, with an
/// index on the searched column or without; the same rows in a table without the policy answer from the index.
#[test]
fn trigram_indexes_serve_no_search() {
    run(async {
        let mut w = world().await;
        let (table, tenant) = (w.table.clone(), w.tenant);
        let leakproof: Vec<(String, bool)> =
            sqlx::query_as("select proname::text, proleakproof from pg_proc where proname in ('texticlike','textlike','ts_match_vq')")
                .fetch_all(&mut w.conn)
                .await
                .unwrap();
        assert!(!leakproof.is_empty() && leakproof.iter().all(|(_, l)| !l), "leakproof now: {leakproof:?}");

        drop_gin(&mut w.conn, &table).await;
        fill(&mut w.conn, &table, tenant, 1, ROWS).await;
        trgm(&mut w.conn, &table).await;
        // The same rows without the policy, with the same indexes.
        exec(&mut w.conn, format!("create table plain_copy (like {table} including all)")).await;
        exec(&mut w.conn, format!("insert into plain_copy select * from {table}")).await;
        exec(&mut w.conn, "analyze plain_copy".into()).await;

        let one = |t: &str| format!("select a.id from {t} a where a.tenant_id = '{tenant}' and a.number ilike '%07771%'");
        let with_policy = plan(&mut w.conn, &one(&table)).await;
        let no_policy = plan(&mut w.conn, &one("plain_copy")).await;
        println!(
            "K13 {ROWS} rows, number ilike alone: with the policy {}; without it {}\n{with_policy}\n{no_policy}",
            millis(&with_policy),
            millis(&no_policy)
        );
        assert!(with_policy.contains("Seq Scan"), "with the policy the index is not used:\n{with_policy}");
        assert!(no_policy.contains("Bitmap Index Scan"), "without the policy it is:\n{no_policy}");
        exec(&mut w.conn, "drop table plain_copy".into()).await;
    });
}

/// The kernel's real search paths, as a workspace user and as the system, over 100,000 customers with trigram indexes
/// on every searched column: none of the indexes is scanned once.
#[test]
fn trigram_indexes_are_never_scanned() {
    run(async {
        let mut w = world().await;
        let (table, tenant) = (w.table.clone(), w.tenant);
        drop_gin(&mut w.conn, &table).await;
        fill(&mut w.conn, &table, tenant, 1, ROWS).await;
        trgm(&mut w.conn, &table).await;
        // The counters work: the same index in a table without the policy is scanned by a search of it.
        exec(&mut w.conn, format!("create table plain_copy (like {table} including all)")).await;
        exec(&mut w.conn, format!("insert into plain_copy select * from {table}")).await;
        exec(&mut w.conn, "analyze plain_copy".into()).await;
        exec(&mut w.conn, "select id from plain_copy where number ilike '%07771%'".into()).await;

        let system = Ctx::system(tenant, w.admin.schema.clone());
        for ctx in [&w.admin, &system] {
            for term in ["07771", "northwind", "Contoso Logistics", "accounts7777@", "+1000", "TX00007"] {
                let query = ListQuery {
                    search: Some(term.into()),
                    fields: Some(vec!["id".into(), "number".into(), "customer_name".into()]),
                    sort: Some("updated_at desc".into()),
                    limit: Some(5),
                    ..Default::default()
                };
                w.k.list(ctx, "Customer", &query).await.unwrap();
            }
            for field in SEARCHED {
                let filter = Filter { field: field.into(), op: Op::Like, value: "07771".into() };
                w.k.list(ctx, "Customer", &ListQuery { filters: vec![filter], limit: Some(5), ..Default::default() }).await.unwrap();
            }
        }
        // Counters reach the shared statistics when a session is idle: give them the time, and the reading session a nudge.
        tokio::time::sleep(Duration::from_secs(3)).await;
        exec(&mut w.conn, "select pg_stat_force_next_flush()".into()).await;
        let scans: Vec<(String, String, i64)> = sqlx::query_as(
            "select relname::text, indexrelname::text, idx_scan from pg_stat_user_indexes where indexrelname like '%trgm\\_%' or indexrelname like 'plain\\_copy\\_%idx' order by 2",
        )
        .fetch_all(&mut w.conn)
        .await
        .unwrap();
        println!("K13 idx_scan after the searches: {scans:?}");
        let mine: Vec<_> = scans.iter().filter(|s| s.0 == table).collect();
        assert_eq!(mine.len(), 5, "{scans:?}");
        assert!(mine.iter().all(|s| s.2 == 0), "a trigram index was scanned: {mine:?}");
        assert!(scans.iter().any(|s| s.0 == "plain_copy" && s.2 > 0), "the counters did not count: {scans:?}");
        let seq: i64 = sqlx::query_scalar("select seq_scan from pg_stat_user_tables where relname = $1").bind(&table).fetch_one(&mut w.conn).await.unwrap();
        assert!(seq >= 20, "the searches read the table: {seq}");
        exec(&mut w.conn, "drop table plain_copy".into()).await;
    });
}

/// What the trigram indexes of the searched columns cost every write: the same table of 100,000 customers with them
/// and without, alternating, six rounds of 5,000 rows in one statement and 500 rows one statement each; median and
/// spread of rows a second, the size of the indexes, and the index entries a row writes.
#[test]
fn trigram_indexes_cost_every_write() {
    run(async {
        let mut w = world().await;
        let (table, tenant) = (w.table.clone(), w.tenant);
        let configs = ["without", "with"];
        for t in configs {
            exec(&mut w.conn, format!("create table cost_{t} (like {table} including all)")).await;
            drop_gin(&mut w.conn, &format!("cost_{t}")).await;
            fill(&mut w.conn, &format!("cost_{t}"), tenant, 1, ROWS).await;
            if t == "with" {
                for c in SEARCHED {
                    exec(&mut w.conn, format!("create index on cost_with using gin ({c} gin_trgm_ops)")).await;
                }
            }
            exec(&mut w.conn, format!("vacuum analyze cost_{t}")).await;
        }
        let size = |t: &'static str| format!("select pg_indexes_size('cost_{t}'::regclass), pg_table_size('cost_{t}'::regclass)");
        let (without_idx, without_tab): (i64, i64) = sqlx::query_as(AssertSqlSafe(size("without"))).fetch_one(&mut w.conn).await.unwrap();
        let (with_idx, with_tab): (i64, i64) = sqlx::query_as(AssertSqlSafe(size("with"))).fetch_one(&mut w.conn).await.unwrap();
        let entries: f64 = sqlx::query_scalar(
            "select avg(coalesce(array_length(show_trgm(customer_name), 1), 0) + coalesce(array_length(show_trgm(email), 1), 0) \
             + coalesce(array_length(show_trgm(phone), 1), 0) + coalesce(array_length(show_trgm(tax_id), 1), 0))::float8 from cost_with",
        )
        .fetch_one(&mut w.conn)
        .await
        .unwrap();

        const BULK: i64 = 5_000;
        const SINGLE: i64 = 500;
        let rate = |n: i64, took: Duration| n as f64 / took.as_secs_f64();
        let mut bulk = [vec![], vec![]];
        let mut single = [vec![], vec![]];
        let mut next = ROWS + 1;
        for round in 0..6 {
            for i in 0..2 {
                let c = (i + round) % 2;
                let t = format!("cost_{}", configs[c]);
                let started = Instant::now();
                fill(&mut w.conn, &t, tenant, next, BULK).await;
                bulk[c].push(rate(BULK, started.elapsed()));
                next += BULK;
                let started = Instant::now();
                for _ in 0..SINGLE {
                    fill(&mut w.conn, &t, tenant, next, 1).await;
                    next += 1;
                }
                single[c].push(rate(SINGLE, started.elapsed()));
            }
        }
        let stat = |v: &mut Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            format!("median {:.0} (min {:.0}, max {:.0})", v[v.len() / 2], v[0], v[v.len() - 1])
        };
        let med = |v: &Vec<f64>| v[v.len() / 2];
        let [mut b0, mut b1] = bulk;
        let [mut s0, mut s1] = single;
        println!(
            "K13 trigram indexes on {SEARCHED:?}, a table of {ROWS} customers, rows a second, six rounds alternating:\n  \
             {BULK} rows in one statement: without {}; with {}\n  one row a statement ({SINGLE}): without {}; with {}\n  \
             medians: with is {:.1}x slower in bulk, {:.1}x slower one at a time\n  \
             indexes {} MB without, {} MB with (tables {} and {} MB); about {entries:.0} index entries written per row",
            stat(&mut b0),
            stat(&mut b1),
            stat(&mut s0),
            stat(&mut s1),
            med(&b0) / med(&b1),
            med(&s0) / med(&s1),
            without_idx >> 20,
            with_idx >> 20,
            without_tab >> 20,
            with_tab >> 20,
        );
        for t in configs {
            exec(&mut w.conn, format!("drop table cost_{t}")).await;
        }
    });
}
