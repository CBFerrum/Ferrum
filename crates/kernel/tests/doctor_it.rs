//! The kernel's health report against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is
//! reset first): it finds what is really wrong, and holds nothing it should not. Then scenario F7: a
//! burst of events leaves requests their connections, as many as the doctor says.

use async_trait::async_trait;
use ferrum_kernel::doctor::Level;
use ferrum_kernel::outbox::{self, Event};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, EventSink, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Holds a transaction of the event's workspace for three seconds, as a sink at work holds a connection, and counts how many
/// do at once.
#[derive(Default)]
struct Busy {
    now: AtomicUsize,
    most: AtomicUsize,
    done: AtomicUsize,
}

#[async_trait]
impl EventSink for Busy {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic != "burst" {
            return Ok(());
        }
        let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
        self.most.fetch_max(self.now.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
        sqlx::query("select pg_sleep(3)").execute(&mut *tx).await?;
        self.now.fetch_sub(1, Ordering::SeqCst);
        tx.commit().await?;
        self.done.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn the_doctor_finds_what_is_wrong() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
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
        .expect("workspace");
    let level = |f: &[ferrum_kernel::doctor::Finding], check: &str| f.iter().find(|x| x.check == check).map(|x| x.level);

    let healthy = k.doctor(30).await.unwrap();
    assert!(healthy.iter().all(|f| f.level == Level::Ok || f.check == "database.connections" || f.check == "database.reserve"), "{healthy:?}");
    assert_eq!(level(&healthy, "database.reserve"), Some(Level::Warning), "this role could take every connection there is");
    let base = std::env::var("FERRUM_TEST_SUPERUSER").unwrap_or_else(|_| format!("postgres://{}@localhost:5432", std::env::var("USER").unwrap_or_default()));
    let root = Db::connect(DbConfig { url: format!("{base}/postgres"), max_connections: 1, ..Default::default() }).await.expect("a superuser");
    sqlx::raw_sql("alter role ferrum connection limit 60").execute(&root.pool).await.unwrap();
    let held = k.doctor(30).await.unwrap();
    sqlx::raw_sql("alter role ferrum connection limit -1").execute(&root.pool).await.unwrap();
    assert_eq!(level(&held, "database.reserve"), Some(Level::Ok), "{held:?}");

    // AT-SCALE-002: every replica at its pool's maximum while a migration starts. The role's limit holds, what is past
    // it is told the database is busy rather than that something broke, and the reserve is there for whoever must get in.
    let name = url.rsplit('/').next().unwrap().split('?').next().unwrap().to_string();
    let budget = format!(
        "drop role if exists ferrum_budget; create role ferrum_budget login password 'budget' connection limit 4; grant connect on database {name} to ferrum_budget"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(budget)).execute(&root.pool).await.unwrap();
    let as_budget = format!("postgres://ferrum_budget:budget@{}", url.split_once('@').unwrap().1);
    let replica = |pool: u32| {
        let url = as_budget.clone();
        async move {
            Db::connect(DbConfig { url, max_connections: pool, min_connections: pool, acquire_timeout: Duration::from_millis(300), ..Default::default() }).await
        }
    };
    let (one, two) = (replica(2).await.expect("a replica"), replica(2).await.expect("another"));
    let mut busy = vec![];
    for db in [&one, &two] {
        for _ in 0..2 {
            busy.push(db.pool.begin().await.unwrap());
        }
    }
    let Err(third) = replica(1).await else { panic!("a replica past the budget got in") };
    assert_eq!(third.status(), 503, "busy, not broken: {third}");
    let Err(past) = one.pool.begin().await.map_err(ferrum_kernel::Error::from) else { panic!("a request past its pool got in") };
    assert_eq!(past.status(), 503, "{past}");
    let migration = Db::connect(DbConfig { url: format!("{base}/{name}"), max_connections: 1, ..Default::default() }).await.expect("the reserve");
    sqlx::raw_sql("select 1").execute(&migration.pool).await.expect("and it works");
    drop(busy);
    for db in [one, two, migration] {
        db.pool.close().await;
    }
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("revoke connect on database {name} from ferrum_budget; drop role ferrum_budget")))
        .execute(&root.pool)
        .await
        .unwrap();
    assert_eq!(level(&healthy, "isolation.row_security"), Some(Level::Ok), "every table the kernel creates enforces it");

    sqlx::raw_sql("create table leaky (tenant_id uuid not null, notes text)").execute(&k.db.pool).await.unwrap();
    let f = k.doctor(30).await.unwrap();
    let leak = f.iter().find(|x| x.check == "isolation.row_security").unwrap();
    assert_eq!(leak.level, Level::Blocker);
    assert!(leak.detail.contains("leaky") && leak.fix.is_some(), "{leak:?}");
    sqlx::raw_sql("alter table leaky enable row level security").execute(&k.db.pool).await.unwrap();
    assert_eq!(level(&k.doctor(30).await.unwrap(), "isolation.row_security"), Some(Level::Blocker), "not forced: the owner still sees everything");
    sqlx::raw_sql("drop table leaky").execute(&k.db.pool).await.unwrap();
    // A partition read by its own name is a table of its own: its parent's policy does not hold there.
    sqlx::raw_sql(
        "create table leaky_days (tenant_id uuid not null, day date not null) partition by range (day); \
         alter table leaky_days enable row level security; alter table leaky_days force row level security; \
         create table leaky_day partition of leaky_days for values from ('2026-01-01') to ('2026-01-02')",
    )
    .execute(&k.db.pool)
    .await
    .unwrap();
    let f = k.doctor(30).await.unwrap();
    let leak = f.iter().find(|x| x.check == "isolation.row_security").unwrap();
    assert!(leak.level == Level::Blocker && leak.detail.contains("leaky_day") && !leak.detail.contains("leaky_days"), "{leak:?}");
    sqlx::raw_sql("alter table leaky_day enable row level security; alter table leaky_day force row level security").execute(&k.db.pool).await.unwrap();
    assert_eq!(level(&k.doctor(30).await.unwrap(), "isolation.row_security"), Some(Level::Ok), "forced on the partition too");
    // So is a table in a schema of its own.
    sqlx::raw_sql("create schema elsewhere; create table elsewhere.notes (tenant_id uuid not null)").execute(&k.db.pool).await.unwrap();
    let f = k.doctor(30).await.unwrap();
    let leak = f.iter().find(|x| x.check == "isolation.row_security").unwrap();
    assert!(leak.level == Level::Blocker && leak.detail.contains("elsewhere.notes"), "{leak:?}");
    sqlx::raw_sql("drop schema elsewhere cascade; drop table leaky_days").execute(&k.db.pool).await.unwrap();

    let mut tx = k.db.system_tenant_tx(tenant).await.unwrap();
    sqlx::query("insert into outbox (tenant_id, topic, payload, dead) values ($1, 'x.y', '{}', true)").bind(tenant).execute(&mut *tx).await.unwrap();
    sqlx::query("insert into outbox (tenant_id, topic, payload, created_at) values ($1, 'x.y', '{}', now() - interval '1 hour')")
        .bind(tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("insert into job (tenant_id, id, kind, status, finished_at, last_error) values ($1, $2, 'x.y', 'failed', now(), 'no')")
        .bind(tenant)
        .bind(Uuid::now_v7())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("insert into job (tenant_id, id, kind, status, lease_until) values ($1, $2, 'x.y', 'running', now() - interval '1 hour')")
        .bind(tenant)
        .bind(Uuid::now_v7())
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let f = k.doctor(30).await.unwrap();
    for check in ["events.dead", "events.delay", "jobs.failed", "jobs.running"] {
        assert_eq!(level(&f, check), Some(Level::Warning), "{check}");
    }
    assert_eq!(level(&k.doctor(1000).await.unwrap(), "database.connections"), Some(Level::Warning), "more than two replicas could open");
    assert_eq!(level(&k.doctor(4).await.unwrap(), "database.pool"), Some(Level::Warning), "too few for requests beside the replica's own work");
    assert_eq!(level(&f, "database.pool"), Some(Level::Ok));

    let text = json!(f).to_string();
    assert!(!text.contains(&url) && !text.contains("ferrum:ferrum"), "no connection string");

    // ───────────── scenario F7: an event burst does not starve requests ─────────────
    // A replica with a pool of 30, and one event waiting in each of 60 workspaces, each handled by a sink at work.
    let busy = Arc::new(Busy::default());
    let mut hooks = HookRegistry::default();
    hooks.add_event_sink(busy.clone());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 30, ..Default::default() }).await.expect("connect");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let r = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    let mut tx = r.db.system_tx().await.unwrap();
    let burst: Vec<Uuid> = sqlx::query_scalar(
        "insert into tenant (id, slug, name) select gen_random_uuid(), 'burst-' || n, 'Burst ' || n from generate_series(1, 60) n returning id",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    for t in &burst {
        let mut tx = r.db.tenant_tx(*t).await.unwrap();
        outbox::enqueue(&mut tx, *t, "burst", None, None, json!({})).await.unwrap();
        tx.commit().await.unwrap();
    }
    let draining = {
        let r = r.clone();
        tokio::spawn(async move { while r.dispatch_outbox_once().await.unwrap() > 0 {} })
    };
    // A request comes once the burst holds what it can: at least the events' share of the pool at work, and no more joining.
    let share = ferrum_kernel::db::background_connections(30) as usize;
    let mut seen = 0;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = busy.now.load(Ordering::SeqCst);
        if now >= share && now == seen {
            break;
        }
        seen = now;
    }
    assert!(busy.now.load(Ordering::SeqCst) >= share, "the burst is being handled");
    let asked = Instant::now();
    let request = r.db.tenant_tx(burst[0]).await.expect("a request's transaction");
    let waited = asked.elapsed();
    drop(request);
    assert!(waited < Duration::from_millis(1500), "a request got a connection within a second or so while the burst drained: it waited {waited:?}");
    draining.await.unwrap();
    assert_eq!(busy.done.load(Ordering::SeqCst), 60, "every event of the burst was handled");
    // At most their share of the pool at once, which is what the doctor counts them as.
    let most = busy.most.load(Ordering::SeqCst);
    assert!(most <= share, "events were handled at most {share} at once (a quarter of the pool): {most}");
    let pool = r.doctor(30).await.unwrap().into_iter().find(|f| f.check == "database.pool").unwrap();
    let own = ferrum_kernel::doctor::background(30);
    assert!(pool.detail.contains(&format!("can hold {own} at once")) && own as usize >= share + 4, "{pool:?}");

    // A release older than its database does not start on it: a migration it does not know was applied by a
    // newer one, and going back that way is refused (FEO-080); the way back is a restore.
    sqlx::query("insert into _sqlx_migrations (version, description, success, checksum, execution_time) values (29991231000000, 'from a newer release', true, '\\x00', 0)")
        .execute(&k.db.pool)
        .await
        .unwrap();
    let refused = k.db.migrate().await.unwrap_err().to_string();
    assert!(refused.contains("29991231000000"), "the migration it does not know is named: {refused}");
    sqlx::query("delete from _sqlx_migrations where version = 29991231000000").execute(&k.db.pool).await.unwrap();
    k.db.migrate().await.expect("and once it is gone, it starts");
}
