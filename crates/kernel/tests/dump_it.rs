//! A whole database dumped and restored as the server's own role, no superuser (D7; docs/operations.md, "Backups and
//! restore"): `deploy/dump.sh` against real PostgreSQL, from `FERRUM_TEST_DATABASE_URL` into the empty
//! `FERRUM_RESTORE_TEST_DATABASE_URL` (both reset first), with pg_dump and pg_restore from `PG_BIN` or the PATH. Every
//! row of every table of every schema (the settings' tables, and the vault's when it is there), and every sequence,
//! comes back as it was, and the server starts on the copy, holding as after any restore.

use ferrum_kernel::outbox;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

fn url(env: &str, database: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| format!("postgres://ferrum:ferrum@localhost:5432/{database}"))
}

fn catalog() -> Catalog {
    Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog")
}

async fn database(url: &str, reset: bool) -> Db {
    let db = Db::connect(DbConfig { url: url.to_string(), max_connections: 4, ..Default::default() }).await.expect("connect");
    if reset {
        // The vault's schema too: it lives beside public (crates/kernel/src/vault), and a restore goes into an empty database.
        sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;")
            .execute(&db.pool)
            .await
            .expect("reset");
    }
    db
}

/// `deploy/dump.sh` on the database at `args[1]`, by PostgreSQL's own address: through a pooler the script refuses (it
/// needs one session, and the setting it gives pg_dump's), so where suites run through one (scripts/local_ci.sh managed)
/// the direct host stands in.
fn script(args: &[&str]) -> std::process::Output {
    let mut args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    if let Ok(host) = std::env::var("FERRUM_TEST_DATABASE_DIRECT_HOST") {
        args[1] = ferrum_kernel::db::with_host(&args[1], &host).expect("a direct address");
    }
    Command::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/dump.sh")).args(&args).output().expect("deploy/dump.sh")
}

/// Every table's rows (as the system sees them) and every sequence, by name: a count and a digest that do not depend on
/// the order rows come in.
async fn contents(db: &Db) -> BTreeMap<String, (i64, String)> {
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.system', 'on', true), set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
    let tables: Vec<String> = sqlx::query_scalar(
        "select format('%I.%I', n.nspname, c.relname) from pg_class c join pg_namespace n on n.oid = c.relnamespace \
         where c.relkind in ('r', 'p') and not c.relispartition and n.nspname not in ('pg_catalog', 'information_schema') \
         and n.nspname !~ '^pg_' and not exists (select 1 from pg_depend d where d.objid = c.oid and d.deptype = 'e') order by 1",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    let mut out = BTreeMap::new();
    for t in tables {
        let row: (i64, Option<String>) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select count(*), md5(string_agg(h, '' order by h)) from (select md5(to_jsonb(x)::text) h from {t} x) s"
        )))
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        out.insert(t, (row.0, row.1.unwrap_or_default()));
    }
    let sequences: Vec<(String, Option<i64>)> =
        sqlx::query_as("select format('%I.%I', schemaname, sequencename), last_value from pg_sequences order by 1").fetch_all(&mut *tx).await.unwrap();
    for (s, last) in sequences {
        out.insert(format!("sequence {s}"), (last.unwrap_or(0), String::new()));
    }
    tx.commit().await.unwrap();
    out
}

#[test]
fn a_dump_as_the_servers_own_role_holds_everything_and_restores() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let live = url("FERRUM_TEST_DATABASE_URL", "ferrum_test");
    let copy = url("FERRUM_RESTORE_TEST_DATABASE_URL", "ferrum_restore_test");
    let db = database(&live, true).await;
    let (superuser, bypass): (bool, bool) =
        sqlx::query_as("select rolsuper, rolbypassrls from pg_roles where rolname = current_user").fetch_one(&db.pool).await.unwrap();
    assert!(!superuser && !bypass, "the server's own role, as it runs");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    // Two workspaces with people, events waiting and done, the audit trail, and an installation's setting.
    for slug in ["acme", "bravo"] {
        let tenant = k
            .create_tenant(NewTenant {
                slug: slug.into(),
                name: slug.into(),
                admin_email: format!("admin@{slug}.test"),
                admin_password: "Correct-Horse-9".into(),
                admin_name: None,
                packages: vec![],
                settings: None,
            })
            .await
            .unwrap();
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        for n in 0..3 {
            outbox::enqueue(&mut tx, tenant, "pay", None, None, json!({"n": n})).await.unwrap();
        }
        tx.commit().await.unwrap();
    }
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("insert into setting (scope, scope_name, key, value, updated_by) values ('installation', '', 'session_days', '\"7\"', 'it')")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let before = contents(&k.db).await;
    assert!(before["public.app_user"].0 >= 2 && before["public.outbox"].0 >= 6 && before["public.setting"].0 == 1, "{before:?}");

    // Without the system's setting the role's dump would hold no workspace's rows, and say nothing: the script sets it.
    let file = std::env::temp_dir().join(format!("ferrum-dump-{}.dump", uuid::Uuid::now_v7()));
    let file = file.to_str().unwrap();
    let done = script(&["dump", &live, file]);
    assert!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));

    // A database that is not empty is refused; an empty one the role owns takes it, all of it.
    let target = database(&copy, true).await;
    sqlx::query("create table left_over (x int)").execute(&target.pool).await.unwrap();
    let refused = script(&["restore", &copy, file]);
    assert!(!refused.status.success() && String::from_utf8_lossy(&refused.stderr).contains("empty database"), "{refused:?}");
    sqlx::query("drop table left_over").execute(&target.pool).await.unwrap();
    target.close().await;
    let done = script(&["restore", &copy, file]);
    let _ = std::fs::remove_file(file);
    assert!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));

    let restored = database(&copy, false).await;
    let after = contents(&restored).await;
    let differ: Vec<&String> = before.keys().chain(after.keys()).filter(|t| before.get(*t) != after.get(*t)).collect();
    assert!(differ.is_empty(), "these came back otherwise: {differ:?}");
    // Still isolated: without the setting the role sees no workspace's rows.
    let seen: i64 = sqlx::query_scalar("select count(*) from app_user").fetch_one(&restored.pool).await.unwrap();
    assert_eq!(seen, 0, "row-level security came back forced");

    // The server starts on it, and holds what reaches outside, as after any restore.
    let k2 = Kernel::boot(restored, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("starts on the copy");
    assert!(k2.restore_held().await.unwrap(), "a restored database is held until a person releases it");
    k2.login("acme", "admin@acme.test", "Correct-Horse-9").await.expect("people sign in as before");
    k.db.close().await;
}
