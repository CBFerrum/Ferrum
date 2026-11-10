//! Booting with `migrate_at_start` off (`FERRUM_MIGRATE_AT_START=false`) changes nothing in the database and refuses a
//! database that `ferrum task migrate` has not brought up to this build; with it on, boot migrates as it always did.
//! Against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first).

use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use std::path::Path;

#[test]
fn a_start_that_does_not_migrate_checks_instead() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let boot = |migrate_at_start: bool| {
        let db = db.clone();
        async move {
            let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
            Kernel::boot(db, catalog, HookRegistry::default(), KernelSettings { migrate_at_start, ..Default::default() }, None).await
        }
    };
    let refused = |what: &str, r: ferrum_kernel::Result<std::sync::Arc<Kernel>>| -> String {
        let Err(e) = r else { panic!("{what}: booted") };
        e.to_string()
    };

    // An empty database: nothing is migrated.
    let said = refused("empty database", boot(false).await);
    assert!(said.contains("ferrum task migrate") && said.contains("migrations are not applied"), "{said}");
    assert!(db.applied_migrations().await.unwrap().is_empty(), "the check wrote to the database");

    // What the task does: boot with the step on. Afterwards the check passes and changes nothing.
    let first = boot(true).await.expect("migrating boot");
    assert!(!first.booted.migrations.is_empty() && !first.booted.schema.skipped);
    let before = db.applied_migrations().await.unwrap();
    let checked = boot(false).await.expect("a migrated database starts");
    assert!(checked.booted.migrations.is_empty() && checked.booted.schema.statements == 0);
    assert_eq!(db.applied_migrations().await.unwrap(), before);

    // With the step on and nothing to do, nothing changes.
    let again = boot(true).await.expect("migrating boot again");
    assert!(again.booted.migrations.is_empty() && again.booted.schema.skipped);

    // A schema synced for other packages.
    sqlx::query("update cluster_state set value = 'older' where key = 'catalog_fingerprint'").execute(&db.pool).await.unwrap();
    let said = refused("stale schema", boot(false).await);
    assert!(said.contains("ferrum task migrate") && said.contains("schema"), "{said}");
    boot(true).await.expect("the task syncs it");
    boot(false).await.expect("and the check passes");

    // A migration the database lacks (the last one, as an older state would).
    sqlx::query("delete from _sqlx_migrations where version = (select max(version) from _sqlx_migrations)").execute(&db.pool).await.unwrap();
    let said = refused("one migration behind", boot(false).await);
    assert!(said.contains("1 of this build's database migrations are not applied"), "{said}");
}
