//! A start that does not migrate (`FERRUM_MIGRATE_AT_START=false`) also checks that the one-time upgrade steps ran, and
//! `ferrum task migrate` is what runs them. Against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; reset first).

use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use std::path::Path;

#[test]
fn upgrade_steps_are_checked_and_run_by_the_task() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages")).expect("catalog");
    let k = Kernel::boot(db, catalog, HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");

    let Err(e) = ferrum_server::upgrade::verify(&k).await else { panic!("steps that never ran were accepted") };
    assert!(e.to_string().contains("ferrum task migrate"), "{e}");
    let steps = ferrum_server::upgrade::run(&k).await.expect("the steps");
    assert!(steps.iter().any(|(s, _)| *s == "core-split"), "{steps:?}");
    ferrum_server::upgrade::verify(&k).await.expect("and now they have");
}
