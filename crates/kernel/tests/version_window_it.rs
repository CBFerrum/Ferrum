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

/// The two tests share one database: one at a time.
static ONE: Mutex<()> = Mutex::new(());

#[test]
fn an_older_build_runs_as_a_guest_of_a_newer_one() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

#[test]
fn a_rollback_never_drops_a_protection_silently() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(floors()).await.unwrap() });
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

fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&chrono::Utc)
}

fn shown(d: chrono::DateTime<chrono::Utc>) -> String {
    d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The features whose floors are in effect, in order, holders apart.
async fn in_effect(db: &Db) -> Vec<(String, String)> {
    let mut conn = db.pool.acquire().await.unwrap();
    let mut f: Vec<(String, String)> = window::floors(&mut conn).await.unwrap().into_iter().map(|f| (f.feature, f.holder)).collect();
    f.sort();
    f
}

/// The recorded `COMPATIBLE_FROM` is the later of the syncing build's own and the latest floor, whatever happened.
async fn settled(db: &Db) -> chrono::DateTime<chrono::Utc> {
    let own = at(&state(db, "schema_compatible_from_own").await.expect("own"));
    let latest: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar("select max(since) from window_floor").fetch_one(&db.pool).await.unwrap();
    let from = at(&state(db, "schema_compatible_from").await.expect("effective"));
    assert_eq!(from, latest.map_or(own, |l| l.max(own)), "the recorded value is not the later of the build's own and the floors");
    from
}

/// What a build of `date` makes of this database, as an older build than the one that synced it.
async fn older_build(db: &Db, date: chrono::DateTime<chrono::Utc>) -> window::Standing {
    let mut conn = db.pool.acquire().await.unwrap();
    window::standing(date, true, &[], window::recorded(&mut conn).await.unwrap())
}

/// Builds from before `floor` are refused, from it on are guests (simulated by their dates, the database synced by
/// this build).
async fn keeps_out(db: &Db, floor: &window::Floor) {
    let since = at(floor.since).min(window::build_date());
    assert_eq!(settled(db).await, since, "{}: the floor is not the latest", floor.feature);
    let before = older_build(db, since - chrono::Duration::seconds(1)).await;
    assert!(matches!(before, window::Standing::Refused(_)), "{}: a build from before it is let in: {before:?}", floor.feature);
    let after = older_build(db, since).await;
    assert!(matches!(after, window::Standing::Guest(_)) || since == window::build_date(), "{}: a build from it on is refused: {after:?}", floor.feature);
}

async fn audited(db: &Db, action: &str) -> i64 {
    let mut tx = db.system_tx().await.unwrap();
    let n = sqlx::query_scalar("select count(*) from audit_event where action = $1").bind(action).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

async fn floors() {
    use ferrum_kernel::zones::Zones;
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 20, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let boot = |migrate_at_start: bool| {
        let db = db.clone();
        async move { Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings { migrate_at_start, ..Default::default() }, None).await }
    };
    let k = boot(true).await.expect("level start");

    // A fresh database with no protections: no floors, and the recorded value is this build's own, as before.
    assert!(in_effect(&db).await.is_empty());
    assert_eq!(state(&db, "schema_compatible_from").await.as_deref(), Some(window::COMPATIBLE_FROM));
    assert_eq!(state(&db, "schema_compatible_from_own").await.as_deref(), Some(window::COMPATIBLE_FROM));
    k.require_zone_floors(&Zones::default()).await.unwrap();
    assert!(in_effect(&db).await.is_empty(), "no zones, no floor");

    // Zones in the settings: level 1's floor, put in place by every pool's replicas at once, once.
    let level1 = Zones::parse(r#"{"public": {"staff": "none"}, "office": {"staff": "all"}}"#, "", "", "enforce").unwrap();
    futures::future::join_all((0..8).map(|_| k.require_zone_floors(&level1))).await.into_iter().for_each(|r| r.unwrap());
    assert_eq!(in_effect(&db).await, vec![("zones".into(), "".into())]);
    keeps_out(&db, &ferrum_kernel::zones::FLOOR).await;
    // Level 2's too, when a rule narrows staff.
    let level2 = Zones::parse(r#"{"public": {"staff": "none"}, "sales": {"staff": ["crm"]}}"#, "", "", "enforce").unwrap();
    k.require_zone_floors(&level2).await.unwrap();
    assert_eq!(in_effect(&db).await, vec![("zones".into(), "".into()), ("zones_limited".into(), "".into())]);
    keeps_out(&db, &ferrum_kernel::zones::FLOOR_LIMITED).await;
    let admins_out = Zones::parse(r#"{"desk": {"staff": "all", "admin": false}}"#, "", "", "enforce").unwrap();
    assert!(admins_out.rule("desk").unwrap().limited());

    // A schema sync by this build, newer than every floor's, never lowers the recorded value.
    ferrum_kernel::ddl::sync(&db, &catalog(), true).await.unwrap();
    keeps_out(&db, &ferrum_kernel::zones::FLOOR_LIMITED).await;
    assert_eq!(state(&db, "schema_compatible_from_own").await.as_deref(), Some(window::COMPATIBLE_FROM));

    // The way down: says what older builds will not enforce, is audited, and leaves the build's own value.
    let tenant = k
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
    assert!(k.allow_older_builds(" ", None, false).await.is_err(), "it takes a reason");
    assert!(k.allow_older_builds("rollback", Some("sandbox"), false).await.is_err(), "no such floor in effect");
    let one = k.allow_older_builds("rolling back the CRM pilot", Some("zones_limited"), false).await.unwrap();
    assert_eq!(one.removed.iter().map(|f| f.feature.as_str()).collect::<Vec<_>>(), ["zones_limited"]);
    assert_eq!(one.compatible_from, Some(at(ferrum_kernel::zones::FLOOR.since).min(window::build_date())));
    keeps_out(&db, &ferrum_kernel::zones::FLOOR).await;
    let all = k.allow_older_builds("rolling back to before zones", None, false).await.unwrap();
    assert!(window::said(&all.removed)[0].starts_with("zones (zones that admit no staff refuse them)"), "{:?}", window::said(&all.removed));
    assert_eq!(all.compatible_from, Some(window::compatible_from()));
    assert!(in_effect(&db).await.is_empty());
    assert_eq!(audited(&db, "platform.allow_older_builds").await, 2);

    // Each workspace setting on save, and a pinned key, each on its own.
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.expect("login");
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let saves: [(serde_json::Value, &window::Floor); 3] = [
        (json!({"staff_networks": {"networks": ["10.0.0.0/8"], "mode": "report"}}), &ferrum_kernel::gate::STAFF_NETWORKS_FLOOR),
        (json!({"idle_minutes": 30}), &ferrum_kernel::gate::IDLE_FLOOR),
        (json!({"break_glass": [session.user]}), &ferrum_kernel::gate::BREAK_GLASS_FLOOR),
    ];
    for (patch, floor) in saves {
        k.update_tenant_settings(&admin, patch.as_object().unwrap()).await.unwrap();
        assert_eq!(in_effect(&db).await, vec![(floor.feature.to_string(), "".into())], "{patch}");
        keeps_out(&db, floor).await;
        k.allow_older_builds("test", None, false).await.unwrap();
    }
    // Saved off, nothing is required; turning one off leaves its floor.
    k.update_tenant_settings(&admin, json!({"idle_minutes": 0, "staff_networks": null, "break_glass": null}).as_object().unwrap()).await.unwrap();
    assert!(in_effect(&db).await.is_empty());
    k.update_tenant_settings(&admin, json!({"idle_minutes": 30}).as_object().unwrap()).await.unwrap();
    k.update_tenant_settings(&admin, json!({"idle_minutes": 0}).as_object().unwrap()).await.unwrap();
    assert_eq!(in_effect(&db).await, vec![("idle_timeout".into(), "".into())], "turned off, the floor stays");
    k.allow_older_builds("test", None, false).await.unwrap();
    k.create_api_key(&admin, session.user, "anywhere", None, None).await.unwrap();
    assert!(in_effect(&db).await.is_empty(), "a key without networks");
    let (key, _) = k.create_api_key_on(&admin, session.user, "office", None, None, &["10.0.0.0/8".into()]).await.unwrap();
    assert_eq!(in_effect(&db).await, vec![("key_networks".into(), "".into())]);
    keeps_out(&db, &ferrum_kernel::gate::KEY_NETWORKS_FLOOR).await;
    k.allow_older_builds("test", None, false).await.unwrap();
    k.set_api_key_networks(&admin, key, &["192.168.0.0/16".into()]).await.unwrap();
    assert_eq!(in_effect(&db).await, vec![("key_networks".into(), "".into())], "networks given later");
    k.allow_older_builds("test", None, false).await.unwrap();

    // A temporary floor counts its holders: two moves at once keep it until both end.
    let freeze = &ferrum_kernel::workspace_move::FLOOR;
    k.require_floor(freeze, "one").await.unwrap();
    k.require_floor(freeze, "two").await.unwrap();
    k.require_floor(freeze, "two").await.unwrap();
    keeps_out(&db, freeze).await;
    k.release_floor(freeze, "one").await.unwrap();
    keeps_out(&db, freeze).await;
    // The way down refuses while a holder is under way, unless forced.
    let said = k.allow_older_builds("rollback", None, false).await.expect_err("a move under way").to_string();
    assert!(said.contains("under way") && said.contains("force:yes") && said.contains("held by two"), "{said}");
    k.allow_older_builds("rollback", Some("workspace_freeze"), true).await.unwrap();
    assert!(in_effect(&db).await.is_empty());
    k.require_floor(freeze, "two").await.unwrap();
    k.release_floor(freeze, "two").await.unwrap();
    k.release_floor(freeze, "two").await.unwrap();
    assert!(in_effect(&db).await.is_empty());
    assert_eq!(settled(&db).await, window::compatible_from());

    // Sandboxes: the floor stays until the last one is deleted.
    k.require_floor(&window::SANDBOX, "sandbox-a").await.unwrap();
    k.require_floor(&window::SANDBOX, "sandbox-b").await.unwrap();
    k.release_floor(&window::SANDBOX, "sandbox-a").await.unwrap();
    keeps_out(&db, &window::SANDBOX).await;
    k.release_floor(&window::SANDBOX, "sandbox-b").await.unwrap();
    assert!(in_effect(&db).await.is_empty());

    // Concurrency: holders raised and released at once, over and over, leave exactly what is held and the right value.
    for round in 0..10 {
        let holders: Vec<String> = (0..6).map(|i| format!("h{i}")).collect();
        let ups = holders.iter().map(|h| k.require_floor(freeze, h));
        futures::future::join_all(ups).await.into_iter().for_each(|r| r.unwrap());
        let (downs, ups) = (holders[..3].iter().map(|h| k.release_floor(freeze, h)), holders[3..].iter().map(|h| k.require_floor(freeze, h)));
        let (a, b) = tokio::join!(futures::future::join_all(downs), futures::future::join_all(ups));
        a.into_iter().chain(b).for_each(|r| r.unwrap());
        assert_eq!(in_effect(&db).await.len(), 3, "round {round}");
        settled(&db).await;
        let downs = holders[3..].iter().map(|h| k.release_floor(freeze, h));
        let (a, b) = tokio::join!(futures::future::join_all(downs), k.require_zone_floors(&level1));
        a.into_iter().for_each(|r| r.unwrap());
        b.unwrap();
        assert_eq!(in_effect(&db).await, vec![("zones".into(), "".into())], "round {round}");
        settled(&db).await;
        k.allow_older_builds("round", None, false).await.unwrap();
    }

    // A guest of a newer build writes no floors, releases none and lowers nothing.
    k.require_zone_floors(&level1).await.unwrap();
    set_state(&db, "schema_build_date", &day(1)).await;
    k.require_floor(freeze, "guest").await.unwrap();
    k.require_zone_floors(&level2).await.unwrap();
    k.update_tenant_settings(&admin, json!({"idle_minutes": 30}).as_object().unwrap()).await.unwrap();
    assert_eq!(in_effect(&db).await, vec![("zones".into(), "".into())], "a guest wrote a floor");
    assert!(k.allow_older_builds("from the guest", None, false).await.is_err());
    let guest = boot(true).await.expect("a guest starts");
    assert!(guest.booted.guest.is_some());
    assert!(matches!(guest.allow_older_builds("from the guest", None, false).await, Err(ferrum_kernel::Error::Conflict(_))));
    assert_eq!(in_effect(&db).await, vec![("zones".into(), "".into())]);
    assert_eq!(state(&db, "schema_build_date").await, Some(day(1)), "a guest recorded itself");

    // A refused build that knows floors names the protections it is too old for (here: a newer build put a floor after
    // this build's date).
    set_state(&db, "schema_compatible_from_own", &day(-1)).await;
    let later = window::build_date() + chrono::Duration::hours(1);
    sqlx::query("insert into window_floor (feature, since, what, switched_on_by) values ('newer_thing', $1, 'a newer protection holds', $2)")
        .bind(later)
        .bind(window::build_date() + chrono::Duration::days(1))
        .execute(&db.pool)
        .await
        .unwrap();
    set_state(&db, "schema_compatible_from", &shown(later)).await;
    for migrate_at_start in [true, false] {
        let Err(e) = boot(migrate_at_start).await else { panic!("a build from before a floor started") };
        let said = e.to_string();
        assert!(said.contains(&format!("Start a build from {} or newer", shown(later))), "{said}");
        assert!(said.contains("newer_thing (a newer protection holds)") && !said.contains("zones (") && said.contains("allow_older_builds"), "{said}");
    }

    // A planned move raises its floor and cancelling it releases it.
    sqlx::query("delete from window_floor where feature = 'newer_thing'").execute(&db.pool).await.unwrap();
    set_state(&db, "schema_build_date", &shown(window::build_date())).await;
    set_state(&db, "schema_compatible_from_own", window::COMPATIBLE_FROM).await;
    k.allow_older_builds("before the move", None, false).await.unwrap();
    let mark = k.plan_move("to Kubernetes", std::time::Duration::from_secs(24 * 3600), std::time::Duration::ZERO).await.unwrap();
    assert_eq!(in_effect(&db).await, vec![("move_fence".into(), mark.id.to_string())]);
    keeps_out(&db, &ferrum_kernel::moves::FLOOR).await;
    let said = k.allow_older_builds("rollback", None, false).await.expect_err("a fenced database").to_string();
    assert!(said.contains("move_fence"), "{said}");
    k.cancel_move("rehearsed").await.unwrap();
    assert!(in_effect(&db).await.is_empty());
    assert_eq!(settled(&db).await, window::compatible_from());
}
