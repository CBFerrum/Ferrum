//! FLC-PROC and FLC-HUMAN: durable processes' business records against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; reset first). A small driver here does what the runtime does, starting
//! accepted runs, carrying out what they ask through the kernel's activities and handing them what was
//! delivered, without the runtime's schedule: deadlines are passed by hand. The runtime's own conformance,
//! on Temporal, is `crates/process/tests`.

use ferrum_kernel::outbox::Event as OutboxEvent;
use ferrum_kernel::process::{DELIVER_TOPIC, START_TOPIC, Waited};
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
                let outcome = k.process_invoke(t, run, &activation, &capability, &version, input, changes, approval.as_ref()).await.unwrap();
                vec![Event::Invoked { activation, outcome }]
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
    let outsider = user(&k, &admin, "outsider@acme.test", &[], false).await;
    let office =
        Calendar { name: "office".into(), zone: "Asia/Kolkata".into(), weekends: vec!["sat".into(), "sun".into()], holidays: Default::default(), revision: 0 };
    assert_eq!(k.set_logic_calendar(&admin, &office).await.unwrap(), 1);
    assert_eq!(code(&k.set_logic_calendar(&reviewer, &office).await.unwrap_err()), "forbidden");
    let saved = k.save_release(&admin, &source()).await.expect("release");
    let release: Uuid = saved["id"].as_str().unwrap().parse().unwrap();
    k.review_release(&admin2, release, saved["artifact_digest"].as_str().unwrap(), true, "").await.unwrap();
    k.activate_release(&admin, release, None).await.unwrap();
    let mut d = Driver { k: k.clone(), tenant: admin.tenant, last: 0, runs: BTreeMap::new(), hold_waits: false, held: vec![] };

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
    assert_eq!(code(&k.decide_logic_task(&reviewer, task1, true, "sha256:other", &json!({}), "").await.unwrap_err()), "stale_approval");
    assert_eq!(code(&k.decide_logic_task(&reviewer, task1, true, &digest1, &json!({"colour": "red"}), "").await.unwrap_err()), "validation_failed");

    // Approved, ordered under its effect key, and completed when the order's own event comes.
    k.decide_logic_task(&reviewer, task1, true, &digest1, &json!({"note": "fine"}), "").await.unwrap();
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
    assert_eq!(scalar::<i64>(&k, admin.tenant, "select count(*) from e_order where request = $1", r1).await, 1);

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

    // Handed on within its bound, never back along its chain nor to who asked; decided by whom it was handed to
    // (FLC-HUMAN-004).
    let (_, run6) = request(&k, &mut d, &requester, "R-6", "5000").await;
    let (task6, digest6) = task_of(&k, admin.tenant, run6, "review").await;
    assert_eq!(code(&k.delegate_logic_task(&reviewer, task6, outsider.user, "").await.unwrap_err()), "forbidden", "not a reviewer");
    assert_eq!(code(&k.delegate_logic_task(&reviewer, task6, requester.user, "").await.unwrap_err()), "forbidden", "who asked");
    k.delegate_logic_task(&reviewer, task6, reviewer2.user, "on leave").await.unwrap();
    assert_eq!(code(&k.decide_logic_task(&reviewer, task6, true, &digest6, &json!({}), "").await.unwrap_err()), "forbidden", "handed on");
    assert_eq!(code(&k.delegate_logic_task(&reviewer2, task6, reviewer.user, "").await.unwrap_err()), "forbidden", "back along the chain");
    assert_eq!(code(&k.delegate_logic_task(&reviewer2, task6, admin2.user, "").await.unwrap_err()), "forbidden", "past its bound");
    k.decide_logic_task(&reviewer2, task6, true, &digest6, &json!({}), "").await.unwrap();
    let by: Uuid = scalar(&k, admin.tenant, "select by_user from logic_task_decision where task_id = $1 and decision = 'approve'", task6).await;
    assert_eq!(by, reviewer2.user, "attributed to who decided");
    d.settle().await;

    // A revised calendar moves the deadlines resolved after it, never those before (FLC-HUMAN-005).
    let mut revised = office.clone();
    revised.holidays.insert(chrono::Utc::now().date_naive() + chrono::Duration::days(1));
    assert_eq!(k.set_logic_calendar(&admin, &revised).await.unwrap(), 2);
    let (_, run7) = request(&k, &mut d, &requester, "R-7", "5000").await;
    let (task7, _) = task_of(&k, admin.tenant, run7, "review").await;
    assert_eq!(k.logic_task(&reviewer, task7).await.unwrap()["deadline_basis"]["revision"], json!(2));
    assert_eq!(k.logic_task(&reviewer, task1).await.unwrap()["deadline_basis"]["revision"], json!(1));

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
}
