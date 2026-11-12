//! Schedules by absence (docs/builder/decisions.md BD-27) against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the
//! database is reset first), driven through the scan's clock as `schedule_it` is: people with no timesheet this week,
//! employees with no contract in the last year, each told once a period however many daily runs look, again in the
//! next period, and not at all when what was missing arrives between the scan and the run.

use ferrum_kernel::automation;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, EntityDef, OverlayOp};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

type At = chrono::DateTime<chrono::Utc>;

fn at(s: &str) -> At {
    chrono::DateTime::parse_from_rfc3339(s).unwrap().to_utc()
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
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

    async fn add(&self, entity: &str, v: Value) -> Uuid {
        self.k.insert(&self.admin().await, entity, obj(v)).await.unwrap()["id"].as_str().unwrap().parse().unwrap()
    }

    async fn auto(&self, name: &str, schedule: Value, subject: &str) -> ferrum_kernel::Result<Uuid> {
        let actions = json!([{"type": "email", "to": "'hr@example.com'", "subject": format!("'{subject} ' + doc.name"), "body": "doc.name"}]);
        let input = obj(json!({"name": name, "entity": "XPerson", "event": "scheduled", "schedule": schedule, "actions": actions}));
        automation::save(&self.k, &self.admin().await, &input).await
    }

    async fn looked(&self, id: Uuid, when: At) {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        sqlx::query("update automation set scanned_until = $2, scan_to = null, scan_cursor = null where id = $1")
            .bind(id)
            .bind(when)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    /// The subjects of the mail queued so far, sorted.
    async fn mails(&self) -> Vec<String> {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let mut v: Vec<String> = sqlx::query_scalar("select payload->>'subject' from outbox where topic = 'ferrum.mail'").fetch_all(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        v.sort();
        v
    }
}

async fn run(k: &Kernel, now: At) {
    while k.scan_schedules_at(now, ferrum_kernel::schedule::PAGE).await.unwrap() {}
}

async fn drain(k: &Kernel) {
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
}

async fn define(w: &W, def: Value) {
    let entity: EntityDef = serde_json::from_value(def).expect("an entity definition");
    let name = entity.name.clone();
    w.k.add_customization(&w.admin().await, &name, OverlayOp::DefineEntity { entity: Box::new(entity) }, None).await.unwrap();
}

#[test]
fn schedules_fire_for_what_is_missing_once_a_period() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let k = Kernel::boot(db, Catalog::from_packages(vec![core]).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "people".into(),
            name: "People".into(),
            admin_email: "admin@people.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into()],
            settings: Some(json!({"timezone": "UTC"})),
        })
        .await
        .expect("workspace");
    let session = k.login("people", "admin@people.test", "Correct-Horse-9").await.unwrap();
    let w = W { k: k.clone(), tenant, user: session.user };
    define(
        &w,
        json!({"name": "XPerson", "label": "Person", "kind": "master", "title_field": "name",
        "fields": [{"name": "name", "type": "data"}, {"name": "active", "type": "check", "default": true}]}),
    )
    .await;
    define(
        &w,
        json!({"name": "XTimesheet", "label": "Timesheet", "kind": "master",
        "fields": [{"name": "person", "type": "link", "to": "XPerson"}, {"name": "work_date", "type": "date"}, {"name": "hours", "type": "int"}]}),
    )
    .await;
    define(
        &w,
        json!({"name": "XPaper", "label": "Paper", "kind": "master",
        "fields": [{"name": "person", "type": "link", "to": "XPerson"}, {"name": "kind", "type": "select", "options": ["Passport", "Contract"]},
                   {"name": "issued", "type": "date"}]}),
    )
    .await;

    // ───────────── refused when saved ─────────────
    let every = |absent: Value| json!({"every": {"period": "day", "at": "09:00", "absent": absent}});
    for (bad, why) in [
        (json!({"entity": "Nothing", "link": "person", "date": "work_date", "within": "week"}), "an unknown entity"),
        (json!({"entity": "XTimesheet", "link": "hours", "date": "work_date", "within": "week"}), "not a link to the record"),
        (json!({"entity": "XTimesheet", "link": "person", "date": "hours", "within": "week"}), "not a date"),
        (json!({"entity": "XTimesheet", "link": "person", "date": "work_date", "within": "year"}), "an unknown period"),
        (json!({"entity": "XTimesheet", "link": "person", "date": "work_date", "within": "P400D"}), "too long"),
        (json!({"entity": "XTimesheet", "link": "person", "date": "work_date", "within": "week", "filter": [["nothing", "=", 1]]}), "a filter on nothing"),
        (json!({"entity": "XTimesheet", "link": "person", "date": "work_date"}), "no period"),
    ] {
        let e = w.auto("bad", every(bad.clone()), "x").await.expect_err(why);
        assert!(matches!(&e, Error::Validation(f) if f[0].field == "schedule"), "{why}: {e}");
    }

    // ───────────── people with no timesheet this week ─────────────
    let ann = w.add("XPerson", json!({"name": "Ann"})).await;
    let bob = w.add("XPerson", json!({"name": "Bob"})).await;
    w.add("XPerson", json!({"name": "Cy", "active": false})).await;
    // Ann logged Tuesday of the week of Monday 9 November; Bob logged the week before only.
    w.add("XTimesheet", json!({"person": ann.to_string(), "work_date": "2026-11-10", "hours": 8})).await;
    w.add("XTimesheet", json!({"person": bob.to_string(), "work_date": "2026-11-06", "hours": 8})).await;
    let sheets = w
        .auto(
            "No timesheet",
            json!({"every": {"period": "day", "at": "09:00", "filter": [["active", "=", 1]],
                "absent": {"entity": "XTimesheet", "link": "person", "date": "work_date", "within": "week", "filter": [["hours", ">", 0]]}}}),
            "nots",
        )
        .await
        .unwrap();
    w.looked(sheets, at("2026-11-09T08:00:00Z")).await;
    run(&k, at("2026-11-09T09:00:30Z")).await;
    drain(&k).await;
    assert_eq!(w.mails().await, ["nots Bob"], "Monday: Bob alone; Ann logged this week, Cy is not active");
    // Every later day of the week looks again and fires nothing more: once a period.
    for day in 10..=15 {
        run(&k, at(&format!("2026-11-{day}T09:00:30Z"))).await;
    }
    drain(&k).await;
    assert_eq!(w.mails().await, ["nots Bob"], "not twice in one week");
    // The next week, both again; then Bob logs before the run reaches him, and is not told.
    run(&k, at("2026-11-16T09:00:30Z")).await;
    w.add("XTimesheet", json!({"person": bob.to_string(), "work_date": "2026-11-16", "hours": 4})).await;
    drain(&k).await;
    assert_eq!(w.mails().await, ["nots Ann", "nots Bob"], "Ann in the new week; Bob logged between the scan and the run");

    // ───────────── employees with no contract in the last year ─────────────
    let dan = w.add("XPerson", json!({"name": "Dan"})).await;
    let eve = w.add("XPerson", json!({"name": "Eve"})).await;
    w.add("XPaper", json!({"person": dan.to_string(), "kind": "Contract", "issued": "2026-06-01"})).await;
    w.add("XPaper", json!({"person": eve.to_string(), "kind": "Passport", "issued": "2026-06-01"})).await;
    w.add("XPaper", json!({"person": eve.to_string(), "kind": "Contract", "issued": "2024-01-01"})).await;
    let papers = w
        .auto(
            "No contract",
            json!({"every": {"period": "week", "on": 1, "at": "10:00",
                "absent": {"entity": "XPaper", "link": "person", "date": "issued", "within": "P365D", "filter": [["kind", "=", "Contract"]]}}}),
            "nocontract",
        )
        .await
        .unwrap();
    w.looked(papers, at("2026-11-23T09:00:00Z")).await;
    // Ann, Bob and Cy have none either: the filter of this one says nothing of being active.
    run(&k, at("2026-11-23T10:00:30Z")).await;
    drain(&k).await;
    let told: Vec<String> = w.mails().await.into_iter().filter(|m| m.starts_with("nocontract")).collect();
    assert_eq!(told, ["nocontract Ann", "nocontract Bob", "nocontract Cy", "nocontract Eve"], "a passport and an old contract are not a contract this year");
}
