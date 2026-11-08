//! Backfills, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first):
//! a package's backfill runs in every workspace in batches, carries on after a failure from where
//! its last batch got without visiting a row twice, reports the rows it could not change, and runs
//! again when an administrator asks, never by itself once done.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::path::PathBuf;
use uuid::Uuid;

const NOTES: &str = "- name: Note\n  label: Note\n  kind: master\n  fields:\n    - {name: title, type: data}\n    - {name: slug, type: data}\n    - {name: touched, type: int}\n";
const SLUGS: &str =
    "  - {name: slugs, entity: Note, needs: \"t.slug is null\", set: \"slug = lower(t.title), touched = coalesce(t.touched, 0) + 1\", batch: 250}\n";

fn catalog(name: &str, backfills: &str) -> Result<Catalog, ferrum_meta::MetaError> {
    let dir: PathBuf = std::env::temp_dir().join(format!("ferrum-backfill-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("notes/entities")).unwrap();
    std::os::unix::fs::symlink(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("notes/package.yaml"), format!("package: notes\nversion: 0.1.0\nlabel: Notes\ndepends: [core]\nbackfills:\n{backfills}")).unwrap();
    std::fs::write(dir.join("notes/entities/notes.yaml"), NOTES).unwrap();
    let c = Catalog::load_dir(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    c
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

async fn workspace(k: &Kernel, slug: &str) -> Uuid {
    k.create_tenant(NewTenant {
        slug: slug.into(),
        name: slug.into(),
        admin_email: format!("admin@{slug}.test"),
        admin_password: "Correct-Horse-9".into(),
        admin_name: None,
        packages: vec!["notes".into()],
        settings: None,
    })
    .await
    .expect("workspace")
}

#[test]
fn a_backfill_runs_in_batches_and_resumes() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    // A declaration names one table or entity, which rows need it and what it sets.
    for (bad, why) in [
        ("  - {name: Slugs, entity: Note, needs: \"t.slug is null\", set: \"slug = t.title\"}\n", "a name in capitals"),
        ("  - {name: slugs, entity: Note, table: e_note, needs: \"t.slug is null\", set: \"slug = t.title\"}\n", "both a table and an entity"),
        ("  - {name: slugs, entity: Nope, needs: \"t.slug is null\", set: \"slug = t.title\"}\n", "an entity nobody defines"),
        ("  - {name: slugs, entity: Note, needs: \" \", set: \"slug = t.title\"}\n", "no rows it needs"),
        ("  - {name: slugs, entity: Note, needs: \"t.slug is null\", set: \"slug = t.title\", batch: 0}\n", "an empty batch"),
        (&format!("{SLUGS}{SLUGS}"), "twice the same name"),
    ] {
        assert!(catalog("bad", bad).is_err(), "{why}");
    }

    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog("ok", SLUGS).expect("catalog"), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = workspace(&k, "acme").await;
    workspace(&k, "other").await;
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    for i in 0..620 {
        k.insert(&admin, "Note", obj(json!({"title": format!("Note {i:04}")}))).await.unwrap();
    }
    let sql = |q: &'static str| {
        let k = k.clone();
        async move {
            let mut tx = k.db.system_tx().await.unwrap();
            sqlx::raw_sql(q).execute(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
        }
    };
    let count = |q: &'static str| {
        let k = k.clone();
        async move {
            let mut tx = k.db.system_tx().await.unwrap();
            let n: i64 = sqlx::query_scalar(q).fetch_one(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            n
        }
    };
    // The second batch fails the first time it reaches note 400, as a crash would.
    sql("create sequence bf_once; \
         create function bf_fail() returns trigger language plpgsql as $$ begin \
           if new.title = 'Note 0400' and nextval('bf_once') = 1 then raise exception 'the first time' using errcode = 'XX000'; end if; return new; end $$; \
         create trigger bf_fail before update on e_note for each row execute function bf_fail();")
    .await;
    assert!(matches!(k.backfills(&k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap()).await, Ok(ref b) if b[0]["state"] == "waiting"));

    k.maintenance().await.unwrap();
    k.run_jobs_once(4).await.unwrap();
    assert_eq!(
        count("select changed from backfill_run where name = 'slugs' and tenant_id = (select id from tenant where slug = 'acme')").await,
        250,
        "the first batch stays"
    );
    assert_eq!(count("select count(*) from e_note where slug is not null").await, 250);
    // Meanwhile what runs keeps to the fields it was reviewed with (FLC-MIG-005): a release whose rule reads the old
    // field, kept and activated while the backfill is stopped half-way, checks every save then and after the backfill
    // has resumed and finished, as it was approved.
    let reviewer = k.create_user(&admin, "reviewer@acme.test", "Reviewer", Some("Correct-Horse-9"), &[], true).await.unwrap();
    let reviewer = k.load_ctx(tenant, reviewer, None, "it".into()).await.unwrap();
    let titled = json!({"schema_version": "ferrum.solution/1", "kind": "Solution", "id": "acme.titles", "version": "1.0.0", "label": "Titles",
        "rules": [{"id": "titled", "entity": "Note", "phase": "validate", "check": "!has(doc.title) || doc.title != \"forbidden\"",
                   "message": "That title is not allowed", "field": "title"}]});
    let kept = k.save_release(&admin, &titled.to_string()).await.unwrap();
    let release: Uuid = kept["id"].as_str().unwrap().parse().unwrap();
    k.review_release(&reviewer, release, kept["artifact_digest"].as_str().unwrap(), true, "").await.unwrap();
    k.activate_release(&admin, release, None).await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    assert!(k.insert(&admin, "Note", obj(json!({"title": "forbidden"}))).await.is_err(), "checked while the backfill is stopped");
    sql("update job set run_at = now() where status = 'queued'").await;
    k.run_jobs_once(4).await.unwrap();
    assert_eq!(count("select count(*) from e_note where slug = lower(title)").await, 620, "every note");
    assert_eq!(count("select max(touched)::bigint from e_note").await, 1, "each once, though a batch failed");
    assert!(k.insert(&admin, "Note", obj(json!({"title": "forbidden"}))).await.is_err(), "and after it finished, as approved");
    assert_eq!(count("select generation from logic_activation where solution = 'acme.titles'").await, 1, "the same activation throughout");
    let report = k.backfills(&admin).await.unwrap();
    assert_eq!(
        (&report[0]["state"], &report[0]["changed"], &report[0]["remaining"], &report[0]["needing_now"]),
        (&json!("done"), &json!(620), &json!(0), &json!(0)),
        "{report:?}"
    );

    // A row its set cannot change is counted, and after it is put right the backfill runs again
    // when asked; the upkeep never runs a finished one by itself.
    k.insert(&admin, "Note", obj(json!({"title": null}))).await.unwrap();
    k.maintenance().await.unwrap();
    assert_eq!(count("select count(*) from job where kind = 'platform.backfill' and status = 'queued'").await, 0, "done is done");
    assert_eq!(k.backfills(&admin).await.unwrap()[0]["needing_now"], json!(1));
    let clerk = k.create_user(&admin, "clerk@acme.test", "Clerk", None, &[], false).await.unwrap();
    let clerk = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    assert!(matches!(k.backfills(&clerk).await, Err(Error::Forbidden(_))));
    assert!(matches!(k.rerun_backfill(&clerk, "notes", "slugs").await, Err(Error::Forbidden(_))));
    assert!(matches!(k.rerun_backfill(&admin, "notes", "nope").await, Err(Error::NotFound(_))));
    k.rerun_backfill(&admin, "notes", "slugs").await.unwrap();
    k.maintenance().await.unwrap();
    k.run_jobs_once(4).await.unwrap();
    let report = k.backfills(&admin).await.unwrap();
    assert_eq!((&report[0]["changed"], &report[0]["remaining"]), (&json!(1), &json!(1)), "what it could not change: {report:?}");
    assert!(matches!(k.rerun_backfill(&admin, "notes", "slugs").await, Ok(())));
    k.maintenance().await.unwrap();
    assert!(matches!(k.rerun_backfill(&admin, "notes", "slugs").await, Err(Error::Conflict(_))), "not while it runs");
}
