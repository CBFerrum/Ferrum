//! Every partitioned table's rows through a dump and a restore made as the server's own role (`deploy/dump.sh`, which
//! reads each partition by its own name as the system), with the platform's packages and the operations app installed:
//! from `FERRUM_OPS_TEST_DATABASE_URL` into the empty `FERRUM_OPS_RESTORE_TEST_DATABASE_URL` (both reset first), with
//! pg_dump and pg_restore from `PG_BIN` or the PATH. A partition that does not carry its table's policies reads as empty
//! there, and the dump would hold none of its rows without a word.

use chrono::Utc;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_ops::contract::telemetry::{Class, Measurement, Quality};
use ferrum_ops::telemetry;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use uuid::Uuid;

fn url(env: &str, database: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| format!("postgres://ferrum:ferrum@localhost:5432/{database}"))
}

async fn reset(url: &str) {
    let db = Db::connect(DbConfig { url: url.to_string(), max_connections: 1, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    db.pool.close().await;
}

async fn boot(url: &str) -> Arc<Kernel> {
    let db = Db::connect(DbConfig { url: url.to_string(), max_connections: 4, ..Default::default() }).await.expect("connect");
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

/// Every partitioned table, with its rows as the system reads them through it.
async fn partitioned(url: &str) -> BTreeMap<String, i64> {
    let db = Db::connect(DbConfig { url: url.to_string(), max_connections: 1, ..Default::default() }).await.expect("connect");
    let mut tx = db.system_tx().await.unwrap();
    let tables: Vec<String> =
        sqlx::query_scalar("select oid::regclass::text from pg_class where relkind = 'p' and not relispartition order by 1").fetch_all(&mut *tx).await.unwrap();
    let mut out = BTreeMap::new();
    for t in tables {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from {t}"))).fetch_one(&mut *tx).await.unwrap();
        out.insert(t, n);
    }
    tx.commit().await.unwrap();
    db.pool.close().await;
    out
}

fn script(args: &[&str]) -> std::process::Output {
    Command::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/dump.sh")).args(args).output().expect("deploy/dump.sh")
}

#[test]
fn every_partitioned_row_survives_a_dump() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let (live, copy) = (url("FERRUM_OPS_TEST_DATABASE_URL", "ferrum_ops_test"), url("FERRUM_OPS_RESTORE_TEST_DATABASE_URL", "ferrum_ops_restore_test"));
    reset(&live).await;
    reset(&copy).await;
    let k = boot(&live).await;
    // A save (the audit trail), and a sensor's measurement (its day, and its minute's rollup).
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
    telemetry::ensure_partitions(&k, now).await.unwrap();
    let m = Measurement {
        device_id: sensor,
        metric: "temp".into(),
        value: 20.0,
        unit: None,
        source_time: now,
        boot: Uuid::nil(),
        seq: 1,
        quality: Quality::Good,
        class: Class::Sample,
    };
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert_eq!(telemetry::ingest(&mut tx, tenant, site, vec![m], now).await.unwrap(), (1, 0));
    tx.commit().await.unwrap();
    k.db.pool.close().await;

    let before = partitioned(&live).await;
    println!("partitioned tables, with every platform package and the operations app: {before:?}");
    for t in ["audit_event", "op_measurement", "op_rollup_minute"] {
        assert!(before.get(t).is_some_and(|n| *n > 0), "{t}: {before:?}");
    }
    let dir = std::env::temp_dir().join(format!("ferrum-ops-dump-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("ferrum.dump");
    let file = file.to_str().unwrap();
    let done = script(&["dump", &live, file]);
    assert!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));
    let done = script(&["restore", &copy, file]);
    assert!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));
    assert_eq!(partitioned(&copy).await, before, "every partitioned table's rows came back");
    // A partition made some other way, without its table's policies, would be dumped empty: the dump is refused, naming it.
    let db = Db::connect(DbConfig { url: live.clone(), max_connections: 1, ..Default::default() }).await.unwrap();
    sqlx::raw_sql("create table op_measurement_20400101 partition of op_measurement for values from ('2040-01-01') to ('2040-01-02')")
        .execute(&db.pool)
        .await
        .unwrap();
    db.pool.close().await;
    let refused = script(&["dump", &live, file]);
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success() && said.contains("op_measurement_20400101"), "{said}");
    std::fs::remove_dir_all(&dir).ok();
    // No database left with that partition: a server would refuse to start on it.
    reset(&live).await;
    reset(&copy).await;
}
