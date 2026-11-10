//! Automations that fire on time (docs/api.md, Automations), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first). Time is driven through the scan's clock
//! (`scan_schedules_at`) and each automation's `scanned_until`, never by waiting: a moment fires once at the
//! right instant in the workspace's zone, across a change to daylight saving, however many scans look; a
//! record changed, deleted or no longer passing between the scan and the run is judged as it is at the run.

use ferrum_kernel::automation;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, EntityDef, OverlayOp};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

type At = chrono::DateTime<chrono::Utc>;

const PACKAGE: &str = "package: clinic\nversion: 0.1.0\ndepends: [core]\n";
const ENTITIES: &str = "name: Visit\nlabel: Visit\nkind: master\ntitle_field: title\nfields:\n  - {name: title, type: data, required: true}\n  - {name: visit_on, type: date}\n  - {name: starts_at, type: datetime}\n  - {name: region, type: data}\n  - {name: status, type: data}\n";

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn at(s: &str) -> At {
    chrono::DateTime::parse_from_rfc3339(s).unwrap().to_utc()
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

    async fn visit(&self, title: &str, extra: Value) -> Uuid {
        let mut d = obj(json!({"title": title}));
        d.extend(obj(extra));
        id_of(&self.k.insert(&self.admin().await, "Visit", d).await.unwrap())
    }

    async fn set(&self, entity: &str, id: Uuid, patch: Value) {
        self.k.update(&self.admin().await, entity, id, obj(patch), None).await.unwrap();
    }

    async fn auto(&self, name: &str, entity: &str, schedule: Value, actions: Value) -> Uuid {
        let input = obj(json!({"name": name, "entity": entity, "event": "scheduled", "schedule": schedule, "actions": actions}));
        automation::save(&self.k, &self.admin().await, &input).await.unwrap()
    }

    /// Where the automation's scans start from, as if it had last looked at `when`.
    async fn looked(&self, id: Uuid, when: At) {
        self.exec("update automation set scanned_until = $2, scan_to = null, scan_cursor = null where id = $1", id, Some(when)).await;
    }

    async fn exec(&self, sql: &str, id: Uuid, when: Option<At>) {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.to_string())).bind(id);
        if let Some(w) = when {
            q = q.bind(w);
        }
        q.execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
    }

    /// Mail queued with this subject (sent or not).
    async fn mails(&self, subject: &str) -> i64 {
        self.scalar("select count(*) from outbox where topic = 'ferrum.mail' and payload->>'subject' = $1", subject).await
    }

    async fn scalar(&self, sql: &str, arg: &str) -> i64 {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(arg).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        n
    }

    /// The moments an automation has fired for, oldest first.
    async fn moments(&self, auto: Uuid) -> Vec<At> {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let v: Vec<At> = sqlx::query_scalar("select due_at from automation_occurrence where automation_id = $1 order by due_at")
            .bind(auto)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        v
    }

    /// Switches every automation of this workspace off, so later scenarios' clocks do not reach them.
    async fn retire(&self) {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        sqlx::query("update automation set is_active = false").execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
    }
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn mail(prefix: &str) -> Value {
    json!([{"type": "email", "to": "'desk@example.com'", "subject": format!("'{prefix} ' + doc.title"), "body": "doc.title"}])
}

/// Every round at `now` until no automation has more to work through.
async fn scan(k: &Kernel, now: At) {
    while k.scan_schedules_at(now, ferrum_kernel::schedule::PAGE).await.unwrap() {}
}

async fn drain(k: &Kernel) {
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
}

async fn workspace(k: &Arc<Kernel>, slug: &str, tz: &str) -> W {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["clinic".into()],
            settings: Some(json!({"timezone": tz})),
        })
        .await
        .expect("workspace");
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    W { k: k.clone(), tenant, user: session.user }
}

async fn define(w: &W, def: Value) {
    let entity: EntityDef = serde_json::from_value(def).expect("an entity definition");
    let name = entity.name.clone();
    w.k.add_customization(&w.admin().await, &name, OverlayOp::DefineEntity { entity: Box::new(entity) }, None).await.unwrap();
}

/// The "monthly charge": a custom header with a table of lines.
async fn define_charge(w: &W) {
    define(
        w,
        json!({"name": "XChargeLine", "label": "Charge Line", "kind": "child",
        "fields": [{"name": "item", "type": "data"}, {"name": "amount", "type": "currency"}]}),
    )
    .await;
    define(
        w,
        json!({"name": "XCharge", "label": "Charge", "kind": "master", "title_field": "title",
        "fields": [{"name": "title", "type": "data"}, {"name": "lease", "type": "data"},
                   {"name": "total", "type": "currency", "rollup": {"table": "lines", "op": "sum", "field": "amount"}},
                   {"name": "lines", "type": "table", "of": "XChargeLine"}]}),
    )
    .await;
}

#[test]
fn automations_fire_on_time() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/visit.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let clinic = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let k =
        Kernel::boot(db, Catalog::from_packages(vec![core, clinic]).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");

    refused(&k).await;
    kolkata(&k).await;
    daylight_saving(&k).await;
    monthly_rent(&k).await;
    every_period(&k).await;
    pages_and_downtime(&k).await;
}

/// Each invalid schedule is refused with a field error on `schedule`.
async fn refused(k: &Arc<Kernel>) {
    let w = workspace(k, "refused", "UTC").await;
    let admin = w.admin().await;
    let rel = |body: Value| json!({"relative": body});
    let every = |body: Value| json!({"every": body});
    let nine: Vec<Value> = (0..9).map(|i| json!(["title", "!=", format!("x{i}")])).collect();
    let cases = [
        rel(json!({})),
        rel(json!({"field": "title"})),
        rel(json!({"field": "nothing"})),
        rel(json!({"field": "visit_on", "offset": "3D"})),
        rel(json!({"field": "visit_on", "offset": "P1M"})),
        rel(json!({"field": "visit_on", "offset": "-P2000D"})),
        rel(json!({"field": "visit_on", "at": "9am"})),
        rel(json!({"field": "visit_on", "at": "25:00"})),
        rel(json!({"field": "visit_on", "repeat": "P2D"})),
        rel(json!({"field": "visit_on", "until": "visit_on"})),
        rel(json!({"field": "visit_on", "repeat": "P1M", "until": "title"})),
        rel(json!({"field": "visit_on", "every": "day"})),
        every(json!({"period": "year", "at": "09:00"})),
        every(json!({"period": "day"})),
        every(json!({"period": "day", "at": "09:00", "on": 1})),
        every(json!({"period": "week", "at": "09:00", "on": 8})),
        every(json!({"period": "month", "at": "09:00", "on": 29})),
        every(json!({"period": "month", "at": "09:00", "on": "first"})),
        every(json!({"period": "day", "at": "09:00", "filter": [["nothing", "=", 1]]})),
        every(json!({"period": "day", "at": "09:00", "filter": [["visit_on", ">", "not a date"]]})),
        every(json!({"period": "day", "at": "09:00", "filter": [["title", "~", "x"]]})),
        every(json!({"period": "day", "at": "09:00", "filter": nine})),
        json!({"relative": {"field": "visit_on"}, "every": {"period": "day", "at": "09:00"}}),
        json!("tomorrow"),
    ];
    for schedule in cases {
        let input = obj(json!({"name": "bad", "entity": "Visit", "event": "scheduled", "schedule": schedule, "actions": mail("bad")}));
        match automation::save(k, &admin, &input).await {
            Err(Error::Validation(fields)) => assert_eq!(fields[0].field, "schedule", "{schedule}: {fields:?}"),
            other => panic!("{schedule} was not refused with a field error: {other:?}"),
        }
    }
    let without = obj(json!({"name": "bad", "entity": "Visit", "event": "scheduled", "actions": mail("bad")}));
    assert!(matches!(automation::save(k, &admin, &without).await, Err(Error::Validation(f)) if f[0].field == "schedule"), "scheduled needs a schedule");
    let stray =
        obj(json!({"name": "bad", "entity": "Visit", "event": "created", "schedule": {"every": {"period": "day", "at": "09:00"}}, "actions": mail("bad")}));
    assert!(matches!(automation::save(k, &admin, &stray).await, Err(Error::Validation(f)) if f[0].field == "schedule"), "a schedule needs event scheduled");
    // What is accepted.
    w.auto("ok", "Visit", json!({"relative": {"field": "visit_on", "offset": "-P1DT2H30M", "at": "07:15", "repeat": "P1W", "until": "visit_on"}}), mail("ok"))
        .await;
    w.auto(
        "ok2",
        "Visit",
        json!({"every": {"period": "month", "at": "23:59", "on": "last", "filter": {"region": "north", "status": ["in", ["a", "b"]]}}}),
        mail("ok"),
    )
    .await;
    w.retire().await;
}

/// A workspace in Asia/Kolkata: a reminder three days before a visit at 09:00 there, and what happens when the
/// visit moves, is deleted, or stops passing between the scan and the run; and automations as they were.
async fn kolkata(k: &Arc<Kernel>) {
    let w = workspace(k, "kolkata", "Asia/Kolkata").await;
    let admin = w.admin().await;

    // An automation saved before schedules existed (a record event, no schedule) runs as it did.
    let old = automation::save(k, &admin, &obj(json!({"name": "on create", "entity": "Visit", "event": "created", "actions": mail("created")}))).await.unwrap();
    let schedule: Option<Value> = {
        let mut tx = k.db.tenant_tx(w.tenant).await.unwrap();
        let s = sqlx::query_scalar("select schedule from automation where id = $1").bind(old).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        s
    };
    assert_eq!(schedule, None);

    // No backfill: moments already past when the automation is saved never fire.
    w.visit("Past", json!({"visit_on": "2026-01-05"})).await;
    let late = w.auto("late", "Visit", json!({"relative": {"field": "visit_on", "offset": "-P3D"}}), mail("late")).await;
    scan(k, chrono::Utc::now() + chrono::Duration::minutes(2)).await;
    drain(k).await;
    assert_eq!(w.mails("created Past").await, 1, "the record event's automation ran as before");
    assert_eq!(w.mails("late Past").await, 0, "nothing past fires for a new automation");
    w.exec("update automation set is_active = false where id = $1", late, None).await;

    // Three days before, at 09:00 in Kolkata: 2027-01-17 09:00 +05:30 is 03:30 UTC.
    let remind = w.auto("remind", "Visit", json!({"relative": {"field": "visit_on", "offset": "-P3D", "at": "09:00"}}), mail("remind")).await;
    let gate = automation::save(
        k,
        &admin,
        &obj(json!({"name": "gate", "entity": "Visit", "event": "scheduled", "condition": "doc.status == 'open'", "actions": mail("gate"),
            "schedule": {"relative": {"field": "visit_on", "offset": "-P1D", "at": "10:00"}}})),
    )
    .await
    .unwrap();
    let v1 = w.visit("V1", json!({"visit_on": "2027-01-20"})).await;
    w.looked(remind, at("2027-01-17T03:00:00Z")).await;
    w.looked(gate, at("2027-01-17T03:00:00Z")).await;
    scan(k, at("2027-01-17T03:29:00Z")).await;
    drain(k).await;
    assert_eq!(w.mails("remind V1").await, 0, "not before its moment");
    scan(k, at("2027-01-17T03:31:00Z")).await;
    drain(k).await;
    assert_eq!(w.mails("remind V1").await, 1, "at its moment");
    assert_eq!(w.moments(remind).await, vec![at("2027-01-17T03:30:00Z")], "09:00 in Kolkata");
    scan(k, at("2027-01-17T03:45:00Z")).await;
    drain(k).await;
    assert_eq!(w.mails("remind V1").await, 1, "once");

    // Two scans at the same moment, as two replicas would: one run.
    w.visit("V2", json!({"visit_on": "2027-01-21"})).await;
    let t = at("2027-01-18T03:31:00Z");
    let (a, b) = tokio::join!(k.scan_schedules_at(t, 500), k.scan_schedules_at(t, 500));
    a.unwrap();
    b.unwrap();
    tokio::join!(drain(k), drain(k));
    assert_eq!(w.mails("remind V2").await, 1, "one run however many look");

    // A date moved later after its reminder fired fires again for the new date; one moved into the past does not.
    w.set("Visit", v1, json!({"visit_on": "2027-01-25"})).await;
    let v3 = w.visit("V3", json!({"visit_on": "2027-01-30"})).await;
    w.set("Visit", v3, json!({"visit_on": "2027-01-10"})).await;
    for day in ["2027-01-19", "2027-01-22", "2027-01-27"] {
        scan(k, at(&format!("{day}T03:31:00Z"))).await;
    }
    drain(k).await;
    assert_eq!(w.mails("remind V1").await, 2, "the new date fires too");
    assert_eq!(w.mails("remind V3").await, 0, "a date moved into the past does not fire");

    // Between the scan and the run: deleted, moved, closed, opened, switched off, deleted automation.
    let v4 = w.visit("V4", json!({"visit_on": "2027-02-05"})).await;
    let v5 = w.visit("V5", json!({"visit_on": "2027-02-05", "status": "open"})).await;
    let v6 = w.visit("V6", json!({"visit_on": "2027-02-05", "status": "closed"})).await;
    let v7 = w.visit("V7", json!({"visit_on": "2027-02-05"})).await;
    let v8 = w.visit("V8", json!({"visit_on": "2027-02-05"})).await;
    let flip = w.auto("flip", "Visit", json!({"relative": {"field": "visit_on", "offset": "-P3D", "at": "09:00"}}), mail("flip")).await;
    let gone = w.auto("gone", "Visit", json!({"relative": {"field": "visit_on", "offset": "-P3D", "at": "09:00"}}), mail("gone")).await;
    for a in [remind, gate, flip, gone] {
        w.looked(a, at("2027-02-01T00:00:00Z")).await;
    }
    // 2027-02-02 09:00 Kolkata (remind, flip, gone) and 2027-02-04 10:00 Kolkata (gate).
    scan(k, at("2027-02-02T03:31:00Z")).await;
    k.delete(&w.admin().await, "Visit", v4, None).await.unwrap();
    w.set("Visit", v7, json!({"visit_on": "2027-02-06"})).await;
    w.set("Visit", v8, json!({"visit_on": null})).await;
    let flip_off = obj(json!({"id": flip.to_string(), "name": "flip", "entity": "Visit", "event": "scheduled", "is_active": false,
        "schedule": {"relative": {"field": "visit_on", "offset": "-P3D", "at": "09:00"}}, "actions": mail("flip")}));
    automation::save(k, &admin, &flip_off).await.unwrap();
    w.exec("delete from automation where id = $1", gone, None).await;
    drain(k).await;
    assert_eq!(w.mails("remind V4").await, 0, "a record deleted before its run is skipped");
    assert_eq!(w.mails("remind V7").await, 0, "a date moved since the scan is not due at the old moment");
    assert_eq!(w.mails("remind V8").await, 0, "a date cleared since the scan cancels it");
    assert_eq!(w.mails("remind V5").await, 1, "the others ran");
    assert_eq!(w.mails("flip V5").await, 0, "an automation switched off since the scan does nothing");
    assert_eq!(w.mails("gone V5").await, 0, "nor one deleted");
    scan(k, at("2027-02-03T03:31:00Z")).await;
    drain(k).await;
    assert_eq!(w.mails("remind V7").await, 1, "the moved date fires at its new moment");
    scan(k, at("2027-02-04T04:29:00Z")).await;
    w.set("Visit", v5, json!({"status": "closed"})).await;
    w.set("Visit", v6, json!({"status": "open"})).await;
    drain(k).await;
    scan(k, at("2027-02-04T04:31:00Z")).await;
    w.set("Visit", v5, json!({"status": "open"})).await;
    w.set("Visit", v6, json!({"status": "closed"})).await;
    drain(k).await;
    assert_eq!(w.mails("gate V5").await, 1, "the condition is judged on the record as it is at the run: open");
    assert_eq!(w.mails("gate V6").await, 0, "closed at the run");
    assert_eq!(w.scalar("select count(*) from automation_run r where r.status = 'skipped' and r.automation_id::text = $1", &gate.to_string()).await, 3);
    w.retire().await;
}

/// A workspace in New York, across the start of daylight saving on 14 March 2027.
async fn daylight_saving(k: &Arc<Kernel>) {
    let w = workspace(k, "newyork", "America/New_York").await;
    let date = w.auto("date", "Visit", json!({"relative": {"field": "visit_on", "offset": "-P3D", "at": "09:00"}}), mail("date")).await;
    let hours = w.auto("hours", "Visit", json!({"relative": {"field": "starts_at", "offset": "-PT2H"}}), mail("hours")).await;
    let day = w.auto("day", "Visit", json!({"relative": {"field": "starts_at", "offset": "-P1D"}}), mail("day")).await;
    w.visit("Before", json!({"visit_on": "2027-03-16"})).await; // 13 March 09:00 EST = 14:00 UTC
    w.visit("After", json!({"visit_on": "2027-03-17"})).await; // 14 March 09:00 EDT = 13:00 UTC
    // 14 March 09:00 EDT, a day before: 13 March 09:00 EST, 14:00 UTC (not 13:00: a day is wall-clock).
    w.visit("Start", json!({"starts_at": "2027-03-14T13:00:00Z"})).await;
    for a in [date, hours, day] {
        w.looked(a, at("2027-03-12T00:00:00Z")).await;
    }
    scan(k, at("2027-03-15T00:00:00Z")).await;
    drain(k).await;
    assert_eq!(w.moments(date).await, vec![at("2027-03-13T14:00:00Z"), at("2027-03-14T13:00:00Z")]);
    assert_eq!(w.moments(hours).await, vec![at("2027-03-14T11:00:00Z")], "two hours before");
    assert_eq!(w.moments(day).await, vec![at("2027-03-13T14:00:00Z")], "the same time of day, the day before");
    assert_eq!(w.mails("date Before").await + w.mails("date After").await + w.mails("hours Start").await + w.mails("day Start").await, 4);
    w.retire().await;
}

/// A custom lease with custom dates charges its rent each month, as a custom charge with lines, from its
/// start until its end, and stops.
async fn monthly_rent(k: &Arc<Kernel>) {
    let w = workspace(k, "rent", "Asia/Kolkata").await;
    define_charge(&w).await;
    define(
        &w,
        json!({"name": "XLease", "label": "Lease", "kind": "master", "title_field": "title",
        "fields": [{"name": "title", "type": "data"}, {"name": "start_on", "type": "date"}, {"name": "end_on", "type": "date"},
                   {"name": "rent", "type": "currency"}]}),
    )
    .await;
    let admin = w.admin().await;
    let rent = w
        .auto(
            "rent",
            "XLease",
            json!({"relative": {"field": "start_on", "at": "08:00", "repeat": "P1M", "until": "end_on"}}),
            json!([{"type": "create_document", "entity": "XCharge",
                    "values": {"title": "'Rent ' + doc.title", "lease": "doc.title", "lines": "[{'item': 'rent', 'amount': doc.rent}]"}}]),
        )
        .await;
    k.insert(&admin, "XLease", obj(json!({"title": "Flat 4", "start_on": "2027-01-31", "end_on": "2027-05-15", "rent": "1200.00"}))).await.unwrap();
    w.looked(rent, at("2027-01-30T00:00:00Z")).await;
    let mut t = at("2027-01-30T00:00:00Z");
    while t < at("2027-06-20T00:00:00Z") {
        t += chrono::Duration::days(6);
        scan(k, t).await;
        drain(k).await;
    }
    // 08:00 in Kolkata is 02:30 UTC; a month after 31 January is 28 February, then 31 March and 30 April.
    assert_eq!(w.moments(rent).await, vec![at("2027-01-31T02:30:00Z"), at("2027-02-28T02:30:00Z"), at("2027-03-31T02:30:00Z"), at("2027-04-30T02:30:00Z")]);
    let charges = k.list(&admin, "XCharge", &Default::default()).await.unwrap().rows;
    assert_eq!(charges.len(), 4, "one charge a month until the lease ends");
    for c in &charges {
        let c = k.get(&admin, "XCharge", id_of(c), false).await.unwrap();
        assert_eq!(c["lease"], "Flat 4");
        assert_eq!(c["lines"].as_array().unwrap().len(), 1, "{c:?}");
        assert_eq!(c["total"], json!("1200"), "{c:?}");
    }
    w.retire().await;
}

/// `every`: the first of the month for records that pass a filter, each Wednesday, and the month's last day;
/// mail made with a template.
async fn every_period(k: &Arc<Kernel>) {
    let w = workspace(k, "every", "UTC").await;
    for (t, r) in [("N1", "north"), ("N2", "north"), ("S1", "south")] {
        w.visit(t, json!({"region": r})).await;
    }
    let monthly =
        w.auto("monthly", "Visit", json!({"every": {"period": "month", "on": 1, "at": "07:00", "filter": [["region", "=", "north"]]}}), mail("monthly")).await;
    let weekly = w
        .auto(
            "weekly",
            "Visit",
            json!({"every": {"period": "week", "on": 3, "at": "07:00"}}),
            json!([{"type": "email", "to": "'desk@example.com'", "subject": "'weekly ' + doc.title", "body": "''", "template": "<p>Week for {{ doc.title }}</p>"}]),
        )
        .await;
    let last = w.auto("last", "Visit", json!({"every": {"period": "month", "on": "last", "at": "18:00"}}), mail("last")).await;
    let daily = w.auto("daily", "Visit", json!({"every": {"period": "day", "at": "06:30", "filter": {"region": "south"}}}), mail("daily")).await;
    for a in [monthly, weekly, last, daily] {
        w.looked(a, at("2027-02-23T23:00:00Z")).await;
    }
    scan(k, at("2027-03-02T00:00:00Z")).await;
    drain(k).await;
    for t in ["N1", "N2"] {
        assert_eq!(w.mails(&format!("monthly {t}")).await, 1, "{t} passes the filter");
    }
    assert_eq!(w.mails("monthly S1").await, 0, "S1 does not");
    assert_eq!(w.moments(monthly).await, vec![at("2027-03-01T07:00:00Z"); 2]);
    assert_eq!(w.moments(weekly).await, vec![at("2027-02-24T07:00:00Z"); 3], "Wednesday 24 February");
    assert_eq!(w.moments(last).await, vec![at("2027-02-28T18:00:00Z"); 3], "28 February, the last day");
    let days: Vec<At> = ["24", "25", "26", "27", "28"].iter().map(|d| at(&format!("2027-02-{d}T06:30:00Z"))).chain([at("2027-03-01T06:30:00Z")]).collect();
    assert_eq!(w.moments(daily).await, days, "each day at 06:30, for the one record in the south");
    let html: String = {
        let mut tx = k.db.tenant_tx(w.tenant).await.unwrap();
        let p: Value = sqlx::query_scalar("select payload from outbox where topic = 'ferrum.mail' and payload->>'subject' = 'weekly N1'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        p["html"].as_str().unwrap().to_string()
    };
    assert!(html.contains("<p>Week for N1</p>"), "{html}");
    scan(k, at("2027-03-02T23:00:00Z")).await;
    drain(k).await;
    assert_eq!(w.mails("monthly N1").await, 1, "once a month");
    w.retire().await;
}

/// More due records than a page holds are worked through over rounds, none lost or doubled; after downtime
/// longer than the catch-up bound, old moments are skipped and recent ones fire.
async fn pages_and_downtime(k: &Arc<Kernel>) {
    let w = workspace(k, "bulk", "UTC").await;
    let bulk = w.auto("bulk", "Visit", json!({"relative": {"field": "visit_on", "at": "09:00"}}), mail("bulk")).await;
    for i in 0..7 {
        w.visit(&format!("B{i}"), json!({"visit_on": "2027-04-01"})).await;
    }
    w.looked(bulk, at("2027-04-01T08:00:00Z")).await;
    let t = at("2027-04-01T09:01:00Z");
    let mut rounds = Vec::new();
    loop {
        let more = k.scan_schedules_at(t, 3).await.unwrap();
        rounds.push(w.moments(bulk).await.len());
        if !more {
            break;
        }
    }
    assert_eq!(rounds, vec![3, 6, 7], "a page of three a round");
    drain(k).await;
    for i in 0..7 {
        assert_eq!(w.mails(&format!("bulk B{i}")).await, 1, "B{i} once");
    }
    assert_eq!(w.scalar("select count(*) from outbox where topic = $1", "Visit.scheduled").await, 7, "one event each");

    // Down from 1 to 21 April: the moment ten days before the clock is skipped, the one two days before fires.
    w.visit("Old", json!({"visit_on": "2027-04-11"})).await;
    w.visit("Recent", json!({"visit_on": "2027-04-19"})).await;
    scan(k, at("2027-04-21T09:00:30Z")).await;
    drain(k).await;
    assert_eq!(w.mails("bulk Old").await, 0, "more than seven days late: skipped");
    assert_eq!(w.mails("bulk Recent").await, 1, "within seven days: fired");

    // Occurrences older than 30 days are pruned; the rest stay.
    let record = w.visit("Pruned", json!({})).await;
    let mut tx = k.db.tenant_tx(w.tenant).await.unwrap();
    sqlx::query(
        "insert into automation_occurrence (tenant_id, automation_id, record_id, due_at) \
         values ($1, $2, $3, now() - interval '31 days'), ($1, $2, $3, now() - interval '29 days')",
    )
    .bind(w.tenant)
    .bind(bulk)
    .bind(record)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(k.prune_occurrences().await.unwrap(), 1, "the one older than 30 days");
    assert_eq!(w.moments(bulk).await.len(), 9, "the rest stay");
    w.retire().await;
}
