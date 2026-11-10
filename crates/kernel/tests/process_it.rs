//! FLC-PROC and FLC-HUMAN: durable processes' business records against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; reset first). A small driver here does what the runtime does, starting
//! accepted runs, carrying out what they ask through the kernel's activities and handing them what was
//! delivered, without the runtime's schedule: deadlines are passed by hand. The runtime's own conformance,
//! on Temporal, is `crates/process/tests`.

use ferrum_kernel::files::FileStore;
use ferrum_kernel::outbox::Event as OutboxEvent;
use ferrum_kernel::process::{DELIVER_TOPIC, Migration, START_TOPIC, Waited};
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::support::GrantInput;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, DocHook, Error, HookCtx, HookRegistry, Kernel, KernelSettings};
use ferrum_logic::calendar::Calendar;
use ferrum_logic::compile::Release;
use ferrum_logic::process::{Command, Event, Invoked, Machine, Run};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// Holds the next order being saved inside its transaction until told, as a worker paused in the middle of a step
/// would: its claim on the step's effect is taken and its effect not yet done.
#[derive(Default)]
struct Pause {
    armed: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    go: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl DocHook for Pause {
    fn name(&self) -> &'static str {
        "test.pause"
    }
    async fn before_validate(&self, _: &mut HookCtx<'_>, _: &mut Doc) -> ferrum_kernel::Result<()> {
        if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.entered.notify_one();
            self.go.notified().await;
        }
        Ok(())
    }
}

/// Files kept in memory, as an object store would keep them.
#[derive(Default)]
struct MemFiles(Mutex<BTreeMap<String, Vec<u8>>>);

#[async_trait::async_trait]
impl FileStore for MemFiles {
    async fn put(&self, key: &str, data: &[u8]) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().insert(key.to_string(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> ferrum_kernel::Result<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned().ok_or_else(|| Error::NotFound("object".into()))
    }
    async fn delete(&self, key: &str) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

const PACKAGE: &str = "package: procs\nversion: 1.0.0\ndepends: [core]\nroles:\n  - name: Requester\n    permissions:\n      - {entity: Request, read: true, write: true, create: true, submit: true, cancel: true}\n      - {entity: Order, read: true, write: true, create: true}\n  - name: Reviewer\n    permissions:\n      - {entity: Request, read: true}\n      - {entity: Order, read: true}\npersonal_data:\n  - {entity: Request, match: contact, erase: [contact]}\n";
const ENTITIES: &str = r#"
- name: Request
  label: Request
  kind: document
  submittable: true
  title_field: reference
  fields:
    - {name: reference, type: data, required: true}
    - {name: total, type: decimal}
    - {name: contact, type: data}
    - {name: pin, type: secret}
- name: Order
  label: Order
  kind: master
  title_field: reference
  fields:
    - {name: reference, type: data, required: true}
    - {name: request, type: link, to: Request}
    - {name: status, type: data}
"#;

fn source() -> String {
    let cap = |id: &str| json!({"id": id, "version": "1.0.0"});
    json!({
        "schema_version": "ferrum.solution/1", "kind": "Solution", "id": "acme.requests", "version": "1.0.0", "label": "Requests",
        "requires": [cap("procs.request.read"), cap("procs.order.create")],
        "decisions": [{"id": "needs_review", "hit_policy": "first", "inputs": [{"name": "total", "value": "snapshot.output.total"}],
            "rows": [{"id": "big", "match": {"total": {"gt": "1000"}}, "outcome": "review"}], "default": "proceed"}],
        "processes": [
            {"id": "approve_request", "label": "Approve a request", "trigger": {"event": "procs.request.submitted", "business_key": "trigger.id"},
             "entry": "snapshot", "max_steps": 50, "nodes": [
                {"type": "query", "id": "snapshot", "label": "Read it", "capability": cap("procs.request.read"), "input": {"id": "trigger.id"}, "next": "decide", "on_error": "attention"},
                {"type": "decision", "id": "decide", "label": "Policy", "decision": "needs_review", "routes": [{"outcome": "review", "target": "review"}, {"outcome": "proceed", "target": "review"}], "on_error": "attention"},
                {"type": "human_task", "id": "review", "label": "Approve the request", "role": "Reviewer", "material": "snapshot.output", "delegation": 2,
                 "timeout": {"business_days": 2, "calendar": "office"}, "form": [{"name": "note", "label": "Note", "type": "text"}],
                 "next": "order", "on_reject": "refused", "on_timeout": "attention"},
                {"type": "action", "id": "order", "label": "Order it", "capability": cap("procs.order.create"), "input": {"values.reference": "trigger.id", "values.request": "trigger.id"},
                 "approval": "review", "next": "delivered", "on_error": "attention", "on_unknown": "attention"},
                {"type": "wait_event", "id": "delivered", "label": "Delivered", "event": "procs.order.updated", "correlation": "order.output.id", "timeout": "P7D", "next": "completed", "on_timeout": "attention"},
                {"type": "end", "id": "completed", "label": "Done", "outcome": "completed", "output": {"order": "order.output.id"}},
                {"type": "end", "id": "refused", "label": "Refused", "outcome": "refused"},
                {"type": "suspend", "id": "attention", "label": "Needs attention", "reason": "Look into it before anything more is done."}
             ]},
            {"id": "investigate", "label": "Investigate a cancellation", "trigger": {"event": "procs.request.cancelled", "business_key": "trigger.id"},
             "entry": "case", "max_steps": 20, "nodes": [
                {"type": "case", "id": "case", "label": "Look into it", "owner": "Reviewer", "timeout": "P7D", "tasks": [
                    {"id": "inspect", "label": "Inspect", "role": "Reviewer", "required": true},
                    {"id": "report", "label": "Report", "role": "Reviewer", "requires": ["inspect"]}
                ], "next": "closed", "on_timeout": "dropped"},
                {"type": "end", "id": "closed", "label": "Closed", "outcome": "completed"},
                {"type": "end", "id": "dropped", "label": "Dropped", "outcome": "refused"}
             ]}
        ]
    })
    .to_string()
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn code(e: &Error) -> &'static str {
    e.code()
}

/// What a runtime does, without its schedule.
struct Driver {
    k: Arc<Kernel>,
    tenant: Uuid,
    last: i64,
    runs: BTreeMap<Uuid, (Release, Run)>,
    /// Waits held back, to let an event come before the run waits for it.
    hold_waits: bool,
    held: Vec<(Uuid, Command)>,
    /// Steps that answered "try again later", as the runtime retries them.
    retrying: Vec<(Uuid, Command)>,
    /// Steps whose answer is lost after they ran, as when a worker dies between the effect and its report: the
    /// run is told the outcome is unknown.
    lose_answers: Vec<String>,
}

impl Driver {
    async fn settle(&mut self) {
        for _ in 0..40 {
            let n = self.k.dispatch_outbox_once().await.unwrap();
            let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
            let rows: Vec<(i64, String, Value)> =
                sqlx::query_as("select id, topic, payload from outbox where tenant_id = $1 and topic in ($2, $3) and id > $4 order by id")
                    .bind(self.tenant)
                    .bind(START_TOPIC)
                    .bind(DELIVER_TOPIC)
                    .bind(self.last)
                    .fetch_all(&mut *tx)
                    .await
                    .unwrap();
            tx.commit().await.unwrap();
            if n == 0 && rows.is_empty() {
                return;
            }
            for (id, topic, payload) in rows {
                self.last = id;
                let run: Uuid = payload["run"].as_str().unwrap().parse().unwrap();
                if topic == START_TOPIC {
                    self.start(run).await;
                } else {
                    self.feed(run, serde_json::from_value(payload["event"].clone()).unwrap()).await;
                }
            }
        }
        panic!("the runs never settled");
    }

    /// Tries again the steps that asked to be, as the runtime would.
    async fn retry(&mut self) {
        for (run, c) in std::mem::take(&mut self.retrying) {
            self.apply(run, vec![c]).await;
        }
    }

    async fn start(&mut self, run: Uuid) {
        if self.runs.contains_key(&run) {
            return;
        }
        let l = self.k.process_load(self.tenant, run).await.unwrap();
        let (state, cmds) = Machine::new(&l.release).start(&l.process, &run.to_string(), l.trigger.clone(), l.requester.map(|u| u.to_string())).unwrap();
        self.runs.insert(run, (l.release, state));
        self.apply(run, cmds).await;
    }

    async fn feed(&mut self, run: Uuid, event: Event) {
        // A move the runtime carries out as the workflow does: on the run as it holds it.
        if let Event::Migrated { release, map, by } = &event {
            let (old, state) = self.runs.remove(&run).unwrap();
            match self.k.process_migrate(self.tenant, run, release.parse().unwrap(), map, by, &state).await.unwrap() {
                Migration::Moved { state: moved, .. } => {
                    let l = self.k.process_load(self.tenant, run).await.unwrap();
                    self.runs.insert(run, (l.release, moved));
                }
                Migration::Refused { .. } => {
                    self.runs.insert(run, (old, state));
                }
            }
            return;
        }
        let (release, mut state) = self.runs.remove(&run).unwrap();
        let cmds = Machine::new(&release).on(&mut state, event);
        self.runs.insert(run, (release, state));
        self.apply(run, cmds).await;
    }

    async fn apply(&mut self, run: Uuid, cmds: Vec<Command>) {
        let mut queue: VecDeque<Command> = cmds.into();
        while let Some(c) = queue.pop_front() {
            for e in self.exec(run, c).await {
                let (release, mut state) = self.runs.remove(&run).unwrap();
                queue.extend(Machine::new(&release).on(&mut state, e));
                self.runs.insert(run, (release, state));
            }
        }
        self.k.process_report(self.tenant, run, &self.runs[&run].1).await.unwrap();
    }

    async fn exec(&mut self, run: Uuid, c: Command) -> Vec<Event> {
        let (k, t) = (self.k.clone(), self.tenant);
        match c {
            Command::Invoke { activation, capability, version, input, changes, approval } => {
                match k.process_invoke(t, run, &activation, &capability, &version, input.clone(), changes, approval.as_ref()).await {
                    Ok(_) if self.lose_answers.contains(&activation) => {
                        vec![Event::Invoked { activation, outcome: Invoked::Unknown { why: "the answer was lost after the step ran".into() } }]
                    }
                    Ok(outcome) => vec![Event::Invoked { activation, outcome }],
                    Err(Error::Unavailable(_)) => {
                        self.retrying.push((run, Command::Invoke { activation, capability, version, input, changes, approval }));
                        vec![]
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            Command::OpenTask { activation, label, role, material, digest, form, approvals, delegation, exclude, timeout } => {
                k.process_open_task(
                    t,
                    run,
                    &activation,
                    "task",
                    &label,
                    &role,
                    &material,
                    &digest,
                    &json!(form),
                    approvals,
                    delegation,
                    &exclude,
                    &json!({}),
                    Some(&timeout),
                )
                .await
                .unwrap();
                vec![]
            }
            Command::Wait { activation, event, correlation, timeout } => {
                if self.hold_waits {
                    self.held.push((run, Command::Wait { activation, event, correlation, timeout }));
                    return vec![];
                }
                match k.process_register_wait(t, run, &activation, &event, &correlation, &timeout).await.unwrap() {
                    Waited::Matched(payload) => vec![Event::Signal { activation, payload }],
                    Waited::Until(_) => vec![],
                }
            }
            Command::OpenCase { activation, label, owner, tasks, exclude, timeout } => {
                k.process_open_task(
                    t,
                    run,
                    &activation,
                    "case",
                    &label,
                    &owner,
                    &json!({}),
                    "",
                    &json!([]),
                    1,
                    0,
                    &exclude,
                    &json!({"tasks": tasks}),
                    Some(&timeout),
                )
                .await
                .unwrap();
                vec![]
            }
            Command::Cancel { activation } => {
                k.process_cancel(t, run, &activation).await.unwrap();
                vec![]
            }
            Command::Attention { activation, why, evidence } => {
                k.process_open_task(
                    t,
                    run,
                    &activation,
                    "attention",
                    &why,
                    "",
                    &json!({}),
                    "",
                    &json!([]),
                    1,
                    0,
                    &[],
                    &json!({"why": why, "evidence": evidence}),
                    None,
                )
                .await
                .unwrap();
                vec![]
            }
            Command::Suspend { activation, reason, resumable } => {
                k.process_open_task(
                    t,
                    run,
                    &activation,
                    "recovery",
                    &reason,
                    "",
                    &json!({}),
                    "",
                    &json!([]),
                    1,
                    0,
                    &[],
                    &json!({"resumable": resumable, "reason": reason}),
                    None,
                )
                .await
                .unwrap();
                vec![]
            }
            Command::Sleep { .. } | Command::Finish { .. } => vec![],
        }
    }

    async fn release_waits(&mut self) {
        for (run, c) in std::mem::take(&mut self.held) {
            self.hold_waits = false;
            let events = self.exec(run, c).await;
            for e in events {
                self.feed(run, e).await;
            }
        }
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

async fn scalar<T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin>(
    k: &Kernel,
    tenant: Uuid,
    sql: &'static str,
    bind: Uuid,
) -> T {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let v: T = sqlx::query_scalar(sql).bind(bind).fetch_one(&mut *tx).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    tx.commit().await.unwrap();
    v
}

/// A submitted request, and the run it started.
async fn request(k: &Kernel, d: &mut Driver, who: &Ctx, reference: &str, total: &str) -> (Uuid, Uuid) {
    let r = k.insert(who, "Request", obj(json!({"reference": reference, "total": total}))).await.unwrap();
    let id: Uuid = r["id"].as_str().unwrap().parse().unwrap();
    k.submit(who, "Request", id, None).await.unwrap();
    d.settle().await;
    let run: Uuid = scalar(k, d.tenant, "select id from logic_run where business_key = $1::uuid::text", id).await;
    (id, run)
}

async fn task_of(k: &Kernel, tenant: Uuid, run: Uuid, activation: &str) -> (Uuid, String) {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let row: (Uuid, String) =
        sqlx::query_as("select id, digest from logic_task where run_id = $1 and activation = $2").bind(run).bind(activation).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    row
}

async fn status(k: &Kernel, tenant: Uuid, run: Uuid) -> String {
    scalar(k, tenant, "select status from logic_run where id = $1", run).await
}

/// A draft request changed once, and the outbox event of the change.
async fn updated(k: &Kernel, who: &Ctx, reference: &str) -> OutboxEvent {
    let r = k.insert(who, "Request", obj(json!({"reference": reference, "total": "1"}))).await.unwrap();
    let id: Uuid = r["id"].as_str().unwrap().parse().unwrap();
    k.update(who, "Request", id, obj(json!({"total": "2"})), None).await.unwrap();
    let mut tx = k.db.tenant_tx(who.tenant).await.unwrap();
    let e: (i64, String, Option<String>, Option<Uuid>, Value) =
        sqlx::query_as("select id, topic, entity, record_id, payload from outbox where topic = 'Request.updated' and record_id = $1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    OutboxEvent { id: e.0, tenant_id: who.tenant, topic: e.1, entity: e.2, record_id: e.3, payload: e.4, ..Default::default() }
}

/// What became of an event of an automation's: its record of the event, and the runs it started.
async fn outcome(k: &Kernel, tenant: Uuid, automation: Uuid, event: i64) -> (Option<String>, i64) {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let status: Option<String> = sqlx::query_scalar("select status from automation_run where automation_id = $1 and outbox_id = $2")
        .bind(automation)
        .bind(event)
        .fetch_optional(&mut *tx)
        .await
        .unwrap();
    let runs: i64 = sqlx::query_scalar("select count(*) from logic_run where trigger_event = $1").bind(event).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    (status, runs)
}

#[test]
fn processes_keep_their_business_records() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 12, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/procs.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let pkg = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let catalog = Catalog::from_packages(vec![core, pkg]).unwrap();
    let settings = KernelSettings { process_runtime: true, secret_key: Some(SecretKey::new([42u8; 32])), ..Default::default() };
    let pause = Arc::new(Pause::default());
    let mut hooks = HookRegistry::default();
    hooks.register("Order", pause.clone());
    let k = Kernel::boot(db, catalog.clone(), hooks, settings.clone(), None).await.expect("boot");
    k.set_file_store(Arc::new(MemFiles::default()));
    let admin = workspace(&k, "acme").await;
    let admin2 = user(&k, &admin, "admin2@acme.test", &[], true).await;
    // The requester also holds the reviewer's role: still not theirs to decide.
    let requester = user(&k, &admin, "requester@acme.test", &["Requester", "Reviewer"], false).await;
    let reviewer = user(&k, &admin, "reviewer@acme.test", &["Reviewer"], false).await;
    let reviewer2 = user(&k, &admin, "reviewer2@acme.test", &["Reviewer"], false).await;
    let reviewer3 = user(&k, &admin, "reviewer3@acme.test", &["Reviewer"], false).await;
    let reviewer4 = user(&k, &admin, "reviewer4@acme.test", &["Reviewer"], false).await;
    let outsider = user(&k, &admin, "outsider@acme.test", &[], false).await;
    let office = Calendar {
        name: "office".into(),
        zone: "Asia/Kolkata".into(),
        weekends: vec!["sat".into(), "sun".into()],
        holidays: Default::default(),
        revision: 0,
        hours: None,
    };
    assert_eq!(k.set_logic_calendar(&admin, &office).await.unwrap(), 1);
    assert_eq!(code(&k.set_logic_calendar(&reviewer, &office).await.unwrap_err()), "forbidden");
    assert_eq!(code(&k.set_logic_calendar(&admin, &Calendar { name: "Office!".into(), ..office.clone() }).await.unwrap_err()), "validation_failed");
    let saved = k.save_release(&admin, &source()).await.expect("release");
    let release: Uuid = saved["id"].as_str().unwrap().parse().unwrap();
    // It may create orders, which nothing running could before: consented to by name.
    k.review_release_consenting(&admin2, release, saved["artifact_digest"].as_str().unwrap(), &["procs.order.create".to_string()], true, "").await.unwrap();
    k.activate_release(&admin, release, None).await.unwrap();
    let mut d =
        Driver { k: k.clone(), tenant: admin.tenant, last: 0, runs: BTreeMap::new(), hold_waits: false, held: vec![], retrying: vec![], lose_answers: vec![] };

    // Accepted once per business key, however often the event comes, at once or again later (FLC-PROC-001).
    let (r1, run1) = request(&k, &mut d, &requester, "R-1", "5000").await;
    let event: (i64, String, Option<String>, Option<Uuid>, Value) = {
        let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
        let e = sqlx::query_as("select id, topic, entity, record_id, payload from outbox where topic = 'Request.submitted' and record_id = $1")
            .bind(r1)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        e
    };
    let ev = OutboxEvent { id: event.0, tenant_id: admin.tenant, topic: event.1, entity: event.2, record_id: event.3, payload: event.4, ..Default::default() };
    let (a, b) = tokio::join!(k.accept_event(&ev), k.accept_event(&ev));
    a.unwrap();
    b.unwrap();
    assert_eq!(scalar::<i64>(&k, admin.tenant, "select count(*) from logic_run where business_key = $1::uuid::text", r1).await, 1);
    assert_eq!(status(&k, admin.tenant, run1).await, "running");

    // The requester may not decide, whatever roles they hold; another task's material is not for others to read
    // (FLC-HUMAN-001, -006).
    let (task1, digest1) = task_of(&k, admin.tenant, run1, "review").await;
    assert!(k.my_logic_tasks(&reviewer).await.unwrap().iter().any(|t| t["id"] == json!(task1)));
    assert!(!k.my_logic_tasks(&requester).await.unwrap().iter().any(|t| t["id"] == json!(task1)));
    assert!(k.my_logic_tasks(&outsider).await.unwrap().is_empty());
    assert_eq!(code(&k.logic_task(&outsider, task1).await.unwrap_err()), "not_found");
    let seen = k.logic_task(&reviewer, task1).await.unwrap();
    assert_eq!((seen["material"]["total"].as_str(), seen["material"]["id"].as_str()), (Some("5000"), Some(r1.to_string().as_str())), "{seen}");
    assert_eq!(seen["deadline_basis"]["calendar"], json!("office"));
    assert_eq!(code(&k.decide_logic_task(&requester, task1, true, &digest1, &json!({}), "").await.unwrap_err()), "forbidden");
    assert_eq!(code(&k.decide_logic_task(&admin2, task1, true, &digest1, &json!({}), "").await.unwrap_err()), "forbidden", "an administrator without the role");
    assert_eq!(code(&k.decide_logic_task(&reviewer, task1, true, "sha256:other", &json!({}), "").await.unwrap_err()), "stale_approval");
    assert_eq!(code(&k.decide_logic_task(&reviewer, task1, true, &digest1, &json!({"colour": "red"}), "").await.unwrap_err()), "validation_failed");

    // Approved, ordered under its effect key, and completed when the order's own event comes.
    k.decide_logic_task(&reviewer, task1, true, &digest1, &json!({"note": "fine"}), "").await.unwrap();
    // Its deadline passing afterwards tells the run the decision, not a timeout.
    let after = k.process_expire_task(admin.tenant, run1, "review").await.unwrap();
    assert!(matches!(&after, Some(Event::TaskDecided { approved: true, .. })), "{after:?}");
    d.settle().await;
    let order1: Uuid = scalar(&k, admin.tenant, "select id from e_order where request = $1", r1).await;
    assert_eq!(scalar::<i64>(&k, admin.tenant, "select count(*) from logic_effect where effect_key = $1::uuid::text || '/order'", run1).await, 1);
    // The order accepted is not the business done: the run waits for the delivery (FLC-CAP-006).
    assert_eq!(status(&k, admin.tenant, run1).await, "running", "accepted, and waiting for the delivery");
    k.update(&requester, "Order", order1, obj(json!({"status": "Delivered"})), None).await.unwrap();
    d.settle().await;
    assert_eq!(status(&k, admin.tenant, run1).await, "completed");
    assert_eq!(scalar::<Value>(&k, admin.tenant, "select output from logic_run where id = $1", run1).await["order"], json!(order1.to_string()));

    // An effect asked again under its key is not done again; other input under it is refused (FLC-PROC-002, -003).
    let again = k
        .process_invoke(
            admin.tenant,
            run1,
            "order",
            "procs.order.create",
            "1.0.0",
            json!({"values": {"reference": r1.to_string(), "request": r1.to_string()}}),
            true,
            None,
        )
        .await
        .unwrap();
    assert!(matches!(&again, Invoked::Done { output } if output["id"] == json!(order1.to_string())), "{again:?}");
    let other = k
        .process_invoke(
            admin.tenant,
            run1,
            "order",
            "procs.order.create",
            "1.0.0",
            json!({"values": {"reference": "X", "request": r1.to_string()}}),
            true,
            None,
        )
        .await
        .unwrap();
    assert!(matches!(&other, Invoked::Refused { code, .. } if code == "input_changed"), "{other:?}");
    // Held after a restore: an effect waits, to be tried again once a person has released the hold; a read
    // goes ahead (FLC-OPS-004).
    let mut sys = k.db.system_tx().await.unwrap();
    sqlx::query("update platform_incarnation set held_since = now(), held_because = 'a restore was noticed'").execute(&mut *sys).await.unwrap();
    sys.commit().await.unwrap();
    let held = k.process_invoke(admin.tenant, run1, "order_held", "procs.order.create", "1.0.0", json!({"values": {"reference": "HELD"}}), true, None).await;
    assert!(matches!(&held, Err(Error::Unavailable(m)) if m.contains("held after a restore")), "{held:?}");
    let read = k.process_invoke(admin.tenant, run1, "probe_held", "procs.request.read", "1.0.0", json!({"id": r1.to_string()}), false, None).await.unwrap();
    assert!(matches!(read, Invoked::Done { .. }), "a read changes nothing: {read:?}");
    k.release_hold(&admin, "the orders after the restore point were checked against the supplier").await.unwrap();
    let released =
        k.process_invoke(admin.tenant, run1, "order_held", "procs.order.create", "1.0.0", json!({"values": {"reference": "HELD"}}), true, None).await.unwrap();
    assert!(matches!(released, Invoked::Done { .. }), "{released:?}");
    assert_eq!(scalar::<i64>(&k, admin.tenant, "select count(*) from e_order where request = $1", r1).await, 1);
    // A record approved and then deleted no longer holds the approval an effect relies on.
    let gone = k.insert(&admin, "Order", obj(json!({"reference": "GONE"}))).await.unwrap();
    let gone_id: Uuid = gone["id"].as_str().unwrap().parse().unwrap();
    k.delete(&admin, "Order", gone_id, None).await.unwrap();
    let approval = ferrum_logic::process::Approval {
        task: "review".into(),
        decision_id: String::new(),
        digest: digest1.clone(),
        material: json!({"id": gone_id.to_string(), "version": gone["version"]}),
        entity: Some("Order".into()),
    };
    let stale =
        k.process_invoke(admin.tenant, run1, "probe", "procs.request.read", "1.0.0", json!({"id": r1.to_string()}), false, Some(&approval)).await.unwrap();
    assert!(matches!(&stale, Invoked::Refused { code, .. } if code == "stale_approval"), "{stale:?}");

    // An event that comes before its wait is kept for it; an event about another record matches nothing
    // (FLC-PROC-004, -005).
    let (r2, run2) = request(&k, &mut d, &requester, "R-2", "5000").await;
    let (task2, digest2) = task_of(&k, admin.tenant, run2, "review").await;
    k.decide_logic_task(&reviewer, task2, true, &digest2, &json!({}), "").await.unwrap();
    d.hold_waits = true;
    d.settle().await;
    let order2: Uuid = scalar(&k, admin.tenant, "select id from e_order where request = $1", r2).await;
    k.update(&requester, "Order", order1, obj(json!({"status": "Returned"})), None).await.unwrap();
    k.update(&requester, "Order", order2, obj(json!({"status": "Delivered"})), None).await.unwrap();
    d.settle().await;
    assert_eq!(status(&k, admin.tenant, run2).await, "running", "nothing waits yet");
    d.release_waits().await;
    d.settle().await;
    assert_eq!(status(&k, admin.tenant, run2).await, "completed", "its own order's event, kept until it waited");
    assert_eq!(scalar::<Value>(&k, admin.tenant, "select output from logic_run where id = $1", run2).await["order"], json!(order2.to_string()));
    let (r3, run3) = request(&k, &mut d, &requester, "R-3", "5000").await;
    let (task3, digest3) = task_of(&k, admin.tenant, run3, "review").await;
    k.decide_logic_task(&reviewer, task3, true, &digest3, &json!({}), "").await.unwrap();
    d.settle().await;
    k.update(&requester, "Order", order2, obj(json!({"status": "Lost"})), None).await.unwrap();
    d.settle().await;
    assert_eq!(status(&k, admin.tenant, run3).await, "running", "another order's event does not advance it");
    let order3: Uuid = scalar(&k, admin.tenant, "select id from e_order where request = $1", r3).await;
    k.update(&requester, "Order", order3, obj(json!({"status": "Delivered"})), None).await.unwrap();
    d.settle().await;
    assert_eq!(status(&k, admin.tenant, run3).await, "completed");

    // What was approved changed before the effect ran: it is refused and waits for a person; there is no way to
    // call it a success, and the cancellation starts its own investigation (FLC-HUMAN-002, -008, -007).
    let (r4, run4) = request(&k, &mut d, &requester, "R-4", "5000").await;
    let (task4, digest4) = task_of(&k, admin.tenant, run4, "review").await;
    k.decide_logic_task(&reviewer, task4, true, &digest4, &json!({}), "").await.unwrap();
    k.cancel(&requester, "Request", r4, None).await.unwrap();
    d.settle().await;
    assert_eq!(scalar::<i64>(&k, admin.tenant, "select count(*) from e_order where request = $1", r4).await, 0, "nothing ordered on a stale approval");
    assert_eq!(status(&k, admin.tenant, run4).await, "needs_attention");
    let (recovery, _) = task_of(&k, admin.tenant, run4, "attention").await;
    assert_eq!(code(&k.recover_logic_task(&reviewer, recovery, false, "x").await.unwrap_err()), "forbidden");
    assert_eq!(code(&k.recover_logic_task(&admin, recovery, true, "it is fine").await.unwrap_err()), "forbidden", "nowhere to resume: no force-success");
    assert_eq!(code(&k.recover_logic_task(&admin, recovery, false, " ").await.unwrap_err()), "validation_failed");
    k.recover_logic_task(&admin, recovery, false, "the request was withdrawn").await.unwrap();
    d.settle().await;
    assert_eq!(status(&k, admin.tenant, run4).await, "refused");
    let case_run: Uuid = scalar(&k, admin.tenant, "select id from logic_run where process = 'investigate' and business_key = $1::uuid::text", r4).await;
    let (case, _) = task_of(&k, admin.tenant, case_run, "case").await;
    assert_eq!(
        code(&k.case_task_done(&reviewer, case, "approve_payment", &json!({})).await.unwrap_err()),
        "forbidden",
        "an unlisted task needs a new definition"
    );
    assert_eq!(code(&k.case_task_done(&reviewer, case, "report", &json!({})).await.unwrap_err()), "conflict", "report waits for inspect");
    assert_eq!(code(&k.close_case(&reviewer, case).await.unwrap_err()), "conflict", "inspect is required");
    k.case_task_done(&reviewer, case, "inspect", &json!({})).await.unwrap();
    assert_eq!(code(&k.close_case(&outsider, case).await.unwrap_err()), "forbidden");
    assert_eq!(code(&k.close_case(&admin, case).await.unwrap_err()), "forbidden", "an administrator is not its owner");
    assert_eq!(code(&k.close_case(&requester, case).await.unwrap_err()), "forbidden", "whoever asked does not close it");
    k.close_case(&reviewer, case).await.unwrap();
    d.settle().await;
    assert_eq!(status(&k, admin.tenant, case_run).await, "completed");

    // A decision and a timeout at the same moment: one of them, never both (FLC-HUMAN-003).
    let (_, run5) = request(&k, &mut d, &requester, "R-5", "5000").await;
    let (task5, digest5) = task_of(&k, admin.tenant, run5, "review").await;
    let nothing = json!({});
    let (decided, expired) =
        tokio::join!(k.decide_logic_task(&reviewer, task5, true, &digest5, &nothing, ""), k.process_expire_task(admin.tenant, run5, "review"));
    let told = expired.unwrap().expect("the run hears one outcome");
    match (&decided, &told) {
        (Ok(_), Event::TaskDecided { approved: true, .. }) => {}
        (Err(e), Event::TaskTimedOut { .. }) => assert_eq!(code(e), "conflict"),
        other => panic!("both or neither: {other:?}"),
    }
    d.feed(run5, told).await;
    d.settle().await;

    // Handed on within its bound (twice here), never back along its chain nor to who asked; decided by whom it
    // was handed to (FLC-HUMAN-004).
    let (_, run6) = request(&k, &mut d, &requester, "R-6", "5000").await;
    let (task6, digest6) = task_of(&k, admin.tenant, run6, "review").await;
    assert_eq!(code(&k.delegate_logic_task(&reviewer, task6, outsider.user, "").await.unwrap_err()), "forbidden", "not a reviewer");
    assert_eq!(code(&k.delegate_logic_task(&reviewer, task6, requester.user, "").await.unwrap_err()), "forbidden", "who asked");
    k.delegate_logic_task(&reviewer, task6, reviewer2.user, "on leave").await.unwrap();
    assert_eq!(code(&k.decide_logic_task(&reviewer, task6, true, &digest6, &json!({}), "").await.unwrap_err()), "forbidden", "handed on");
    assert_eq!(code(&k.delegate_logic_task(&reviewer2, task6, reviewer.user, "").await.unwrap_err()), "forbidden", "back along the chain");
    k.delegate_logic_task(&reviewer2, task6, reviewer3.user, "away").await.unwrap();
    assert_eq!(code(&k.delegate_logic_task(&reviewer3, task6, reviewer4.user, "").await.unwrap_err()), "forbidden", "past its bound");
    k.decide_logic_task(&reviewer3, task6, true, &digest6, &json!({}), "").await.unwrap();
    let by: Uuid = scalar(&k, admin.tenant, "select by_user from logic_task_decision where task_id = $1 and decision = 'approve'", task6).await;
    assert_eq!(by, reviewer3.user, "attributed to who decided");
    d.settle().await;

    // A revised calendar moves the deadlines resolved after it, never those before (FLC-HUMAN-005).
    let mut revised = office.clone();
    revised.holidays.insert(chrono::Utc::now().date_naive() + chrono::Duration::days(1));
    assert_eq!(k.set_logic_calendar(&admin, &revised).await.unwrap(), 2);
    let (_, run7) = request(&k, &mut d, &requester, "R-7", "5000").await;
    let (task7, _) = task_of(&k, admin.tenant, run7, "review").await;
    assert_eq!(k.logic_task(&reviewer, task7).await.unwrap()["deadline_basis"]["revision"], json!(2));
    assert_eq!(k.logic_task(&reviewer, task1).await.unwrap()["deadline_basis"]["revision"], json!(1));
    // Asked again after its answer was lost, a deadline that passed says so again, not nothing.
    let first = k.process_expire_task(admin.tenant, run7, "review").await.unwrap();
    let again = k.process_expire_task(admin.tenant, run7, "review").await.unwrap();
    assert!(matches!((&first, &again), (Some(Event::TaskTimedOut { .. }), Some(Event::TaskTimedOut { .. }))), "{first:?} then {again:?}");

    // Rights revoked while the run waited: the effect after it is refused, not done with borrowed power
    // (FLC-PROC-006).
    let (r8, run8) = request(&k, &mut d, &requester, "R-8", "5000").await;
    let (task8, digest8) = task_of(&k, admin.tenant, run8, "review").await;
    k.decide_logic_task(&reviewer, task8, true, &digest8, &json!({}), "").await.unwrap();
    k.set_user_roles(&admin, requester.user, &["Reviewer".to_string()]).await.unwrap();
    d.settle().await;
    assert_eq!(scalar::<i64>(&k, admin.tenant, "select count(*) from e_order where request = $1", r8).await, 0);
    assert_eq!(status(&k, admin.tenant, run8).await, "needs_attention");
    k.set_user_roles(&admin, requester.user, &["Requester".to_string(), "Reviewer".to_string()]).await.unwrap();

    // Stopped by whoever started it: its task closes and the run ends cancelled.
    let (_, run9) = request(&k, &mut d, &requester, "R-9", "5000").await;
    assert_eq!(code(&k.cancel_logic_run(&outsider, run9, "no").await.unwrap_err()), "forbidden");
    k.cancel_logic_run(&requester, run9, "no longer needed").await.unwrap();
    d.settle().await;
    assert_eq!(status(&k, admin.tenant, run9).await, "cancelled");
    assert_eq!(scalar::<String>(&k, admin.tenant, "select status from logic_task where run_id = $1 and activation = 'review'", run9).await, "cancelled");

    // Handed to someone who then leaves (FEO-012): it goes on to their successor when the successor may
    // take it, else back to whoever handed it to them, else to its role; each move is a decision.
    let lee = user(&k, &admin, "lee@acme.test", &["Reviewer"], false).await;
    let sam = user(&k, &admin, "sam@acme.test", &["Reviewer"], false).await;
    let kim = user(&k, &admin, "kim@acme.test", &["Reviewer"], false).await;
    let jo = user(&k, &admin, "jo@acme.test", &["Reviewer"], false).await;
    let max = user(&k, &admin, "max@acme.test", &["Reviewer"], false).await;
    let (_, run10) = request(&k, &mut d, &requester, "R-10", "5000").await;
    let (task10, digest10) = task_of(&k, admin.tenant, run10, "review").await;
    k.delegate_logic_task(&reviewer, task10, lee.user, "on leave").await.unwrap();
    let (_, run11) = request(&k, &mut d, &requester, "R-11", "5000").await;
    let (task11, digest11) = task_of(&k, admin.tenant, run11, "review").await;
    k.delegate_logic_task(&reviewer, task11, kim.user, "").await.unwrap();
    let (_, run12) = request(&k, &mut d, &requester, "R-12", "5000").await;
    let (task12, digest12) = task_of(&k, admin.tenant, run12, "review").await;
    k.delegate_logic_task(&jo, task12, max.user, "").await.unwrap();
    // A successor who holds the role takes it on.
    let left = k.deprovision_user(&admin, lee.user, Some(sam.user), "left the company").await.unwrap();
    assert_eq!(left["tasks_handed_on"], json!(1));
    // One without it does not: it goes back to whoever handed it on.
    k.deprovision_user(&admin, kim.user, Some(outsider.user), "left the company").await.unwrap();
    // Whoever handed it on is not holding it, so their leaving leaves it; once its holder leaves too,
    // with nobody taking over, it goes back to the role.
    assert_eq!(k.deprovision_user(&admin, jo.user, None, "left").await.unwrap()["tasks_handed_on"], json!(0));
    k.deprovision_user(&admin, max.user, None, "left").await.unwrap();
    let holds = |who: &Ctx, task: Uuid| {
        let (k, who) = (&k, who.clone());
        async move { k.my_logic_tasks(&who).await.unwrap().iter().any(|t| t["id"] == json!(task)) }
    };
    assert!(holds(&sam, task10).await && !holds(&reviewer, task10).await, "the successor's now");
    assert!(holds(&reviewer, task11).await && !holds(&outsider, task11).await, "back to whoever handed it on");
    assert!(holds(&reviewer2, task12).await && holds(&reviewer, task12).await && !holds(&requester, task12).await, "the role's again, never the requester's");
    let to: Option<Uuid> = scalar(
        &k,
        admin.tenant,
        "select to_user from logic_task_decision where task_id = $1 and decision = 'delegate' and reason like '%left the workspace%'",
        task10,
    )
    .await;
    assert_eq!(to, Some(sam.user));
    let to: Option<Uuid> = scalar(
        &k,
        admin.tenant,
        "select to_user from logic_task_decision where task_id = $1 and decision = 'delegate' and reason like '%left the workspace%'",
        task11,
    )
    .await;
    assert_eq!(to, Some(reviewer.user));
    let (to, by): (Option<Uuid>, Uuid) = {
        let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
        let row = sqlx::query_as("select to_user, by_user from logic_task_decision where task_id = $1 and reason like '%left the workspace%'")
            .bind(task12)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        row
    };
    assert_eq!((to, by), (None, admin.user), "recorded as the administrator's, to nobody by name");
    let told: i64 = scalar(&k, admin.tenant, "select count(*) from notification where user_id = $1 and title like '%is yours to decide%'", sam.user).await;
    assert_eq!(told, 1, "the successor is told");
    assert_eq!(code(&k.decide_logic_task(&reviewer, task10, false, &digest10, &json!({}), "no").await.unwrap_err()), "forbidden");
    k.decide_logic_task(&sam, task10, false, &digest10, &json!({}), "not now").await.expect("the successor decides");
    k.decide_logic_task(&reviewer, task11, false, &digest11, &json!({}), "not now").await.expect("whoever handed it on decides");
    k.decide_logic_task(&reviewer2, task12, false, &digest12, &json!({}), "not now").await.expect("anyone of the role decides");
    d.settle().await;

    // Someone away names who stands in for them (FEO-034): a task handed to them is the substitute's to decide
    // meanwhile, if they hold its role and it is not their own request.
    let today = chrono::Utc::now().date_naive();
    let week = today + chrono::Duration::days(7);
    assert!(k.set_away(&reviewer2, reviewer2.user, today, week, reviewer2.user, "Leave").await.is_err(), "not themselves");
    assert!(k.set_away(&reviewer2, reviewer2.user, week, today, reviewer3.user, "Leave").await.is_err(), "not ending before it starts");
    assert!(k.set_away(&reviewer3, reviewer2.user, today, week, reviewer3.user, "Leave").await.is_err(), "only they or an administrator say so");
    k.set_away(&reviewer2, reviewer2.user, today, week, reviewer3.user, "Annual leave").await.unwrap();
    let (_, run13) = request(&k, &mut d, &requester, "R-13", "5000").await;
    let (task13, digest13) = task_of(&k, admin.tenant, run13, "review").await;
    k.delegate_logic_task(&reviewer, task13, reviewer2.user, "for Reviewer 2").await.unwrap();
    assert!(k.my_logic_tasks(&reviewer3).await.unwrap().iter().any(|t| t["id"] == json!(task13)), "the substitute holds it while they are away");
    k.decide_logic_task(&reviewer3, task13, false, &digest13, &json!({}), "not now").await.expect("decided by the substitute");
    let (_, run14) = request(&k, &mut d, &requester, "R-14", "5000").await;
    let (task14, digest14) = task_of(&k, admin.tenant, run14, "review").await;
    k.delegate_logic_task(&reviewer, task14, reviewer2.user, "").await.unwrap();
    k.set_away(&admin, reviewer2.user, today, week, requester.user, "Annual leave").await.unwrap();
    assert_eq!(code(&k.decide_logic_task(&requester, task14, true, &digest14, &json!({}), "").await.unwrap_err()), "forbidden", "never on their own request");
    k.set_away(&admin, reviewer2.user, today, week, outsider.user, "Annual leave").await.unwrap();
    assert_eq!(
        code(&k.decide_logic_task(&outsider, task14, true, &digest14, &json!({}), "").await.unwrap_err()),
        "forbidden",
        "a substitute without the role does not"
    );
    k.decide_logic_task(&reviewer2, task14, false, &digest14, &json!({}), "not now").await.expect("whoever holds it still may");
    // An absence that is over no longer stands anyone in.
    k.set_away(&admin, reviewer4.user, today - chrono::Duration::days(14), today - chrono::Duration::days(7), reviewer.user, "Last month's leave")
        .await
        .unwrap();
    let (_, run15) = request(&k, &mut d, &requester, "R-15", "5000").await;
    let (task15, digest15) = task_of(&k, admin.tenant, run15, "review").await;
    k.delegate_logic_task(&reviewer3, task15, reviewer4.user, "").await.unwrap();
    assert_eq!(
        code(&k.decide_logic_task(&reviewer, task15, false, &digest15, &json!({}), "").await.unwrap_err()),
        "forbidden",
        "back from leave, it is theirs again"
    );
    k.decide_logic_task(&reviewer4, task15, false, &digest15, &json!({}), "not now").await.unwrap();
    d.settle().await;

    // Another workspace starts nothing of this and sees none of it.
    let other = workspace(&k, "globex").await;
    let x = k.insert(&other, "Request", obj(json!({"reference": "G-1", "total": "1"}))).await.unwrap();
    k.submit(&other, "Request", x["id"].as_str().unwrap().parse().unwrap(), None).await.unwrap();
    d.settle().await;
    assert!(k.logic_runs(&other, 100).await.unwrap().is_empty());
    assert_eq!(code(&k.logic_run(&other, run1).await.unwrap_err()), "not_found");
    assert!(k.logic_runs(&admin, 100).await.unwrap().len() >= 10);

    // Each run records the generation of the activation it started under (FLC-MIG-003).
    let g1: i64 = scalar(&k, admin.tenant, "select generation from logic_activation where tenant_id = $1 and solution = 'acme.requests'", admin.tenant).await;
    assert_eq!(scalar::<Option<i64>>(&k, admin.tenant, "select generation from logic_run where id = $1", run1).await, Some(g1));

    // Going back to an earlier release is a new generation selecting it: new runs start under it, the run
    // started under the later one goes on under it, and what that run did stays (FLC-MIG-006).
    let mut later: Value = serde_json::from_str(&source()).unwrap();
    later["version"] = json!("1.1.0");
    later["processes"][0]["nodes"][3]["input"]["values.status"] = json!("\"Placed under 1.1.0\"");
    let saved2 = k.save_release(&admin, &later.to_string()).await.unwrap();
    let release2: Uuid = saved2["id"].as_str().unwrap().parse().unwrap();
    k.review_release_consenting(&admin2, release2, saved2["artifact_digest"].as_str().unwrap(), &[], true, "").await.unwrap();
    let to2 = k.activate_release_explained(&admin, release2, Some(g1)).await.unwrap();
    assert_eq!((to2["generation"].as_i64(), to2["replaced"]["version"].as_str()), (Some(g1 + 1), Some("1.0.0")), "{to2}");
    let (r10, run10) = request(&k, &mut d, &requester, "R-10", "5000").await;
    let (task10, digest10) = task_of(&k, admin.tenant, run10, "review").await;
    k.decide_logic_task(&reviewer, task10, true, &digest10, &json!({}), "").await.unwrap();
    d.settle().await;
    let order10: Uuid = scalar(&k, admin.tenant, "select id from e_order where request = $1", r10).await;
    let back = k.activate_release_explained(&admin, release, Some(g1 + 1)).await.unwrap();
    assert_eq!(
        (back["generation"].as_i64(), back["replaced"]["version"].as_str(), back["open_elsewhere"].as_i64()),
        (Some(g1 + 2), Some("1.1.0"), Some(1)),
        "{back}"
    );
    assert!(back["kept"].as_str().unwrap().contains("nothing a run did is undone"), "{back}");
    let (r11, run11) = request(&k, &mut d, &requester, "R-11", "5000").await;
    assert_eq!(scalar::<Uuid>(&k, admin.tenant, "select release_id from logic_run where id = $1", run11).await, release);
    assert_eq!(scalar::<Option<i64>>(&k, admin.tenant, "select generation from logic_run where id = $1", run11).await, Some(g1 + 2));
    let (task11, digest11) = task_of(&k, admin.tenant, run11, "review").await;
    k.decide_logic_task(&reviewer, task11, true, &digest11, &json!({}), "").await.unwrap();
    d.settle().await;
    assert_eq!(scalar::<Option<String>>(&k, admin.tenant, "select status from e_order where request = $1", r11).await, None, "ordered as 1.0.0 orders");
    assert_eq!(scalar::<Uuid>(&k, admin.tenant, "select release_id from logic_run where id = $1", run10).await, release2, "still under 1.1.0");
    assert_eq!(status(&k, admin.tenant, run10).await, "running");
    assert_eq!(scalar::<Option<String>>(&k, admin.tenant, "select status from e_order where id = $1", order10).await.as_deref(), Some("Placed under 1.1.0"));
    assert_eq!(scalar::<i64>(&k, admin.tenant, "select count(*) from logic_effect where effect_key = $1::uuid::text || '/order'", run10).await, 1);

    // A candidate beside what runs now, replayed on what happened with the answers, decisions and events each
    // run met: what differs is said run by run, and nothing is run, changed or kept (FLC-MIG-002).
    let counts = || {
        let k = k.clone();
        let t = admin.tenant;
        async move {
            (
                scalar::<i64>(&k, t, "select count(*) from e_order where tenant_id = $1", t).await,
                scalar::<i64>(&k, t, "select count(*) from logic_effect where tenant_id = $1", t).await,
                scalar::<i64>(&k, t, "select count(*) from logic_release where tenant_id = $1", t).await,
                scalar::<i64>(&k, t, "select count(*) from logic_run where tenant_id = $1", t).await,
            )
        }
    };
    let before = counts().await;
    let mut candidate: Value = serde_json::from_str(&source()).unwrap();
    candidate["version"] = json!("1.2.0");
    candidate["decisions"][0]["rows"][0]["match"]["total"]["gt"] = json!("6000");
    candidate["processes"][0]["nodes"][3]["input"]["values.status"] = json!("\"Shadowed\"");
    let shadow = k.shadow_solution(&admin, Some(&candidate.to_string()), None, 50, &BTreeMap::new()).await.unwrap();
    assert_eq!((shadow["against"]["kind"].as_str(), shadow["against"]["version"].as_str()), (Some("release"), Some("1.0.0")), "{shadow}");
    let cases = shadow["cases"].as_array().unwrap();
    assert!(cases.len() >= 10, "{shadow}");
    let mut decided_otherwise = 0;
    let mut ordered_otherwise = 0;
    for c in cases {
        let differences = c["differences"].as_array().unwrap();
        if c["now"]["decisions"]["decide"] == "review" {
            assert_eq!(c["candidate"]["decisions"]["decide"], "proceed", "{c}");
            assert!(differences.contains(&json!("decisions")), "{c}");
            decided_otherwise += 1;
        }
        if c["now"]["effects"].as_array().unwrap().iter().any(|e| e["capability"] == "procs.order.create") {
            assert!(differences.contains(&json!("effects")), "{c}");
            assert!(c["candidate"]["effects"].to_string().contains("Shadowed"), "{c}");
            ordered_otherwise += 1;
        }
    }
    assert!(decided_otherwise >= 5 && ordered_otherwise >= 3, "{decided_otherwise} decided, {ordered_otherwise} ordered otherwise: {shadow}");
    // A refusal replays as the refusal it was: the run whose order was refused for its requester's rights
    // stopped for attention then, and does so in the replay.
    let refused_then = cases.iter().find(|c| c["run"] == json!(run8)).expect("the run refused for rights");
    assert_eq!(refused_then["now"]["outcome"], "needs_attention", "{refused_then}");
    let steps: Vec<&str> = refused_then["now"]["steps"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(!steps.contains(&"delivered"), "it stopped at the refusal, not after waiting for a delivery: {refused_then}");
    assert_eq!(shadow["differing"].as_u64(), Some(cases.iter().filter(|c| c["same"] == json!(false)).count() as u64));
    // A step the candidate adds has no answer on record: not modelled, unless the person assumes one, which each
    // case then lists.
    let mut added: Value = serde_json::from_str(&source()).unwrap();
    added["version"] = json!("1.4.0");
    added["processes"][0]["nodes"][0]["next"] = json!("check");
    let check = json!({"type": "query", "id": "check", "label": "Read it again", "capability": {"id": "procs.request.read", "version": "1.0.0"},
        "input": {"id": "trigger.id"}, "next": "decide", "on_error": "attention"});
    added["processes"][0]["nodes"].as_array_mut().unwrap().insert(1, check);
    let blind = k.shadow_solution(&admin, Some(&added.to_string()), None, 5, &BTreeMap::new()).await.unwrap();
    assert!(blind["cases"].as_array().unwrap().iter().all(|c| c["candidate"]["not_modelled"].as_array().unwrap().contains(&json!("check"))), "{blind}");
    // Assumed answers fill only what history lacks: the recorded read of the request still answers "snapshot".
    let assume = BTreeMap::from([
        ("check".to_string(), Invoked::Done { output: json!({"total": "5000"}) }),
        ("snapshot".to_string(), Invoked::Refused { code: "forbidden".into(), message: "assumed".into() }),
    ]);
    let seen = k.shadow_solution(&admin, Some(&added.to_string()), None, 5, &assume).await.unwrap();
    for c in seen["cases"].as_array().unwrap() {
        assert_eq!(c["assumed"], json!(["check"]), "{c}");
        assert!(c["candidate"]["not_modelled"].as_array().unwrap().is_empty(), "{c}");
    }
    assert_eq!(counts().await, before, "nothing ran, changed or was kept");
    assert_eq!(code(&k.shadow_solution(&reviewer, Some(&candidate.to_string()), None, 5, &BTreeMap::new()).await.unwrap_err()), "forbidden");

    // One writer at the cutover from an automation (FLC-MIG-003). Each of its events is run by it or by the
    // solution imported from it, never both and never neither, whichever of the two comes to the event first:
    // before the activation, while it commits, or after it, and however often the event comes again.
    let told = user(&k, &admin, "told@acme.test", &["Reviewer"], false).await;
    let saved = obj(json!({"name": "Tell about changes", "entity": "Request", "event": "updated",
        "actions": [{"type": "notify", "to": format!("user:{}", told.user), "title": "\"Changed\"", "body": "doc.reference"}]}));
    let automation = ferrum_kernel::automation::save(&k, &admin, &saved).await.unwrap();
    let imported = k.import_automations(&admin).await.unwrap();
    let mine = imported.iter().find(|i| i["automation"] == json!(automation)).expect("imported").clone();
    assert_eq!(mine["check"]["ok"], json!(true), "{mine}");
    let draft = k.logic_draft(&admin, mine["draft"]["id"].as_str().unwrap().parse().unwrap()).await.unwrap();
    let text = draft["source"].as_str().unwrap().to_string();
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["processes"][0]["trigger"]["business_key"], "event.id", "a run per event, as it ran");
    let kept = k.save_release(&admin, &text).await.unwrap();
    let imported_release: Uuid = kept["id"].as_str().unwrap().parse().unwrap();
    let growth: Vec<String> = k.logic_release_review(&admin2, imported_release).await.unwrap()["diff"]["privilege_growth"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    k.review_release_consenting(&admin2, imported_release, kept["artifact_digest"].as_str().unwrap(), &growth, true, "").await.unwrap();
    let e1 = updated(&k, &requester, "C-1").await;
    let e2 = updated(&k, &requester, "C-2").await;
    let e6 = updated(&k, &requester, "C-6").await;
    ferrum_kernel::automation::run_for_event(&k, &e1).await.unwrap();
    k.accept_event(&e2).await.unwrap();
    // Before it takes over, the imported solution is compared with what the automation did, event by event.
    let beside = k.shadow_solution(&admin, Some(&text), None, 20, &BTreeMap::new()).await.unwrap();
    assert_eq!(beside["against"]["kind"], "automation", "{beside}");
    let first = beside["cases"].as_array().unwrap().iter().find(|c| c["event"] == json!(e1.id)).expect("the event the automation ran").clone();
    assert_eq!(
        (first["now"]["outcome"].as_str(), first["candidate"]["outcome"].as_str(), first["same"].as_bool()),
        (Some("acted"), Some("acted"), Some(true)),
        "{first}"
    );
    let (activated, a6, l6) =
        tokio::join!(k.activate_release_explained(&admin, imported_release, None), ferrum_kernel::automation::run_for_event(&k, &e6), k.accept_event(&e6));
    let activated = activated.unwrap();
    a6.unwrap();
    l6.unwrap();
    assert_eq!(activated["handed_over"]["automation"], json!(automation), "{activated}");
    assert!(!scalar::<bool>(&k, admin.tenant, "select is_active from automation where id = $1", automation).await, "turned off in the same step");
    k.accept_event(&e1).await.unwrap();
    ferrum_kernel::automation::run_for_event(&k, &e2).await.unwrap();
    let e3 = updated(&k, &requester, "C-3").await;
    let e4 = updated(&k, &requester, "C-4").await;
    let e5 = updated(&k, &requester, "C-5").await;
    k.accept_event(&e3).await.unwrap();
    ferrum_kernel::automation::run_for_event(&k, &e3).await.unwrap();
    ferrum_kernel::automation::run_for_event(&k, &e4).await.unwrap();
    k.accept_event(&e4).await.unwrap();
    let (a5, l5) = tokio::join!(ferrum_kernel::automation::run_for_event(&k, &e5), k.accept_event(&e5));
    a5.unwrap();
    l5.unwrap();
    // Delivered again, as the outbox may: nothing more is run.
    for e in [&e1, &e2, &e3, &e4, &e5, &e6] {
        ferrum_kernel::automation::run_for_event(&k, e).await.unwrap();
        k.accept_event(e).await.unwrap();
    }
    d.settle().await;
    assert_eq!(outcome(&k, admin.tenant, automation, e1.id).await, (Some("done".into()), 0), "the automation ran it before the cutover");
    for e in [&e2, &e3, &e4, &e5] {
        assert_eq!(outcome(&k, admin.tenant, automation, e.id).await, (Some("handed_over".into()), 1), "the solution's: event {}", e.id);
    }
    let during = outcome(&k, admin.tenant, automation, e6.id).await;
    assert!(during == (Some("done".into()), 0) || during == (Some("handed_over".into()), 1), "one of the two, during the activation: {during:?}");
    for reference in ["C-1", "C-2", "C-3", "C-4", "C-5", "C-6"] {
        let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
        let told_once: i64 = sqlx::query_scalar("select count(*) from notification where user_id = $1 and body = $2")
            .bind(told.user)
            .bind(reference)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(told_once, 1, "told once about {reference}");
    }
    // Never both: the automation stays off while the solution runs in its place.
    let mut again = saved.clone();
    again.insert("id".into(), json!(automation));
    assert_eq!(code(&ferrum_kernel::automation::save(&k, &admin, &again).await.unwrap_err()), "conflict");

    // An app taken out of the workspace while a run waits (FLC-OPS-007): the step that would use its capability
    // is neither done nor failed. It waits; administrators see why and may end the run instead; and once the app
    // is back the step goes on by itself, once, under its own effect key.
    let (r12, run12) = request(&k, &mut d, &requester, "R-12", "5000").await;
    let (task12, digest12) = task_of(&k, admin.tenant, run12, "review").await;
    k.decide_logic_task(&reviewer, task12, true, &digest12, &json!({}), "").await.unwrap();
    k.disable_packages(&admin, &["procs".to_string()]).await.unwrap();
    d.settle().await;
    assert_eq!(scalar::<i64>(&k, admin.tenant, "select count(*) from e_order where request = $1", r12).await, 0, "nothing done");
    assert_eq!(status(&k, admin.tenant, run12).await, "running", "waiting, not failed");
    let item: Uuid =
        scalar(&k, admin.tenant, "select id from logic_task where run_id = $1 and activation = 'order#unavailable' and status = 'open'", run12).await;
    assert!(k.my_logic_tasks(&admin).await.unwrap().iter().any(|t| t["id"] == json!(item) && t["kind"] == "attention"), "administrators see it");
    assert!(!k.my_logic_tasks(&reviewer).await.unwrap().iter().any(|t| t["id"] == json!(item)), "nobody else is asked");
    let why: Value = scalar(&k, admin.tenant, "select detail from logic_task where id = $1", item).await;
    assert!(why["why"].as_str().unwrap().contains("cancel the run"), "{why}");
    d.retry().await;
    assert_eq!(scalar::<i64>(&k, admin.tenant, "select count(*) from e_order where request = $1", r12).await, 0, "still out of service");
    k.install_packages(admin.tenant, &["procs".to_string()]).await.unwrap();
    d.retry().await;
    d.settle().await;
    assert_eq!(scalar::<i64>(&k, admin.tenant, "select count(*) from e_order where request = $1", r12).await, 1, "done once it is back");
    assert_eq!(scalar::<String>(&k, admin.tenant, "select status from logic_task where id = $1", item).await, "resolved");
    // The run says how often its order was tried, and how the last try went (FLC-OPS-001).
    let tried = &k.logic_run(&admin, run12).await.unwrap()["attempts"]["order"];
    assert!(tried["attempts"].as_i64().unwrap() >= 2 && tried["last"] == "done", "{tried}");

    // A database restored to a point before Temporal's histories (FLC-OPS-005): the run says it ordered, and no
    // receipt of it is here. Administrators are told to check; nothing is ordered again by itself.
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    let gone = sqlx::query("delete from logic_effect where effect_key = $1").bind(format!("{run1}/order")).execute(&mut *tx).await.unwrap().rows_affected();
    tx.commit().await.unwrap();
    assert_eq!(gone, 1, "the receipt, as the restored database lacks it");
    // As a server notices a restore and a person releases its hold; a bare test kernel has not looked yet.
    sqlx::query(
        "insert into platform_incarnation (system_identifier, timeline, database_oid, released_at) values (0, 1, 0, now()) \
         on conflict (id) do update set released_at = now()",
    )
    .execute(&k.db.pool)
    .await
    .unwrap();
    let state1 = d.runs[&run1].1.clone();
    k.process_report(admin.tenant, run1, &state1).await.unwrap();
    let lost: Uuid = scalar(&k, admin.tenant, "select id from logic_task where run_id = $1 and activation = 'order#restored' and status = 'open'", run1).await;
    assert!(k.my_logic_tasks(&admin).await.unwrap().iter().any(|t| t["id"] == json!(lost)), "administrators see it");
    let said: Value = scalar(&k, admin.tenant, "select detail from logic_task where id = $1", lost).await;
    assert!(said["why"].as_str().unwrap().contains("Nothing is done again by itself"), "{said}");
    k.process_report(admin.tenant, run1, &state1).await.unwrap();
    assert_eq!(
        scalar::<i64>(&k, admin.tenant, "select count(*) from logic_task where run_id = $1 and activation = 'order#restored'", run1).await,
        1,
        "said once"
    );
    assert_eq!(scalar::<i64>(&k, admin.tenant, "select count(*) from e_order where request = $1", r1).await, 1, "and not ordered again");
    // A run still open, whose history is ahead too, says so as soon as it is asked where it stands: as every
    // open run is when a restore's hold is released.
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    let gone = sqlx::query("delete from logic_effect where effect_key = $1").bind(format!("{run10}/order")).execute(&mut *tx).await.unwrap().rows_affected();
    tx.commit().await.unwrap();
    assert_eq!(gone, 1);
    // Held, as a server holds when it notices a restore; a person releases it, and every open run is asked.
    sqlx::query("update platform_incarnation set held_since = now(), held_because = 'a restore'").execute(&k.db.pool).await.unwrap();
    k.release_hold(&admin, "checked the restored database against the order book").await.unwrap();
    d.settle().await;
    let _: Uuid = scalar(&k, admin.tenant, "select id from logic_task where run_id = $1 and activation = 'order#restored' and status = 'open'", run10).await;
    sqlx::query("update platform_incarnation set released_at = null").execute(&k.db.pool).await.unwrap();

    // A waiting run moves onto another release only by a mapping one administrator plans and another approves
    // (FLC-MIG-004). The release that runs now has no "review": unmapped, or mapped to a step that does not wait
    // the same way, the move is refused and the run stays as it was; mapped to its new task, the run moves with
    // its open task, and goes on under the new release. A move that reaches a run which moved on meanwhile is
    // refused where the records are, and nothing is dropped.
    let g_now: i64 =
        scalar(&k, admin.tenant, "select generation from logic_activation where tenant_id = $1 and solution = 'acme.requests'", admin.tenant).await;
    let (_, run13) = request(&k, &mut d, &requester, "R-13", "5000").await;
    let (_, run14) = request(&k, &mut d, &requester, "R-14", "5000").await;
    let mut moved_to: Value = serde_json::from_str(&source()).unwrap();
    moved_to["version"] = json!("1.3.0");
    moved_to["processes"][0]["nodes"][1]["routes"] = json!([{"outcome": "review", "target": "approve"}, {"outcome": "proceed", "target": "approve"}]);
    moved_to["processes"][0]["nodes"][2]["id"] = json!("approve");
    moved_to["processes"][0]["nodes"][3]["approval"] = json!("approve");
    moved_to["processes"][0]["nodes"][3]["input"]["values.status"] = json!("\"Moved\"");
    let saved3 = k.save_release(&admin, &moved_to.to_string()).await.unwrap();
    let release3: Uuid = saved3["id"].as_str().unwrap().parse().unwrap();
    k.review_release_consenting(&admin2, release3, saved3["artifact_digest"].as_str().unwrap(), &[], true, "").await.unwrap();
    k.activate_release(&admin, release3, Some(g_now)).await.unwrap();
    let none = BTreeMap::new();
    let refused_plan = k.plan_logic_migration(&admin, run13, release3, &none).await.unwrap_err();
    assert!(code(&refused_plan) == "conflict" && refused_plan.to_string().contains("review has no counterpart in 1.3.0"), "{refused_plan}");
    let wrong = BTreeMap::from([("review".to_string(), "order".to_string())]);
    let refused_plan = k.plan_logic_migration(&admin, run13, release3, &wrong).await.unwrap_err();
    assert!(refused_plan.to_string().contains("waits as a task, and order is a step that does not wait"), "{refused_plan}");
    assert_eq!(code(&k.plan_logic_migration(&reviewer, run13, release3, &wrong).await.unwrap_err()), "forbidden");
    let map = BTreeMap::from([("review".to_string(), "approve".to_string())]);
    let plan = k.plan_logic_migration(&admin, run13, release3, &map).await.unwrap();
    let digest = plan["digest"].as_str().unwrap().to_string();
    assert_eq!(code(&k.approve_logic_migration(&admin, run13, &digest, "").await.unwrap_err()), "forbidden", "not who planned it");
    let (task13, _) = task_of(&k, admin.tenant, run13, "review").await;
    k.approve_logic_migration(&admin2, run13, &digest, "1.3.0 asks the same people").await.unwrap();
    d.settle().await;
    assert_eq!(scalar::<Uuid>(&k, admin.tenant, "select release_id from logic_run where id = $1", run13).await, release3);
    assert_eq!(
        scalar::<String>(&k, admin.tenant, "select activation from logic_task where id = $1", task13).await,
        "approve",
        "the same task, under its new step"
    );
    let digest13: String = scalar(&k, admin.tenant, "select digest from logic_task where id = $1", task13).await;
    k.decide_logic_task(&reviewer, task13, true, &digest13, &json!({}), "").await.unwrap();
    d.settle().await;
    assert_eq!(
        scalar::<Option<String>>(
            &k,
            admin.tenant,
            "select o.status from e_order o join logic_run l on l.business_key = o.request::text where l.id = $1",
            run13
        )
        .await
        .as_deref(),
        Some("Moved")
    );
    let record: Vec<String> =
        k.logic_run(&admin, run13).await.unwrap()["moves"].as_array().unwrap().iter().map(|m| m["action"].as_str().unwrap().to_string()).collect();
    assert_eq!(record, ["plan_migration", "approve_migration", "migrated"], "the run keeps the record of its move");
    // Planned and approved, but decided before its runtime took the move up: refused, and it goes on as it was.
    let plan14 = k.plan_logic_migration(&admin, run14, release3, &map).await.unwrap();
    k.approve_logic_migration(&admin2, run14, plan14["digest"].as_str().unwrap(), "").await.unwrap();
    let (task14, digest14) = task_of(&k, admin.tenant, run14, "review").await;
    k.decide_logic_task(&reviewer, task14, true, &digest14, &json!({}), "").await.unwrap();
    d.settle().await;
    assert_eq!(scalar::<Uuid>(&k, admin.tenant, "select release_id from logic_run where id = $1", run14).await, release, "it stayed");
    assert_eq!(
        scalar::<Option<String>>(
            &k,
            admin.tenant,
            "select o.status from e_order o join logic_run l on l.business_key = o.request::text where l.id = $1",
            run14
        )
        .await,
        None,
        "ordered as it was going to be"
    );
    let refusal: Value =
        scalar(&k, admin.tenant, "select diff from audit_event where record_id = $1 and action = 'migration_refused' order by id desc limit 1", run14).await;
    assert!(refusal["why"][0].as_str().unwrap().contains("review was approved meanwhile"), "{refusal}");

    // An event and the wait for it arriving at the same moment, again and again: each pair ends matched, never
    // the event kept as early while the wait waits on (FLC-PROC-004). Found by the walk on the assembled build.
    for i in 0..60 {
        let subject = Uuid::now_v7();
        let ev = OutboxEvent {
            id: 9_000_000 + i,
            tenant_id: admin.tenant,
            topic: "Order.updated".into(),
            entity: Some("Order".into()),
            record_id: Some(subject),
            payload: json!({"doc": {"id": subject, "reference": format!("RACE-{i}")}}),
            ..Default::default()
        };
        let (activation, value, timeout) = (format!("race{i}"), json!(subject.to_string()), json!("P1D"));
        let (accepted, waited) =
            tokio::join!(k.accept_event(&ev), k.process_register_wait(admin.tenant, run1, &activation, "procs.order.updated", &value, &timeout));
        accepted.unwrap();
        let waited = waited.unwrap();
        let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
        let state: String = sqlx::query_scalar("select status from logic_wait where run_id = $1 and activation = $2")
            .bind(run1)
            .bind(&activation)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(state == "matched" || matches!(waited, Waited::Matched(_)), "round {i}: the wait is {state} and registering said {waited:?}");
    }

    // A replica that has not yet heard of an activation made on another takes it up with the very next event:
    // what starts runs follows the workspace's version in the database, not the replica's last word of it
    // (FLC-MIG-003). This one has its own pool and no listeners, so it hears of nothing by itself, and it has
    // read the workspace, and what starts runs there, just before the activation.
    let pool = Db::connect(DbConfig { url, max_connections: 4, ..Default::default() }).await.expect("connect");
    let replica = Kernel::boot(pool, catalog, HookRegistry::default(), settings, None).await.expect("a replica");
    replica.accept_event(&OutboxEvent { id: 9_100_000, tenant_id: admin.tenant, topic: "Nothing.happened".into(), ..Default::default() }).await.unwrap();
    let noted = json!({
        "schema_version": "ferrum.solution/1", "kind": "Solution", "id": "acme.noted", "version": "1.0.0", "label": "Orders noted",
        "processes": [{"id": "note", "label": "Note an order", "trigger": {"event": "procs.order.created", "business_key": "trigger.id"},
            "entry": "noted", "max_steps": 5, "nodes": [{"type": "end", "id": "noted", "label": "Noted", "outcome": "completed"}]}]
    });
    let kept = k.save_release(&admin, &noted.to_string()).await.unwrap();
    let noted_release: Uuid = kept["id"].as_str().unwrap().parse().unwrap();
    let growth: Vec<String> = k.logic_release_review(&admin2, noted_release).await.unwrap()["diff"]["privilege_growth"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    k.review_release_consenting(&admin2, noted_release, kept["artifact_digest"].as_str().unwrap(), &growth, true, "").await.unwrap();
    k.activate_release(&admin, noted_release, None).await.unwrap();
    let order: Uuid = k.insert(&requester, "Order", obj(json!({"reference": "REPLICA-1"}))).await.unwrap()["id"].as_str().unwrap().parse().unwrap();
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    let e: (i64, String, Option<String>, Option<Uuid>, Value) =
        sqlx::query_as("select id, topic, entity, record_id, payload from outbox where topic = 'Order.created' and record_id = $1")
            .bind(order)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    let created = OutboxEvent { id: e.0, tenant_id: admin.tenant, topic: e.1, entity: e.2, record_id: e.3, payload: e.4, ..Default::default() };
    replica.accept_event(&created).await.unwrap();
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    let runs: i64 = sqlx::query_scalar("select count(*) from logic_run where trigger_event = $1").bind(created.id).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(runs, 1, "the replica started the run, under an activation it had not heard of");

    // The simulator decides as the runtime does (FLC-UI-007): the same boundary fixtures, run for real and simulated
    // with what the real run read standing in, reach the same decision, and the simulation labels the stand-in.
    for (reference, total, expected) in [("EDGE-1", "1000", "proceed"), ("EDGE-2", "1000.01", "review")] {
        let (_, run) = request(&k, &mut d, &requester, reference, total).await;
        let real = d.runs[&run].1.clone();
        assert_eq!(real.outputs["decide"]["outcome"], expected, "{reference}: {:?}", real.outputs);
        let scenario = json!({"process": "approve_request", "trigger": real.trigger, "requester": real.requester, "start": "2026-11-10T09:00:00Z",
            "results": {"snapshot": {"result": "done", "output": real.outputs["snapshot"]}}});
        let sim = k.simulate_solution(&admin, None, Some(release3), scenario).await.unwrap();
        assert_eq!(sim["simulation"]["run"]["outputs"]["decide"], real.outputs["decide"], "{reference}: {sim}");
        let entries = sim["simulation"]["entries"].as_array().unwrap();
        assert!(entries.iter().any(|e| e["activation"] == "snapshot" && e["source"] == "stand_in"), "the stand-in is labelled: {sim}");
    }

    // A capability's version replaced while a run waits (FLC-CAP-004): the run neither adopts the new code nor fails.
    // Its step waits and administrators are told why; with the version it was reviewed against back, it goes on, once.
    let (r15, run15) = request(&k, &mut d, &requester, "R-15", "5000").await;
    let (task15, digest15) = task_of(&k, admin.tenant, run15, "approve").await;
    let original = k.registry.catalog();
    let mut packages: Vec<_> = original.packages().cloned().collect();
    packages.iter_mut().find(|p| p.manifest.package == "procs").expect("procs").manifest.version = "1.1.0".into();
    k.registry.set_catalog(Arc::new(Catalog::from_packages(packages).unwrap()));
    k.decide_logic_task(&reviewer, task15, true, &digest15, &json!({}), "").await.unwrap();
    d.settle().await;
    assert_eq!(scalar::<i64>(&k, admin.tenant, "select count(*) from e_order where request = $1", r15).await, 0, "the new version is not adopted");
    assert_eq!(status(&k, admin.tenant, run15).await, "running", "waiting, not failed");
    let why: Value =
        scalar(&k, admin.tenant, "select detail from logic_task where run_id = $1 and activation = 'order#unavailable' and status = 'open'", run15).await;
    assert!(why["why"].as_str().unwrap().contains("bound to version 1.0.0 of procs.order.create"), "{why}");
    k.registry.set_catalog(original);
    d.retry().await;
    d.settle().await;
    assert_eq!(scalar::<i64>(&k, admin.tenant, "select count(*) from e_order where request = $1", r15).await, 1, "on its own version again: done once");

    // A step whose answer is lost after it ran (FLC-UI-008): the run stops for a person with the outcome unknown. The
    // console looks the effect up by its receipt and keeps what it found on the item, so another replica and a page
    // read again show the same; resumed, the step repeats under its key and the receipt answers it: one order.
    let mut resumable: Value = serde_json::from_str(&moved_to.to_string()).unwrap();
    resumable["version"] = json!("1.5.0");
    // Resumable only where everything the order reads has surely run: a step of its own after the order.
    resumable["processes"][0]["nodes"][3]["on_unknown"] = json!("lost");
    resumable["processes"][0]["nodes"].as_array_mut().unwrap().push(json!({"type": "suspend", "id": "lost", "label": "Outcome unknown",
        "reason": "Find out whether the order was made before anything more is done.", "resume_to": "order"}));
    let saved4 = k.save_release(&admin, &resumable.to_string()).await.unwrap();
    let release4: Uuid = saved4["id"].as_str().unwrap().parse().unwrap();
    k.review_release_consenting(&admin2, release4, saved4["artifact_digest"].as_str().unwrap(), &[], true, "").await.unwrap();
    let g_requests: i64 =
        scalar(&k, admin.tenant, "select generation from logic_activation where tenant_id = $1 and solution = 'acme.requests'", admin.tenant).await;
    k.activate_release(&admin, release4, Some(g_requests)).await.unwrap();
    d.lose_answers = vec!["order".into()];
    let (r16, run16) = request(&k, &mut d, &requester, "R-16", "5000").await;
    let (task16, digest16) = task_of(&k, admin.tenant, run16, "approve").await;
    k.decide_logic_task(&reviewer, task16, true, &digest16, &json!({}), "").await.unwrap();
    d.settle().await;
    d.lose_answers.clear();
    assert_eq!(status(&k, admin.tenant, run16).await, "needs_attention");
    assert_eq!(scalar::<i64>(&k, admin.tenant, "select count(*) from e_order where request = $1", r16).await, 1, "it ran; only its answer was lost");
    let recovery: Uuid = scalar(&k, admin.tenant, "select id from logic_task where run_id = $1 and kind = 'recovery' and status = 'open'", run16).await;
    assert_eq!(code(&k.look_up_logic_effects(&reviewer, recovery).await.unwrap_err()), "forbidden");
    let found = k.look_up_logic_effects(&admin, recovery).await.unwrap();
    let first = &found["effects"][0];
    assert_eq!(
        (first["activation"].as_str(), first["on_record"].as_bool(), first["capability"].as_str()),
        (Some("order"), Some(true), Some("procs.order.create")),
        "{found}"
    );
    let kept: Value = scalar(&replica, admin.tenant, "select detail from logic_task where id = $1", recovery).await;
    assert_eq!(kept["lookup"], found, "another replica reads the evidence the console found");
    let elsewhere = replica.load_ctx(admin.tenant, admin.user, None, "it".into()).await.unwrap();
    assert_eq!(replica.logic_run(&elsewhere, run16).await.unwrap(), k.logic_run(&admin, run16).await.unwrap(), "the same run on another replica");
    k.recover_logic_task(&admin, recovery, true, "looked up: on record, so resumed").await.unwrap();
    d.settle().await;
    assert_eq!(scalar::<i64>(&k, admin.tenant, "select count(*) from e_order where request = $1", r16).await, 1, "the receipt answered: nothing twice");
    assert!(d.runs[&run16].1.outputs["order"].get("unknown").is_none(), "its outcome is known now: {:?}", d.runs[&run16].1.outputs);

    // A run no person's act started acts as its solution's own account (FLC-CAP-005), holding what its release was
    // reviewed to use and never its activator's rights: offboarding the administrator who activated it changes
    // nothing, the account cannot use what its release does not, and it stops with the solution.
    let admin3 = user(&k, &admin, "admin3@acme.test", &[], true).await;
    let stamped = json!({
        "schema_version": "ferrum.solution/1", "kind": "Solution", "id": "acme.stamped", "version": "1.0.0", "label": "Orders stamped",
        "requires": [{"id": "procs.order.update", "version": "1.0.0"}],
        "processes": [{"id": "stamp", "label": "Stamp an order", "trigger": {"event": "procs.order.created", "business_key": "trigger.id"},
            "entry": "stamp", "max_steps": 5, "nodes": [
                {"type": "action", "id": "stamp", "label": "Stamp it", "capability": {"id": "procs.order.update", "version": "1.0.0"},
                 "input": {"id": "trigger.id", "values.reference": "trigger.reference", "values.status": "\"Stamped\""}, "next": "stamped", "on_error": "stopped", "on_unknown": "stopped"},
                {"type": "end", "id": "stamped", "label": "Stamped", "outcome": "completed"},
                {"type": "end", "id": "stopped", "label": "Stopped", "outcome": "refused"}]}]
    });
    let kept = k.save_release(&admin, &stamped.to_string()).await.unwrap();
    let stamped_release: Uuid = kept["id"].as_str().unwrap().parse().unwrap();
    let growth: Vec<String> = k.logic_release_review(&admin2, stamped_release).await.unwrap()["diff"]["privilege_growth"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    k.review_release_consenting(&admin2, stamped_release, kept["artifact_digest"].as_str().unwrap(), &growth, true, "").await.unwrap();
    k.activate_release(&admin3, stamped_release, None).await.unwrap();
    k.set_user_active(&admin, admin3.user, false).await.unwrap();
    let platform = Ctx::system(admin.tenant, k.registry.schema(admin.tenant).await.unwrap());
    let order: Uuid = k.insert(&platform, "Order", obj(json!({"reference": "STAMP-1"}))).await.unwrap()["id"].as_str().unwrap().parse().unwrap();
    d.settle().await;
    let stamp_run: Uuid = scalar(&k, admin.tenant, "select id from logic_run where solution = 'acme.stamped' and business_key = $1::uuid::text", order).await;
    assert!(scalar::<Option<Uuid>>(&k, admin.tenant, "select requester from logic_run where id = $1", stamp_run).await.is_none(), "nobody's act");
    assert_eq!(scalar::<Option<String>>(&k, admin.tenant, "select status from e_order where id = $1", order).await.as_deref(), Some("Stamped"));
    let actor: Uuid = scalar(&k, admin.tenant, "select actor from logic_effect where effect_key = $1::uuid::text || '/stamp'", stamp_run).await;
    assert_eq!(scalar::<String>(&k, admin.tenant, "select email from app_user where id = $1", actor).await, "solution.acme.stamped@services.ferrum.invalid");
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    let rights: Vec<(String, bool, bool, bool, bool)> = sqlx::query_as(
        "select entity, can_read, can_write, can_create, can_delete from role_permission where tenant_id = $1 and role = 'solution:acme.stamped'",
    )
    .bind(admin.tenant)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(rights, vec![("Order".to_string(), false, true, false, false)], "exactly what its release was reviewed to use");
    let account = k.load_ctx(admin.tenant, actor, None, "it".into()).await.unwrap();
    assert!(!account.is_admin, "never an administrator");
    let outside = ferrum_kernel::capability::Invocation::new("procs.request.read", json!({"id": r16}));
    assert_eq!(code(&k.invoke(&account, outside).await.unwrap_err()), "forbidden", "nothing its release does not use");
    let g_stamped: i64 =
        scalar(&k, admin.tenant, "select generation from logic_activation where tenant_id = $1 and solution = 'acme.stamped'", admin.tenant).await;
    k.deactivate_solution(&admin, "acme.stamped", g_stamped).await.unwrap();
    assert!(!scalar::<bool>(&k, admin.tenant, "select is_active from app_user where id = $1", actor).await, "it stops with the solution");

    // An old event delivered again after its receipt has expired (FLC-OPS-006): the run it started is found by its
    // business key, and nothing starts again.
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    sqlx::query("delete from event_receipt where tenant_id = $1 and event_id = $2").bind(admin.tenant).bind(ev.id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    k.accept_event(&ev).await.unwrap();
    assert_eq!(scalar::<i64>(&k, admin.tenant, "select count(*) from logic_run where business_key = $1::uuid::text", r1).await, 1, "no second run");

    // A secret on a record never reaches a run (FLC-SEC-003): not the event that starts it, not the material a person
    // decides on, not the run's own record; and a pack carries the source, never a record.
    let pinned: Uuid = k.insert(&requester, "Request", obj(json!({"reference": "R-17", "total": "5000", "pin": "4242-not-for-runs"}))).await.unwrap()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(scalar::<i64>(&k, admin.tenant, "select count(*) from record_secret where record_id = $1", pinned).await, 1, "kept as a secret");
    k.submit(&requester, "Request", pinned, None).await.unwrap();
    d.settle().await;
    let run17: Uuid = scalar(&k, admin.tenant, "select id from logic_run where business_key = $1::uuid::text", pinned).await;
    let kept: String = scalar(&k, admin.tenant, "select trigger::text || coalesce(state::text, '') from logic_run where id = $1", run17).await;
    let shown: String = scalar(&k, admin.tenant, "select coalesce(string_agg(material::text, ''), '') from logic_task where run_id = $1", run17).await;
    assert!(!kept.contains("4242-not-for-runs") && !shown.contains("4242-not-for-runs"), "{kept} {shown}");
    assert!(!k.solution_pack(&admin, release4).await.unwrap().to_string().contains("4242"), "nothing of a record travels in a pack");

    // A task's evidence is its own (FLC-HUMAN-006): attached and read only by someone who may decide that task, or an
    // administrator, and never through another task's id or the ordinary file routes.
    let (task17, _) = task_of(&k, admin.tenant, run17, "approve").await;
    let (_, run19) = request(&k, &mut d, &requester, "R-19", "5000").await;
    let (task19, _) = task_of(&k, admin.tenant, run19, "approve").await;
    let file = k.attach_task_evidence(&reviewer, task17, "quote.pdf", "application/pdf", b"%PDF-1.4 the supplier's quote").await.unwrap();
    let evidence: Uuid = file["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(k.read_task_evidence(&reviewer2, task17, evidence).await.unwrap().2, b"%PDF-1.4 the supplier's quote".to_vec(), "another who may decide it");
    assert_eq!(k.read_task_evidence(&admin, task17, evidence).await.unwrap().0, "quote.pdf", "an administrator");
    assert_eq!(code(&k.read_task_evidence(&reviewer, task19, evidence).await.unwrap_err()), "not_found", "not through another task");
    assert_eq!(code(&k.read_task_evidence(&requester, task17, evidence).await.unwrap_err()), "not_found", "the requester may not decide it");
    assert_eq!(code(&k.attach_task_evidence(&outsider, task17, "x.txt", "text/plain", b"x").await.unwrap_err()), "not_found");
    assert!(k.read_file(&admin, evidence).await.is_err(), "the ordinary file route does not reach it");
    let listed = k.logic_task(&reviewer, task17).await.unwrap();
    assert!(listed["evidence"].as_array().unwrap().iter().any(|e| e["id"] == json!(evidence) && e["file_name"] == "quote.pdf"), "{listed}");
    assert!(k.logic_task(&reviewer, task19).await.unwrap()["evidence"].as_array().unwrap().is_empty(), "nor listed on another");

    // An erasure reaches what runs keep of a record (FLC-OPS-006): the event that started its run, the run's own record
    // and the material shown lose the erased fields, and a runtime reporting the run again from what it held before
    // cannot bring them back.
    let asked: Uuid =
        k.insert(&requester, "Request", obj(json!({"reference": "R-18", "total": "5000", "contact": "erase.me@example.com"}))).await.unwrap()["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
    k.submit(&requester, "Request", asked, None).await.unwrap();
    d.settle().await;
    let run18: Uuid = scalar(&k, admin.tenant, "select id from logic_run where business_key = $1::uuid::text", asked).await;
    assert!(scalar::<String>(&k, admin.tenant, "select trigger::text from logic_run where id = $1", run18).await.contains("erase.me@example.com"));
    let answer = k.erase_personal_data(&admin, "erase.me@example.com", "asked to be forgotten").await.unwrap();
    assert!(answer["erased"].as_array().unwrap().iter().any(|e| e["id"] == json!(asked)), "{answer}");
    let after: String = scalar(&k, admin.tenant, "select trigger::text || coalesce(state::text, '') from logic_run where id = $1", run18).await;
    let shown: String = scalar(&k, admin.tenant, "select coalesce(string_agg(material::text, ''), '') from logic_task where run_id = $1", run18).await;
    assert!(!after.contains("erase.me@example.com") && !shown.contains("erase.me@example.com"), "{after} {shown}");
    let held = d.runs[&run18].1.clone();
    assert!(serde_json::to_string(&held).unwrap().contains("erase.me@example.com"), "the runtime's own copy still has it");
    k.process_report(admin.tenant, run18, &held).await.unwrap();
    let reported: String = scalar(&k, admin.tenant, "select coalesce(state::text, '') from logic_run where id = $1", run18).await;
    assert!(!reported.contains("erase.me@example.com"), "reported again, it stays erased: {reported}");

    // What runs on an app's capabilities is known before it goes, and recorded when it goes (FLC-CAP-008).
    let deps = k.logic_dependents(&admin, &["procs".to_string()]).await.unwrap();
    let on_procs = deps.iter().find(|d| d["solution"] == "acme.requests").expect("acme.requests uses procs");
    assert!(on_procs["uses"].as_array().unwrap().iter().any(|u| u["capability"] == "procs.order.create"), "{on_procs}");
    assert!(on_procs["open_runs"].as_i64().unwrap() >= 1, "{on_procs}");
    assert_eq!(code(&k.logic_dependents(&reviewer, &["procs".to_string()]).await.unwrap_err()), "forbidden");
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    let removal: Value =
        sqlx::query_scalar("select diff from audit_event where tenant_id = $1 and entity = 'Package' and action = 'remove' order by id limit 1")
            .bind(admin.tenant)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    assert!(removal["waiting"].as_array().unwrap().iter().any(|w| w["solution"] == "acme.requests"), "taking procs out recorded what waits: {removal}");

    // A partner's time-bound support sees which runs are stuck and at what step, never what they carry, and opens no
    // run (FLC-SEC-007). Its expiry and the customer's list of every request it made are support_it's.
    let (_, key) = k
        .grant_support(
            &admin,
            GrantInput { level: "diagnostics".into(), grantee: "Partner Ltd".into(), entities: vec![], can_write: false, reason: "Ticket 77".into(), hours: 1 },
        )
        .await
        .unwrap();
    let (session, _) = k.verify_api_key(&key).await.unwrap();
    let partner = k.load_ctx(session.tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let report = k.support_report(&partner, "diagnostics").await.unwrap();
    let stuck = report["runs"]["stuck"].as_array().unwrap();
    assert!(report["runs"]["needing_attention"].as_i64().unwrap() >= 1 && stuck.iter().any(|r| r["run"] == json!(run8)), "{report}");
    assert!(stuck.iter().all(|r| r.get("trigger").is_none() && r.get("state").is_none()), "{report}");
    assert!(!report.to_string().contains("R-8\""), "no record's contents: {report}");
    assert_eq!(code(&k.logic_run(&partner, run8).await.unwrap_err()), "not_found", "and no run opened");

    // A worker paused in the middle of a step past its claim, another doing the step, and the first resumed
    // (FLC-PROC-012): the effect's receipt is claimed in the effect's own transaction, so the second waits behind the
    // first's claim; when the first's session ends, as a database ends one left idle in a transaction, its claim and
    // its half-done effect go with it, the second does the step, and the first, resumed, cannot commit anything.
    // Asked again, as a retry would, it finds the receipt and does nothing more.
    let fenced = || ferrum_kernel::capability::Invocation::new("procs.order.create", json!({"values": {"reference": "FENCE-1"}})).key("fence/order");
    pause.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let first = tokio::spawn({
        let (k, who, inv) = (k.clone(), requester.clone(), fenced());
        async move { k.invoke(&who, inv).await }
    });
    pause.entered.notified().await;
    let second = tokio::spawn({
        let (k, who, inv) = (k.clone(), requester.clone(), fenced());
        async move { k.invoke(&who, inv).await }
    });
    let mut blocker: Option<i32> = None;
    for _ in 0..200 {
        let mut tx = k.db.system_tx().await.unwrap();
        blocker = sqlx::query_scalar(
            "select held.pid from pg_locks waiting join pg_locks held on held.locktype = 'transactionid' and held.transactionid = waiting.transactionid \
             and held.granted where waiting.locktype = 'transactionid' and not waiting.granted limit 1",
        )
        .fetch_optional(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        if blocker.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let blocker = blocker.expect("the second worker waits behind the first's claim");
    let mut tx = k.db.system_tx().await.unwrap();
    let ended: bool = sqlx::query_scalar("select pg_terminate_backend($1)").bind(blocker).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(ended, "the first worker's session ended");
    let did = second.await.unwrap().expect("the second does the step");
    assert!(!did.repeated, "it did it, not repeated it");
    pause.go.notify_one();
    assert!(first.await.unwrap().is_err(), "the first, resumed, commits nothing");
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    let orders: i64 = sqlx::query_scalar("select count(*) from e_order where reference = 'FENCE-1'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(orders, 1, "one order");
    let again = k.invoke(&requester, fenced()).await.unwrap();
    assert!(again.repeated && again.result["id"] == did.result["id"], "asked again, it answers with what was done");

    // A reference is to one kind of record (FLC-COMP-002): the id of the request that started a run does not go where
    // an order's id is wanted, and such a release is refused before it ever runs; the request's id still goes where a
    // request's, or plain text, is wanted.
    let mut wrong: Value = serde_json::from_str(&source()).unwrap();
    wrong["version"] = json!("1.9.0");
    wrong["requires"].as_array_mut().unwrap().push(json!({"id": "procs.order.update", "version": "1.0.0"}));
    wrong["processes"][0]["nodes"][3]["capability"] = json!({"id": "procs.order.update", "version": "1.0.0"});
    wrong["processes"][0]["nodes"][3]["input"] = json!({"id": "trigger.id", "values.reference": "trigger.id"});
    let e = k.save_release(&admin, &wrong.to_string()).await.unwrap_err();
    assert!(e.to_string().contains("a reference to a Request"), "{e}");
}
