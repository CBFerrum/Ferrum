//! End-to-end Projects test against a real PostgreSQL (`FERRUM_PROJECTS_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_projects_test`). The database is reset first.
//!
//! A consultancy plans a time-and-materials and a fixed price project: task trees and dependencies
//! (with cycle and start refusals), timesheets with per-user rates, overlap checks (also under
//! concurrent submits), own-only access for staff, invoicing time and milestones (twice, after
//! deleting a draft, after cancelling), cancel/amend consistency, forced completion, the overdue
//! job and the three reports.

use chrono::{Duration, NaiveDate, Utc};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, Result};
use ferrum_meta::Catalog;
use ferrum_meta::catalog::load_package_dir;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn dec(v: &Value) -> Decimal {
    match v {
        Value::String(s) => Decimal::from_str(s).unwrap_or_else(|_| panic!("not a decimal: {s}")),
        Value::Number(n) => Decimal::from_str(&n.to_string()).unwrap(),
        Value::Null => Decimal::ZERO,
        other => panic!("not a decimal: {other}"),
    }
}

fn n(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn uid(doc: &Doc) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

fn vid(v: &Value) -> Uuid {
    v["id"].as_str().unwrap().parse().unwrap()
}

fn field_error<T: std::fmt::Debug>(r: &Result<T>, field: &str, needle: &str) -> bool {
    matches!(r, Err(Error::Validation(e)) if e.iter().any(|f| f.field == field && f.message.contains(needle)))
}

fn invalid<T: std::fmt::Debug>(r: &Result<T>, needle: &str) -> bool {
    matches!(r, Err(Error::Invalid(m)) if m.contains(needle))
}

async fn fresh_kernel() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_PROJECTS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_projects_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    // Projects and what it depends on, so packages being written elsewhere cannot break this test
    // (`cargo test -p ferrum-meta` validates every package together).
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages");
    let packages = ["core", "accounting", "inventory", "selling", "projects"].map(|p| load_package_dir(&dir.join(p)).expect("package"));
    let catalog = Catalog::from_packages(packages.to_vec()).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_pkg_projects::register(&mut hooks);
    for source in ferrum_pkg_projects::reports() {
        ferrum_domain::reports::add_source(source);
    }
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("domain maintenance");
    k
}

/// One signed-in user driving the kernel API.
struct U {
    k: Arc<Kernel>,
    ctx: Ctx,
}

impl U {
    async fn try_insert(&self, entity: &str, v: Value) -> Result<Doc> {
        self.k.insert(&self.ctx, entity, obj(v)).await
    }
    async fn insert(&self, entity: &str, v: Value) -> Doc {
        self.try_insert(entity, v).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }
    async fn try_update(&self, entity: &str, id: Uuid, v: Value) -> Result<Doc> {
        self.k.update(&self.ctx, entity, id, obj(v), None).await
    }
    async fn update(&self, entity: &str, id: Uuid, v: Value) -> Doc {
        self.try_update(entity, id, v).await.unwrap_or_else(|e| panic!("update {entity}: {e:?}"))
    }
    async fn submit(&self, entity: &str, id: Uuid) -> Doc {
        self.k.submit(&self.ctx, entity, id, None).await.unwrap_or_else(|e| panic!("submit {entity}: {e:?}"))
    }
    async fn cancel(&self, entity: &str, id: Uuid) -> Doc {
        self.k.cancel(&self.ctx, entity, id, None).await.unwrap_or_else(|e| panic!("cancel {entity}: {e:?}"))
    }
    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.ctx, entity, id, false).await.unwrap_or_else(|e| panic!("get {entity}: {e:?}"))
    }
    async fn try_action(&self, entity: &str, id: Uuid, action: &str, params: Value) -> Result<Value> {
        self.k.run_action(&self.ctx, entity, id, action, params).await
    }
    async fn action(&self, entity: &str, id: Uuid, action: &str, params: Value) -> Value {
        self.try_action(entity, id, action, params).await.unwrap_or_else(|e| panic!("{entity}.{action}: {e:?}"))
    }
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() };
        let page = self.k.list(&self.ctx, entity, &q).await.unwrap();
        assert_eq!(page.rows.len(), 1, "{entity} {filters}");
        page.rows[0].clone()
    }
    async fn report(&self, name: &str, params: &[(&str, String)]) -> Result<Value> {
        let p: HashMap<String, String> = params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        ferrum_domain::reports::run(&self.k, &self.ctx, name, &p).await
    }
}

#[test]
fn projects_end_to_end() {
    // Same runtime shape as the server: unoptimized async hook chains need more than 2 MiB.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let k = fresh_kernel().await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme Consulting".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["projects".into()],
            settings: Some(json!({"fiscal_year_start_month": 4})),
        })
        .await
        .expect("tenant");
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.expect("login");
    let admin = U { k: k.clone(), ctx: k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx") };
    let mut users = HashMap::new();
    for (email, role) in [("alice", "Projects User"), ("bob", "Projects User"), ("pm", "Projects Manager"), ("acc", "Accounts Manager")] {
        let id = k.create_user(&admin.ctx, &format!("{email}@acme.test"), email, Some("Correct-Horse-9"), &[role.into()], false).await.expect("user");
        users.insert(email, U { k: k.clone(), ctx: k.load_ctx(tenant, id, None, "it".into()).await.expect("user ctx") });
    }
    let (alice, bob, pm, acc) = (&users["alice"], &users["bob"], &users["pm"], &users["acc"]);

    let today = Utc::now().date_naive();
    let (d1, d2, d3) = (today - Duration::days(3), today - Duration::days(2), today - Duration::days(1));
    let at = |day: NaiveDate, hm: &str| format!("{day}T{hm}:00Z");

    // ───────────── masters ─────────────
    let inr = admin.find_one("Currency", json!({"code": "INR"})).await;
    let company = admin.insert("Company", json!({"company_name": "Acme Consulting Pvt Ltd", "abbr": "AC", "default_currency": inr["id"]})).await;
    let cid = uid(&company).to_string();
    let company = admin.get("Company", uid(&company)).await;
    let globex = uid(&admin.insert("Customer", json!({"customer_name": "Globex"})).await).to_string();
    let services = admin.find_one("ItemGroup", json!({"group_name": "Services"})).await;
    let hour = admin.find_one("Uom", json!({"uom_name": "Hour"})).await;
    let service = |code: &str| json!({"item_code": code, "item_name": code, "item_group": services["id"], "stock_uom": hour["id"], "is_stock_item": false, "is_purchased": false});
    let svc_dev = uid(&admin.insert("Item", service("SVC-DEV")).await).to_string();
    let svc_consult = uid(&admin.insert("Item", service("SVC-CONSULT")).await).to_string();
    let svc_fixed = uid(&admin.insert("Item", service("SVC-FIXED")).await).to_string();

    let dev = uid(&pm.insert("ActivityType", json!({"activity_type": "Development", "billing_rate": "100", "costing_rate": "60", "item": svc_dev})).await)
        .to_string();
    let consult =
        uid(&pm.insert("ActivityType", json!({"activity_type": "Consulting", "billing_rate": "150", "costing_rate": "80", "item": svc_consult})).await)
            .to_string();
    let internal = uid(&pm.insert("ActivityType", json!({"activity_type": "Internal", "costing_rate": "50"})).await).to_string();
    let stocked = admin.insert("Item", json!({"item_code": "WIDGET", "item_name": "Widget", "item_group": services["id"], "stock_uom": hour["id"]})).await;
    let r = pm.try_insert("ActivityType", json!({"activity_type": "Bad", "item": stocked["id"]})).await;
    assert!(field_error(&r, "item", "is_stock_item"), "a stock item cannot bill time: {r:?}");
    pm.insert("ActivityCost", json!({"user": "Alice@Acme.test", "activity_type": dev, "billing_rate": "120", "costing_rate": "70"})).await;
    let r = pm.try_insert("ActivityCost", json!({"user": "alice@acme.test", "activity_type": dev, "billing_rate": "130"})).await;
    assert!(matches!(r, Err(Error::Conflict(_))), "one rate per user and activity: {r:?}");
    let r = pm.try_insert("ActivityCost", json!({"user": "nobody@acme.test", "activity_type": dev, "billing_rate": "130"})).await;
    assert!(field_error(&r, "user", "no user"), "{r:?}");

    // ───────────── projects ─────────────
    let website = pm
        .insert(
            "Project",
            json!({"project_name": "Website", "company": cid, "customer": globex, "proj_billing_method": "Time and Materials",
                   "proj_project_manager": "pm@acme.test", "proj_estimated_hours": "60", "start_date": d1.to_string()}),
        )
        .await;
    let web = uid(&website);
    assert_eq!(website["proj_percent_complete_method"], json!("Task Progress"));
    assert_eq!(website["currency"], inr["id"], "fetched from the company");
    let r = pm.try_insert("Project", json!({"project_name": "No Value", "company": cid, "customer": globex, "proj_billing_method": "Fixed Price"})).await;
    assert!(field_error(&r, "proj_contract_value", "required") || field_error(&r, "proj_contract_value", "contract value"), "{r:?}");
    let r = pm.try_insert("Project", json!({"project_name": "Ghost PM", "company": cid, "proj_project_manager": "ghost@acme.test"})).await;
    assert!(field_error(&r, "proj_project_manager", "no user"), "{r:?}");
    let erp = uid(&pm
        .insert(
            "Project",
            json!({"project_name": "ERP Rollout", "company": cid, "customer": globex, "proj_billing_method": "Fixed Price",
                   "proj_contract_value": "10000", "proj_billing_item": svc_fixed}),
        )
        .await);

    // ───────────── tasks: tree, dependencies, starts ─────────────
    let task = |subject: &str, hours: &str, assignees: &[&str]| {
        json!({"subject": subject, "project": web.to_string(), "expected_hours": hours,
               "assigned_to": assignees.iter().map(|a| json!({"user": format!("{a}@acme.test")})).collect::<Vec<_>>()})
    };
    let design = uid(&pm.insert("Task", task("Design", "10", &["alice"])).await);
    let design_doc = pm.get("Task", design).await;
    assert_eq!(design_doc["company"], json!(cid), "company follows the project");
    let mut build_input = task("Build", "30", &["alice", "bob"]);
    build_input["depends_on"] = json!([{"task": design.to_string()}]);
    let build = uid(&pm.insert("Task", build_input).await);
    let mut test_input = task("Test", "20", &["bob"]);
    test_input["depends_on"] = json!([{"task": build.to_string()}]);
    let test = uid(&pm.insert("Task", test_input).await);
    let sub = uid(&pm.insert("Task", json!({"subject": "Wireframes", "project": web.to_string(), "parent_task": design.to_string()})).await);

    let r = pm.try_update("Task", design, json!({"depends_on": [{"task": test.to_string()}]})).await;
    assert!(field_error(&r, "depends_on", "cycle"), "indirect cycle design → test → build → design: {r:?}");
    let r = pm.try_update("Task", design, json!({"depends_on": [{"task": build.to_string()}]})).await;
    assert!(field_error(&r, "depends_on", "cycle"), "direct cycle: {r:?}");
    let r = pm.try_update("Task", design, json!({"depends_on": [{"task": design.to_string()}]})).await;
    assert!(field_error(&r, "task", "itself"), "{r:?}");
    let r = pm.try_update("Task", build, json!({"depends_on": [{"task": design.to_string()}, {"task": design.to_string()}]})).await;
    assert!(field_error(&r, "task", "twice"), "{r:?}");
    let r = pm.try_update("Task", design, json!({"parent_task": sub.to_string()})).await;
    assert!(field_error(&r, "parent_task", "cycle"), "the task tree refuses cycles too: {r:?}");
    let r = pm.try_update("Task", build, json!({"status": "Working"})).await;
    assert!(field_error(&r, "status", "until these tasks are completed"), "cannot start before Design is done: {r:?}");
    let r = pm.try_update("Task", design, json!({"status": "Completed"})).await;
    assert!(field_error(&r, "status", "sub-tasks"), "a parent closes after its sub-tasks: {r:?}");
    pm.update("Task", sub, json!({"status": "Completed"})).await;
    let done = pm.update("Task", design, json!({"status": "Completed"})).await;
    assert_eq!((dec(&done["progress"]), done["completed_on"].clone()), (n("100"), json!(today.to_string())));
    pm.update("Task", build, json!({"status": "Working", "progress": "50"})).await;
    // (100 × 10 + 100 × 0 + 50 × 30 + 0 × 20) / 60 hours
    assert_eq!(dec(&pm.get("Project", web).await["proj_percent_complete"]), n("41.67"));

    // Staff see and change the tasks assigned to them (owner_fields), and never billing.
    let seen: Vec<String> =
        bob.k.list(&bob.ctx, "Task", &ListQuery::default()).await.unwrap().rows.iter().map(|t| t["subject"].as_str().unwrap().to_string()).collect();
    assert_eq!(seen.len(), 2, "Bob is assigned to Build and Test only: {seen:?}");
    assert!(seen.contains(&"Build".to_string()) && seen.contains(&"Test".to_string()));
    let r = bob.k.get(&bob.ctx, "Task", design, false).await;
    assert!(matches!(r, Err(Error::NotFound(_))), "Design is not Bob's: {r:?}");
    let r = bob.try_update("Task", design, json!({"description": "bob was here"})).await;
    assert!(matches!(r, Err(Error::NotFound(_))), "nor can he change it: {r:?}");
    bob.update("Task", test, json!({"description": "Regression suite ready"})).await;
    alice.update("Task", design, json!({"description": "Signed off by the client"})).await;
    let r = alice.try_update("Task", build, json!({"billing_percent": "10"})).await;
    assert!(field_error(&r, "billing_percent", "not permitted"), "{r:?}");

    // ───────────── timesheets ─────────────
    let ts1_input = json!({"company": cid, "time_logs": [
        {"activity_type": dev, "task": design.to_string(), "from_time": at(d1, "09:00"), "to_time": at(d1, "12:00")},
        {"activity_type": consult, "project": web.to_string(), "from_time": at(d1, "13:00"), "hours": "2.5"},
        {"activity_type": internal, "from_time": at(d1, "16:00"), "to_time": at(d1, "17:00")}]});
    let ts1 = alice.insert("Timesheet", ts1_input.clone()).await;
    let logs = ts1["time_logs"].as_array().unwrap().clone();
    assert_eq!(ts1["user"], json!("alice@acme.test"), "defaults to the signed-in user");
    assert_eq!(logs[0]["project"], json!(web.to_string()), "project derived from the task");
    assert_eq!((dec(&logs[0]["hours"]), dec(&logs[0]["billing_rate"]), dec(&logs[0]["billing_amount"])), (n("3"), n("120"), n("360")), "Alice's rate");
    assert_eq!(logs[1]["to_time"], json!(format!("{d1}T15:30:00.000Z")), "end derived from the hours");
    assert_eq!(dec(&logs[1]["billing_amount"]), n("375"));
    assert_eq!((logs[2]["is_billable"].clone(), dec(&logs[2]["billing_amount"])), (json!(false), Decimal::ZERO), "no project, not billable");
    assert!(logs[0].get("costing_rate").is_none() && ts1.get("total_costing_amount").is_none(), "staff do not see costing");
    assert_eq!((dec(&ts1["total_hours"]), dec(&ts1["total_billable_amount"]), ts1["status"].clone()), (n("6.5"), n("735"), json!("Draft")));
    let ts1_id = uid(&ts1);
    let costed = pm.get("Timesheet", ts1_id).await;
    assert_eq!(dec(&costed["total_costing_amount"]), n("460"), "3 × 70 + 2.5 × 80 + 1 × 50");

    let bad = |logs: Value| json!({"company": cid, "time_logs": logs});
    let r = alice
        .try_insert("Timesheet", bad(json!([{"activity_type": dev, "project": web.to_string(), "from_time": at(d1, "18:00"), "to_time": at(d1, "17:00")}])))
        .await;
    assert!(field_error(&r, "to_time", "after the start"), "{r:?}");
    let r = alice.try_insert("Timesheet", bad(json!([{"activity_type": dev, "project": erp.to_string(), "task": design.to_string(), "from_time": at(d1, "18:00"), "to_time": at(d1, "19:00")}]))).await;
    assert!(field_error(&r, "task", "another project"), "{r:?}");
    let r = alice
        .try_insert(
            "Timesheet",
            bad(json!([
            {"activity_type": dev, "project": web.to_string(), "from_time": at(d1, "18:00"), "to_time": at(d1, "20:00")},
            {"activity_type": dev, "project": web.to_string(), "from_time": at(d1, "19:00"), "to_time": at(d1, "21:00")}])),
        )
        .await;
    assert!(field_error(&r, "from_time", "overlaps time log 1"), "{r:?}");
    let r = alice
        .try_insert(
            "Timesheet",
            json!({"company": cid, "user": "bob@acme.test", "time_logs": [{"activity_type": dev, "from_time": at(d1, "18:00"), "hours": "1"}]}),
        )
        .await;
    assert!(matches!(r, Err(Error::Forbidden(_))), "staff record only their own time: {r:?}");

    let ts1 = alice.submit("Timesheet", ts1_id).await;
    assert_eq!(ts1["status"], json!("Submitted"));
    assert!(ts1["number"].as_str().unwrap().starts_with("TS-"));
    let p = pm.get("Project", web).await;
    assert_eq!(
        (dec(&p["proj_actual_hours"]), dec(&p["proj_costing_amount"]), dec(&p["proj_billable_amount"]), p["proj_actual_start_date"].clone()),
        (n("5.5"), n("410"), n("735"), json!(d1.to_string()))
    );
    assert_eq!(dec(&pm.get("Task", design).await["actual_hours"]), n("3"));

    let r = alice
        .try_insert("Timesheet", bad(json!([{"activity_type": dev, "project": web.to_string(), "from_time": at(d1, "11:00"), "to_time": at(d1, "12:30")}])))
        .await;
    assert!(field_error(&r, "from_time", "already submitted"), "Alice already booked 11:00-12:00: {r:?}");
    let ts3 = bob
        .insert("Timesheet", bad(json!([{"activity_type": dev, "task": build.to_string(), "from_time": at(d1, "09:00"), "to_time": at(d1, "11:00")}])))
        .await;
    assert_eq!(dec(&ts3["total_billable_amount"]), n("200"), "Bob has no own rate: the activity's 100");
    let ts3_id = uid(&ts3);
    bob.submit("Timesheet", ts3_id).await;
    let r = bob.k.get(&bob.ctx, "Timesheet", ts1_id, false).await;
    assert!(matches!(r, Err(Error::NotFound(_))), "Bob cannot see Alice's timesheet: {r:?}");
    let mine = bob.k.list(&bob.ctx, "Timesheet", &ListQuery::default()).await.unwrap();
    assert!(mine.rows.iter().all(|t| t["user"] == json!("bob@acme.test")) && mine.rows.len() == 1, "{:?}", mine.rows);
    // Time a manager records for Alice is Alice's (owner field `user`), not Bob's.
    let entered = pm
        .insert(
            "Timesheet",
            json!({"company": cid, "user": "alice@acme.test", "time_logs": [{"activity_type": internal, "from_time": at(d3, "13:00"), "hours": "1"}]}),
        )
        .await;
    let entered = uid(&entered);
    assert_eq!(alice.get("Timesheet", entered).await["user"], json!("alice@acme.test"), "Alice sees the timesheet entered for her");
    let hers = alice.k.list(&alice.ctx, "Timesheet", &ListQuery::default()).await.unwrap();
    assert!(hers.rows.iter().any(|t| t["id"] == json!(entered.to_string())), "and finds it in her list");
    assert!(matches!(bob.k.get(&bob.ctx, "Timesheet", entered, false).await, Err(Error::NotFound(_))));
    alice.k.delete(&alice.ctx, "Timesheet", entered, None).await.expect("Alice owns it, so she may discard the draft");

    // Two overlapping timesheets of one user submitted at the same moment: exactly one wins.
    let race = bad(json!([{"activity_type": consult, "project": web.to_string(), "from_time": at(d2, "09:00"), "to_time": at(d2, "10:00")}]));
    let (a, b) = (uid(&alice.insert("Timesheet", race.clone()).await), uid(&alice.insert("Timesheet", race).await));
    let (ra, rb) = tokio::join!(alice.k.submit(&alice.ctx, "Timesheet", a, None), alice.k.submit(&alice.ctx, "Timesheet", b, None));
    assert_eq!([ra.is_ok(), rb.is_ok()].iter().filter(|x| **x).count(), 1, "exactly one overlapping submit passes: {ra:?} / {rb:?}");
    let ts4 = bob
        .insert("Timesheet", bad(json!([{"activity_type": dev, "task": build.to_string(), "from_time": at(d2, "14:00"), "to_time": at(d2, "16:00")}])))
        .await;
    let ts4_id = uid(&ts4);
    bob.submit("Timesheet", ts4_id).await;
    let p = pm.get("Project", web).await;
    assert_eq!((dec(&p["proj_actual_hours"]), dec(&p["proj_costing_amount"]), dec(&p["proj_billable_amount"])), (n("10.5"), n("730"), n("1285")));
    assert_eq!(dec(&pm.get("Task", build).await["actual_hours"]), n("4"));
    // Time on a fixed price project is never billed by the hour.
    let ts5 = alice
        .insert("Timesheet", bad(json!([{"activity_type": dev, "project": erp.to_string(), "from_time": at(d3, "10:00"), "to_time": at(d3, "11:00")}])))
        .await;
    assert_eq!(ts5["time_logs"][0]["is_billable"], json!(false));
    alice.submit("Timesheet", uid(&ts5)).await;

    // ───────────── invoicing time ─────────────
    let r = alice.try_action("Timesheet", ts1_id, "make_sales_invoice", json!({})).await;
    assert!(matches!(r, Err(Error::Forbidden(_))), "staff cannot invoice: {r:?}");
    let draft = pm.action("Timesheet", ts1_id, "make_sales_invoice", json!({})).await;
    let si = acc.get("SalesInvoice", vid(&draft)).await;
    assert_eq!((dec(&si["net_total"]), si["project"].clone(), si["customer"].clone()), (n("735"), json!(web.to_string()), json!(globex)));
    let lines = si["items"].as_array().unwrap();
    assert_eq!(lines.len(), 2, "one line per activity and rate: {lines:?}");
    let dev_line = lines.iter().find(|l| l["item"] == json!(svc_dev)).unwrap();
    assert_eq!((dec(&dev_line["qty"]), dec(&dev_line["rate"]), dev_line["proj_time_logs"].as_array().unwrap().len()), (n("3"), n("120"), 1));
    assert_eq!(acc.get("Timesheet", ts1_id).await["status"], json!("Submitted"), "a draft bills nothing yet");
    let r = pm.try_action("Timesheet", ts1_id, "make_sales_invoice", json!({})).await;
    assert!(invalid(&r, "draft invoice"), "the logs are on the pending draft: {r:?}");
    pm.k.delete(&pm.ctx, "SalesInvoice", vid(&draft), None).await.expect("delete draft");
    let si1 = vid(&pm.action("Timesheet", ts1_id, "make_sales_invoice", json!({})).await);
    acc.submit("SalesInvoice", si1).await;
    let ts1 = acc.get("Timesheet", ts1_id).await;
    assert_eq!((ts1["status"].clone(), dec(&ts1["per_billed"]), dec(&ts1["total_billed_amount"])), (json!("Billed"), n("100"), n("735")));
    assert!(ts1["time_logs"][0]["billed"] == json!(true) && ts1["time_logs"][0]["sales_invoice"] == json!(si1.to_string()));
    assert_eq!(dec(&pm.get("Project", web).await["proj_billed_amount"]), n("735"));
    let r = pm.try_action("Timesheet", ts1_id, "make_sales_invoice", json!({})).await;
    assert!(invalid(&r, "nothing to invoice"), "invoicing twice finds nothing: {r:?}");
    let r = alice.k.cancel(&alice.ctx, "Timesheet", ts1_id, None).await;
    assert!(invalid(&r, "cancel their sales invoice first"), "billed time cannot be cancelled: {r:?}");
    acc.cancel("SalesInvoice", si1).await;
    let ts1 = acc.get("Timesheet", ts1_id).await;
    assert_eq!(
        (ts1["status"].clone(), ts1["time_logs"][0]["billed"].clone(), ts1["time_logs"][0]["sales_invoice"].clone()),
        (json!("Submitted"), json!(false), Value::Null)
    );
    assert_eq!(dec(&pm.get("Project", web).await["proj_billed_amount"]), Decimal::ZERO, "cancelling the invoice unbills the time");

    // The whole project up to a date, from the project itself.
    let r = alice.try_action("Project", web, "make_sales_invoice", json!({})).await;
    assert!(matches!(r, Err(Error::Forbidden(_))), "staff cannot invoice the project either: {r:?}");
    let si2 = pm.action("Project", web, "make_sales_invoice", json!({"to_date": d1.to_string()})).await;
    assert_eq!(si2["logs"], json!(3), "Alice's two logs and Bob's first; nothing after {d1}");
    let si2 = vid(&si2);
    let doc = acc.submit("SalesInvoice", si2).await;
    assert_eq!(dec(&doc["net_total"]), n("935"), "360 + 375 + 200");
    assert_eq!(doc["items"].as_array().unwrap().len(), 3, "Development at 120 and at 100 are separate lines");
    assert_eq!(acc.get("Timesheet", ts3_id).await["status"], json!("Billed"));
    assert_eq!(acc.get("Timesheet", ts4_id).await["status"], json!("Submitted"));
    acc.cancel("SalesInvoice", si2).await;

    // Cancel and amend keep the project figures exact.
    alice.cancel("Timesheet", ts1_id).await;
    let p = pm.get("Project", web).await;
    assert_eq!((dec(&p["proj_actual_hours"]), dec(&p["proj_costing_amount"]), dec(&p["proj_billable_amount"])), (n("5"), n("320"), n("550")));
    assert_eq!(dec(&pm.get("Task", design).await["actual_hours"]), Decimal::ZERO);
    let amended = alice.k.amend(&alice.ctx, "Timesheet", ts1_id).await.expect("amend");
    assert_eq!(amended["time_logs"][0]["billed"], json!(false));
    alice.submit("Timesheet", uid(&amended)).await;
    let p = pm.get("Project", web).await;
    assert_eq!((dec(&p["proj_actual_hours"]), dec(&p["proj_costing_amount"]), dec(&p["proj_billable_amount"])), (n("10.5"), n("730"), n("1285")));
    assert_eq!(dec(&pm.get("Task", design).await["actual_hours"]), n("3"));
    let si3 = vid(&pm.action("Project", web, "make_sales_invoice", json!({})).await);
    let doc = acc.submit("SalesInvoice", si3).await;
    assert_eq!(dec(&doc["net_total"]), n("1285"), "all the project's time, once");
    let p = pm.get("Project", web).await;
    assert_eq!(dec(&p["proj_billed_amount"]), n("1285"));
    assert_eq!(acc.get("Timesheet", ts4_id).await["status"], json!("Billed"));
    let r = pm.try_action("Project", web, "make_sales_invoice", json!({})).await;
    assert!(invalid(&r, "nothing to invoice"), "a fully billed project has nothing left: {r:?}");

    // ───────────── milestones on the fixed price project ─────────────
    let milestone = |subject: &str, pct: &str| json!({"subject": subject, "project": erp.to_string(), "is_milestone": true, "billing_percent": pct});
    let m1 = uid(&pm.insert("Task", milestone("Go-live", "40")).await);
    let r = pm.try_insert("Task", milestone("Hypercare", "70")).await;
    assert!(field_error(&r, "billing_percent", "110%"), "milestones cannot bill more than the contract: {r:?}");
    pm.insert("Task", milestone("Hypercare", "50")).await;
    let r = pm.try_insert("Task", json!({"subject": "Kickoff", "project": erp.to_string(), "billing_percent": "10"})).await;
    assert!(field_error(&r, "billing_percent", "only milestones"), "{r:?}");
    let r = pm.try_insert("Task", json!({"subject": "Extra", "project": web.to_string(), "is_milestone": true, "billing_percent": "10"})).await;
    assert!(field_error(&r, "billing_percent", "Fixed Price"), "{r:?}");
    let draft = vid(&pm.action("Task", m1, "make_milestone_invoice", json!({})).await);
    let si = acc.get("SalesInvoice", draft).await;
    assert_eq!((dec(&si["net_total"]), si["items"][0]["proj_milestone"].clone()), (n("4000"), json!(m1.to_string())));
    let r = pm.try_action("Task", m1, "make_milestone_invoice", json!({})).await;
    assert!(invalid(&r, "already bills this milestone"), "{r:?}");
    acc.submit("SalesInvoice", draft).await;
    let m = pm.get("Task", m1).await;
    assert_eq!((m["sales_invoice"].clone(), dec(&m["billed_amount"])), (json!(draft.to_string()), n("4000")));
    assert_eq!(dec(&pm.get("Project", erp).await["proj_billed_amount"]), n("4000"));
    let r = pm.try_action("Task", m1, "make_milestone_invoice", json!({})).await;
    assert!(invalid(&r, "already invoiced"), "a milestone is invoiced once: {r:?}");
    let r = pm.try_update("Task", m1, json!({"billing_percent": "45"})).await;
    assert!(field_error(&r, "billing_percent", "invoiced"), "{r:?}");
    acc.cancel("SalesInvoice", draft).await;
    assert_eq!(pm.get("Task", m1).await["sales_invoice"], Value::Null);
    assert_eq!(dec(&pm.get("Project", erp).await["proj_billed_amount"]), Decimal::ZERO);
    let again = vid(&pm.action("Task", m1, "make_milestone_invoice", json!({})).await);
    acc.submit("SalesInvoice", again).await;

    // ───────────── an expense booked to the project ─────────────
    let je = acc
        .insert(
            "JournalEntry",
            json!({"company": cid, "accounts": [
                {"account": company["default_expense_account"], "debit": "500", "project": web.to_string()},
                {"account": company["default_cash_account"], "credit": "500"}]}),
        )
        .await;
    acc.submit("JournalEntry", uid(&je)).await;

    // ───────────── reports ─────────────
    let company_param = ("company", cid.clone());
    let pr = acc.report("project_profitability", std::slice::from_ref(&company_param)).await.expect("profitability");
    let row = |rep: &Value, project: &str| rep["rows"].as_array().unwrap().iter().find(|r| r["project"].as_str().unwrap().ends_with(project)).cloned().unwrap();
    let w = row(&pr, "Website");
    assert_eq!(
        (dec(&w["billed_revenue"]), dec(&w["timesheet_cost"]), dec(&w["expense_cost"]), dec(&w["margin"]), dec(&w["margin_pct"])),
        (n("1285"), n("730"), n("500"), n("55"), n("4.28")),
        "cancelled invoices do not count: {w}"
    );
    let e = row(&pr, "ERP Rollout");
    assert_eq!(
        (dec(&e["contract_value"]), dec(&e["billed_revenue"]), dec(&e["timesheet_cost"]), dec(&e["margin"]), dec(&e["margin_pct"])),
        (n("10000"), n("4000"), n("70"), n("3930"), n("98.25"))
    );
    assert_eq!(dec(&pr["totals"]["billed_revenue"]), n("5285"));
    let r = alice.report("project_profitability", std::slice::from_ref(&company_param)).await;
    assert!(matches!(r, Err(Error::Forbidden(_))), "staff do not see margins: {r:?}");

    let period = [company_param.clone(), ("from_date", d1.to_string()), ("to_date", today.to_string())];
    let ts = pm.report("timesheet_summary", &period).await.expect("timesheet summary");
    let line = |rep: &Value, user: &str, project: &str, activity: &str| {
        rep["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["user"] == json!(user) && r["project"].as_str().unwrap().ends_with(project) && r["activity_type"] == json!(activity))
            .cloned()
            .unwrap_or_else(|| panic!("no row {user}/{project}/{activity}: {}", rep["rows"]))
    };
    let ad = line(&ts, "alice@acme.test", "Website", "Development");
    assert_eq!((dec(&ad["hours"]), dec(&ad["billed_hours"]), dec(&ad["cost"]), dec(&ad["billable_amount"])), (n("3"), n("3"), n("210"), n("360")));
    let ac = line(&ts, "alice@acme.test", "Website", "Consulting");
    assert_eq!((dec(&ac["hours"]), dec(&ac["billed_amount"])), (n("3.5"), n("525")));
    let ai = line(&ts, "alice@acme.test", "(no project)", "Internal");
    assert_eq!((dec(&ai["hours"]), dec(&ai["billable_hours"]), dec(&ai["cost"])), (n("1"), Decimal::ZERO, n("50")));
    assert_eq!((dec(&ts["totals"]["hours"]), dec(&ts["totals"]["cost"]), dec(&ts["totals"]["billed_amount"])), (n("12.5"), n("850"), n("1285")));
    let own = bob.report("timesheet_summary", &period).await.expect("own summary");
    assert!(own["rows"].as_array().unwrap().iter().all(|r| r["user"] == json!("bob@acme.test")), "{}", own["rows"]);
    assert_eq!(dec(&own["totals"]["hours"]), n("4"));
    assert!(own["columns"].as_array().unwrap().iter().all(|c| c["name"] != json!("cost")) && own["rows"][0].get("cost").is_none(), "no costs for staff");

    // ───────────── completing the project ─────────────
    let r = pm.try_update("Project", web, json!({"status": "Completed"})).await;
    assert!(field_error(&r, "status", "still open"), "Build and Test are open: {r:?}");
    let r = pm.try_action("Project", web, "complete", json!({})).await;
    assert!(invalid(&r, "still open"), "completing needs force while tasks are open: {r:?}");
    let out = pm.action("Project", web, "complete", json!({"force": true})).await;
    assert_eq!(out, json!({"status": "Completed", "cancelled_tasks": 2}));
    let done = pm.get("Project", web).await;
    assert_eq!(done["status"], json!("Completed"));
    let r = pm.try_action("Project", web, "complete", json!({"force": true})).await;
    assert!(invalid(&r, "not available"), "a completed project is not completed twice: {r:?}");
    assert_eq!(pm.get("Task", build).await["status"], json!("Cancelled"));
    assert_eq!(pm.get("Task", test).await["status"], json!("Cancelled"));
    assert_eq!(dec(&done["proj_percent_complete"]), n("100"), "cancelled tasks drop out of the progress");
    let r = alice
        .try_insert(
            "Timesheet",
            bad(json!([{"activity_type": dev, "project": web.to_string(), "from_time": at(today, "07:00"), "to_time": at(today, "08:00")}])),
        )
        .await;
    assert!(field_error(&r, "project", "completed"), "no time on a completed project: {r:?}");
    let r = pm.try_update("Task", design, json!({"description": "late edit"})).await;
    assert!(field_error(&r, "project", "reopen it"), "{r:?}");
    pm.update("Project", web, json!({"status": "Open"})).await;

    // ───────────── the overdue job ─────────────
    let mut late_input = task("Late", "5", &["bob"]);
    late_input["exp_end_date"] = json!((today + Duration::days(3)).to_string());
    let late = uid(&pm.insert("Task", late_input).await);
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update e_task set exp_end_date = $3 where tenant_id = $1 and id = $2").bind(tenant).bind(late).bind(d3).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    ferrum_pkg_projects::maintenance(&k).await.expect("maintenance");
    assert_eq!(pm.get("Task", late).await["status"], json!("Overdue"));
    ferrum_pkg_projects::maintenance(&k).await.expect("maintenance is idempotent");
    assert_eq!(pm.get("Task", late).await["status"], json!("Overdue"));
    assert_eq!(pm.get("Task", design).await["status"], json!("Completed"), "closed tasks are left alone");

    let to = pm.report("task_overview", std::slice::from_ref(&company_param)).await.expect("task overview");
    let group = |project: &str, assignee: &str| {
        to["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["project"].as_str().unwrap().ends_with(project) && r["assignee"] == json!(assignee))
            .cloned()
            .unwrap_or_else(|| panic!("no row {project}/{assignee}: {}", to["rows"]))
    };
    let b_row = group("Website", "bob@acme.test");
    assert_eq!((b_row["open"].clone(), b_row["overdue"].clone(), dec(&b_row["planned_hours"])), (json!(1), json!(1), n("5")), "{b_row}");
    let a_row = group("Website", "alice@acme.test");
    assert_eq!((a_row["completed"].clone(), dec(&a_row["planned_hours"]), dec(&a_row["actual_hours"])), (json!(1), n("10"), n("3")), "{a_row}");
    assert_eq!(group("ERP Rollout", "(unassigned)")["open"], json!(2));
    assert_eq!((to["totals"]["tasks"].clone(), to["totals"]["completed"].clone(), to["totals"]["overdue"].clone()), (json!(5), json!(2), json!(1)));
    let mine = bob.report("task_overview", std::slice::from_ref(&company_param)).await.expect("own task overview");
    assert_eq!((mine["totals"]["tasks"].clone(), mine["totals"]["overdue"].clone()), (json!(1), json!(1)), "staff see only their tasks: {}", mine["rows"]);

    let moved = pm.update("Task", late, json!({"exp_end_date": (today + Duration::days(5)).to_string()})).await;
    assert_eq!(moved["status"], json!("Open"), "a new end date lifts Overdue");
    // (100 × 10 + 100 × 0 + 0 × 5) / 15 hours: the reopened project counts its new task.
    assert_eq!(dec(&pm.get("Project", web).await["proj_percent_complete"]), n("66.67"));
}
