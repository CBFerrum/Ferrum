//! FLC-PROC on the real runtime: Ferrum's workflow on a Temporal server (`TEMPORAL_ADDRESS`, default
//! http://127.0.0.1:7233, namespace `TEMPORAL_NAMESPACE` or ferrum-flc) and PostgreSQL
//! (`FERRUM_PROCESS_TEST_DATABASE_URL`, reset first). Without a Temporal server it says so and runs nothing.
//!
//! Every run continues as new after each event it handles, so each scenario also crosses continuations.

use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings};
use ferrum_logic::calendar::Calendar;
use ferrum_meta::Catalog;
use ferrum_process::{Dispatch, ProcessWf, Settings, replay, spawn};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use temporalio_client::WorkflowFetchHistoryOptions;
use uuid::Uuid;

/// Something only the first run's data holds.
const MARKER: &str = "Sealed-Marker-R1";
const PACKAGE: &str = "package: procs\nversion: 1.0.0\ndepends: [core]\nroles:\n  - name: Requester\n    permissions:\n      - {entity: Request, read: true, write: true, create: true, submit: true, cancel: true}\n      - {entity: Order, read: true, write: true, create: true}\n  - name: Reviewer\n    permissions:\n      - {entity: Request, read: true}\n      - {entity: Order, read: true}\n";
const ENTITIES: &str = r#"
- name: Request
  label: Request
  kind: document
  submittable: true
  title_field: reference
  fields:
    - {name: reference, type: data, required: true}
    - {name: total, type: decimal}
- name: Order
  label: Order
  kind: master
  title_field: reference
  fields:
    - {name: reference, type: data, required: true}
    - {name: request, type: link, to: Request}
    - {name: status, type: data}
"#;

/// The kernel test's solution; `timeout` is the review's.
fn source(timeout: Value) -> String {
    let cap = |id: &str| json!({"id": id, "version": "1.0.0"});
    json!({
        "schema_version": "ferrum.solution/1", "kind": "Solution", "id": "acme.requests", "version": "1.0.0", "label": "Requests",
        "requires": [cap("procs.request.read"), cap("procs.order.create")],
        "processes": [{"id": "approve_request", "label": "Approve a request", "trigger": {"event": "procs.request.submitted", "business_key": "trigger.id"},
            "entry": "snapshot", "max_steps": 50, "nodes": [
            {"type": "query", "id": "snapshot", "label": "Read it", "capability": cap("procs.request.read"), "input": {"id": "trigger.id"}, "next": "review", "on_error": "attention"},
            {"type": "human_task", "id": "review", "label": "Approve the request", "role": "Reviewer", "material": "snapshot.output", "timeout": timeout,
             "next": "order", "on_reject": "refused", "on_timeout": "attention"},
            {"type": "action", "id": "order", "label": "Order it", "capability": cap("procs.order.create"), "input": {"values.reference": "trigger.id", "values.request": "trigger.id"},
             "approval": "review", "next": "delivered", "on_error": "attention", "on_unknown": "attention"},
            {"type": "wait_event", "id": "delivered", "label": "Delivered", "event": "procs.order.updated", "correlation": "order.output.id", "timeout": "P7D", "next": "completed", "on_timeout": "attention"},
            {"type": "end", "id": "completed", "label": "Done", "outcome": "completed", "output": {"order": "order.output.id"}},
            {"type": "end", "id": "refused", "label": "Refused", "outcome": "refused"},
            {"type": "suspend", "id": "attention", "label": "Needs attention", "reason": "Look into it before anything more is done."}
        ]}]
    })
    .to_string()
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

/// The encoding of every payload in a history (`{"metadata": {"encoding": …}, "data": …}`), as base64.
fn payload_encodings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            if let (Some(Value::String(e)), Some(_)) = (m.get("metadata").and_then(|x| x.get("encoding")), m.get("data")) {
                out.push(e.clone());
            }
            m.values().for_each(|x| payload_encodings(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| payload_encodings(x, out)),
        _ => {}
    }
}

async fn workspace(k: &Kernel, slug: &str) -> Ctx {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["procs".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap()
}

async fn user(k: &Kernel, admin: &Ctx, email: &str, roles: &[&str], is_admin: bool) -> Ctx {
    let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
    let id = k.create_user(admin, email, email, Some("Correct-Horse-9"), &roles, is_admin).await.unwrap();
    k.load_ctx(admin.tenant, id, None, "it".into()).await.unwrap()
}

/// A workspace with the solution active, its people, and its calendar.
async fn deployed(k: &Kernel, slug: &str, timeout: Value) -> (Ctx, Ctx, Ctx) {
    let admin = workspace(k, slug).await;
    let admin2 = user(k, &admin, &format!("admin2@{slug}.test"), &[], true).await;
    let requester = user(k, &admin, &format!("requester@{slug}.test"), &["Requester"], false).await;
    let reviewer = user(k, &admin, &format!("reviewer@{slug}.test"), &["Reviewer"], false).await;
    let office =
        Calendar { name: "office".into(), zone: "Asia/Kolkata".into(), weekends: vec!["sat".into(), "sun".into()], holidays: Default::default(), revision: 0 };
    k.set_logic_calendar(&admin, &office).await.unwrap();
    let saved = k.save_release(&admin, &source(timeout)).await.expect("release");
    let release: Uuid = saved["id"].as_str().unwrap().parse().unwrap();
    k.review_release(&admin2, release, saved["artifact_digest"].as_str().unwrap(), true, "").await.unwrap();
    k.activate_release(&admin, release, None).await.unwrap();
    (admin, requester, reviewer)
}

/// Waits for a query to give a row, as people would: the runtime works on its own time.
async fn eventually<T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin>(
    k: &Kernel,
    tenant: Uuid,
    sql: &'static str,
    bind: Uuid,
    what: &str,
) -> T {
    let until = Instant::now() + Duration::from_secs(45);
    loop {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        let v: Option<T> = sqlx::query_scalar(sql).bind(bind).fetch_optional(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        if let Some(v) = v {
            return v;
        }
        assert!(Instant::now() < until, "waited 45 s for {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn request(k: &Kernel, who: &Ctx, reference: &str) -> (Uuid, Uuid) {
    let r = k.insert(who, "Request", obj(json!({"reference": reference, "total": "5000"}))).await.unwrap();
    let id: Uuid = r["id"].as_str().unwrap().parse().unwrap();
    k.submit(who, "Request", id, None).await.unwrap();
    let run: Uuid = eventually(k, who.tenant, "select id from logic_run where business_key = $1::uuid::text", id, "the run to be accepted").await;
    (id, run)
}

async fn review(k: &Kernel, tenant: Uuid, run: Uuid) -> (Uuid, String) {
    let task: Uuid = eventually(k, tenant, "select id from logic_task where run_id = $1 and activation = 'review'", run, "the review to open").await;
    let digest: String = eventually(k, tenant, "select digest from logic_task where id = $1", task, "its digest").await;
    (task, digest)
}

async fn finished(k: &Kernel, tenant: Uuid, run: Uuid) -> String {
    eventually(
        k,
        tenant,
        "select status from logic_run where id = $1 and finished_at is not null or (id = $1 and status = 'needs_attention')",
        run,
        "the run to settle",
    )
    .await
}

#[test]
fn the_runtime_runs_processes_durably() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    // A hang fails here, with what the scenario had reached on stderr, instead of stalling the run.
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(300), tokio::spawn(scenario())).await.expect("the scenario took more than five minutes").unwrap()
    });
}

async fn scenario() {
    let address = std::env::var("TEMPORAL_ADDRESS").unwrap_or_else(|_| "http://127.0.0.1:7233".into());
    let host = address.trim_start_matches("http://").trim_start_matches("https://").to_string();
    let reachable = std::net::ToSocketAddrs::to_socket_addrs(&host.as_str())
        .ok()
        .and_then(|mut a| a.next())
        .is_some_and(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_secs(2)).is_ok());
    if !reachable {
        eprintln!("skipped: no Temporal server at {address}");
        return;
    }
    let url = std::env::var("FERRUM_PROCESS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_process_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 16, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/procs.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let pkg = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let key = Arc::new(SecretKey::new([9u8; 32]));
    let settings = Settings {
        address,
        namespace: std::env::var("TEMPORAL_NAMESPACE").unwrap_or_else(|_| "ferrum-flc".into()),
        queue: format!("ferrum-test-{}", Uuid::now_v7()),
        key: key.clone(),
    };
    let client = ferrum_process::client(&settings).await.expect("temporal client");
    let mut hooks = HookRegistry::default();
    hooks.add_event_sink(Arc::new(Dispatch::new(client.clone(), settings.queue.clone()).continuing_after(1)));
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, pkg]).unwrap(), hooks, KernelSettings { process_runtime: true, ..Default::default() }, None)
        .await
        .expect("boot");
    let worker = spawn(k.clone(), settings.clone()).await.expect("worker");
    let pump = tokio::spawn({
        let k = k.clone();
        async move {
            loop {
                if let Err(e) = k.dispatch_outbox_once().await {
                    eprintln!("outbox: {e}");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    });
    let (admin, requester, reviewer) = deployed(&k, "acme", json!({"business_days": 2, "calendar": "office"})).await;
    let t = admin.tenant;

    // Started by its event, approved by a person, ordered under its effect key, completed by the order's own
    // event: across continuations, on the runtime's own time.
    // The first request's reference is a marker for the sealing check below.
    let (r1, run1) = request(&k, &requester, MARKER).await;
    let (task1, digest1) = review(&k, t, run1).await;
    k.decide_logic_task(&reviewer, task1, true, &digest1, &json!({}), "").await.unwrap();
    let order1: Uuid = eventually(&k, t, "select id from e_order where request = $1", r1, "the order").await;
    // Its wait registers in its own time; the order's event is matched whether it comes before or after.
    k.update(&requester, "Order", order1, obj(json!({"status": "Delivered"})), None).await.unwrap();
    assert_eq!(finished(&k, t, run1).await, "completed");
    let effects: i64 = eventually(&k, t, "select count(*) from logic_effect where effect_key = $1::uuid::text || '/order'", run1, "the receipt").await;
    assert_eq!(effects, 1);

    // Started again, as after a lost answer: the run is not started twice, before or after it ended.
    let again = Dispatch::new(client.clone(), settings.queue.clone());
    again.start(t, run1).await.expect("a repeated start is no error");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(eventually::<i64>(&k, t, "select count(*) from e_order where request = $1", r1, "orders").await, 1);
    assert_eq!(eventually::<String>(&k, t, "select status from logic_run where id = $1", run1, "the run").await, "completed", "no second run reported anything");

    // Its history replays with the same code and codec; without the codec it cannot even be read (FLC-PROC-007).
    // Fetched by a client without the codec, as Temporal keeps it: the marker the run read is nowhere in it.
    let wid = ferrum_kernel::process::workflow_id(t, run1);
    let raw = temporalio_client::Client::new(
        temporalio_client::Connection::connect(temporalio_client::ConnectionOptions::new(url::Url::parse(&settings.address).unwrap()).build()).await.unwrap(),
        temporalio_client::ClientOptions::new(settings.namespace.clone()).build(),
    )
    .unwrap();
    let history = raw.get_workflow_handle::<ProcessWf>(wid).fetch_history(WorkflowFetchHistoryOptions::default()).to_json().await.expect("history");
    // In history JSON a payload's metadata and data are base64: every payload must carry the codec's encoding
    // (`binary/ferrum-sealed`), whose data is AES-GCM output that cannot hold the marker.
    let mut encodings = vec![];
    payload_encodings(&serde_json::from_slice(&history).unwrap(), &mut encodings);
    assert!(encodings.len() > 5, "the history holds the run's payloads: {encodings:?}");
    assert!(encodings.iter().all(|e| e == "YmluYXJ5L2ZlcnJ1bS1zZWFsZWQ="), "every payload Temporal keeps is sealed: {encodings:?}");
    replay(history.clone(), Some(key.clone())).await.expect("replays with its codec");
    assert!(replay(history, None).await.is_err(), "unreadable without the key");

    // The worker goes away while a person decides; another takes over and the run goes on.
    let (r2, run2) = request(&k, &requester, "R-2").await;
    let (task2, digest2) = review(&k, t, run2).await;
    worker.stop();
    worker.stopped().await.expect("stopped cleanly");
    k.decide_logic_task(&reviewer, task2, true, &digest2, &json!({}), "").await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let worker = spawn(k.clone(), settings.clone()).await.expect("another worker");
    let order2: Uuid = eventually(&k, t, "select id from e_order where request = $1", r2, "the order after the worker came back").await;
    k.update(&requester, "Order", order2, obj(json!({"status": "Delivered"})), None).await.unwrap();
    assert_eq!(finished(&k, t, run2).await, "completed");

    // Nobody decides in time: the runtime's timer fires and the database says the deadline came first.
    let (_, fast_requester, _) = deployed(&k, "fast", json!(3)).await;
    let (_, run3) = request(&k, &fast_requester, "F-1").await;
    let _ = review(&k, fast_requester.tenant, run3).await;
    assert_eq!(finished(&k, fast_requester.tenant, run3).await, "needs_attention");
    assert_eq!(
        eventually::<String>(&k, fast_requester.tenant, "select status from logic_task where run_id = $1 and activation = 'review'", run3, "the task").await,
        "timed_out"
    );

    pump.abort();
    worker.stop();
    worker.stopped().await.expect("stopped cleanly");
}
