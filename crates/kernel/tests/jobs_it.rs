//! Background jobs against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset
//! first): a flaky job gets there and a poisoned one stops; a crashed worker's job is taken over and
//! the worker cannot write over its successor; stopping a replica costs no attempt; cancelling drops
//! the work; workspaces take turns; a job runs with the rights its owner has when it runs; bulk jobs
//! reach exactly the records they named; an export holds a workspace and none of its secrets.

use async_trait::async_trait;
use ferrum_kernel::bulk::{BulkOp, BulkTarget};
use ferrum_kernel::files::FileStore;
use ferrum_kernel::jobs::{Ended, Job, JobHandler, NewJob, enqueue};
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, Result, Trace};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use uuid::Uuid;

const ENTITIES: &str = r#"
- name: CaseRow
  label: Case Row
  kind: child
  fields:
    - {name: note, type: data}
- name: Case
  label: Case
  kind: master
  title_field: title
  list: {sort: title}
  rules:
    - {name: stays-open, check: "!doc.locked || doc.status == 'Open'", message: "This case is locked"}
  fields:
    - {name: title, type: data, required: true}
    - {name: status, type: select, options: [Open, Closed], default: Open}
    - {name: locked, type: check}
    - {name: token, type: secret}
    - {name: rows, type: table, of: CaseRow}
"#;

const PACKAGE: &str = r#"package: cases
version: 0.1.0
label: Cases
depends: [core]
export_tables: [case_log]
roles:
  - name: Clerk
    permissions:
      - {entity: Case, read: true, write: true, create: true}
"#;

const ENGINE: &str = r#"
create table case_log (tenant_id uuid not null references tenant(id) on delete cascade, seq bigint generated always as identity, what text not null, primary key (tenant_id, seq));
create table case_keys (tenant_id uuid not null references tenant(id) on delete cascade, api_key text not null, primary key (tenant_id));
alter table case_log enable row level security;
alter table case_log force row level security;
create policy tenant_isolation on case_log using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
alter table case_keys enable row level security;
alter table case_keys force row level security;
create policy tenant_isolation on case_keys using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
"#;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-jobs-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("cases/entities")).unwrap();
    std::fs::create_dir_all(dir.join("cases/sql")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("cases/package.yaml"), PACKAGE).unwrap();
    std::fs::write(dir.join("cases/entities/cases.yaml"), ENTITIES).unwrap();
    std::fs::write(dir.join("cases/sql/001_engine.sql"), ENGINE).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

#[derive(Default)]
struct MemFiles(Mutex<HashMap<String, Vec<u8>>>);

#[async_trait]
impl FileStore for MemFiles {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        self.0.lock().unwrap().insert(key.to_string(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned().ok_or_else(|| Error::NotFound("object".into()))
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

/// Does what each job's payload says.
struct Scripted;

#[async_trait]
impl JobHandler for Scripted {
    fn kind(&self) -> &'static str {
        "test.scripted"
    }

    fn title(&self) -> &'static str {
        "Test job"
    }

    async fn run(&self, k: &Kernel, job: &Job) -> Result<Value> {
        match job.payload["do"].as_str() {
            Some("ok") => Ok(json!({"attempt": job.attempts})),
            Some("flaky") if job.attempts < 3 => Err(Error::Unavailable("not yet".into())),
            Some("flaky") => Ok(json!({"attempt": job.attempts})),
            Some("down") => Err(Error::Unavailable("still down".into())),
            Some("db") => Err(Error::Db("relation case_keys is broken".into())),
            Some("refused") => Err(Error::Forbidden("not for this one".into())),
            Some("hang") => {
                // A write it never gets to commit.
                let mut tx = k.db.system_tenant_tx(job.tenant_id).await?;
                sqlx::query("update app_user set full_name = 'Half done' where tenant_id = $1").bind(job.tenant_id).execute(&mut *tx).await?;
                std::future::pending::<()>().await;
                tx.commit().await?;
                Ok(json!({}))
            }
            _ => Err(Error::invalid("no script")),
        }
    }
}

#[test]
fn background_work_survives_what_goes_wrong() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

struct World {
    k: Arc<Kernel>,
    admin: Ctx,
    tenant: Uuid,
}

impl World {
    async fn queue(&self, who: &Ctx, job: NewJob) -> Uuid {
        let mut tx = self.k.db.tenant_tx(who.tenant).await.unwrap();
        let id = enqueue(&mut tx, who.tenant, Some(who.user), job).await.unwrap();
        tx.commit().await.unwrap();
        id
    }

    async fn script(&self, what: &str) -> Uuid {
        self.queue(&self.admin, NewJob::new("test.scripted", json!({"do": what}))).await
    }

    async fn sql(&self, sql: &'static str) {
        let mut tx = self.k.db.system_tx().await.unwrap();
        sqlx::query(sql).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
    }

    /// Time passes: backoff is over, and so is every lease.
    async fn later(&self) {
        self.sql("update job set run_at = now() where status = 'queued'").await;
        self.sql("update job set lease_until = now() - interval '1 second' where status = 'running'").await;
    }

    async fn view(&self, id: Uuid) -> Value {
        self.k.job(&self.admin, id).await.unwrap()
    }

    async fn told(&self, title: &str) -> i64 {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let n = sqlx::query_scalar("select count(*) from notification where tenant_id = $1 and title = $2")
            .bind(self.tenant)
            .bind(title)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        n
    }

    async fn claim(&self, slots: usize) -> Vec<Job> {
        self.k.claim_jobs(slots).await.unwrap()
    }

    async fn settle(&self) {
        self.sql("update job set status = 'cancelled', finished_at = now() where status in ('queued', 'running')").await;
    }
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut hooks = HookRegistry::default();
    hooks.add_job_handler(Arc::new(Scripted));
    let settings = KernelSettings { secret_key: Some(SecretKey::new([7u8; 32])), ..Default::default() };
    let k = Kernel::boot(db, catalog(), hooks, settings, None).await.expect("boot");
    let files = Arc::new(MemFiles::default());
    k.set_file_store(files.clone());
    let workspace = |slug: &str| NewTenant {
        slug: slug.into(),
        name: slug.into(),
        admin_email: format!("admin@{slug}.test"),
        admin_password: "Correct-Horse-9".into(),
        admin_name: None,
        packages: vec!["cases".into()],
        settings: None,
    };
    let tenant = k.create_tenant(workspace("acme")).await.expect("workspace");
    let other = k.create_tenant(workspace("other")).await.expect("workspace");
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let session = k.login("other", "admin@other.test", "Correct-Horse-9").await.unwrap();
    let outsider = k.load_ctx(other, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let w = World { k: k.clone(), admin: admin.clone(), tenant };

    let quiet = watch::channel(false);
    let idle = quiet.1.clone();

    // ───────────── retried until it works; given up on when it will not ─────────────
    let flaky = w.script("flaky").await;
    assert_eq!(k.run_jobs_once(4).await.unwrap(), 1);
    let v = w.view(flaky).await;
    assert_eq!((v["status"].as_str(), v["attempts"].as_i64(), v["last_error"].as_str()), (Some("queued"), Some(1), Some("not yet")));
    assert_eq!(k.run_jobs_once(4).await.unwrap(), 0, "backing off");
    w.later().await;
    k.run_jobs_once(4).await.unwrap();
    w.later().await;
    k.run_jobs_once(4).await.unwrap();
    let v = w.view(flaky).await;
    assert_eq!((v["status"].as_str(), v["attempts"].as_i64(), &v["result"], &v["last_error"]), (Some("done"), Some(3), &json!({"attempt": 3}), &Value::Null));
    assert_eq!(w.told("Test job finished").await, 1);

    let mut poisoned = NewJob::new("test.scripted", json!({"do": "down"}));
    poisoned.max_attempts = 3;
    let poisoned = w.queue(&admin, poisoned).await;
    for _ in 0..3 {
        assert_eq!(k.run_jobs_once(4).await.unwrap(), 1);
        w.later().await;
    }
    assert_eq!(k.run_jobs_once(4).await.unwrap(), 0, "given up on");
    let v = w.view(poisoned).await;
    assert_eq!((v["status"].as_str(), v["attempts"].as_i64()), (Some("failed"), Some(3)));
    assert_eq!(w.told("Test job failed").await, 1);
    k.retry_job(&admin, poisoned).await.expect("someone looked at it");
    assert_eq!(w.view(poisoned).await["attempts"], json!(0));
    assert!(matches!(k.retry_job(&admin, flaky).await, Err(Error::NotFound(_))), "only failed jobs go back");
    w.settle().await;

    let refused = w.script("refused").await;
    k.run_jobs_once(4).await.unwrap();
    let v = w.view(refused).await;
    assert_eq!((v["status"].as_str(), v["attempts"].as_i64()), (Some("failed"), Some(1)), "what cannot succeed is not retried");
    let internal = w.script("db").await;
    k.run_jobs_once(4).await.unwrap();
    assert_eq!(w.view(internal).await["last_error"], json!("An internal error occurred"), "the detail stays in the log");
    w.settle().await;
    let orphan = w.queue(&admin, NewJob::new("gone.app", json!({}))).await;
    k.run_jobs_once(4).await.unwrap();
    let v = w.view(orphan).await;
    assert_eq!(v["status"], json!("failed"), "its app is not installed");
    assert!(v["last_error"].as_str().unwrap().contains("gone.app"));

    // ───────────── a worker dies; the next one takes over; the first has no say ─────────────
    let mut fragile = NewJob::new("test.scripted", json!({"do": "ok"}));
    fragile.max_attempts = 2;
    let fragile = w.queue(&admin, fragile).await;
    let first = w.claim(4).await.pop().expect("claimed");
    assert!(w.claim(4).await.is_empty(), "held");
    w.later().await;
    let second = w.claim(4).await.pop().expect("taken over");
    assert_eq!((second.id, second.attempts), (fragile, 2), "the crash counted");
    assert_ne!(first.lease_token, second.lease_token);
    assert!(!first.progress(&k, json!({"step": "stale"})).await.unwrap(), "the first worker lost it");
    k.finish_job(&first, "Test job", Ended::Done(json!({"from": "the first"}))).await.unwrap();
    k.finish_job(&first, "Test job", Ended::Failed(Error::Unavailable("late".into()))).await.unwrap();
    let v = w.view(fragile).await;
    assert_eq!((v["status"].as_str(), &v["result"], &v["progress"]), (Some("running"), &Value::Null, &json!({})), "fenced");
    assert!(second.progress(&k, json!({"step": "real"})).await.unwrap());
    let before = w.told("Test job failed").await;
    w.later().await;
    assert!(w.claim(4).await.is_empty(), "its last attempt died with its worker");
    let v = w.view(fragile).await;
    assert_eq!(v["status"], json!("failed"));
    assert!(v["last_error"].as_str().unwrap().contains("worker was lost"));
    assert_eq!(w.told("Test job failed").await, before + 1, "someone hears of it");

    // ───────────── a replica stopping hands its work back ─────────────
    let handed = w.script("ok").await;
    let held = w.claim(4).await.pop().unwrap();
    k.finish_job(&held, "Test job", Ended::Released).await.unwrap();
    let v = w.view(handed).await;
    assert_eq!((v["status"].as_str(), v["attempts"].as_i64()), (Some("queued"), Some(0)), "no attempt spent");
    assert_eq!(k.run_jobs_once(4).await.unwrap(), 1, "and it runs at once");
    assert_eq!(w.view(handed).await["status"], json!("done"));

    let (stop, stopping) = watch::channel(false);
    let runner = k.spawn_job_runner(stopping);
    let hung = w.script("hang").await;
    let quick = w.script("ok").await;
    for _ in 0..100 {
        if w.view(quick).await["status"] == json!("done") && w.view(hung).await["status"] == json!("running") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        (w.view(quick).await["status"].as_str(), w.view(hung).await["status"].as_str()),
        (Some("done"), Some("running")),
        "the runner runs jobs side by side"
    );
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), runner).await.expect("stops promptly").unwrap();
    let v = w.view(hung).await;
    assert_eq!((v["status"].as_str(), v["attempts"].as_i64()), (Some("queued"), Some(0)), "handed back by the runner too");

    // ───────────── cancelling ─────────────
    k.cancel_job(&admin, hung).await.expect("queued");
    assert_eq!(w.view(hung).await["status"], json!("cancelled"), "at once");
    assert!(matches!(k.cancel_job(&admin, hung).await, Err(Error::NotFound(_))), "it is over");
    let hung = w.script("hang").await;
    let job = w.claim(1).await.pop().unwrap();
    let running = tokio::spawn({
        let (k, stopping) = (k.clone(), idle.clone());
        async move { k.run_job(job, stopping).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    k.cancel_job(&admin, hung).await.expect("running");
    assert_eq!(w.view(hung).await["status"], json!("running"), "it has to hear of it");
    tokio::time::timeout(Duration::from_secs(10), running).await.expect("within a lease renewal").unwrap().unwrap();
    assert_eq!(w.view(hung).await["status"], json!("cancelled"));
    assert_eq!(k.load_ctx(tenant, admin.user, None, "it".into()).await.unwrap().full_name, admin.full_name, "its transaction rolled back");
    let abandoned = w.script("ok").await;
    let _lost = w.claim(1).await.pop().unwrap();
    k.cancel_job(&admin, abandoned).await.unwrap();
    w.later().await;
    assert!(w.claim(4).await.is_empty());
    assert_eq!(w.view(abandoned).await["status"], json!("cancelled"), "its worker died: nobody was left to hear it");

    // ───────────── workspaces take turns ─────────────
    for _ in 0..8 {
        w.script("ok").await;
    }
    let theirs = w.queue(&outsider, NewJob::new("test.scripted", json!({"do": "ok"}))).await;
    let claimed = w.claim(2).await;
    assert!(claimed.iter().any(|j| j.id == theirs), "one job among eight still gets its turn");
    assert!(claimed.iter().any(|j| j.tenant_id == tenant));
    assert_eq!(w.claim(8).await.len(), 1, "one per workspace per round");
    assert_eq!(w.claim(8).await.len(), 1, "up to the workspace's share");
    assert!(w.claim(8).await.is_empty(), "three of a workspace's jobs at once, however many wait");
    w.settle().await;

    let mut soon = NewJob::new("test.scripted", json!({"do": "ok", "n": "urgent"}));
    soon.priority = ferrum_kernel::outbox::PRIORITY_URGENT;
    let mut late = NewJob::new("test.scripted", json!({"do": "ok", "n": "bulk"}));
    late.priority = ferrum_kernel::outbox::PRIORITY_BULK;
    w.queue(&admin, late).await;
    w.queue(&admin, NewJob::new("test.scripted", json!({"do": "ok", "n": "normal"}))).await;
    w.queue(&admin, soon).await;
    assert_eq!(w.claim(1).await[0].payload["n"], json!("urgent"), "most urgent first, whatever came first");
    assert_eq!(w.claim(1).await[0].payload["n"], json!("normal"));
    w.settle().await;

    // ───────────── asking twice ─────────────
    let mut once = NewJob::new("test.scripted", json!({"do": "ok"}));
    once.dedupe_key = Some("the-one".into());
    let a = w.queue(&admin, once.clone()).await;
    assert_eq!(w.queue(&admin, once.clone()).await, a, "still on its way");
    k.run_jobs_once(1).await.unwrap();
    assert_ne!(w.queue(&admin, once.clone()).await, a, "a new one once it is over");
    w.settle().await;

    // ───────────── who sees what ─────────────
    let clerk = k.create_user(&admin, "clerk@acme.test", "Clerk", None, &["Clerk".to_string()], false).await.unwrap();
    let clerk = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    let peer = k.create_user(&admin, "peer@acme.test", "Peer", None, &["Clerk".to_string()], false).await.unwrap();
    let peer = k.load_ctx(tenant, peer, None, "it".into()).await.unwrap();
    let mine = w.queue(&clerk, NewJob::new("test.scripted", json!({"do": "ok"}))).await;
    assert_eq!(k.job(&clerk, mine).await.unwrap()["id"], json!(mine));
    assert_eq!(k.jobs(&clerk, 50).await.unwrap().len(), 1, "their own");
    assert!(k.jobs(&admin, 200).await.unwrap().len() > 1, "an administrator's view");
    assert!(matches!(k.job(&peer, mine).await, Err(Error::NotFound(_))));
    assert!(matches!(k.cancel_job(&peer, mine).await, Err(Error::NotFound(_))));
    assert!(matches!(k.job(&outsider, mine).await, Err(Error::NotFound(_))), "another workspace");
    assert!(matches!(k.cancel_job(&outsider, mine).await, Err(Error::NotFound(_))));
    w.settle().await;

    // ───────────── bulk: every record it named, as the person it ran for ─────────────
    let mut cases = Vec::new();
    for i in 0..620 {
        let locked = i % 300 == 7;
        let d = k
            .insert(
                &admin,
                "Case",
                obj(json!({"title": format!("case-{i:04}"), "locked": locked, "token": "s3cret-case-token", "rows": [{"note": format!("row of {i}")}]})),
            )
            .await
            .unwrap();
        cases.push(d["id"].as_str().unwrap().parse::<Uuid>().unwrap());
    }
    let close = BulkOp::Update { values: obj(json!({"status": "Closed"})) };
    assert!(matches!(k.bulk_job(&clerk, "Case", None, None, &close).await, Err(Error::Invalid(_))), "names nothing");
    assert!(matches!(k.bulk_job(&clerk, "Nothing", Some(vec![]), None, &close).await, Err(Error::NotFound(_))));
    assert!(
        matches!(k.bulk_job(&clerk, "Case", None, Some(json!({"nothing": 1})), &close).await, Err(Error::Invalid(_) | Error::Forbidden(_))),
        "refused while someone is there to hear it"
    );
    let reader = k.create_user(&admin, "reader@acme.test", "Reader", None, &[], false).await.unwrap();
    let reader = k.load_ctx(tenant, reader, None, "it".into()).await.unwrap();
    assert!(matches!(k.bulk_job(&reader, "Case", None, Some(json!({})), &close).await, Err(Error::Forbidden(_))));

    let job = k.bulk_job(&clerk, "Case", None, Some(json!({"status": "Open"})), &close).await.expect("queued");
    k.run_jobs_once(1).await.unwrap();
    let v = w.view(job).await;
    assert_eq!(v["status"], json!("done"), "{v}");
    let r = &v["result"];
    assert_eq!(
        (r["total"].as_u64(), r["succeeded"].as_u64(), r["failed"].as_u64()),
        (Some(620), Some(617), Some(3)),
        "pages shifting under it skipped nothing"
    );
    assert_eq!(r["failures"].as_array().unwrap().len(), 3, "and it says which");
    assert!(r["failures"][0]["error"]["message"].as_str().is_some());
    let closed = k
        .list(
            &admin,
            "Case",
            &ListQuery {
                filters: ferrum_kernel::query::parse_filters(&json!({"status": "Open"})).unwrap(),
                with_count: true,
                limit: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(closed.total, Some(3), "the locked ones stayed");
    let mut tx = k.db.system_tx().await.unwrap();
    let bulk: i64 = sqlx::query_scalar("select count(*) from outbox where topic = 'Case.updated' and priority = 2").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(bulk >= 617, "its events wait behind people's single edits ({bulk})");

    // A worker lost part-way: the next attempt carries on, and adds to what was done.
    let mut sorted = cases.clone();
    sorted.sort();
    let reopen = BulkOp::Update { values: obj(json!({"status": "Open"})) };
    let three = sorted[..3].to_vec();
    let job = k.bulk_job(&clerk, "Case", Some(three.clone()), None, &reopen).await.unwrap();
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update job set progress = $2 where id = $1")
        .bind(job)
        .bind(json!({"after": three[1], "total": 3, "succeeded": 2, "failed": 0, "failures": []}))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    k.run_jobs_once(1).await.unwrap();
    let v = w.view(job).await;
    assert_eq!((v["result"]["total"].as_u64(), v["result"]["succeeded"].as_u64()), (Some(3), Some(3)), "{v}");
    assert_eq!(k.get(&admin, "Case", three[0], false).await.unwrap()["status"], json!("Closed"), "done before: not done twice");
    assert_eq!(k.get(&admin, "Case", three[2], false).await.unwrap()["status"], json!("Open"));

    // Rights are what they are when it runs: someone who left gets nothing done.
    let job = k.bulk_job(&peer, "Case", Some(vec![cases[1]]), None, &reopen).await.unwrap();
    k.deprovision_user(&admin, peer.user, None, "left").await.unwrap();
    k.run_jobs_once(1).await.unwrap();
    let v = w.view(job).await;
    assert_eq!((v["status"].as_str(), v["attempts"].as_i64()), (Some("failed"), Some(1)), "{v}");
    assert!(v["last_error"].as_str().unwrap().contains("no longer"), "{v}");
    assert_eq!(k.get(&admin, "Case", cases[1], false).await.unwrap()["status"], json!("Closed"));

    // A request's bulk operation queues its events behind others' too.
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update outbox set processed_at = now()").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    k.bulk(&admin, "Case", BulkTarget::Ids(vec![cases[1]]), &reopen).await.unwrap();
    k.update(&admin, "Case", cases[2], obj(json!({"title": "edited by hand"})), None).await.unwrap();
    let mut tx = k.db.system_tx().await.unwrap();
    let queued: Vec<(Uuid, i16)> =
        sqlx::query_as("select record_id, priority from outbox where processed_at is null and topic = 'Case.updated'").fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(queued.contains(&(cases[1], 2)), "{queued:?}");
    assert!(queued.contains(&(cases[2], 1)), "{queued:?}");

    // Mail goes first, unless it is a send to a whole list.
    let mailed = |tx_tenant: Uuid| {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(tx_tenant).await.unwrap();
            k.queue_mail(&mut tx, tx_tenant, "someone@acme.test", "Your link", "<p>Here</p>").await.unwrap();
            let p: i16 = sqlx::query_scalar("select priority from outbox order by id desc limit 1").fetch_one(&mut *tx).await.unwrap();
            tx.rollback().await.unwrap();
            p
        }
    };
    assert_eq!(mailed(tenant).await, 0, "someone is waiting for this");
    assert_eq!(Trace::at_priority(ferrum_kernel::outbox::PRIORITY_BULK).scope(mailed(tenant)).await, 2, "nobody is waiting for this");

    // ───────────── an import: each row its own record, at bulk priority ─────────────
    let rows = [json!({"title": "imported-a"}), json!({"status": "Open"}), json!("not a record")];
    assert!(matches!(k.import_rows(&clerk, "Case", &rows).await, Err(Error::Forbidden(_))), "importing is a permission of its own");
    assert!(matches!(k.import_rows(&admin, "Case", &vec![json!({}); ferrum_kernel::bulk::MAX_IMPORT + 1]).await, Err(Error::Invalid(_))));
    let (made, failed) = k.import_rows(&admin, "Case", &rows).await.unwrap();
    assert_eq!(made.len(), 1);
    assert_eq!((&failed[0]["row"], &failed[0]["error"]), (&json!(1), &json!("validation_failed")), "a title is required: {failed:?}");
    assert!(failed[0]["fields"].as_array().is_some_and(|f| !f.is_empty()));
    assert_eq!((&failed[1]["row"], &failed[1]["message"]), (&json!(2), &json!("not an object")));
    let id: Uuid = made[0].as_str().unwrap().parse().unwrap();
    let mut tx = k.db.system_tx().await.unwrap();
    let priorities: Vec<i16> = sqlx::query_scalar("select priority from outbox where record_id = $1").bind(id).fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(
        !priorities.is_empty() && priorities.iter().all(|p| *p == ferrum_kernel::outbox::PRIORITY_BULK),
        "what an import sets off waits behind people's single edits: {priorities:?}"
    );

    // ───────────── the workspace's export ─────────────
    assert!(matches!(k.export_workspace(&clerk).await, Err(Error::Forbidden(_))), "administrators only");
    let attached = k.store_file(&admin, Some(("Case", cases[0])), "note.txt", "text/plain", b"a file of the case").await.unwrap();
    let attached = attached["id"].as_str().unwrap().to_string();
    // A file whose object storage has lost is named in the export, which goes on without it.
    let lost = k.store_file(&admin, Some(("Case", cases[1])), "lost.txt", "text/plain", b"gone from storage").await.unwrap();
    let lost_id = lost["id"].as_str().unwrap().to_string();
    let lost_key: String = {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        let key = sqlx::query_scalar("select storage_key from file_object where id = $1::uuid").bind(&lost_id).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        key
    };
    files.0.lock().unwrap().remove(&lost_key);
    let mut tx = k.db.system_tenant_tx(tenant).await.unwrap();
    sqlx::query("insert into case_log (tenant_id, what) values ($1, 'first posting'), ($1, 'second posting')").bind(tenant).execute(&mut *tx).await.unwrap();
    sqlx::query("insert into case_keys (tenant_id, api_key) values ($1, 'engine-held-secret')").bind(tenant).execute(&mut *tx).await.unwrap();
    sqlx::query("insert into case_log (tenant_id, what) values ($1, 'another workspace posting')").bind(other).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    k.insert(&outsider, "Case", obj(json!({"title": "their case"}))).await.unwrap();

    let job = k.export_workspace(&admin).await.expect("queued");
    assert_eq!(k.export_workspace(&admin).await.unwrap(), job, "one at a time");
    assert_eq!(k.run_jobs_once(1).await.unwrap(), 1);
    let v = w.view(job).await;
    assert_eq!(v["status"], json!("done"), "{v}");
    let result = &v["result"];
    assert_eq!(result["parts"].as_array().unwrap().len(), 1);
    let part: Uuid = result["parts"][0]["id"].as_str().unwrap().parse().unwrap();
    assert!(matches!(k.export_part(&clerk, job, part).await, Err(Error::Forbidden(_))));
    assert!(matches!(k.export_part(&outsider, job, part).await, Err(Error::Forbidden(_) | Error::NotFound(_))));
    assert!(matches!(k.export_part(&admin, Uuid::now_v7(), part).await, Err(Error::NotFound(_))));
    assert!(k.read_file(&admin, part).await.is_err(), "the ordinary file route serves it to nobody");
    let (name, bytes) = k.export_part(&admin, job, part).await.expect("an administrator's copy");
    assert!(name.ends_with(".zip"));

    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).expect("a zip");
    let mut contents = HashMap::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).unwrap();
        let mut out = Vec::new();
        f.read_to_end(&mut out).unwrap();
        contents.insert(f.name().unwrap().to_string(), out);
    }
    let text = |name: &str| String::from_utf8(contents.get(name).unwrap_or_else(|| panic!("{name} is missing")).clone()).unwrap();
    let manifest: Value = serde_json::from_str(&text("manifest.json")).unwrap();
    assert_eq!(manifest["workspace"]["slug"], json!("acme"));
    assert!(manifest["packages"].as_array().unwrap().contains(&json!("cases")));
    assert!(manifest["entities"].as_array().unwrap().contains(&json!({"entity": "Case", "records": 620})), "{manifest}");
    assert!(manifest["entities"].as_array().unwrap().contains(&json!({"table": "case_log", "records": 2})), "{manifest}");
    assert!(manifest["tables_left_out"].as_array().unwrap().contains(&json!("case_keys")), "a table nobody declared is named, not silently lost: {manifest}");
    assert_eq!(
        (manifest["files"].clone(), manifest["files_missing"][0]["file_name"].clone(), result["files_missing"].clone()),
        (json!(1), json!("lost.txt"), json!(1))
    );
    assert!(!contents.contains_key(&format!("files/{lost_id}")) && !text("files/index.jsonl").contains("lost.txt"));
    let records: String = contents.keys().filter(|n| n.starts_with("records/Case/")).map(|n| text(n)).collect();
    assert_eq!(records.lines().count(), 620);
    assert!(records.contains("case-0619") && records.contains("row of 619"), "whole records, child rows too");
    let log = text("tables/case_log/000000.jsonl");
    assert!(log.contains("first posting") && !log.contains("another workspace"), "{log}");
    assert_eq!(contents.get(&format!("files/{attached}")).map(Vec::as_slice), Some(&b"a file of the case"[..]));
    assert!(text("files/index.jsonl").contains("note.txt"));
    assert!(text("config/people.jsonl").contains("clerk@acme.test"));
    assert!(text("config/permissions.jsonl").contains("Clerk"));
    assert!(contents.keys().any(|n| n.starts_with("audit/")), "the audit trail");
    let all = contents.values().map(|b| String::from_utf8_lossy(b).to_string()).collect::<Vec<_>>().join("\n");
    for secret in ["s3cret-case-token", "engine-held-secret", "$argon2", "password_hash", "totp_secret", "their case", "other.test"] {
        assert!(!all.contains(secret), "{secret} must not be in the export");
    }
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let audited: i64 = sqlx::query_scalar("select count(*) from audit_event where tenant_id = $1 and action in ('export', 'export_download')")
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(audited, 3, "who asked, twice, and who took a copy, are on the record");

    // An administrator demoted before it runs gets no export.
    let second_admin = k.create_user(&admin, "deputy@acme.test", "Deputy", None, &[], true).await.unwrap();
    let deputy = k.load_ctx(tenant, second_admin, None, "it".into()).await.unwrap();
    let denied = k.export_workspace(&deputy).await.unwrap();
    k.deprovision_user(&admin, second_admin, None, "left").await.unwrap();
    k.run_jobs_once(1).await.unwrap();
    assert_eq!(w.view(denied).await["status"], json!("failed"));

    // Another export does not hold the first.
    let next = k.export_workspace(&admin).await.unwrap();
    k.run_jobs_once(1).await.unwrap();
    let v = w.view(next).await;
    assert_eq!(v["result"]["files"], json!(1), "exports are not files of the workspace: {v}");

    // An erasure lands while an export is made (here, as its part is stored): it may hold what was
    // erased, so it goes and is made again, at no cost to its attempts.
    w.sql(
        "create function erase_meanwhile() returns trigger language plpgsql as $$ begin \
         insert into outbox (tenant_id, topic, payload) values (new.tenant_id, 'ferrum.privacy.erased', '{}'); return new; end $$",
    )
    .await;
    w.sql("create trigger erase_meanwhile after insert on file_object for each row when (new.entity = 'ferrum.export') execute function erase_meanwhile()")
        .await;
    let again = k.export_workspace(&admin).await.unwrap();
    k.run_jobs_once(1).await.unwrap();
    let v = w.view(again).await;
    assert_eq!((&v["status"], &v["attempts"]), (&json!("queued"), &json!(0)), "{v}");
    assert!(v["last_error"].as_str().unwrap().contains("erasure"), "{v}");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let stored: i64 = sqlx::query_scalar("select count(*) from file_object where record_id = $1").bind(again).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(stored, 0, "what it had stored went");
    w.sql("drop trigger erase_meanwhile on file_object").await;
    k.run_jobs_once(1).await.unwrap();
    let v = w.view(again).await;
    assert_eq!((&v["status"], v["result"]["parts"].as_array().map(Vec::len)), (&json!("done"), Some(1)), "{v}");

    // ───────────── nothing stays for ever ─────────────
    let key: String = {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        let key = sqlx::query_scalar("select storage_key from file_object where tenant_id = $1 and id = $2")
            .bind(tenant)
            .bind(part)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        key
    };
    assert!(files.0.lock().unwrap().contains_key(&key));
    w.sql("update file_object set created_at = now() - interval '8 days' where entity = 'ferrum.export'").await;
    w.sql("update job set finished_at = now() - interval '31 days' where kind = 'test.scripted'").await;
    k.maintenance().await.unwrap();
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    assert!(!files.0.lock().unwrap().contains_key(&key), "the object goes with its row");
    assert!(matches!(k.export_part(&admin, job, part).await, Err(Error::NotFound(_))));
    assert!(matches!(k.job(&admin, mine).await, Err(Error::NotFound(_))), "old jobs are forgotten");
    drop(quiet);
}
