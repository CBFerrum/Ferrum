//! The audit trail partitioned by month (`audit_event`), against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the
//! database is reset first): a start and every maintenance round make this month and three ahead; a save lands in its
//! month; a workspace reads only its own rows, through the trail or naming a month; the append-only triggers
//! hold in every month; a month the rows of a copy need is made for them; the doctor tells when the months ahead run
//! out; and anchoring a day reads one month through an index.
//!
//! It prints what removing a month costs by `delete` (what retention by rows would do) against dropping its partition,
//! and how long anchoring a day takes, over `FERRUM_AUDIT_TEST_EVENTS` events (default 30,000) spread over three months.

use chrono::{Datelike, Duration, Utc};
use ferrum_kernel::doctor::Level;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings, audit};
use ferrum_meta::Catalog;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

const PASSWORD: &str = "Correct-Horse-9";
/// The workspaces the events of three months are spread over.
const WORKSPACES: usize = 50;

fn events() -> i64 {
    std::env::var("FERRUM_AUDIT_TEST_EVENTS").ok().and_then(|n| n.parse().ok()).unwrap_or(30_000)
}

async fn kernel(url: &str) -> Arc<Kernel> {
    let db = Db::connect(DbConfig { url: url.to_string(), max_connections: 8, ..Default::default() }).await.expect("connect");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    Kernel::boot(db, catalog, HookRegistry::default(), KernelSettings::default(), None).await.expect("boot")
}

/// The partition a month is kept in.
fn month(y: i32, m: u32) -> String {
    format!("audit_event_y{y:04}m{m:02}")
}

/// This month and the `n` after it.
fn months_from_now(n: u32) -> Vec<String> {
    let today = Utc::now().date_naive();
    (0..=n)
        .map(|i| {
            let d = today.with_day(1).unwrap().checked_add_months(chrono::Months::new(i)).unwrap();
            month(d.year(), d.month())
        })
        .collect()
}

async fn partitions(k: &Kernel) -> Vec<String> {
    sqlx::query_scalar("select c.relname::text from pg_inherits i join pg_class c on c.oid = i.inhrelid where i.inhparent = 'audit_event'::regclass order by 1")
        .fetch_all(&k.db.pool)
        .await
        .unwrap()
}

async fn drop_partitions(k: &Kernel, which: &[String]) {
    for p in which {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("drop table if exists {p}"))).execute(&k.db.pool).await.unwrap();
    }
}

async fn workspace(k: &Kernel, slug: &str) -> Uuid {
    k.create_tenant(NewTenant {
        slug: slug.into(),
        name: slug.into(),
        admin_email: format!("admin@{slug}.test"),
        admin_password: PASSWORD.into(),
        admin_name: None,
        packages: vec![],
        settings: None,
    })
    .await
    .unwrap()
}

/// A save's audit event, written as every save writes it, in a transaction of the workspace's.
async fn save(k: &Kernel, slug: &str) -> ferrum_kernel::Result<()> {
    let s = k.login(slug, &format!("admin@{slug}.test"), PASSWORD).await?;
    let ctx = k.load_ctx(s.tenant, s.user, Some(s.token_version), "it".into()).await?;
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    audit::record_admin(&mut tx, &ctx, "User", ctx.user, "renamed", None, serde_json::json!({"full_name": "Admin"})).await?;
    tx.commit().await?;
    Ok(())
}

/// How many rows `sql` counts in a transaction of `tenant` (or the system's).
async fn count(k: &Kernel, tenant: Option<Uuid>, sql: &str) -> i64 {
    let mut tx = match tenant {
        Some(t) => k.db.tenant_tx(t).await.unwrap(),
        None => k.db.system_tx().await.unwrap(),
    };
    let n = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

/// What a statement of the system's answers: Ok, or the database's refusal.
async fn system(k: &Kernel, sql: &str) -> Result<u64, String> {
    let mut tx = k.db.system_tx().await.unwrap();
    let done = sqlx::query(sqlx::AssertSqlSafe(sql.to_string())).execute(&mut *tx).await.map(|r| r.rows_affected()).map_err(|e| e.to_string());
    tx.commit().await.unwrap();
    done
}

#[test]
fn audit_event_is_partitioned_by_month() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let reset = Db::connect(DbConfig { url: url.clone(), max_connections: 1, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;")
        .execute(&reset.pool)
        .await
        .expect("reset");
    reset.pool.close().await;
    let k = kernel(&url).await;
    let ahead = months_from_now(3);

    // A fresh database: this month and three ahead.
    let have = partitions(&k).await;
    for m in &ahead {
        assert!(have.contains(m), "{m} missing from {have:?}");
    }
    let partitioned: bool = sqlx::query_scalar("select relkind = 'p' from pg_class where oid = 'audit_event'::regclass").fetch_one(&k.db.pool).await.unwrap();
    assert!(partitioned);

    // A save lands in its month.
    let (a, b) = (workspace(&k, "alpha").await, workspace(&k, "bravo").await);
    save(&k, "alpha").await.unwrap();
    save(&k, "bravo").await.unwrap();
    let mut tx = k.db.tenant_tx(a).await.unwrap();
    let homes: Vec<String> = sqlx::query_scalar("select distinct tableoid::regclass::text from audit_event").fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(homes, vec![ahead[0].clone()]);

    // Through the trail a workspace reads its own rows; naming a month is held to the same rule (its own policy, the
    // trail's): its own rows there, none of another's, all of them for the system (as a dump reads), nothing for the role
    // with no setting.
    let mine = count(&k, Some(a), &format!("select count(*) from audit_event where tenant_id = '{a}'")).await;
    assert!(mine > 0);
    assert_eq!(count(&k, Some(a), "select count(*) from audit_event").await, mine, "alpha reads its own rows only");
    assert_eq!(count(&k, Some(a), &format!("select count(*) from audit_event where tenant_id = '{b}'")).await, 0);
    let all = count(&k, None, "select count(*) from audit_event").await;
    assert!(all > mine, "the system reads both");
    let named = &ahead[0];
    assert_eq!(count(&k, Some(a), &format!("select count(*) from {named}")).await, mine, "alpha naming {named}");
    assert_eq!(count(&k, Some(a), &format!("select count(*) from {named} where tenant_id = '{b}'")).await, 0, "alpha naming {named} for bravo's");
    assert_eq!(count(&k, None, &format!("select count(*) from {named}")).await, all);
    let bare: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from {named}"))).fetch_one(&k.db.pool).await.unwrap();
    assert_eq!(bare, 0, "the role alone, forced");
    // The append-only triggers hold in every month, through the trail or naming the month: no delete, no change but a
    // redaction, no truncate of the trail.
    for refused in [format!("delete from audit_event where tenant_id = '{a}'"), format!("delete from {named}")] {
        let r = system(&k, &refused).await;
        assert!(r.as_ref().is_err_and(|e| e.contains(&format!("DELETE on {named} is not allowed"))), "the month's own trigger: {r:?}");
    }
    for refused in [format!("update audit_event set action = 'other' where tenant_id = '{a}'"), "truncate audit_event".to_string()] {
        let r = system(&k, &refused).await;
        assert!(r.as_ref().is_err_and(|e| e.contains("append-only")), "{refused}: {r:?}");
    }
    assert_eq!(count(&k, Some(a), "select count(*) from audit_event").await, mine);

    // A server stopped for months: no month is there. Saves fail until a maintenance round, or a start, makes them.
    drop_partitions(&k, &partitions(&k).await).await;
    let r = save(&k, "alpha").await;
    assert!(r.is_err(), "a save with no month to go in: {r:?}");
    k.maintenance().await.expect("maintenance");
    assert_eq!(partitions(&k).await, ahead, "maintenance makes this month and three ahead");
    save(&k, "alpha").await.expect("a save after maintenance");
    drop_partitions(&k, &partitions(&k).await).await;
    let again = kernel(&url).await;
    assert_eq!(partitions(&again).await, ahead, "a start makes them before it serves");
    save(&again, "alpha").await.expect("a save after a start");
    again.db.pool.close().await;

    // The doctor tells when fewer than two months are there ahead.
    let finding = |f: &[ferrum_kernel::doctor::Finding]| f.iter().find(|f| f.check == "audit.months").map(|f| f.level);
    assert_eq!(finding(&k.doctor(30).await.unwrap()), Some(Level::Ok));
    drop_partitions(&k, &ahead[2..3]).await;
    assert_eq!(finding(&k.doctor(30).await.unwrap()), Some(Level::Blocker));
    k.maintenance().await.unwrap();
    assert_eq!(finding(&k.doctor(30).await.unwrap()), Some(Level::Ok));

    // A copy brings rows with their own times (a move, a sandbox): their months are made for them first.
    let old = format!(
        "insert into audit_event (tenant_id, at, entity, record_id, action, hash) values ('{a}', '2019-03-05 10:00+00', 'Old', '{a}', 'kept', '\\x00')"
    );
    assert!(system(&k, &old).await.is_err_and(|e| e.contains("no partition")), "a month nobody made");
    let made: i32 = sqlx::query_scalar("select ferrum_ensure_month_partitions('audit_event', '2019-03-05 10:00+00', '2019-03-05 10:00+00')")
        .fetch_one(&k.db.pool)
        .await
        .unwrap();
    assert_eq!(made, 1);
    assert_eq!(system(&k, &old).await, Ok(1));
    let made: i32 =
        sqlx::query_scalar("select ferrum_ensure_month_partitions('audit_event', now(), now() + interval '3 months')").fetch_one(&k.db.pool).await.unwrap();
    assert_eq!(made, 0, "made once");

    // Three months of events of fifty workspaces: anchoring a day reads its month alone, through the index.
    let today = Utc::now().date_naive().with_day(1).unwrap();
    let start = today.checked_sub_months(chrono::Months::new(2)).unwrap();
    sqlx::query("select ferrum_ensure_month_partitions('audit_event', $1, $2)")
        .bind(start.and_hms_opt(0, 0, 0).unwrap().and_utc())
        .bind(today.and_hms_opt(0, 0, 0).unwrap().and_utc())
        .execute(&k.db.pool)
        .await
        .unwrap();
    let span = (today - start).num_days() + Utc::now().date_naive().day0() as i64;
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("set local statement_timeout = 0").execute(&mut *tx).await.unwrap();
    let many: Vec<Uuid> = (0..WORKSPACES).map(|_| Uuid::now_v7()).collect();
    sqlx::query("insert into tenant (id, slug, name) select id, 'w' || n, 'w' || n from unnest($1::uuid[]) with ordinality u(id, n)")
        .bind(&many)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "insert into audit_event (tenant_id, at, entity, record_id, action, diff, hash, category) \
         select $1[1 + g % cardinality($1)], $2::date::timestamptz + (g::float8 / $3) * make_interval(days => $4), \
           'Contact', gen_random_uuid(), 'update', jsonb_build_object('name', repeat('x', 60)), sha256(g::text::bytea), 'records' \
         from generate_series(1, $3) g",
    )
    .bind(&many)
    .bind(start)
    .bind(events())
    .bind(span as i32)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    sqlx::raw_sql("analyze audit_event").execute(&k.db.pool).await.unwrap();
    let day = start + Duration::days(40);
    let from = day.and_hms_opt(0, 0, 0).unwrap().and_utc();
    let mut tx = k.db.system_tx().await.unwrap();
    let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("explain (analyze, costs off) {}", audit::DAY_TENANTS)))
        .bind(from)
        .bind(from + Duration::days(1))
        .bind(day)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let plan = plan.join("\n");
    println!("anchor_day's workspaces of {day}, over {} events:\n{plan}", events());
    let read: std::collections::BTreeSet<&str> = plan.split_whitespace().filter(|w| w.starts_with("audit_event_y")).map(|w| &w[..20]).collect();
    assert_eq!(read.into_iter().collect::<Vec<_>>(), vec![month(day.year(), day.month()).as_str()], "one month read:\n{plan}");
    // A range of the (tenant_id, at) index for each workspace: not the month scanned, nor its whole index read for `at`.
    assert!(
        plan.contains("Index Cond: ((tenant_id = ") && !plan.contains("Index Cond: ((at >=") && !plan.contains("Seq Scan on audit_event"),
        "through the (tenant_id, at) index:\n{plan}"
    );
    let t0 = Instant::now();
    let expected =
        count(&k, None, &format!("select count(distinct tenant_id) from audit_event where at >= '{day}' and at < '{}'", day + Duration::days(1))).await;
    assert!(expected > 0);
    assert_eq!(audit::anchor_day(&k.db, day).await.unwrap() as i64, expected);
    println!("anchor_day({day}): {:?}", t0.elapsed());

    // What the structure makes possible (retention stays the owner's decision: nothing here drops a month of the trail).
    let oldest = month(start.year(), start.month());
    let size: String = sqlx::query_scalar("select pg_size_pretty(pg_total_relation_size($1::regclass))").bind(&oldest).fetch_one(&k.db.pool).await.unwrap();
    let rows = count(&k, None, &format!("select count(*) from audit_event where at < '{}'", start.checked_add_months(chrono::Months::new(1)).unwrap())).await;
    let t0 = Instant::now();
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("select set_config('ferrum.copy', 'on', true), set_config('statement_timeout', '0', true)").execute(&mut *tx).await.unwrap();
    sqlx::query("select pg_stat_force_next_flush()").execute(&mut *tx).await.unwrap();
    let deleted =
        sqlx::query(sqlx::AssertSqlSafe(format!("delete from audit_event where at < '{}'", start.checked_add_months(chrono::Months::new(1)).unwrap())))
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected();
    tx.commit().await.unwrap();
    let by_delete = t0.elapsed();
    let dead: i64 = sqlx::query_scalar("select coalesce(sum(n_dead_tup), 0)::bigint from pg_stat_user_tables where relname = $1")
        .bind(&oldest)
        .fetch_one(&k.db.pool)
        .await
        .unwrap();
    assert_eq!(deleted as i64, rows);
    let next = start.checked_add_months(chrono::Months::new(1)).unwrap();
    let second = month(next.year(), next.month());
    let rows2 = count(
        &k,
        None,
        &format!("select count(*) from audit_event where at >= '{next}' and at < '{}'", next.checked_add_months(chrono::Months::new(1)).unwrap()),
    )
    .await;
    let t0 = Instant::now();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("alter table audit_event detach partition {second}; drop table {second}"))).execute(&k.db.pool).await.unwrap();
    let by_drop = t0.elapsed();
    println!(
        "removing a month ({size}): delete of {rows} rows {by_delete:?}, {dead} dead tuples left to vacuum; detach and drop of {rows2} rows {by_drop:?}, none"
    );
}
