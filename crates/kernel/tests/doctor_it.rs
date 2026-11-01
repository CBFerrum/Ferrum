//! The kernel's health report against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is
//! reset first): it finds what is really wrong, and holds nothing it should not.

use ferrum_kernel::doctor::Level;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use uuid::Uuid;

#[test]
fn the_doctor_finds_what_is_wrong() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
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
    assert!(healthy.iter().all(|f| f.level == Level::Ok || f.check == "database.connections"), "{healthy:?}");
    assert_eq!(level(&healthy, "isolation.row_security"), Some(Level::Ok), "every table the kernel creates enforces it");

    sqlx::raw_sql("create table leaky (tenant_id uuid not null, notes text)").execute(&k.db.pool).await.unwrap();
    let f = k.doctor(30).await.unwrap();
    let leak = f.iter().find(|x| x.check == "isolation.row_security").unwrap();
    assert_eq!(leak.level, Level::Blocker);
    assert!(leak.detail.contains("leaky") && leak.fix.is_some(), "{leak:?}");
    sqlx::raw_sql("alter table leaky enable row level security").execute(&k.db.pool).await.unwrap();
    assert_eq!(level(&k.doctor(30).await.unwrap(), "isolation.row_security"), Some(Level::Blocker), "not forced: the owner still sees everything");
    sqlx::raw_sql("drop table leaky").execute(&k.db.pool).await.unwrap();

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

    let text = json!(f).to_string();
    assert!(!text.contains(&url) && !text.contains("ferrum:ferrum"), "no connection string");
}
