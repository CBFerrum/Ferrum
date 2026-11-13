//! `FERRUM_DATABASE_DIRECT_URL` (docs/operations.md, "Connection pooler"): with the main pool on another address than the
//! direct one, the main pool starts no session of its own, every kernel transaction sets its own lock and idle timeouts,
//! and what needs a session (LISTEN, the start's locks, migrations) goes direct; a listener whose connection is lost
//! says so, and its replica drops what it may have missed.
//!
//! It runs against `FERRUM_TEST_DATABASE_URL`, the main pool on the same database under another address.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn direct_url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

fn run(main: String) {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario(main)).await.unwrap() });
}

#[test]
fn the_session_side_goes_direct() {
    // The same database under another address: the server takes it for a pooler's.
    let url = direct_url();
    run(format!("{url}{}application_name=main", if url.contains('?') { '&' } else { '?' }));
}

async fn connect(main: &str) -> Db {
    Db::connect(DbConfig { url: main.into(), direct_url: Some(direct_url()), max_connections: 8, ..Default::default() }).await.expect("connect")
}

async fn shown(tx: &mut ferrum_kernel::Tx, name: &str) -> String {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("show {name}"))).fetch_one(&mut **tx).await.unwrap()
}

/// Ends the server sessions LISTENing on `channel` in this database.
async fn cut(db: &Db, channel: &str) -> i64 {
    sqlx::query_scalar("select count(*) from (select pg_terminate_backend(pid) from pg_stat_activity where datname = current_database() and query = $1) t")
        .bind(format!("LISTEN \"{channel}\""))
        .fetch_one(&db.direct)
        .await
        .unwrap()
}

async fn scenario(main: String) {
    let db = connect(&main).await;
    assert!(db.pooled());
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.direct).await.expect("reset");

    // The main pool starts no session settings (a pooler refuses them); each transaction sets its own, a direct
    // connection starts with them.
    let autocommit: String = sqlx::query_scalar("show lock_timeout").fetch_one(&db.pool).await.unwrap();
    assert_eq!(autocommit, "0", "the main pool sent startup options");
    let direct: String = sqlx::query_scalar("show lock_timeout").fetch_one(&db.direct).await.unwrap();
    assert_eq!(direct, "10s");
    let tenant = uuid::Uuid::now_v7();
    for (what, mut tx) in [
        ("tenant", db.tenant_tx(tenant).await.unwrap()),
        ("timed tenant", db.tenant_tx_with_timeout(tenant, 1000).await.unwrap()),
        ("system", db.system_tx().await.unwrap()),
        ("system tenant", db.system_tenant_tx(tenant).await.unwrap()),
        ("side", db.side_tenant_tx(tenant).await.unwrap()),
    ] {
        assert_eq!(shown(&mut tx, "lock_timeout").await, "10s", "{what}");
        assert_eq!(shown(&mut tx, "idle_in_transaction_session_timeout").await, "1min", "{what}");
        tx.rollback().await.unwrap();
    }

    // A listener whose connection is lost says so, rather than reconnecting as if nothing had been missed.
    let mut l = db.listen("ferrum_test_cut").await.unwrap();
    assert_eq!(cut(&db, "ferrum_test_cut").await, 1, "the listener is on the direct address");
    let lost = tokio::time::timeout(Duration::from_secs(10), ferrum_kernel::db::notification(&mut l)).await.expect("noticed within 10 s");
    assert!(lost.is_err(), "{lost:?}");

    // Two replicas: boot (migrations and the schema sync, under the start's session locks) and cache invalidation.
    let catalog = || Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let a = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let b = Kernel::boot(connect(&main).await, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("second boot");
    assert!(b.booted.migrations.is_empty() && b.booted.schema.skipped);
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let listening = b.spawn_listeners(stopped);
    let t = a
        .create_tenant(NewTenant {
            slug: "pooled".into(),
            name: "Pooled".into(),
            admin_email: "admin@pooled.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    let wait = |what: &'static str, b: Arc<Kernel>, f: Box<dyn Fn(&Kernel) -> bool + Send>| async move {
        for _ in 0..100 {
            if f(&b) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{what}: not within 10 s");
    };
    // Both listeners are up before anything is announced.
    for _ in 0..50 {
        let n: i64 = sqlx::query_scalar("select count(*) from pg_stat_activity where datname = current_database() and query like 'LISTEN %'")
            .fetch_one(&a.db.direct)
            .await
            .unwrap();
        if n >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let before = b.registry.schema(t).await.unwrap().version;
    let (_, changes) = b.registry.revision(t);
    let mut tx = a.db.system_tx().await.unwrap();
    ferrum_kernel::registry::Registry::bump_version(&mut tx, t).await.unwrap();
    tx.commit().await.unwrap();
    wait("the change reached the other replica", b.clone(), Box::new(move |b| b.registry.revision(t).1 > changes)).await;
    assert!(b.registry.schema(t).await.unwrap().version > before);

    // Its listener's connection is cut: it drops its whole cache (what it may have missed), listens again, and drops
    // it once more then (what was announced in between).
    let (epoch, _) = b.registry.revision(t);
    assert!(cut(&a.db, "ferrum_meta").await >= 1);
    wait("the cut was noticed, and it listens again", b.clone(), Box::new(move |b| b.registry.revision(t).0 >= epoch + 2)).await;
    let (_, changes) = b.registry.revision(t);
    let mut tx = a.db.system_tx().await.unwrap();
    ferrum_kernel::registry::Registry::bump_version(&mut tx, t).await.unwrap();
    tx.commit().await.unwrap();
    wait("listening again", b.clone(), Box::new(move |b| b.registry.revision(t).1 > changes)).await;

    let _ = stop.send(true);
    for h in listening {
        let _ = h.await;
    }
}
