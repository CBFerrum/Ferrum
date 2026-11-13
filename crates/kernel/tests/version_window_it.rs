//! The two-version window (src/window.rs): on a database a newer build migrated and synced, this build starts as a
//! guest when the newer build's COMPATIBLE_FROM lets it, serves what it knows and changes nothing else, and leaves the
//! newer build's events and jobs alone; it refuses, naming what to start, when it is not let in. Against a real
//! PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first).

use async_trait::async_trait;
use ferrum_kernel::outbox::Event;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, EventSink, HookRegistry, Kernel, KernelSettings, window};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// The topics of the events handled, in the order they were.
#[derive(Default)]
struct Seen(Mutex<Vec<i64>>);

#[async_trait]
impl EventSink for Seen {
    async fn on_event(&self, _k: &Kernel, ev: &Event) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().push(ev.id);
        Ok(())
    }
}

#[test]
fn an_older_build_runs_as_a_guest_of_a_newer_one() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

fn catalog() -> Catalog {
    Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog")
}

fn day(n: i64) -> String {
    (window::build_date() + chrono::Duration::days(n)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

async fn state(db: &Db, key: &str) -> Option<String> {
    sqlx::query_scalar("select value from cluster_state where key = $1").bind(key).fetch_optional(&db.pool).await.unwrap()
}

async fn set_state(db: &Db, key: &str, value: &str) {
    sqlx::query("insert into cluster_state (key, value) values ($1, $2) on conflict (key) do update set value = excluded.value")
        .bind(key)
        .bind(value)
        .execute(&db.pool)
        .await
        .unwrap();
}

async fn scalar(db: &Db, sql: &'static str) -> i64 {
    let mut tx = db.system_tx().await.unwrap();
    let n = sqlx::query_scalar(sql).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 12, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let seen = Arc::new(Seen::default());
    let boot = |migrate_at_start: bool| {
        let (db, seen) = (db.clone(), seen.clone());
        async move {
            let mut hooks = HookRegistry::default();
            hooks.add_event_sink(seen);
            Kernel::boot(db, catalog(), hooks, KernelSettings { migrate_at_start, ..Default::default() }, None).await
        }
    };
    let refused = |what: &str, r: ferrum_kernel::Result<std::sync::Arc<Kernel>>| -> String {
        let Err(e) = r else { panic!("{what}: booted") };
        e.to_string()
    };

    // Several starts of this build at once: each takes its turn, and the database records this build.
    let starts = futures::future::join_all((0..3).map(|_| boot(true))).await;
    for s in &starts {
        assert!(s.as_ref().map_err(|e| e.to_string()).expect("a start").booted.guest.is_none());
    }
    let own = day(0);
    assert_eq!(state(&db, "schema_build_date").await.as_deref(), Some(own.as_str()));
    assert_eq!(state(&db, "schema_compatible_from").await.as_deref(), Some(window::COMPATIBLE_FROM));
    let level = boot(true).await.expect("level start");
    let hold = level.hold_start().await.unwrap().expect("this build's database: it does the start-up writes");
    // While a server does them, no build decides, migrates or syncs (they wait for the exclusive lock).
    let taken: bool = sqlx::query_scalar("select pg_try_advisory_lock($1)").bind(window::LOCK).fetch_one(&db.pool).await.unwrap();
    assert!(!taken, "a migrating start got in during the start-up writes");
    hold.release().await;
    let mut conn = db.pool.acquire().await.unwrap();
    let taken: bool = sqlx::query_scalar("select pg_try_advisory_lock($1)").bind(window::LOCK).fetch_one(&mut *conn).await.unwrap();
    assert!(taken);
    sqlx::query("select pg_advisory_unlock($1)").bind(window::LOCK).execute(&mut *conn).await.unwrap();
    drop(conn);

    let tenant = level
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();

    // A newer build migrated (one migration this build lacks), synced another catalog, and wrote an event and a job
    // of kinds this build does not know.
    sqlx::query("insert into _sqlx_migrations (version, description, success, checksum, execution_time) values (99991231000000, 'from a newer build', true, '\\x00', 0)")
        .execute(&db.pool)
        .await
        .unwrap();
    set_state(&db, "catalog_fingerprint", "newer").await;
    set_state(&db, "schema_build_date", &day(1)).await;
    set_state(&db, "schema_compatible_from", &day(-1)).await;
    let mut tx = db.system_tx().await.unwrap();
    sqlx::query("insert into outbox (tenant_id, topic, payload, build_date) values ($1, 'newer.topic', '{}', now() + interval '400 days')")
        .bind(tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("insert into job (tenant_id, id, kind, build_date) values ($1, gen_random_uuid(), 'newer.kind', now() + interval '400 days')")
        .bind(tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let migrations = db.applied_migrations().await.unwrap();

    // Let in, with either setting: a guest that migrates, syncs and records nothing.
    for migrate_at_start in [true, false] {
        let guest = boot(migrate_at_start).await.expect("a guest starts");
        assert!(guest.booted.guest.is_some() && guest.booted.migrations.is_empty() && guest.booted.schema.statements == 0);
        assert!(guest.hold_start().await.unwrap().is_none(), "a guest does the start-up writes");
    }
    let guest = boot(true).await.unwrap();
    assert_eq!(db.applied_migrations().await.unwrap(), migrations);
    assert_eq!(state(&db, "catalog_fingerprint").await.as_deref(), Some("newer"));
    assert_eq!(state(&db, "schema_build_date").await, Some(day(1)));
    assert!(guest.newer_build_here().await.unwrap());
    let said = ferrum_kernel::ddl::sync(&guest.db, &guest.registry.catalog(), true).await.expect_err("a guest synced the schema").to_string();
    assert!(said.contains("newer build"), "{said}");

    // It serves reads and writes on what it knows.
    let session = guest.login("acme", "admin@acme.test", "Correct-Horse-9").await.expect("login");
    let ctx = guest.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let doc = json!({"code": "XTS", "currency_name": "Testing"}).as_object().unwrap().clone();
    let made = guest.insert(&ctx, "Currency", doc).await.expect("a write");
    let id = made["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(guest.get(&ctx, "Currency", id, false).await.unwrap()["code"], "XTS");

    // The newer build's event and job stay as they were, for a newer replica, and the gauges count them; the guest's own
    // events written after the newer one in the same workspace go out.
    while guest.dispatch_outbox_once().await.unwrap() > 0 {}
    while guest.run_jobs_once(4).await.unwrap() > 0 {}
    let newer: i64 = scalar(&db, "select id from outbox where topic = 'newer.topic'").await;
    assert!(scalar(&db, "select count(*) from outbox where topic = 'Currency.created' and id > (select id from outbox where topic = 'newer.topic')").await > 0);
    assert_eq!(scalar(&db, "select count(*) from outbox where id > (select id from outbox where topic = 'newer.topic') and processed_at is null").await, 0);
    assert_eq!(scalar(&db, "select count(*) from outbox where topic = 'newer.topic' and processed_at is null and not dead and attempts = 0").await, 1);
    assert_eq!(scalar(&db, "select count(*) from job where kind = 'newer.kind' and status = 'queued' and attempts = 0").await, 1);
    assert_eq!(guest.waiting_for_newer().await.unwrap(), (1, 1));
    assert!(!seen.0.lock().unwrap().contains(&newer));
    // A build at least as new as the one that wrote it (here: the event dated as this build) takes it.
    let mut tx = db.system_tx().await.unwrap();
    sqlx::query("update outbox set build_date = $1 where topic = 'newer.topic'").bind(window::build_date()).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    while guest.dispatch_outbox_once().await.unwrap() > 0 {}
    assert!(seen.0.lock().unwrap().contains(&newer));
    assert_eq!(guest.waiting_for_newer().await.unwrap(), (0, 1));
    let mut tx = db.system_tx().await.unwrap();
    sqlx::query("insert into outbox (tenant_id, topic, payload, build_date) values ($1, 'newer.topic', '{}', now() + interval '400 days')")
        .bind(tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // A build with an older date that the newer one was not made from is no guest: one of its migrations is not
    // applied, or the database lacks a column its catalog has.
    let last: i64 = scalar(&db, "select max(version) from _sqlx_migrations where version < 99991231000000").await;
    sqlx::query("update _sqlx_migrations set version = -version where version = $1").bind(last).execute(&db.pool).await.unwrap();
    for migrate_at_start in [true, false] {
        let said = refused("a migration not applied", boot(migrate_at_start).await);
        assert!(said.contains("not an older build of the one that migrated") && said.contains(&format!("migration {last}")), "{said}");
    }
    sqlx::query("update _sqlx_migrations set version = -version where version = $1").bind(-last).execute(&db.pool).await.unwrap();
    sqlx::query("alter table e_currency rename column currency_name to currency_name_gone").execute(&db.pool).await.unwrap();
    for migrate_at_start in [true, false] {
        let said = refused("a column missing", boot(migrate_at_start).await);
        assert!(said.contains("not an older build of the one that migrated") && said.contains("currency_name"), "{said}");
    }
    sqlx::query("alter table e_currency rename column currency_name_gone to currency_name").execute(&db.pool).await.unwrap();
    boot(false).await.expect("a guest again");

    // A newer build whose COMPATIBLE_FROM is past this build: refused with either setting, saying what to start.
    set_state(&db, "schema_compatible_from", &day(1)).await;
    for migrate_at_start in [true, false] {
        let said = refused("not let in", boot(migrate_at_start).await);
        assert!(said.contains(&format!("Start a build from {} or newer", day(1))), "{said}");
    }
    // One that started before that newer build arrived does not do its start-up writes on it either.
    assert!(level.hold_start().await.is_err());
    assert_eq!(db.applied_migrations().await.unwrap(), migrations);

    // The newer build gone again (as if restored): this build's database, behind by a migration, is refused as before
    // when it does not migrate, and brought up when it does.
    sqlx::query("delete from _sqlx_migrations where version = 99991231000000").execute(&db.pool).await.unwrap();
    sqlx::query("delete from cluster_state where key in ('schema_build_date', 'schema_compatible_from')").execute(&db.pool).await.unwrap();
    sqlx::query("delete from _sqlx_migrations where version = (select max(version) from _sqlx_migrations)").execute(&db.pool).await.unwrap();
    let said = refused("one migration behind", boot(false).await);
    assert!(said.contains("1 of this build's database migrations are not applied"), "{said}");
}
