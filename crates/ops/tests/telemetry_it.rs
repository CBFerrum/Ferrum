//! Telemetry's partitions against a real PostgreSQL (`FERRUM_OPS_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_ops_test`; reset by each round): measurements are stored on a
//! database reset after another round in the same process (what was made is asked of the database, not remembered), a
//! sample with a wrong clock is stored and not rolled up into a month that is not there, and retention drops a whole
//! month of rollups where it deleted rows.

use chrono::{Duration, Utc};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_ops::contract::telemetry::{Class, Measurement, Quality};
use ferrum_ops::telemetry;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

async fn boot() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_OPS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_ops_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("packages");
    packages.extend(ferrum_ops::packages().expect("ops"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    let ops: Vec<String> = ferrum_ops::packages().expect("ops packages").iter().map(|p| p.name().to_string()).collect();
    hooks.app("ops", &ops, ferrum_ops::register).own("ops", &ops);
    Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot")
}

fn obj(v: serde_json::Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(doc: &Doc) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

/// What rows `sql` counts, as the system.
async fn count(k: &Kernel, sql: &str) -> i64 {
    let mut tx = k.db.system_tx().await.unwrap();
    let n = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

async fn months(k: &Kernel) -> Vec<String> {
    sqlx::query_scalar(
        "select c.relname::text from pg_inherits i join pg_class c on c.oid = i.inhrelid where i.inhparent = 'op_rollup_minute'::regclass order by 1",
    )
    .fetch_all(&k.db.pool)
    .await
    .unwrap()
}

/// A workspace with a sensor, and three samples of it: one now, one from a clock years ahead and one from a clock years
/// behind. All three are stored; only the first is rolled up.
async fn round(k: &Arc<Kernel>) {
    let tenant = k
        .create_tenant(NewTenant {
            slug: "north".into(),
            name: "north".into(),
            admin_email: "admin@north.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["ops".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("north", "admin@north.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let site = id_of(&k.insert(&admin, "Site", obj(json!({"site_name": "North", "site_code": "north"}))).await.unwrap());
    let sensor = id_of(&k.insert(&admin, "Device", obj(json!({"device_name": "Temp", "device_code": "T-1", "kind": "Sensor", "site": site}))).await.unwrap());
    let now = Utc::now();
    let m = |seq: i64, source_time| Measurement {
        device_id: sensor,
        metric: "temp".into(),
        value: 20.0,
        unit: None,
        source_time,
        boot: Uuid::nil(),
        seq,
        quality: Quality::Good,
        class: Class::Sample,
    };
    telemetry::ensure_partitions(k, now).await.expect("partitions");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let batch = vec![m(1, now), m(2, now + Duration::days(4000)), m(3, now - Duration::days(4000))];
    let (stored, refused) = telemetry::ingest(&mut tx, tenant, site, batch, now).await.expect("stored");
    tx.commit().await.unwrap();
    assert_eq!((stored, refused), (3, 0));
    assert_eq!(count(k, "select count(*) from op_rollup_minute").await, 1, "only the sample with a believable clock is rolled up");
}

#[test]
fn old_rollup_month_dropped() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    // Twice, each on a database reset first: the second must make its partitions again.
    round(&boot().await).await;
    let k = boot().await;
    round(&k).await;

    // Rollups 430 days old sit in a month wholly past the 400 kept; retention drops that month and keeps the rest.
    let have = months(&k).await;
    let old = Utc::now() - Duration::days(430);
    let made: i32 = sqlx::query_scalar("select ferrum_ensure_month_partitions('op_rollup_minute', $1, $1)").bind(old).fetch_one(&k.db.pool).await.unwrap();
    assert_eq!(made, 1);
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query(
        "insert into op_rollup_minute (tenant_id, device_id, metric, minute, samples) select tenant_id, device_id, metric, $1, 1 from op_rollup_minute",
    )
    .bind(old)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(count(&k, "select count(*) from op_rollup_minute").await, 2);
    assert!(telemetry::purge(&k, Utc::now()).await.unwrap() >= 1);
    assert_eq!(months(&k).await, have, "the old month alone is dropped");
    assert_eq!(count(&k, "select count(*) from op_rollup_minute").await, 1, "today's rollup stays");
    let oldest = Utc::now() - Duration::days(400);
    assert!(have[0] <= format!("op_rollup_minute_y{}", oldest.format("%Ym%m")), "the month of the oldest minute kept is there: {have:?}");
}
