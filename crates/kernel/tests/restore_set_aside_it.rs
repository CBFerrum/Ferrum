//! A restore noticed while the outbox is large, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is
//! reset first): setting aside what was waiting is not cut short by the statement timeout, however long it takes, so
//! the hold and the set-aside are recorded together. A held row lock stands in for a large outbox (300,000 waiting
//! events take 11 s on a laptop, against the 15 s every system transaction is given).

use ferrum_kernel::outbox;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::json;
use std::path::Path;
use std::time::Duration;

#[test]
fn setting_aside_is_not_cut_short_by_the_statement_timeout() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let catalog = || Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 8, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
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
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    outbox::enqueue(&mut tx, tenant, "paid", None, None, json!({})).await.unwrap();
    tx.commit().await.unwrap();

    // A replica whose statements get one second, as a large outbox outlasts the fifteen they get.
    let db = Db::connect(DbConfig { url, max_connections: 8, statement_timeout_ms: 1000, ..Default::default() }).await.expect("connect");
    let replica = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    assert!(!replica.restore_held().await.unwrap());

    // The database is swapped for another under it, and the set-aside takes three seconds.
    sqlx::query("update platform_incarnation set system_identifier = system_identifier + 1").execute(&k.db.pool).await.unwrap();
    let mut slow = k.db.system_tx().await.unwrap();
    sqlx::query("select id from outbox where topic = 'paid' for update").fetch_one(&mut *slow).await.unwrap();
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        slow.commit().await.unwrap();
    });
    let looked = replica.look_again().await;
    release.await.unwrap();
    assert!(looked.is_ok(), "the restore was noticed: {looked:?}");
    assert!(replica.restore_held().await.unwrap(), "and is held");
    let mut tx = k.db.system_tx().await.unwrap();
    let set_aside: bool =
        sqlx::query_scalar("select dead and last_error like '%restored%' from outbox where topic = 'paid'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(set_aside, "what was waiting is set aside with it");
}
