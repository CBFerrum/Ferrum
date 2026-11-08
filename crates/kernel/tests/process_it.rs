//! FLC-PROC and FLC-HUMAN: durable processes' business records against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; reset first). A small driver here does what the runtime does, starting
//! accepted runs, carrying out what they ask through the kernel's activities and handing them what was
//! delivered, without the runtime's schedule: deadlines are passed by hand. The runtime's own conformance,
//! on Temporal, is `crates/process/tests`.

use ferrum_kernel::outbox::Event as OutboxEvent;
use ferrum_kernel::process::{DELIVER_TOPIC, Migration, START_TOPIC, Waited};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_logic::calendar::Calendar;
use ferrum_logic::compile::Release;
use ferrum_logic::process::{Command, Event, Invoked, Machine, Run};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

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
    let v: T = sqlx::query_scalar(sql).bind(bind).fetch_one(&mut *tx).await.unwrap();
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
    let db = Db::connect(DbConfig { url, max_connections: 12, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/procs.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let pkg = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let settings = KernelSettings { process_runtime: true, ..Default::default() };
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, pkg]).unwrap(), HookRegistry::default(), settings, None).await.expect("boot");
    let admin = workspace(&k, "acme").await;
    let admin2 = user(&k, &admin, "admin2@acme.test", &[], true).await;
    // The requester also holds the reviewer's role: still not theirs to decide.
    let requester = user(&k, &admin, "requester@acme.test", &["Requester", "Reviewer"], false).await;
    let reviewer = user(&k, &admin, "reviewer@acme.test", &["Reviewer"], false).await;
    let reviewer2 = user(&k, &admin, "reviewer2@acme.test", &["Reviewer"], false).await;
    let reviewer3 = user(&k, &admin, "reviewer3@acme.test", &["Reviewer"], false).await;
    let reviewer4 = user(&k, &admin, "reviewer4@acme.test", &["Reviewer"], false).await;
    let outsider = user(&k, &admin, "outsider@acme.test", &[], false).await;
    let office =
        Calendar { name: "office".into(), zone: "Asia/Kolkata".into(), weekends: vec!["sat".into(), "sun".into()], holidays: Default::default(), revision: 0 };
    assert_eq!(k.set_logic_calendar(&admin, &office).await.unwrap(), 1);
    assert_eq!(code(&k.set_logic_calendar(&reviewer, &office).await.unwrap_err()), "forbidden");
    assert_eq!(code(&k.set_logic_calendar(&admin, &Calendar { name: "Office!".into(), ..office.clone() }).await.unwrap_err()), "validation_failed");
    let saved = k.save_release(&admin, &source()).await.expect("release");
    let release: Uuid = saved["id"].as_str().unwrap().parse().unwrap();
    // It may create orders, which nothing running could before: consented to by name.
    k.review_release_consenting(&admin2, release, saved["artifact_digest"].as_str().unwrap(), &["procs.order.create".to_string()], true, "").await.unwrap();
    k.activate_release(&admin, release, None).await.unwrap();
    let mut d = Driver { k: k.clone(), tenant: admin.tenant, last: 0, runs: BTreeMap::new(), hold_waits: false, held: vec![], retrying: vec![] };

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
    let shadow = k.shadow_solution(&admin, Some(&candidate.to_string()), None, 50).await.unwrap();
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
    assert_eq!(counts().await, before, "nothing ran, changed or was kept");
    assert_eq!(code(&k.shadow_solution(&reviewer, Some(&candidate.to_string()), None, 5).await.unwrap_err()), "forbidden");

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
    let beside = k.shadow_solution(&admin, Some(&text), None, 20).await.unwrap();
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
}
