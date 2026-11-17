//! The hourly upkeep (`Kernel::maintenance`) against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset
//! first): a step that fails stops none after it, a purge that has fallen far behind catches up in batches within the
//! statement timeout, and two replicas running it at once do the work once.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Processed events a backlog is made of: 50 batches' worth, kept small for the suite's disk and time
/// (`FERRUM_MAINTENANCE_TEST_EVENTS=1000000` for the million the roadmap names).
fn backlog() -> i64 {
    std::env::var("FERRUM_MAINTENANCE_TEST_EVENTS").ok().and_then(|n| n.parse().ok()).unwrap_or(250_000)
}

async fn kernel(url: &str) -> Arc<Kernel> {
    let db = Db::connect(DbConfig { url: url.to_string(), max_connections: 8, ..Default::default() }).await.expect("connect");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    Kernel::boot(db, catalog, HookRegistry::default(), KernelSettings::default(), None).await.expect("boot")
}

/// `n` events the dispatcher handled `days` ago, each carrying a record as a change's event does.
async fn processed(k: &Kernel, tenant: Uuid, n: i64, days: i32) {
    let mut tx = k.db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.system', 'on', true)").execute(&mut *tx).await.unwrap();
    sqlx::query(
        "insert into outbox (tenant_id, topic, payload, created_at, processed_at) \
         select $1, 'paid', jsonb_build_object('doc', jsonb_build_object('n', g, 'note', repeat('x', 200))), \
           now() - make_interval(days => $3), now() - make_interval(days => $3) from generate_series(1, $2) g",
    )
    .bind(tenant)
    .bind(n)
    .bind(days)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// Processed events older than the retention (30 days by default), which maintenance must leave none of.
async fn past_retention(k: &Kernel) -> i64 {
    let mut tx = k.db.system_tx().await.unwrap();
    let n = sqlx::query_scalar("select count(*) from outbox where processed_at < now() - interval '30 days'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

#[test]
fn maintenance_runs_every_step_in_batches_on_one_replica() {
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
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    k.maintenance().await.expect("a round with nothing to do");

    // K3: the first step fails (its table is not there): every purge after it still runs, and the round says which failed.
    processed(&k, tenant, 3, 40).await;
    sqlx::raw_sql("alter table audit_anchor rename to audit_anchor_away").execute(&k.db.pool).await.unwrap();
    let round = k.maintenance().await;
    sqlx::raw_sql("alter table audit_anchor_away rename to audit_anchor").execute(&k.db.pool).await.unwrap();
    assert_eq!(past_retention(&k).await, 0, "the purges ran although anchoring the audit trail failed first ({round:?})");
    let failed = round.expect_err("the round reports the step that failed").to_string();
    assert!(failed.contains("anchor"), "{failed}");

    // K3: a backlog far past the retention is purged in batches, each statement within the statement timeout (15 s), and
    // none is left. Every delete on the outbox is counted as it happens.
    sqlx::raw_sql(
        "create table purge_statement (deleted bigint not null); \
         create function purge_counted() returns trigger language plpgsql as $$ begin insert into purge_statement select count(*) from gone; return null; end $$; \
         create trigger purge_counted after delete on outbox referencing old table as gone for each statement execute function purge_counted();",
    )
    .execute(&k.db.pool)
    .await
    .unwrap();
    let events = backlog();
    processed(&k, tenant, events, 40).await;
    let started = Instant::now();
    k.maintenance().await.expect("maintenance completes");
    eprintln!("{events} processed events purged in {:?}", started.elapsed());
    assert_eq!(past_retention(&k).await, 0, "none older than the retention is left");
    let (largest, deleted): (i64, i64) = sqlx::query_as("select max(deleted), sum(deleted)::bigint from purge_statement").fetch_one(&k.db.pool).await.unwrap();
    assert_eq!(deleted, events);
    assert!(largest <= 5000, "a statement deleted {largest} events at once: a purge goes in batches");
    sqlx::raw_sql("drop trigger purge_counted on outbox; drop function purge_counted(); drop table purge_statement").execute(&k.db.pool).await.unwrap();

    // K3: two replicas at once. The first is held inside its round (an old event's row is locked by someone else); the
    // second leaves the round to it at once, and the first finishes it once the row is free.
    let other = kernel(&url).await;
    processed(&k, tenant, 1, 40).await;
    let mut holding = k.db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.system', 'on', true)").execute(&mut *holding).await.unwrap();
    sqlx::query("select id from outbox where processed_at < now() - interval '30 days' for update").fetch_all(&mut *holding).await.unwrap();
    let first = tokio::spawn({
        let k = k.clone();
        async move { k.maintenance().await }
    });
    let waiting = Instant::now();
    loop {
        let blocked: bool =
            sqlx::query_scalar("select exists (select 1 from pg_stat_activity where datname = current_database() and wait_event_type = 'Lock')")
                .fetch_one(&other.db.pool)
                .await
                .unwrap();
        if blocked {
            break;
        }
        assert!(waiting.elapsed() < Duration::from_secs(8), "the first replica's round reaches the held row");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let second = tokio::time::timeout(Duration::from_secs(5), other.maintenance()).await;
    assert!(matches!(second, Ok(Ok(()))), "the second replica leaves the round to the one running it: {second:?}");
    holding.rollback().await.unwrap();
    first.await.unwrap().expect("the first replica's round");
    assert_eq!(past_retention(&k).await, 0);
}
