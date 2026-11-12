//! Service targets (docs/api.md, "Service targets") against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, reset
//! first). Time is the pinned clock saves take as now (`Kernel::pin_clock`) and the clock the sweep and the
//! schedule scan are given, never waiting. A help desk in Kolkata: an office calendar with hours and a holiday,
//! a target on a packaged ticket's own fields, a reminder before the due moment and an automation on the breach.

use ferrum_kernel::automation;
use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, OverlayOp};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

type At = chrono::DateTime<chrono::Utc>;

const PACKAGE: &str = "package: desk\nversion: 0.1.0\ndepends: [core]\n";
const SHIPMENTS: &str =
    "name: Shipment\nlabel: Shipment\nkind: document\nsubmittable: true\ntitle_field: reference\nfields:\n  - {name: reference, type: data, required: true}\n";
const ENTITIES: &str = "name: Ticket\nlabel: Ticket\nkind: master\ntitle_field: subject\nfields:\n  - {name: subject, type: data, required: true}\n  - {name: priority, type: select, options: [Low, High], default: Low}\n  - {name: status, type: select, options: [Open, Waiting, Closed], default: Open}\n";

fn at(s: &str) -> At {
    s.parse().unwrap()
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn op(entity: &str, op: Value) -> NewCustomization {
    serde_json::from_value(json!({"entity": entity, "op": op})).unwrap()
}

/// The office: Monday to Friday 09:00–13:00 and 14:00–18:00 in Kolkata; Tuesday 10 November 2026 a holiday.
fn office() -> Value {
    let day = json!([["09:00", "13:00"], ["14:00", "18:00"]]);
    json!({"zone": "Asia/Kolkata", "weekends": ["sat", "sun"], "holidays": ["2026-11-10"], "hours": {"mon": day, "tue": day, "wed": day, "thu": day, "fri": day}})
}

/// The help desk's customizations: four fields of its own, the respond target on the office's hours and a resolve
/// target in plain time, a workflow, a button that puts a ticket on hold, and a rule.
fn desk() -> Vec<NewCustomization> {
    let field = |name: &str, ty: &str| op("Ticket", json!({"op": "add_field", "field": {"name": name, "type": ty}}));
    vec![
        field("x_respond_by", "datetime"),
        field("x_respond_state", "data"),
        field("x_resolve_by", "datetime"),
        field("x_resolve_state", "data"),
        op(
            "Ticket",
            json!({"op": "add_target", "target": {"name": "respond", "label": "First response", "calendar": "office",
            "within": "doc.priority == 'High' ? 'PT1H' : 'PT4H'", "pause_when": "doc.status == 'Waiting'", "met_when": "doc.status == 'Closed'",
            "due_field": "x_respond_by", "state_field": "x_respond_state"}}),
        ),
        op(
            "Ticket",
            json!({"op": "add_target", "target": {"name": "resolve", "label": "Resolution", "within": "28800",
            "met_when": "doc.workflow_state == 'Resolved'", "due_field": "x_resolve_by", "state_field": "x_resolve_state"}}),
        ),
        op(
            "Ticket",
            json!({"op": "set_workflow", "workflow": {"name": "desk", "entity": "Ticket", "initial_state": "Open",
            "states": [{"name": "Open"}, {"name": "Resolved"}],
            "transitions": [{"action": "resolve", "from": "Open", "to": "Resolved"}, {"action": "reopen", "from": "Resolved", "to": "Open"}]}}),
        ),
        op(
            "Ticket",
            json!({"op": "add_action", "action": {"name": "x_hold", "label": "Hold", "effects": [{"type": "set_fields", "values": {"status": "'Waiting'"}}]}}),
        ),
        op("Ticket", json!({"op": "add_rule", "rule": {"name": "not_locked", "check": "doc.subject != 'locked'", "message": "this ticket is locked"}})),
    ]
}

struct W {
    k: Arc<Kernel>,
    tenant: Uuid,
    user: Uuid,
}

impl W {
    async fn admin(&self) -> Ctx {
        self.k.load_ctx(self.tenant, self.user, None, "it".into()).await.unwrap()
    }

    /// A ticket saved at `now`.
    async fn ticket(&self, now: &str, extra: Value) -> Doc {
        self.k.pin_clock(Some(at(now)));
        let mut d = obj(json!({"subject": "Printer"}));
        d.extend(obj(extra));
        self.k.insert(&self.admin().await, "Ticket", d).await.unwrap()
    }

    async fn set(&self, now: &str, id: Uuid, patch: Value) -> Doc {
        self.k.pin_clock(Some(at(now)));
        self.k.update(&self.admin().await, "Ticket", id, obj(patch), None).await.unwrap()
    }

    async fn get(&self, id: Uuid) -> Doc {
        self.k.get(&self.admin().await, "Ticket", id, false).await.unwrap()
    }

    async fn count(&self, sql: &str, id: Uuid) -> i64 {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(id).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        n
    }

    async fn exec(&self, sql: &str, id: Uuid) {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string())).bind(id).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
    }
}

/// A target's two fields on a record: the due moment and the state word.
fn kept(d: &Doc, target: &str) -> (Option<At>, Option<String>) {
    let due = d.get(&format!("x_{target}_by")).and_then(Value::as_str).map(at);
    (due, d.get(&format!("x_{target}_state")).and_then(Value::as_str).map(str::to_string))
}

fn running(due: &str) -> (Option<At>, Option<String>) {
    (Some(at(due)), Some("running".into()))
}

async fn workspace(k: &Arc<Kernel>, slug: &str) -> W {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["desk".into()],
            settings: Some(json!({"timezone": "Asia/Kolkata"})),
        })
        .await
        .expect("workspace");
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    W { k: k.clone(), tenant, user: session.user }
}

/// A workspace with the office calendar and the desk set up.
async fn set_up(k: &Arc<Kernel>, slug: &str) -> W {
    let w = workspace(k, slug).await;
    k.put_logic_calendar(&w.admin().await, "office", &office()).await.unwrap();
    k.batch_customizations(&w.admin().await, desk(), false).await.map_err(|(i, e)| format!("{i:?}: {e}")).unwrap();
    w
}

async fn drain(k: &Kernel) {
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
}

#[test]
fn service_targets_keep_due_moments_and_mark_breaches_once() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> = [("package.yaml", PACKAGE), ("entities/ticket.yaml", ENTITIES), ("entities/shipment.yaml", SHIPMENTS)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let desk = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, desk]).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");

    calendars(&k).await;
    a_life(&k).await;
    reminders(&k).await;
    submitted(&k).await;
    travel(&k).await;
    by_record(&k).await;
    engine_writes(&k).await;
}

/// An app's engine write that is not a save (`set_values_tx`) moves the targets as a save does (BD-45): a paused
/// ticket put back to open by it runs again.
async fn engine_writes(k: &Arc<Kernel>) {
    let w = set_up(k, "engine").await;
    // Monday 9 November 09:00 in Kolkata; on hold at 11:00, two office hours in.
    let t = w.ticket("2026-11-09T03:30:00Z", json!({})).await;
    let id = id_of(&t);
    let t = w.set("2026-11-09T05:30:00Z", id, json!({"status": "Waiting"})).await;
    assert_eq!(kept(&t, "respond"), (None, Some("paused".into())));
    // Back on Wednesday 09:00 through the engine's write: two hours left, to 11:00 there.
    k.pin_clock(Some(at("2026-11-11T03:30:00Z")));
    let admin = w.admin().await;
    let mut tx = k.db.tenant_tx(w.tenant).await.unwrap();
    k.set_values_tx(&admin, &mut tx, "Ticket", id, obj(json!({"status": "Open"}))).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(kept(&w.get(id).await, "respond"), running("2026-11-11T05:30:00Z"));
}

/// A target counting on the calendar a record's agreement names and from the moment the record says it opened
/// (docs/builder/decisions.md BD-41).
async fn by_record(k: &Arc<Kernel>) {
    let w = set_up(k, "byrecord").await;
    let admin = w.admin().await;
    let day = json!([["09:00", "17:00"]]);
    k.put_logic_calendar(
        &admin,
        "berlin",
        &json!({"zone": "Europe/Berlin", "weekends": ["sat", "sun"], "holidays": [],
        "hours": {"mon": day, "tue": day, "wed": day, "thu": day, "fri": day}}),
    )
    .await
    .unwrap();
    k.put_logic_calendar(&admin, "round", &json!({"zone": "UTC", "weekends": [], "holidays": []})).await.unwrap();
    let field = |name: &str, f: Value| {
        let mut f = f;
        f["name"] = json!(name);
        op("Ticket", json!({"op": "add_field", "field": f}))
    };
    let target = |extra: Value| {
        let mut t = json!({"name": "agreed", "label": "Agreed", "calendar": "office", "within": "'PT4H'", "met_when": "doc.status == 'Closed'",
            "due_field": "x_agreed_by", "state_field": "x_agreed_state"});
        t.as_object_mut().unwrap().extend(obj(extra));
        op("Ticket", json!({"op": "add_target", "target": t}))
    };
    let base = vec![
        op(
            "XAgreement",
            json!({"op": "define_entity", "entity": {"name": "XAgreement", "label": "Agreement", "kind": "master", "title_field": "title",
            "fields": [{"name": "title", "type": "data", "required": true}, {"name": "hours", "type": "data"}]}}),
        ),
        field("x_agreement", json!({"type": "link", "to": "XAgreement"})),
        field("x_opened", json!({"type": "datetime"})),
        field("x_note", json!({"type": "data"})),
        field("x_agreed_by", json!({"type": "datetime"})),
        field("x_agreed_state", json!({"type": "data"})),
    ];
    // Checked when saved.
    for (bad, why) in [
        (json!({"calendar_field": "x_note.hours"}), "calendar_field"),
        (json!({"calendar_field": "nothing"}), "calendar_field"),
        (json!({"start_field": "x_note"}), "start_field"),
    ] {
        let mut ops = base.clone();
        ops.push(target(bad.clone()));
        let r = k.batch_customizations(&admin, ops, false).await;
        assert!(matches!(&r, Err((_, e)) if e.to_string().contains(why)), "{bad}: {r:?}");
    }
    let mut ops = base.clone();
    ops.push(target(json!({"calendar_field": "x_agreement.hours", "start_field": "x_opened"})));
    k.batch_customizations(&admin, ops, false).await.map_err(|(i, e)| format!("{i:?}: {e}")).unwrap();
    let admin = w.admin().await;
    let agreement = |hours: &str| {
        let (k, admin) = (k.clone(), admin.clone());
        let hours = hours.to_string();
        async move { id_of(&k.insert(&admin, "XAgreement", obj(json!({"title": hours, "hours": hours}))).await.unwrap()) }
    };
    let (berlin, round) = (agreement("berlin").await, agreement("round").await);
    let agreed = |d: &Doc| kept(d, "agreed");

    // Opened Friday 23 October 15:00 in Berlin's summer time, imported on Saturday: two hours that Friday, two on
    // Monday after the clocks went back, to 11:00 there (10:00 UTC).
    let t = w.ticket("2026-10-24T09:00:00Z", json!({"x_agreement": berlin, "x_opened": "2026-10-23T13:00:00Z"})).await;
    assert_eq!(agreed(&t), running("2026-10-26T10:00:00Z"));
    // No agreement: the target's own calendar, the office in Kolkata from the save (Monday 10:00 there to 15:00).
    let t = w.ticket("2026-11-02T04:30:00Z", json!({})).await;
    assert_eq!(agreed(&t), running("2026-11-02T09:30:00Z"));
    // A start still to come starts at the save.
    let t = w.ticket("2026-11-02T04:30:00Z", json!({"x_agreement": round, "x_opened": "2026-11-03T00:00:00Z"})).await;
    assert_eq!(agreed(&t), running("2026-11-02T08:30:00Z"));

    // Swapped mid-flight: two hours on the round-the-clock agreement, then Berlin's hours for the two left, from
    // 07:30 there, which opens at 09:00: due 11:00 there (10:00 UTC). Once: a later save leaves it.
    let t = w.ticket("2026-11-02T04:30:00Z", json!({"x_agreement": round})).await;
    let id = id_of(&t);
    assert_eq!(agreed(&t), running("2026-11-02T08:30:00Z"));
    let t = w.set("2026-11-02T06:30:00Z", id, json!({"x_agreement": berlin})).await;
    assert_eq!(agreed(&t), running("2026-11-02T10:00:00Z"));
    let t = w.set("2026-11-02T07:00:00Z", id, json!({"x_note": "still"})).await;
    assert_eq!(agreed(&t), running("2026-11-02T10:00:00Z"));
    // The opening moment corrected to Friday 16:00 in Berlin: the open hour between counted, on Berlin's hours.
    let t = w.set("2026-11-02T07:00:00Z", id, json!({"x_opened": "2026-11-02T03:30:00Z"})).await;
    assert_eq!(agreed(&t), running("2026-11-02T10:00:00Z"), "a start first given is recorded, not worked out again");
    let t = w.set("2026-11-02T07:00:00Z", id, json!({"x_opened": "2026-10-30T15:00:00Z"})).await;
    assert_eq!(agreed(&t), running("2026-11-02T09:00:00Z"));

    // Opened long ago: past due at once, breached once by the sweep, and still breached after a swap.
    let t = w.ticket("2026-11-02T10:00:00Z", json!({"x_agreement": round, "x_opened": "2026-11-01T00:00:00Z"})).await;
    let id = id_of(&t);
    assert_eq!(agreed(&t), running("2026-11-01T04:00:00Z"));
    let now = at("2026-11-02T10:00:30Z");
    assert!(k.sweep_targets_at(now).await.unwrap() >= 1);
    assert_eq!(k.sweep_targets_at(now).await.unwrap(), 0);
    assert_eq!(agreed(&w.get(id).await).1.as_deref(), Some("breached"));
    let t = w.set("2026-11-02T10:05:00Z", id, json!({"x_agreement": berlin})).await;
    assert_eq!(agreed(&t).1.as_deref(), Some("breached"));
    let breaches = w.count("select count(*) from record_target where record_id = $1 and breached_at is not null", id).await;
    assert_eq!(breaches, 1);
    k.sweep_targets_at(at("2026-11-03T10:00:00Z")).await.unwrap();
    let first = w.count("select count(*) from record_target where record_id = $1 and breached_at = '2026-11-02T10:00:30Z'", id).await;
    assert_eq!(first, 1, "recorded once, at the first sweep");
}

/// A target names a calendar that is there, and the calendar stays while a target names it.
async fn calendars(k: &Arc<Kernel>) {
    let w = workspace(k, "cal").await;
    let admin = w.admin().await;
    let r = k.batch_customizations(&admin, desk(), false).await;
    assert!(matches!(&r, Err((Some(4), e)) if e.to_string().contains("no business calendar office")), "{r:?}");
    k.put_logic_calendar(&admin, "office", &office()).await.unwrap();
    k.batch_customizations(&admin, desk(), false).await.map_err(|(i, e)| format!("{i:?}: {e}")).unwrap();
    let r = k.delete_logic_calendar(&w.admin().await, "office").await;
    assert!(matches!(&r, Err(Error::Conflict(m)) if m.contains("Ticket's target respond")), "{r:?}");
    // The description marks the two fields as the target's, and lists the targets.
    let admin = w.admin().await;
    let d = ferrum_kernel::describe::describe(&admin, admin.schema.entity("Ticket").unwrap());
    let field = |n: &str| d["fields"].as_array().unwrap().iter().find(|f| f["name"] == n).cloned().unwrap();
    assert_eq!(field("x_respond_by")["kept_by_target"], json!("respond"));
    assert_eq!(field("x_resolve_state")["kept_by_target"], json!("resolve"));
    assert!(field("subject").get("kept_by_target").is_none());
    assert_eq!(d["targets"].as_array().map(Vec::len), Some(2));
    let s = ferrum_kernel::describe::entity_schema(&admin, admin.schema.entity("Ticket").unwrap(), 0);
    assert_eq!(s["properties"]["x_respond_state"]["readOnly"], json!(true));
    // Removing the target lets the calendar go.
    let mut tx = k.db.tenant_tx(w.tenant).await.unwrap();
    let id: Uuid = sqlx::query_scalar("select id from meta_overlay where op->>'op' = 'add_target' and op->'target'->>'name' = 'respond'")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    k.add_customization(&w.admin().await, "Ticket", OverlayOp::RemoveTarget { name: "respond".into() }, None).await.unwrap();
    let rows = k.list_customizations(&w.admin().await).await.unwrap();
    assert!(rows.iter().find(|r| r.id == id).unwrap().superseded_by.is_some(), "the remove supersedes the add");
    k.delete_logic_calendar(&w.admin().await, "office").await.unwrap();
}

/// One ticket through its life, the sweep, and what does not reach other workspaces.
async fn a_life(k: &Arc<Kernel>) {
    let w = set_up(k, "life").await;
    let other = set_up(k, "other").await;
    let breached = json!([{"type": "email", "to": "'desk@example.com'", "subject": "'Breached ' + doc.subject", "body": "doc.subject"}]);
    let input = obj(json!({"name": "on breach", "entity": "Ticket", "event": "updated",
        "condition": "doc.x_respond_state == 'breached' && old.x_respond_state != 'breached'", "actions": breached}));
    automation::save(k, &w.admin().await, &input).await.unwrap();
    let mails = |w: &W| {
        let (k, tenant) = (w.k.clone(), w.tenant);
        async move {
            let mut tx = k.db.tenant_tx(tenant).await.unwrap();
            let n: i64 = sqlx::query_scalar("select count(*) from outbox where topic = 'ferrum.mail' and payload->>'subject' like 'Breached %'")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            n
        }
    };

    // Monday 9 November 09:00 in Kolkata. What people send for the kept fields is ignored.
    let t = w.ticket("2026-11-09T03:30:00Z", json!({"x_respond_state": "met", "x_respond_by": "not a date"})).await;
    let id = id_of(&t);
    assert_eq!(kept(&t, "respond"), running("2026-11-09T07:30:00Z"), "four office hours: 13:00 there");
    assert_eq!(kept(&t, "resolve"), running("2026-11-09T11:30:00Z"), "eight hours of plain time");
    let t = w.set("2026-11-09T04:00:00Z", id, json!({"x_respond_state": "met", "x_resolve_by": null})).await;
    assert_eq!((kept(&t, "respond"), kept(&t, "resolve").0), (running("2026-11-09T07:30:00Z"), Some(at("2026-11-09T11:30:00Z"))));

    // The button puts it on hold at 11:00 there, two office hours in: paused, the due moment unknown.
    k.pin_clock(Some(at("2026-11-09T05:30:00Z")));
    k.run_action(&w.admin().await, "Ticket", id, "x_hold", json!({})).await.unwrap();
    assert_eq!(kept(&w.get(id).await, "respond"), (None, Some("paused".into())));

    // Back on Wednesday 09:00 (Tuesday is a holiday): two hours left, to 11:00 there.
    let t = w.set("2026-11-11T03:30:00Z", id, json!({"status": "Open"})).await;
    assert_eq!(kept(&t, "respond"), running("2026-11-11T05:30:00Z"));
    // High priority at 10:00: an hour, and three are used up: due now.
    let t = w.set("2026-11-11T04:30:00Z", id, json!({"priority": "High"})).await;
    assert_eq!(kept(&t, "respond"), running("2026-11-11T04:30:00Z"));

    // The sweep a minute later marks it once, however many look at once, and the save it makes as the system
    // sets the state an automation reacts to. The resolve target is long overdue too.
    let now = at("2026-11-11T04:31:00Z");
    let (a, b) = tokio::join!(k.sweep_targets_at(now), k.sweep_targets_at(now));
    assert_eq!(a.unwrap() + b.unwrap(), 2, "two rounds, each marked once");
    assert_eq!(k.sweep_targets_at(now).await.unwrap(), 0, "a second pass finds nothing");
    let t = w.get(id).await;
    assert_eq!((kept(&t, "respond").1, kept(&t, "resolve").1), (Some("breached".into()), Some("breached".into())));
    drain(k).await;
    assert_eq!(mails(&w).await, 1, "the automation saw the change once");
    assert_eq!(w.count("select count(*) from record_target where record_id = $1 and breached_at is not null", id).await, 2);

    // Closed: late. The workflow resolves it: the resolve target is met (late), through the transition.
    let t = w.set("2026-11-11T05:00:00Z", id, json!({"status": "Closed"})).await;
    assert_eq!(kept(&t, "respond"), (Some(at("2026-11-11T04:30:00Z")), Some("met_late".into())));
    k.pin_clock(Some(at("2026-11-11T05:10:00Z")));
    let t = k.transition(&w.admin().await, "Ticket", id, "resolve", None).await.unwrap();
    assert_eq!(kept(&t, "resolve").1.as_deref(), Some("met_late"));
    // Re-opened: round two of each, from now.
    k.pin_clock(Some(at("2026-11-11T05:30:00Z")));
    let t = k.transition(&w.admin().await, "Ticket", id, "reopen", None).await.unwrap();
    assert_eq!(kept(&t, "resolve"), running("2026-11-11T13:30:00Z"));
    let t = w.set("2026-11-11T05:30:00Z", id, json!({"status": "Open", "priority": "Low"})).await;
    assert_eq!(kept(&t, "respond"), running("2026-11-11T10:30:00Z"), "11:00 there plus four office hours, with lunch: 16:00");
    assert_eq!(w.count("select count(*) from record_target where record_id = $1 and round = 2 and breached_at is null", id).await, 2);

    // A breach is the platform's fact: a rule the record now breaks does not keep it out of the field.
    let ruled = id_of(&w.ticket("2026-11-11T05:30:00Z", json!({"priority": "High"})).await);
    w.exec("update e_ticket set subject = 'locked' where id = $1", ruled).await;
    assert_eq!(k.sweep_targets_at(at("2026-11-11T06:31:00Z")).await.unwrap(), 1);
    assert_eq!(kept(&w.get(ruled).await, "respond").1.as_deref(), Some("breached"), "written past the rule");
    // A real failure (the calendar gone under it) leaves the breach recorded; the field catches up at the next save.
    let locked = id_of(&w.ticket("2026-11-11T05:40:00Z", json!({"priority": "High"})).await);
    w.exec("delete from logic_calendar where name = 'office' and $1::uuid is not null", locked).await;
    assert_eq!(k.sweep_targets_at(at("2026-11-11T06:41:00Z")).await.unwrap(), 1);
    assert_eq!(w.count("select count(*) from record_target where record_id = $1 and target = 'respond' and breached_at is not null", locked).await, 1);
    assert_eq!(kept(&w.get(locked).await, "respond").1.as_deref(), Some("running"), "the field could not be worked out");
    k.put_logic_calendar(&w.admin().await, "office", &office()).await.unwrap();
    let t = w.set("2026-11-11T06:45:00Z", locked, json!({})).await;
    assert_eq!(kept(&t, "respond").1.as_deref(), Some("breached"));

    // Another workspace sees none of it; a record's deletion takes its rounds.
    assert_eq!(other.count("select count(*) from record_target where $1::uuid is not null", id).await, 0);
    k.delete(&w.admin().await, "Ticket", locked, None).await.unwrap();
    assert_eq!(w.count("select count(*) from record_target where record_id = $1", locked).await, 0);
    k.pin_clock(None);
}

/// A reminder before the due moment is a time trigger relative to the due field: it fires once, and follows the
/// field when a pause moves it.
async fn reminders(k: &Arc<Kernel>) {
    let w = set_up(k, "remind").await;
    let mail = json!([{"type": "email", "to": "'desk@example.com'", "subject": "'Soon ' + doc.subject", "body": "doc.subject"}]);
    let input = obj(json!({"name": "an hour before", "entity": "Ticket", "event": "scheduled",
        "schedule": {"relative": {"field": "x_respond_by", "offset": "-PT1H"}}, "actions": mail}));
    let auto = automation::save(k, &w.admin().await, &input).await.unwrap();
    let mut tx = k.db.tenant_tx(w.tenant).await.unwrap();
    sqlx::query("update automation set scanned_until = $2 where id = $1").bind(auto).bind(at("2026-11-09T03:30:00Z")).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let scan = |now: &str| {
        let (k, now) = (k.clone(), at(now));
        async move { while k.scan_schedules_at(now, ferrum_kernel::schedule::PAGE).await.unwrap() {} }
    };

    // B is due at 13:00 there (07:30Z); C is paused an hour in and resumed on Wednesday at 09:00, three hours left:
    // due at 12:00 there (06:30Z).
    let b = id_of(&w.ticket("2026-11-09T03:30:00Z", json!({"subject": "B"})).await);
    let c = id_of(&w.ticket("2026-11-09T03:30:00Z", json!({"subject": "C"})).await);
    w.set("2026-11-09T04:30:00Z", c, json!({"status": "Waiting"})).await;
    scan("2026-11-09T06:31:00Z").await;
    scan("2026-11-09T06:35:00Z").await;
    let t = w.set("2026-11-11T03:30:00Z", c, json!({"status": "Open"})).await;
    assert_eq!(kept(&t, "respond"), running("2026-11-11T06:30:00Z"));
    scan("2026-11-11T05:31:00Z").await;
    let mut tx = k.db.tenant_tx(w.tenant).await.unwrap();
    let fired: Vec<(Uuid, At)> = sqlx::query_as("select record_id, due_at from automation_occurrence where automation_id = $1 order by due_at")
        .bind(auto)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(fired, [(b, at("2026-11-09T06:30:00Z")), (c, at("2026-11-11T05:30:00Z"))]);
    k.pin_clock(None);
}

/// A submitted document cannot be edited, yet its breach reaches its state field and an automation with `old`.
async fn submitted(k: &Arc<Kernel>) {
    let w = workspace(k, "ship").await;
    let field = |name: &str, ty: &str| op("Shipment", json!({"op": "add_field", "field": {"name": name, "type": ty}}));
    let target = op(
        "Shipment",
        json!({"op": "add_target", "target": {"name": "ship", "label": "Shipped", "start_when": "doc.docstatus == 1", "within": "'P2D'",
        "met_when": "doc.docstatus == 2", "due_field": "x_ship_by", "state_field": "x_ship_state"}}),
    );
    k.batch_customizations(&w.admin().await, vec![field("x_ship_by", "datetime"), field("x_ship_state", "data"), target], false)
        .await
        .map_err(|(i, e)| format!("{i:?}: {e}"))
        .unwrap();
    let mail = json!([{"type": "email", "to": "'yard@example.com'", "subject": "'Late ' + doc.reference", "body": "doc.reference"}]);
    let input = obj(json!({"name": "late", "entity": "Shipment", "event": "updated",
        "condition": "doc.x_ship_state == 'breached' && old.x_ship_state != 'breached'", "actions": mail}));
    automation::save(k, &w.admin().await, &input).await.unwrap();
    k.pin_clock(Some(at("2026-11-09T10:00:00Z")));
    let d = k.insert(&w.admin().await, "Shipment", obj(json!({"reference": "S-1"}))).await.unwrap();
    assert_eq!(d.get("x_ship_state"), Some(&Value::Null), "a draft has not started");
    let id = id_of(&d);
    let d = k.submit(&w.admin().await, "Shipment", id, None).await.unwrap();
    assert_eq!(d["x_ship_state"], json!("running"));
    // (Other workspaces' overdue rounds are swept in the same pass.)
    assert!(k.sweep_targets_at(at("2026-11-11T10:01:00Z")).await.unwrap() >= 1);
    let d = k.get(&w.admin().await, "Shipment", id, false).await.unwrap();
    assert_eq!((d["x_ship_state"].clone(), d["docstatus"].clone()), (json!("breached"), json!(1)), "still submitted");
    drain(k).await;
    let mut tx = k.db.tenant_tx(w.tenant).await.unwrap();
    let n: i64 =
        sqlx::query_scalar("select count(*) from outbox where topic = 'ferrum.mail' and payload->>'subject' = 'Late S-1'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(n, 1, "the automation saw it once");
    k.pin_clock(None);
}

/// Targets travel as customizations: an export imported elsewhere, and a pack carrying its calendar.
async fn travel(k: &Arc<Kernel>) {
    let w = set_up(k, "from").await;
    let set = k.export_customizations(&w.admin().await).await.unwrap();
    let to = workspace(k, "to").await;
    let r = k.import_customizations(&to.admin().await, &set).await;
    assert!(matches!(&r, Err(e) if e.to_string().contains("no business calendar office")), "{r:?}");
    k.put_logic_calendar(&to.admin().await, "office", &office()).await.unwrap();
    k.import_customizations(&to.admin().await, &set).await.unwrap();
    assert_eq!(to.admin().await.schema.entity("Ticket").unwrap().def.targets.len(), 2);

    let opts = ferrum_kernel::pack::ExportOptions { name: "desk".into(), label: "Desk".into(), version: 1, records: vec![] };
    let pack = k.export_pack(&w.admin().await, &opts).await.unwrap();
    assert_eq!(pack["calendars"][0]["name"], json!("office"));
    assert_eq!(pack["calendars"][0]["hours"], office()["hours"]);
    let fresh = workspace(k, "fresh").await;
    let plan = k.plan_pack(&fresh.admin().await, &pack).await.unwrap();
    assert_eq!(plan["sections"]["calendars"]["created"], json!(["office"]), "{plan}");
    let done = k.install_pack(&fresh.admin().await, &pack, plan["digest"].as_str().unwrap()).await.unwrap();
    assert_eq!(done["summary"]["calendars"]["created"], json!(["office"]));
    assert_eq!(fresh.admin().await.schema.entity("Ticket").unwrap().def.targets.len(), 2);
    // Where the calendar is there already it is left alone; a pack whose targets name one it lacks says so.
    let plan = k.plan_pack(&to.admin().await, &pack).await.unwrap();
    assert_eq!(plan["sections"]["calendars"]["unchanged"], json!(["office"]));
    let mut bare = pack.clone();
    bare.as_object_mut().unwrap().remove("calendars");
    let plan = k.plan_pack(&workspace(k, "bare").await.admin().await, &bare).await.unwrap();
    assert_eq!(plan["missing"], json!(["the business calendar office is not here, and the pack does not carry it"]));
}
