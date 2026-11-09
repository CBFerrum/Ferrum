//! Receipts for writes sent again (FEO-003), against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the
//! database is reset first): a change that adds a row, sent again after its answer was lost, adds it once,
//! including when the client rebuilds its body on the record as it stands; an action runs once; a key is
//! its person's and its request's alone; one still in hand is retryable; receipts last thirty days.

use async_trait::async_trait;
use ferrum_kernel::receipt::digest;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, DocHook, Error, HookCtx, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;

const PACKAGE: &str = "package: jobs\nversion: 0.1.0\ndepends: [core]\nroles:\n  - name: Technician\n    permissions:\n      - {entity: Job, read: true, write: true, create: true}\n";

const ENTITIES: &str = r#"
- name: Job
  label: Job
  kind: master
  title_field: reference
  fields:
    - {name: reference, type: data, required: true}
    - {name: status, type: data}
    - {name: parts, type: table, of: JobPart}
  actions:
    - {name: finish, label: Finish, permission: write}
- name: JobPart
  label: Job Part
  kind: child
  fields:
    - {name: item, type: data}
    - {name: qty, type: int}
"#;

/// The owning app: finishing a job, and how often it really ran.
#[derive(Default)]
struct Jobs {
    finished: AtomicUsize,
}

#[async_trait]
impl DocHook for Jobs {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "finish" {
            return Ok(None);
        }
        let n = self.finished.fetch_add(1, Ordering::SeqCst) + 1;
        let id: Uuid = doc["id"].as_str().unwrap().parse().unwrap();
        sqlx::query("update e_job set status = 'Finished' where id = $1").bind(id).execute(&mut **hc.tx).await?;
        Ok(Some(json!({"finished": true, "time": n})))
    }
}

#[test]
fn a_write_sent_again_is_done_once() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

async fn parts(k: &Kernel, ctx: &Ctx, job: Uuid) -> usize {
    k.get(ctx, "Job", job, false).await.unwrap()["parts"].as_array().map(Vec::len).unwrap_or(0)
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/jobs.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let pkg = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let jobs = Arc::new(Jobs::default());
    let mut hooks = HookRegistry::default();
    hooks.register("Job", jobs.clone());
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, pkg]).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["jobs".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let person = |email: &'static str| {
        let (k, admin) = (k.clone(), admin.clone());
        async move {
            let id = k.create_user(&admin, email, email, Some("Correct-Horse-9"), &["Technician".to_string()], false).await.unwrap();
            k.load_ctx(tenant, id, None, "it".into()).await.unwrap()
        }
    };
    let ann = person("ann@acme.test").await;
    let bob = person("bob@acme.test").await;
    let job: Uuid = k.insert(&ann, "Job", obj(json!({"reference": "WO-1"}))).await.unwrap()["id"].as_str().unwrap().parse().unwrap();
    let target = format!("PATCH Job/{job}");

    // ── a row added, its answer lost after the commit, and sent again: added once ──
    let add = obj(json!({"parts": [{"item": "Filter", "qty": 1}]}));
    let d = digest(&target, None, &Value::Object(add.clone()));
    let (first, again) = k.update_idempotent(&ann, "Job", job, add.clone(), None, "wo-1.save.1", &d).await.unwrap();
    assert!(!again);
    let (second, again) = k.update_idempotent(&ann, "Job", job, add.clone(), None, "wo-1.save.1", &d).await.unwrap();
    assert!(again, "the second answer is the first, given again");
    assert_eq!(first, second, "both callers get the same receipt");
    assert_eq!(parts(&k, &ann, job).await, 1, "one business effect");

    // ── the client rebuilds the change on the record as it stands, with its own digest of the operation ──
    let op = "add Filter x1 as new-1";
    let rebuilt = |rows: Value| obj(json!({"parts": rows}));
    let now = k.get(&ann, "Job", job, false).await.unwrap();
    let keep_ids: Vec<Value> = now["parts"].as_array().unwrap().iter().map(|r| json!({"id": r["id"]})).collect();
    let with_belt = |mut rows: Vec<Value>| {
        rows.push(json!({"item": "Belt", "qty": 2}));
        rebuilt(Value::Array(rows))
    };
    let body1 = with_belt(keep_ids);
    let d1 = digest(&target, Some(op), &Value::Object(body1.clone()));
    k.update_idempotent(&ann, "Job", job, body1, None, "wo-1.save.2", &d1).await.unwrap();
    assert_eq!(parts(&k, &ann, job).await, 2);
    // Sent again from a reloaded page: its body now holds the row it added, and adds it again; its digest
    // is of the same operation, so the receipt answers and nothing is added.
    let now = k.get(&ann, "Job", job, false).await.unwrap();
    let all: Vec<Value> = now["parts"].as_array().unwrap().iter().map(|r| json!({"id": r["id"]})).collect();
    let body2 = with_belt(all);
    let d2 = digest(&target, Some(op), &Value::Object(body2.clone()));
    assert_eq!(d1, d2, "the operation's digest, whatever the body");
    let (_, again) = k.update_idempotent(&ann, "Job", job, body2, None, "wo-1.save.2", &d2).await.unwrap();
    assert!(again);
    assert_eq!(parts(&k, &ann, job).await, 2, "not added twice");

    // ── a key is its request's, and its person's ──
    let other = digest(&target, Some("set the status"), &json!({}));
    assert!(matches!(k.update_idempotent(&ann, "Job", job, obj(json!({"status": "Open"})), None, "wo-1.save.2", &other).await, Err(Error::Conflict(_))));
    assert!(matches!(k.update_idempotent(&bob, "Job", job, add.clone(), None, "wo-1.save.1", &d).await, Err(Error::Conflict(_))), "someone else's key");
    assert!(matches!(k.update_idempotent(&ann, "Job", job, add.clone(), None, "short", &d).await, Err(Error::Validation(_) | Error::Invalid(_))));

    // ── an action runs once, and its answer comes back ──
    let fin = digest(&format!("POST Job/{job}/actions/finish"), None, &json!({}));
    let (a1, again) = k.run_action_idempotent(&ann, "Job", job, "finish", json!({}), "wo-1.finish.1", &fin).await.unwrap();
    assert!(!again);
    let (a2, again) = k.run_action_idempotent(&ann, "Job", job, "finish", json!({}), "wo-1.finish.1", &fin).await.unwrap();
    assert!(again && a1 == a2, "{a1} {a2}");
    assert_eq!(jobs.finished.load(Ordering::SeqCst), 1, "the app's handler ran once");

    // ── a request still in hand when the same key comes again: retryable, not refused ──
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("insert into idempotency_key (tenant_id, key, request_hash, actor) values ($1, 'wo-1.save.3', $2, $3)")
        .bind(tenant)
        .bind(&d)
        .bind(ann.user)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(k.update_idempotent(&ann, "Job", job, add.clone(), None, "wo-1.save.3", &d).await, Err(Error::Retryable(_))));

    // ── kept for thirty days, so a device back online after days still finds its receipt ──
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update idempotency_key set created_at = now() - interval '29 days' where key = 'wo-1.save.1'").execute(&mut *tx).await.unwrap();
    sqlx::query("update idempotency_key set created_at = now() - interval '31 days' where key = 'wo-1.save.2'").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    k.purge_old_events(30).await.unwrap();
    let mut tx = k.db.system_tx().await.unwrap();
    let left: Vec<String> = sqlx::query_scalar("select key from idempotency_key where key like 'wo-1.save.%' order by key").fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(left, ["wo-1.save.1", "wo-1.save.3"], "a receipt older than thirty days goes");
    let (_, again) = k.update_idempotent(&ann, "Job", job, add.clone(), None, "wo-1.save.1", &d).await.unwrap();
    assert!(again, "after 29 days, the same answer");
    assert_eq!(parts(&k, &ann, job).await, 2);
}
