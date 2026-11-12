//! A backfill over a child entity's lines, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset
//! first): the lines table is found from the parent's field, a child nothing holds is refused when the package loads,
//! and a backfill that fails for a cause retrying cannot change ends as failed with its reason, once, and is not queued
//! again by the upkeep.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::path::PathBuf;

const NOTES: &str = "- name: Note\n  label: Note\n  kind: master\n  fields:\n    - {name: title, type: data}\n    - {name: lines, type: table, of: NoteLine}\n\
- name: NoteLine\n  label: Note line\n  kind: child\n  fields:\n    - {name: tag, type: data}\n    - {name: seen, type: int}\n";
const ORPHAN: &str = "- name: Orphan\n  label: Orphan\n  kind: child\n  fields:\n    - {name: tag, type: data}\n";
const SEEN: &str = "  - {name: seen, entity: NoteLine, needs: \"t.seen is null\", set: \"seen = 1\"}\n";
const BROKEN: &str = "  - {name: broken, entity: NoteLine, needs: \"t.seen is null\", set: \"no_such_column = 1\"}\n";

fn catalog(name: &str, entities: &str, backfills: &str) -> Result<Catalog, ferrum_meta::MetaError> {
    let dir: PathBuf = std::env::temp_dir().join(format!("ferrum-backfill-lines-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("notes/entities")).unwrap();
    std::os::unix::fs::symlink(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("notes/package.yaml"), format!("package: notes\nversion: 0.1.0\nlabel: Notes\ndepends: [core]\nbackfills:\n{backfills}")).unwrap();
    std::fs::write(dir.join("notes/entities/notes.yaml"), entities).unwrap();
    let c = Catalog::load_dir(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    c
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

#[test]
fn a_backfill_over_lines() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let both = format!("{NOTES}{ORPHAN}");
    let orphan = "  - {name: orphan, entity: Orphan, needs: \"t.tag is null\", set: \"tag = 'x'\"}\n";
    let e = catalog("orphan", &both, orphan).expect_err("a child no field holds");
    assert!(e.to_string().contains("notes: backfill orphan"), "{e}");
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog("ok", NOTES, &format!("{SEEN}{BROKEN}")).expect("catalog"), HookRegistry::default(), KernelSettings::default(), None)
        .await
        .expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["notes".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    k.insert(&admin, "Note", obj(json!({"title": "a", "lines": [{"tag": "x"}, {"tag": "y"}]}))).await.unwrap();
    let count = |q: &'static str| {
        let k = k.clone();
        async move {
            let mut tx = k.db.system_tx().await.unwrap();
            let n: i64 = sqlx::query_scalar(q).fetch_one(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            n
        }
    };
    // Ten rounds of upkeep and jobs, each as soon as it may run.
    for _ in 0..10 {
        k.maintenance().await.unwrap();
        let mut tx = k.db.system_tx().await.unwrap();
        sqlx::raw_sql("update job set run_at = now() where status = 'queued'").execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        k.run_jobs_once(4).await.unwrap();
    }
    assert_eq!(count("select count(*) from e_note__lines where seen = 1").await, 2, "the lines of a child entity are found and set");
    assert_eq!(
        count("select count(*) from job where kind = 'platform.backfill' and dedupe_key = 'notes/broken' and status = 'failed' and attempts <= max_attempts")
            .await,
        1,
        "a backfill that cannot succeed ends as failed, once"
    );
    assert_eq!(count("select count(*) from job where kind = 'platform.backfill' and dedupe_key = 'notes/broken'").await, 1, "and is not queued again");
    assert_eq!(count("select count(*) from job where kind = 'platform.backfill' and status in ('queued', 'running')").await, 0);
    let report = k.backfills(&admin).await.unwrap();
    let broken = report.iter().find(|r| r["name"] == "broken").unwrap();
    assert_eq!(broken["state"], json!("failed"), "{report:?}");
    assert!(broken["error"].as_str().is_some_and(|e| !e.is_empty()), "it says why: {report:?}");
}
