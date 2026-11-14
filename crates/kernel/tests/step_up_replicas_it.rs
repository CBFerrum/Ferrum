//! A step-up counts on every replica at once (docs/operations.md, "Asking again before sensitive work"), against a real
//! PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first): two kernels on one database, the second
//! holding the session as not stepped up; the first takes the step-up and the second admits marked work straight away.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::zones::{Credential, Entry, Placed, Zones};
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const PASSWORD: &str = "Correct-Horse-9";

async fn connect() -> Db {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect")
}

async fn replica() -> Arc<Kernel> {
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    Kernel::boot(connect().await, catalog, HookRegistry::default(), KernelSettings::default(), None).await.expect("boot")
}

#[test]
fn a_step_up_counts_on_every_replica_at_once() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let db = connect().await;
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let a = replica().await;
    let tenant = a
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec!["parties".into()],
            settings: None,
        })
        .await
        .unwrap();
    let s = a.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
    let admin = a.load_ctx(tenant, s.user, None, "it".into()).await.unwrap();
    let mut patch = serde_json::Map::new();
    patch.insert("step_up".into(), json!({"entities": {"Customer": {"work": ["write"]}}}));
    a.update_tenant_settings(&admin, &patch).await.unwrap();
    let sid = a.open_session(&s, Duration::from_secs(3600), "it", "10.1.2.3", None).await.unwrap();
    {
        let mut tx = a.db.tenant_tx(tenant).await.unwrap();
        let aged = sqlx::query("update user_session set step_up_at = now() - interval '1 hour' where id = $1").bind(sid).execute(&mut *tx).await.unwrap();
        assert_eq!(aged.rows_affected(), 1);
        tx.commit().await.unwrap();
    }

    let b = replica().await;
    let (_stop, stopped) = tokio::sync::watch::channel(false);
    let _listener = b.spawn_user_listener(stopped);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let entry = Arc::new(Entry { zones: Arc::new(Zones::default()), placed: Placed::Nowhere, address: "10.1.2.3".into(), credential: Credential::Cookie });
    // What a request through B builds: its session as B holds it (cached for 20 seconds).
    let on_b = || async {
        let state = b.check_session(tenant, s.user, sid).await.unwrap();
        b.request_ctx(tenant, s.user, None, "it".into(), &entry).await.unwrap().proved(state.step_up_at)
    };
    assert!(on_b().await.check_step_up("Customer", "write").is_err(), "B holds the session as not stepped up");
    assert!(on_b().await.check_step_up("Customer", "read").is_ok(), "reading is not marked");

    a.step_up_session(tenant, sid).await.unwrap();
    assert!(a.check_session(tenant, s.user, sid).await.unwrap().stepped_up(), "the replica that took it sees it");
    // The notice travels through the database: give it a moment, far less than the cache's 20 seconds.
    let started = std::time::Instant::now();
    while on_b().await.check_step_up("Customer", "write").is_err() {
        assert!(started.elapsed() < Duration::from_secs(3), "B still refuses after {:?}", started.elapsed());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
