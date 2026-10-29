//! End-to-end test against a real PostgreSQL (`FERRUM_MFG_OPS_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_mfg_ops_test`). The database is reset first.
//!
//! A chair plant in Asia/Kolkata: workstations with shifts, holidays and parallel lanes; a routing
//! that costs the chair BOM; work orders scheduled forward around each other; job cards with
//! material transfers, time logs refused beyond a workstation's capacity, in-sequence completion
//! and manufacture capped by the last operation; a production plan from a sales order netted
//! against stock, its work orders, purchase orders and (with a Procurement-shaped app installed)
//! material requests; and a subcontractor plating parts from materials sent to it, with exact
//! receipt valuation, an excess return and a cancelled receipt — books balanced throughout.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::Catalog;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

/// Stand-ins for the Procurement app's material requests and the HR app's employees (shaped like
/// theirs): this package finds them by their metadata, not by a compile-time dependency.
const STAND_IN_PACKAGE: &str = "package: stand_ins\nversion: 0.1.0\ndepends: [inventory]\n";
const STAND_IN_ENTITIES: &str = r#"
- name: Employee
  label: Employee
  kind: master
  company_scoped: true
  title_field: employee_name
  numbering: {template: "EMP-{seq:3}"}
  fields:
    - {name: employee_name, type: data, required: true}
    - {name: company, type: link, to: Company, required: true}
    - {name: status, type: select, options: [Active, Left], default: Active}
- name: MaterialRequestItem
  label: Material Request Item
  kind: child
  fields:
    - {name: item, type: link, to: Item, required: true}
    - {name: qty, type: decimal, required: true}
    - {name: warehouse, type: link, to: Warehouse}
    - {name: schedule_date, type: date}
    - {name: ordered_qty, type: decimal, read_only: true}
- name: MaterialRequest
  label: Material Request
  kind: document
  submittable: true
  company_scoped: true
  fields:
    - {name: company, type: link, to: Company, required: true}
    - {name: material_request_type, type: select, options: [Purchase, Manufacture, Material Transfer], required: true}
    - {name: transaction_date, type: date, default_expr: today}
    - {name: schedule_date, type: date}
    - {name: items, type: table, of: MaterialRequestItem, required: true}
"#;

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

fn s(u: Uuid) -> String {
    u.to_string()
}

/// The error mentions `text` (validation messages included).
fn says(e: &Error, text: &str) -> bool {
    match e {
        Error::Validation(list) => list.iter().any(|f| f.message.contains(text) || f.field.contains(text)),
        other => other.to_string().contains(text),
    }
}

struct T {
    k: Arc<Kernel>,
    tenant: Uuid,
    admin: Ctx,
}

impl T {
    async fn insert(&self, ctx: &Ctx, entity: &str, v: Value) -> Doc {
        self.k.insert(ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }
    async fn update(&self, ctx: &Ctx, entity: &str, id: Uuid, v: Value) -> Doc {
        self.k.update(ctx, entity, id, obj(v), None).await.unwrap_or_else(|e| panic!("update {entity}: {e:?}"))
    }
    async fn submit(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Doc {
        self.k.submit(ctx, entity, id, None).await.unwrap_or_else(|e| panic!("submit {entity}: {e:?}"))
    }
    async fn cancel(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Doc {
        self.k.cancel(ctx, entity, id, None).await.unwrap_or_else(|e| panic!("cancel {entity}: {e:?}"))
    }
    async fn action(&self, ctx: &Ctx, entity: &str, id: Uuid, action: &str, params: Value) -> Value {
        self.k.run_action(ctx, entity, id, action, params).await.unwrap_or_else(|e| panic!("{entity}.{action}: {e:?}"))
    }
    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.admin, entity, id, false).await.unwrap_or_else(|e| panic!("get {entity}: {e:?}"))
    }
    async fn find(&self, entity: &str, filters: Value) -> Vec<Doc> {
        self.k
            .list(&self.admin, entity, &ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), limit: Some(500), ..Default::default() })
            .await
            .unwrap()
            .rows
    }
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let rows = self.find(entity, filters.clone()).await;
        assert_eq!(rows.len(), 1, "{entity} {filters}");
        rows[0].clone()
    }
    async fn count(&self, sql: &str, id: Uuid) -> i64 {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(self.tenant).bind(id).fetch_one(&mut *tx).await.unwrap()
    }
    /// Debit minus credit on an account (by name) across all postings.
    async fn gl(&self, account: &str) -> Decimal {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        sqlx::query_scalar(
            "select coalesce(sum(l.debit - l.credit), 0) from journal_line l join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id \
             where l.tenant_id = $1 and a.account_name = $2",
        )
        .bind(self.tenant)
        .bind(account)
        .fetch_one(&mut *tx)
        .await
        .unwrap()
    }
    async fn stock(&self, item: Uuid, wh: Uuid) -> (Decimal, Decimal) {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        sqlx::query_as::<_, (Decimal, Decimal)>("select qty, value from stock_balance where tenant_id = $1 and item_id = $2 and warehouse_id = $3")
            .bind(self.tenant)
            .bind(item)
            .bind(wh)
            .fetch_optional(&mut *tx)
            .await
            .unwrap()
            .unwrap_or_default()
    }
    async fn report(&self, ctx: &Ctx, name: &str, params: &[(&str, String)]) -> Value {
        let p: HashMap<String, String> = params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        ferrum_domain::reports::run(&self.k, ctx, name, &p).await.unwrap_or_else(|e| panic!("report {name}: {e:?}"))
    }
    async fn user(&self, email: &str, role: &str) -> Ctx {
        let id = self.k.create_user(&self.admin, email, email, Some("Correct-Horse-9"), &[role.into()], false).await.expect("user");
        self.k.load_ctx(self.tenant, id, None, "it".into()).await.expect("ctx")
    }
    /// Every entry balances, the trial balance balances, and stock equals the stock accounts.
    async fn books_balance(&self, company: Uuid) {
        let tb = self.report(&self.admin, "trial_balance", &[("company", s(company))]).await;
        assert_eq!(tb["meta"]["balanced"], json!(true), "{}", tb["totals"]);
        let unbalanced = self
            .count("select count(*) from (select entry_id from journal_line where tenant_id = $1 and $2::uuid is not null group by entry_id having sum(debit) <> sum(credit)) x", company)
            .await;
        assert_eq!(unbalanced, 0);
        let sb = self.report(&self.admin, "stock_balance", &[("company", s(company))]).await;
        assert_eq!(dec(&sb["totals"]["value"]), self.gl("Stock In Hand").await + self.gl("Work In Progress").await, "perpetual inventory");
    }
}

fn row<'a>(doc: &'a Doc, table: &str, field: &str, value: Uuid) -> &'a Value {
    doc[table].as_array().unwrap().iter().find(|r| r[field] == json!(s(value))).unwrap_or_else(|| panic!("{table} row with {field} = {value}"))
}

fn ist(day: &str, hm: &str) -> String {
    format!("{day}T{hm}:00+05:30")
}

#[test]
fn manufacturing_operations_end_to_end() {
    // Same runtime shape as the server: unoptimized async hook chains need more than 2 MiB.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_MFG_OPS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_mfg_ops_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("platform packages");
    packages.extend(ferrum_mfg_ops::packages().expect("app packages"));
    let files =
        BTreeMap::from([("package.yaml".to_string(), STAND_IN_PACKAGE.to_string()), ("entities/stand_ins.yaml".to_string(), STAND_IN_ENTITIES.to_string())]);
    packages.push(ferrum_meta::catalog::load_package_files(&files).expect("stand-ins"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_mfg_ops::register(&mut hooks);
    for source in ferrum_mfg_ops::reports() {
        ferrum_domain::reports::add_source(source);
    }
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("partitions");
    ferrum_mfg_ops::maintenance(&k).await.expect("upkeep");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "plant".into(),
            name: "Chair Plant".into(),
            admin_email: "admin@plant.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["mfg_ops".into()],
            settings: Some(json!({"fiscal_year_start_month": 4, "timezone": "Asia/Kolkata"})),
        })
        .await
        .expect("tenant");
    let session = k.login("plant", "admin@plant.test", "Correct-Horse-9").await.expect("login");
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    let mut t = T { k: k.clone(), tenant, admin };
    let a = t.admin.clone();

    // ───────────── company, items, opening stock ─────────────
    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let company = t.insert(&a, "Company", json!({"company_name": "Acme Works", "abbr": "AW", "default_currency": inr["id"]})).await;
    let cid = uid(&company);
    let wh = |name: &'static str| {
        let t = &t;
        async move { uid(&t.find_one("Warehouse", json!({"warehouse_name": name, "company": s(cid)})).await) }
    };
    let (stores, fg, wip, all) = (wh("Stores").await, wh("Finished Goods").await, wh("Work In Progress").await, wh("All Warehouses").await);
    let bolt_store = uid(&t.insert(&a, "Warehouse", json!({"warehouse_name": "Bolt Co Store", "company": s(cid), "parent_warehouse": s(all)})).await);
    let group = |name: &'static str| {
        let t = &t;
        async move { t.find_one("ItemGroup", json!({"group_name": name})).await["id"].clone() }
    };
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await["id"].clone();
    let (raw_g, sub_g, prod_g, svc_g) = (group("Raw Material").await, group("Sub Assemblies").await, group("Products").await, group("Services").await);
    let item = |code: &str, g: &Value, extra: Value| {
        let mut v = json!({"item_code": code, "item_name": code.to_lowercase(), "item_group": g, "stock_uom": nos});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    };
    let made = json!({"is_manufactured": true, "is_purchased": false});
    let steel = uid(&t.insert(&a, "Item", item("RAW-STEEL", &raw_g, json!({"safety_stock": "80"}))).await);
    let paint = uid(&t.insert(&a, "Item", item("RAW-PAINT", &raw_g, json!({}))).await);
    let frame = uid(&t.insert(&a, "Item", item("FRAME", &sub_g, made.clone())).await);
    let chair = uid(&t.insert(&a, "Item", item("CHAIR", &prod_g, json!({"is_manufactured": true, "is_purchased": false, "standard_rate": "500"}))).await);
    let part = uid(&t.insert(&a, "Item", item("SC-PART", &prod_g, json!({}))).await);
    let rm1 = uid(&t.insert(&a, "Item", item("SC-RM", &raw_g, json!({}))).await);
    let rm2 = uid(&t.insert(&a, "Item", item("SC-RM2", &raw_g, json!({}))).await);
    let plating = uid(&t.insert(&a, "Item", item("SVC-PLATING", &svc_g, json!({"is_stock_item": false}))).await);
    let opening = t
        .insert(
            &a,
            "StockEntry",
            json!({"company": s(cid), "purpose": "Opening Stock", "items": [
                {"item": s(steel), "qty": "100", "basic_rate": "10", "target_warehouse": s(stores)},
                {"item": s(paint), "qty": "40", "basic_rate": "4", "target_warehouse": s(stores)},
                {"item": s(frame), "qty": "30", "basic_rate": "20", "target_warehouse": s(stores)},
                {"item": s(rm1), "qty": "50", "basic_rate": "6", "target_warehouse": s(stores)},
                {"item": s(rm2), "qty": "30", "basic_rate": "2", "target_warehouse": s(stores)}]}),
        )
        .await;
    t.submit(&a, "StockEntry", uid(&opening)).await;
    let mfg_user = t.user("operator@plant.test", "Manufacturing User").await;

    // ───────────── workstations, operations, routing ─────────────
    let welding_type = t
        .insert(&a, "WorkstationType", json!({"workstation_type": "Welding", "hour_rate_electricity": "20", "hour_rate_consumable": "10", "hour_rate_rent": "15", "hour_rate_labour": "75"}))
        .await;
    assert_eq!(dec(&welding_type["hour_rate"]), n("120"));
    let shifts = |days: &[&str], spans: &[(&str, &str)]| -> Value {
        Value::Array(days.iter().flat_map(|d| spans.iter().map(move |(f, to)| json!({"weekday": d, "start_time": f, "end_time": to}))).collect())
    };
    let weekdays = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday"];
    let weld = t
        .insert(
            &a,
            "Workstation",
            json!({"workstation_name": "Weld Bay", "company": s(cid), "workstation_type": welding_type["id"], "production_capacity": 1,
                   "working_hours": shifts(&weekdays, &[("09:00", "17:00")]), "holidays": [{"holiday_date": "2026-11-03", "description": "Maintenance"}]}),
        )
        .await;
    assert_eq!(dec(&weld["hour_rate"]), n("120"), "rates come from the type");
    let weld = uid(&weld);
    let weld2 = t
        .insert(
            &a,
            "Workstation",
            json!({"workstation_name": "Weld Bay 2", "company": s(cid), "workstation_type": welding_type["id"], "hour_rate_labour": "90"}),
        )
        .await;
    assert_eq!(dec(&weld2["hour_rate"]), n("135"), "a rate typed on the workstation is kept, the rest come from the type");
    let overlapping =
        t.k.insert(
            &a,
            "Workstation",
            obj(json!({"workstation_name": "Bad", "company": s(cid), "working_hours": shifts(&["Monday"], &[("08:00", "12:00"), ("11:00", "15:00")])})),
        )
        .await;
    assert!(matches!(&overlapping, Err(e) if says(e, "overlaps another shift")), "{overlapping:?}");
    let malformed =
        t.k.insert(&a, "Workstation", obj(json!({"workstation_name": "Bad", "company": s(cid), "working_hours": shifts(&["Monday"], &[("8:00", "12:00")])})))
            .await;
    assert!(matches!(&malformed, Err(e) if says(e, "HH:MM")), "{malformed:?}");
    let six_days = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    let booth = uid(&t
        .insert(
            &a,
            "Workstation",
            json!({"workstation_name": "Paint Booth", "company": s(cid), "hour_rate_labour": "60", "production_capacity": 2,
                   "working_hours": shifts(&six_days, &[("08:00", "12:00"), ("13:00", "17:00")])}),
        )
        .await);
    let denied = t.k.insert(&mfg_user, "Workstation", obj(json!({"workstation_name": "Mine", "company": s(cid)}))).await;
    assert!(matches!(denied, Err(Error::Forbidden(_))), "operators do not create workstations: {denied:?}");
    let op_weld = uid(&t.insert(&a, "Operation", json!({"operation_name": "Welding", "workstation": s(weld)})).await);
    let op_paint = uid(&t.insert(&a, "Operation", json!({"operation_name": "Painting", "workstation": s(booth), "batch_size": "10"})).await);
    let op_drill = uid(&t.insert(&a, "Operation", json!({"operation_name": "Drilling", "workstation": s(weld)})).await);
    let routing = t
        .insert(
            &a,
            "Routing",
            json!({"routing_name": "Chair Routing", "operations": [
                {"operation": s(op_weld), "time_in_mins": "30", "setup_time": "60"},
                {"operation": s(op_paint), "time_basis": "Per Batch", "time_in_mins": "120"}]}),
        )
        .await;
    assert_eq!(routing["operations"][0]["workstation"], json!(s(weld)), "the operation's workstation");
    assert_eq!((routing["operations"][1]["workstation"].clone(), dec(&routing["operations"][1]["batch_size"])), (json!(s(booth)), n("10")));
    let routing_id = uid(&routing);
    let no_time = t.k.insert(&a, "Routing", obj(json!({"routing_name": "Empty", "operations": [{"operation": s(op_weld), "time_in_mins": "0"}]}))).await;
    assert!(matches!(&no_time, Err(e) if says(e, "needs a time")), "{no_time:?}");

    // ───────────── BOMs: the routing costs the chair ─────────────
    let frame_bom = uid(&t.insert(&a, "Bom", json!({"company": s(cid), "item": s(frame), "quantity": "1", "items": [{"item": s(steel), "qty": "2"}]})).await);
    t.submit(&a, "Bom", frame_bom).await;
    let chair_items = json!([{"item": s(frame), "qty": "10"}, {"item": s(paint), "qty": "5", "mfo_operation": s(op_paint)}]);
    let wrong_op =
        t.k.insert(
            &a,
            "Bom",
            obj(json!({"company": s(cid), "item": s(chair), "quantity": "10", "mfo_routing": routing["id"],
                                     "items": [{"item": s(frame), "qty": "10", "mfo_operation": s(op_drill)}]})),
        )
        .await;
    assert!(matches!(&wrong_op, Err(e) if says(e, "not one of this BOM's operations")), "{wrong_op:?}");
    let chair_bom =
        t.insert(&a, "Bom", json!({"company": s(cid), "item": s(chair), "quantity": "10", "mfo_routing": routing["id"], "items": chair_items})).await;
    let ops = chair_bom["mfo_operations"].as_array().unwrap();
    assert_eq!(ops.len(), 2, "copied from the routing");
    // Welding 10 × 30 + 60 setup = 6 h at 120; painting one batch of 10 = 2 h at 60.
    assert_eq!((dec(&ops[0]["operating_cost"]), dec(&ops[1]["operating_cost"])), (n("720"), n("120")));
    assert_eq!(dec(&chair_bom["operating_cost"]), n("840"));
    assert_eq!(dec(&chair_bom["raw_material_cost"]), n("220"), "10 frames at 20 + 5 paint at 4");
    assert_eq!(dec(&chair_bom["total_cost"]), n("1060"), "materials + operations");
    let chair_bom = uid(&chair_bom);
    t.submit(&a, "Bom", chair_bom).await;
    t.update(&a, "Workstation", booth, json!({"hour_rate_labour": "90"})).await;
    let in_use = t.k.delete(&a, "Routing", routing_id, None).await;
    assert!(matches!(&in_use, Err(e) if says(e, "use this routing")), "{in_use:?}");
    let recost = t.action(&a, "Bom", chair_bom, "mfo_update_operating_cost", json!({})).await;
    assert_eq!((dec(&recost["operating_cost"]), dec(&recost["total_cost"])), (n("900"), n("1120")), "painting re-costed at 90 an hour");
    let bom_doc = t.get("Bom", chair_bom).await;
    assert_eq!((dec(&bom_doc["mfo_operations"][1]["hour_rate"]), dec(&bom_doc["total_cost"])), (n("90"), n("1120")));

    // ───────────── work orders: operations and forward scheduling ─────────────
    let wo = |qty: &str| json!({"company": s(cid), "item": s(chair), "qty": qty, "source_warehouse": s(wip), "target_warehouse": s(fg), "planned_start_date": "2026-11-02"});
    let wo_a = t.insert(&a, "WorkOrder", wo("2")).await;
    let ops = wo_a["mfo_operations"].as_array().unwrap();
    assert_eq!((dec(&ops[0]["time_in_mins"]), dec(&ops[1]["time_in_mins"])), (n("120"), n("120")), "2 × 30 + 60; one started batch");
    assert_eq!(dec(&wo_a["mfo_planned_operating_cost"]), n("420"), "2 h at 120 + 2 h at 90");
    assert!(ops[0]["planned_start_time"].is_null(), "scheduled on submit");
    let wo_a = uid(&wo_a);
    let wo_a_doc = t.submit(&a, "WorkOrder", wo_a).await;
    let times = |d: &Doc, i: usize| (d["mfo_operations"][i]["planned_start_time"].clone(), d["mfo_operations"][i]["planned_end_time"].clone());
    // Monday 09:00-11:00 IST welding, then painting 11:00-12:00 and 13:00-14:00 IST.
    assert_eq!(times(&wo_a_doc, 0), (json!("2026-11-02T03:30:00.000Z"), json!("2026-11-02T05:30:00.000Z")));
    assert_eq!(times(&wo_a_doc, 1), (json!("2026-11-02T05:30:00.000Z"), json!("2026-11-02T08:30:00.000Z")));
    let wo_b = t.insert(&a, "WorkOrder", wo("20")).await;
    assert_eq!(dec(&wo_b["mfo_planned_operating_cost"]), n("1680"), "11 h at 120 + 4 h (two batches) at 90");
    let wo_b = uid(&wo_b);
    let wo_b_doc = t.submit(&a, "WorkOrder", wo_b).await;
    // The weld bay is taken until 11:00 Monday and Tuesday is a holiday: 6 h Monday + 5 h Wednesday.
    assert_eq!(times(&wo_b_doc, 0), (json!("2026-11-02T05:30:00.000Z"), json!("2026-11-04T08:30:00.000Z")));
    // Painting follows: Wednesday 14:00-17:00 and Thursday 08:00-09:00 IST.
    assert_eq!(times(&wo_b_doc, 1), (json!("2026-11-04T08:30:00.000Z"), json!("2026-11-05T03:30:00.000Z")));
    assert_eq!(wo_b_doc["expected_end_date"], json!("2026-11-05"));
    let cap = t
        .report(
            &a,
            "capacity_planning",
            &[("company", s(cid)), ("start_date", "2026-11-02".into()), ("end_date", "2026-11-06".into()), ("workstation", s(weld))],
        )
        .await;
    let day = |d: &str| cap["rows"].as_array().unwrap().iter().find(|r| r["date"] == json!(d)).cloned();
    let monday = day("2026-11-02").expect("monday");
    assert_eq!((dec(&monday["capacity_mins"]), dec(&monday["load_mins"]), dec(&monday["utilization"])), (n("480"), n("480"), n("100")));
    assert!(day("2026-11-03").is_none(), "the holiday has no capacity and no load");
    assert_eq!(dec(&day("2026-11-04").expect("wednesday")["load_mins"]), n("300"));

    // ───────────── job cards ─────────────
    let cards = |wo: Uuid| {
        let t = &t;
        async move {
            let mut c = t.find("JobCard", json!([["work_order", "=", s(wo)], ["docstatus", "<", 2]])).await;
            c.sort_by_key(|d| d["sequence"].as_i64());
            c
        }
    };
    let b_cards = cards(wo_b).await;
    assert_eq!(b_cards.len(), 2, "one per operation on submit");
    let (b_weld, b_paint) = (uid(&b_cards[0]), uid(&b_cards[1]));
    assert_eq!((dec(&b_cards[0]["for_quantity"]), b_cards[0]["status"].clone(), b_cards[0]["workstation"].clone()), (n("20"), json!("Open"), json!(s(weld))));
    let a_cards = cards(wo_a).await;
    let (a_weld, a_paint) = (uid(&a_cards[0]), uid(&a_cards[1]));

    // Materials to the shop floor: frames go with the first operation, paint with painting.
    t.action(&a, "JobCard", b_weld, "make_material_transfer", json!({"from_warehouse": s(stores)})).await;
    assert_eq!(t.stock(frame, wip).await, (n("20"), n("400")));
    assert_eq!(t.stock(paint, wip).await.0, Decimal::ZERO, "paint is consumed at painting");
    let card = t.get("JobCard", b_weld).await;
    assert_eq!((card["status"].clone(), dec(&card["transferred_qty"])), (json!("Material Transferred"), n("20")));
    let pending = t.action(&a, "JobCard", b_paint, "make_material_transfer", json!({"from_warehouse": s(stores), "submit": false})).await;
    assert_eq!(t.get("JobCard", b_paint).await["status"], json!("Open"), "a draft transfer moves nothing");
    t.submit(&a, "StockEntry", vid(&pending)).await;
    assert_eq!(t.get("JobCard", b_paint).await["status"], json!("Material Transferred"));
    t.cancel(&a, "StockEntry", vid(&pending)).await;
    let card = t.get("JobCard", b_paint).await;
    assert_eq!((card["status"].clone(), dec(&card["transferred_qty"])), (json!("Open"), Decimal::ZERO));
    t.action(&a, "JobCard", b_paint, "make_material_transfer", json!({"from_warehouse": s(stores)})).await;
    assert_eq!(t.stock(paint, wip).await, (n("10"), n("40")));
    let gone = t.k.delete(&a, "JobCard", b_paint, None).await;
    assert!(matches!(&gone, Err(e) if says(e, "materials were transferred")), "{gone:?}");

    // Time logs; the weld bay runs one job at a time, the paint booth two.
    let log = |day: &str, from: &str, to: &str, qty: &str| json!({"from_time": ist(day, from), "to_time": ist(day, to), "completed_qty": qty});
    let mon = "2026-11-02";
    let card = t.update(&mfg_user, "JobCard", b_weld, json!({"employee": "Ravi", "time_logs": [log(mon, "09:00", "13:00", "8")]})).await;
    assert_eq!((dec(&card["total_time_in_mins"]), dec(&card["operating_cost"]), card["status"].clone()), (n("240"), n("480"), json!("Work In Progress")));
    assert_eq!(card["time_logs"][0]["employee"], json!("Ravi"), "free text without the HR app");
    let clash = t.k.update(&a, "JobCard", a_weld, obj(json!({"time_logs": [log(mon, "12:00", "14:00", "2")]})), None).await;
    assert!(
        matches!(&clash, Err(Error::Validation(e)) if e.iter().any(|f| f.message.contains("fully booked") && f.row == Some(("time_logs".into(), 0)))),
        "{clash:?}"
    );
    t.update(&a, "JobCard", a_weld, json!({"time_logs": [log(mon, "13:00", "15:00", "2")]})).await;
    t.update(&a, "JobCard", a_paint, json!({"time_logs": [log(mon, "10:00", "12:00", "0")]})).await;
    let three = t.k.update(&a, "JobCard", b_paint, obj(json!({"time_logs": [log(mon, "10:00", "11:00", "4"), log(mon, "10:30", "11:30", "4")]})), None).await;
    assert!(matches!(&three, Err(e) if says(e, "fully booked")), "a third job in a two-lane booth: {three:?}");
    t.update(&a, "JobCard", b_paint, json!({"time_logs": [log(mon, "14:00", "16:00", "8")]})).await;

    // Completion follows the operation sequence.
    let early = t.k.submit(&a, "JobCard", b_paint, None).await;
    assert!(matches!(&early, Err(e) if says(e, "previous operation")), "{early:?}");
    t.submit(&mfg_user, "JobCard", b_weld).await;
    let wo_doc = t.get("WorkOrder", wo_b).await;
    let weld_op = &wo_doc["mfo_operations"][0];
    assert_eq!((dec(&weld_op["completed_qty"]), dec(&weld_op["actual_time_in_mins"]), dec(&weld_op["actual_operating_cost"])), (n("8"), n("240"), n("480")));
    assert_eq!((weld_op["status"].clone(), dec(&wo_doc["mfo_actual_operating_cost"])), (json!("Work in Progress"), n("480")));
    let too_soon = t.k.run_action(&a, "WorkOrder", wo_b, "manufacture", json!({"qty": "1"})).await;
    assert!(matches!(&too_soon, Err(e) if says(e, "last operation")), "nothing is through painting yet: {too_soon:?}");
    t.submit(&a, "JobCard", b_paint).await;
    let too_many = t.k.run_action(&a, "WorkOrder", wo_b, "manufacture", json!({"qty": "9"})).await;
    assert!(matches!(&too_many, Err(e) if says(e, "at most 8")), "{too_many:?}");
    let entry = t.action(&a, "WorkOrder", wo_b, "manufacture", json!({"qty": "8"})).await;
    let entry = t.get("StockEntry", vid(&entry)).await;
    // Actual operating cost per unit: 480 / 8 welding + 180 / 8 painting = 82.5.
    assert_eq!(dec(&entry["mfg_additional_cost"]), n("660"));
    assert_eq!(t.stock(chair, fg).await, (n("8"), n("836")), "8 frames at 20 + 4 paint at 4 + 660 operations");
    let wo_doc = t.get("WorkOrder", wo_b).await;
    assert_eq!((dec(&wo_doc["produced_qty"]), dec(&wo_doc["actual_cost"])), (n("8"), n("836")));
    assert_eq!(t.gl("Manufacturing Overheads Absorbed").await, n("-660"));
    let undo_weld = t.k.cancel(&a, "JobCard", b_weld, None).await;
    assert!(matches!(&undo_weld, Err(e) if says(e, "next operation")), "{undo_weld:?}");
    let undo_paint = t.k.cancel(&a, "JobCard", b_paint, None).await;
    assert!(matches!(&undo_paint, Err(e) if says(e, "already produced")), "{undo_paint:?}");
    let more = t.action(&a, "WorkOrder", wo_b, "mfo_make_job_cards", json!({})).await;
    assert_eq!(more["created"].as_array().unwrap().len(), 2, "cards for the 12 still to do");
    let again = t.k.run_action(&a, "WorkOrder", wo_b, "mfo_make_job_cards", json!({})).await;
    assert!(again.is_err(), "nothing left to card");
    let open: Vec<Doc> = cards(wo_b).await.into_iter().filter(|c| c["docstatus"] == json!(0)).collect();
    assert!(open.iter().all(|c| dec(&c["for_quantity"]) == n("12")));
    let open_weld = uid(open.iter().find(|c| c["sequence"] == json!(1)).unwrap());

    // A work order with completed job cards cannot be cancelled; its draft cards go with it.
    t.submit(&a, "JobCard", a_weld).await;
    let blocked = t.k.cancel(&a, "WorkOrder", wo_a, None).await;
    assert!(matches!(&blocked, Err(e) if says(e, "job card")), "{blocked:?}");
    t.cancel(&a, "JobCard", a_weld).await;
    assert_eq!(t.get("WorkOrder", wo_a).await["mfo_operations"][0]["status"], json!("Pending"));
    t.cancel(&a, "WorkOrder", wo_a).await;
    assert_eq!(t.count("select count(*) from e_job_card where tenant_id = $1 and work_order = $2 and docstatus = 0", wo_a).await, 0);
    assert_eq!(t.get("JobCard", a_weld).await["status"], json!("Cancelled"));

    let by_station = t.report(&a, "job_card_summary", &[("company", s(cid))]).await;
    let station = |name: &str| by_station["rows"].as_array().unwrap().iter().find(|r| r["group"] == json!(name)).cloned().unwrap();
    let w = station("Weld Bay");
    // 8 chairs at 660 / 20 = 33 standard minutes each = 264 against 240 logged.
    assert_eq!((dec(&w["completed_qty"]), dec(&w["actual_mins"]), dec(&w["planned_mins"]), dec(&w["efficiency"])), (n("8"), n("240"), n("264"), n("110")));
    let p = station("Paint Booth");
    assert_eq!((dec(&p["efficiency"]), dec(&p["operating_cost"])), (n("80"), n("180")), "96 standard minutes in 120");
    let by_operator = t.report(&a, "job_card_summary", &[("company", s(cid)), ("group_by", "Operator".into())]).await;
    assert!(by_operator["rows"].as_array().unwrap().iter().any(|r| r["group"] == json!("Ravi") && dec(&r["actual_mins"]) == n("240")));
    let progress = t.report(&a, "work_order_operations", &[("company", s(cid)), ("work_order", s(wo_b))]).await;
    assert_eq!(progress["rows"].as_array().unwrap().len(), 2);
    assert_eq!(dec(&progress["rows"][0]["progress"]), n("40"));
    t.books_balance(cid).await;

    // ───────────── production plan from a sales order ─────────────
    let globex = uid(&t.insert(&a, "Customer", json!({"customer_name": "Globex"})).await);
    let steel_corp = uid(&t.insert(&a, "Supplier", json!({"supplier_name": "Steel Corp"})).await);
    let so = t
        .insert(&a, "SalesOrder", json!({"company": s(cid), "customer": s(globex), "set_warehouse": s(fg), "items": [{"item": s(chair), "qty": "30"}, {"item": s(paint), "qty": "5"}]}))
        .await;
    let so = uid(&so);
    t.submit(&a, "SalesOrder", so).await;
    let po = t
        .insert(
            &a,
            "PurchaseOrder",
            json!({"company": s(cid), "supplier": s(steel_corp), "items": [{"item": s(steel), "qty": "20", "price_list_rate": "10", "warehouse": s(stores)}]}),
        )
        .await;
    t.submit(&a, "PurchaseOrder", uid(&po)).await;
    let plan_input = json!({"company": s(cid), "for_warehouse": s(stores), "fg_warehouse": s(fg)});
    let pp = uid(&t.insert(&a, "ProductionPlan", plan_input.clone()).await);
    assert_eq!(t.action(&a, "ProductionPlan", pp, "get_sales_orders", json!({})).await["added"], json!(1));
    assert_eq!(t.action(&a, "ProductionPlan", pp, "get_items", json!({})).await["items"], json!(1), "paint has no BOM");
    let plan = t.get("ProductionPlan", pp).await;
    let line = &plan["po_items"][0];
    assert_eq!(
        (line["item"].clone(), line["bom"].clone(), dec(&line["planned_qty"]), line["sales_order"].clone()),
        (json!(s(chair)), json!(s(chair_bom)), n("30"), json!(s(so)))
    );
    let got = t.action(&a, "ProductionPlan", pp, "get_raw_materials", json!({})).await;
    assert_eq!((got["raw_materials"].clone(), got["sub_assemblies"].clone()), (json!(2), json!(1)));
    let plan = t.get("ProductionPlan", pp).await;
    let sub = &plan["sub_assemblies"][0];
    assert_eq!(
        (sub["item"].clone(), dec(&sub["qty"]), sub["bom"].clone(), sub["bom_level"].clone()),
        (json!(s(frame)), n("30"), json!(s(frame_bom)), json!(1))
    );
    let mat = |item: Uuid| row(&plan, "raw_materials", "item", item).clone();
    let (p_row, s_row) = (mat(paint), mat(steel));
    // Paint: 15 needed, 30 left in stores (10 went to the shop floor).
    assert_eq!((dec(&p_row["required_qty"]), dec(&p_row["projected_qty"]), dec(&p_row["qty_to_order"])), (n("15"), n("30"), Decimal::ZERO));
    // Steel: 60 needed + 80 safety stock against 100 in stores + 20 on order.
    assert_eq!(
        (dec(&s_row["required_qty"]), dec(&s_row["actual_qty"]), dec(&s_row["projected_qty"]), dec(&s_row["safety_stock"]), dec(&s_row["qty_to_order"])),
        (n("60"), n("100"), n("120"), n("80"), n("20"))
    );
    assert_eq!(t.submit(&a, "ProductionPlan", pp).await["status"], json!("Submitted"));
    let pp2 = uid(&t.insert(&a, "ProductionPlan", plan_input.clone()).await);
    t.action(&a, "ProductionPlan", pp2, "get_sales_orders", json!({})).await;
    assert_eq!(t.action(&a, "ProductionPlan", pp2, "get_items", json!({})).await["items"], json!(0), "the order is already planned");
    let made_wos = t.action(&a, "ProductionPlan", pp, "make_work_orders", json!({})).await;
    assert_eq!(made_wos["created"].as_array().unwrap().len(), 2, "the chairs and the frames");
    let plan = t.get("ProductionPlan", pp).await;
    assert_eq!(
        (plan["status"].clone(), dec(&plan["po_items"][0]["ordered_qty"]), dec(&plan["sub_assemblies"][0]["ordered_qty"])),
        (json!("In Process"), n("30"), n("30"))
    );
    let twice = t.k.run_action(&a, "ProductionPlan", pp, "make_work_orders", json!({})).await;
    assert!(matches!(&twice, Err(e) if says(e, "already cover")), "{twice:?}");
    let busy = t.k.cancel(&a, "ProductionPlan", pp, None).await;
    assert!(matches!(&busy, Err(e) if says(e, "work order")), "{busy:?}");
    let frame_wo = t.find_one("WorkOrder", json!({"item": s(frame), "mfo_production_plan": s(pp)})).await;
    assert_eq!(
        (frame_wo["source_warehouse"].clone(), frame_wo["target_warehouse"].clone()),
        (json!(s(stores)), json!(s(stores))),
        "sub-assemblies feed the stores"
    );
    let chair_wo = t.find_one("WorkOrder", json!({"item": s(chair), "mfo_production_plan": s(pp)})).await;
    assert_eq!(chair_wo["sales_order"], json!(s(so)));

    // Without the Procurement app, materials are bought on purchase order drafts per supplier.
    let no_supplier = t.k.run_action(&a, "ProductionPlan", pp, "make_material_requests", json!({})).await;
    assert!(matches!(&no_supplier, Err(e) if says(e, "RAW-STEEL")), "{no_supplier:?}");
    t.update(&a, "Item", steel, json!({"suppliers": [{"supplier": s(steel_corp)}]})).await;
    let bought = t.action(&a, "ProductionPlan", pp, "make_material_requests", json!({})).await;
    let plan_po = t.get("PurchaseOrder", vid(&bought["created"][0])).await;
    assert_eq!(
        (plan_po["supplier"].clone(), dec(&plan_po["items"][0]["qty"]), plan_po["items"][0]["warehouse"].clone()),
        (json!(s(steel_corp)), n("20"), json!(s(stores)))
    );
    let plan = t.get("ProductionPlan", pp).await;
    assert!(row(&plan, "raw_materials", "item", steel)["order_number"].as_str().unwrap().starts_with("PurchaseOrder"));
    let nothing = t.k.run_action(&a, "ProductionPlan", pp, "make_material_requests", json!({})).await;
    assert!(matches!(&nothing, Err(e) if says(e, "no raw material is left")), "{nothing:?}");

    // Producing the plan: frames, then chairs through both operations.
    let frame_wo = uid(&frame_wo);
    t.submit(&a, "WorkOrder", frame_wo).await;
    t.action(&a, "WorkOrder", frame_wo, "manufacture", json!({})).await;
    assert_eq!(dec(&t.get("ProductionPlan", pp).await["sub_assemblies"][0]["produced_qty"]), n("30"));
    let chair_wo = uid(&chair_wo);
    t.submit(&a, "WorkOrder", chair_wo).await;
    let c_cards = cards(chair_wo).await;
    let nov10 = "2026-11-10";
    t.update(&a, "JobCard", uid(&c_cards[0]), json!({"time_logs": [log(nov10, "09:00", "12:00", "30")]})).await;
    t.submit(&a, "JobCard", uid(&c_cards[0])).await;
    t.update(&a, "JobCard", uid(&c_cards[1]), json!({"time_logs": [log(nov10, "13:00", "14:00", "30")]})).await;
    t.submit(&a, "JobCard", uid(&c_cards[1])).await;
    t.action(&a, "WorkOrder", chair_wo, "manufacture", json!({})).await;
    let plan = t.get("ProductionPlan", pp).await;
    assert_eq!((plan["status"].clone(), dec(&plan["total_produced_qty"])), (json!("Completed"), n("30")));
    let status = t.report(&a, "production_plan_status", &[("company", s(cid)), ("production_plan", s(pp))]).await;
    let rows = status["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| dec(&r["progress"]) == n("100")), "{rows:?}");
    t.books_balance(cid).await;

    // ───────────── with HR and Procurement apps: employees and material requests ─────────────
    t.k.install_packages(tenant, &["stand_ins".into()]).await.expect("install the stand-ins");
    t.admin = t.k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    let a = t.admin.clone();
    let asha = t.insert(&a, "Employee", json!({"employee_name": "Asha Rao", "company": s(cid)})).await;
    t.insert(&a, "Employee", json!({"employee_name": "Old Hand", "company": s(cid), "status": "Left"})).await;
    let card = t.update(&a, "JobCard", open_weld, json!({"employee": "Asha Rao"})).await;
    assert_eq!((card["employee"].clone(), card["employee_name"].clone()), (asha["number"].clone(), json!("Asha Rao")), "the operator is an employee now");
    let gone = t.k.update(&a, "JobCard", open_weld, obj(json!({"employee": "Old Hand"})), None).await;
    assert!(matches!(&gone, Err(e) if says(e, "no current employee")), "{gone:?}");
    let mut by_stranger = log("2026-11-12", "09:00", "10:00", "1");
    by_stranger["employee"] = json!("Nobody");
    let stranger = t.k.update(&a, "JobCard", open_weld, obj(json!({"time_logs": [by_stranger]})), None).await;
    assert!(matches!(&stranger, Err(e) if says(e, "no current employee")), "{stranger:?}");
    let mr = t
        .insert(
            &a,
            "MaterialRequest",
            json!({"company": s(cid), "material_request_type": "Manufacture", "schedule_date": "2026-11-20",
                   "items": [{"item": s(chair), "qty": "4", "warehouse": s(fg), "schedule_date": "2026-11-20"}]}),
        )
        .await;
    let mr = uid(&mr);
    t.submit(&a, "MaterialRequest", mr).await;
    let pp3 = uid(&t.insert(&a, "ProductionPlan", plan_input.clone()).await);
    assert_eq!(t.action(&a, "ProductionPlan", pp3, "get_material_requests", json!({})).await["added"], json!(1));
    t.action(&a, "ProductionPlan", pp3, "get_items", json!({})).await;
    t.action(&a, "ProductionPlan", pp3, "get_raw_materials", json!({})).await;
    let plan3 = t.get("ProductionPlan", pp3).await;
    assert_eq!((dec(&plan3["po_items"][0]["planned_qty"]), plan3["po_items"][0]["material_request"].clone()), (n("4"), json!(s(mr))));
    // Steel: 8 + 80 safety against 40 left in stores + the 20 on the submitted order (the plan's own PO is a draft).
    assert_eq!(dec(&row(&plan3, "raw_materials", "item", steel)["qty_to_order"]), n("28"));
    t.submit(&a, "ProductionPlan", pp3).await;
    let requested = t.action(&a, "ProductionPlan", pp3, "make_material_requests", json!({})).await;
    assert_eq!(requested["created"][0]["entity"], json!("MaterialRequest"));
    let request = t.get("MaterialRequest", vid(&requested["created"][0])).await;
    assert_eq!(request["material_request_type"], json!("Purchase"));
    assert_eq!(
        (request["items"][0]["item"].clone(), dec(&request["items"][0]["qty"]), request["items"][0]["warehouse"].clone()),
        (json!(s(steel)), n("28"), json!(s(stores)))
    );
    assert_eq!(t.get("ProductionPlan", pp3).await["status"], json!("Material Requested"));

    // ───────────── subcontracting ─────────────
    let bolt = uid(&t.insert(&a, "Supplier", json!({"supplier_name": "Bolt Co"})).await);
    let freight = uid(&t.find_one("Account", json!({"account_name": "Freight and Forwarding", "company": s(cid)})).await);
    let part_bom = uid(&t
        .insert(&a, "Bom", json!({"company": s(cid), "item": s(part), "quantity": "1", "items": [{"item": s(rm1), "qty": "2"}, {"item": s(rm2), "qty": "1"}]}))
        .await);
    t.submit(&a, "Bom", part_bom).await;
    let sc_bom = json!({"finished_good": s(part), "finished_good_bom": s(part_bom), "service_item": s(plating), "supplier": s(bolt)});
    let mut stock_service = sc_bom.clone();
    stock_service["service_item"] = json!(s(rm1));
    let refused = t.k.insert(&a, "SubcontractingBom", obj(stock_service)).await;
    assert!(matches!(&refused, Err(e) if says(e, "non-stock")), "{refused:?}");
    let scb = uid(&t.insert(&a, "SubcontractingBom", sc_bom.clone()).await);
    let dup = t.k.insert(&a, "SubcontractingBom", obj(sc_bom)).await;
    assert!(matches!(&dup, Err(e) if says(e, "another active")), "{dup:?}");
    let sco = t
        .insert(
            &a,
            "SubcontractingOrder",
            json!({"company": s(cid), "supplier": s(bolt), "supplier_warehouse": s(bolt_store), "set_warehouse": s(fg), "set_reserve_warehouse": s(stores),
                   "schedule_date": "2026-11-15", "items": [{"item": s(part), "qty": "10", "service_rate": "15"}]}),
        )
        .await;
    let line = &sco["items"][0];
    assert_eq!(
        (line["subcontracting_bom"].clone(), line["service_item"].clone(), dec(&line["service_qty"]), dec(&line["service_amount"])),
        (json!(s(scb)), json!(s(plating)), n("10"), n("150"))
    );
    assert_eq!(dec(&row(&sco, "supplied_items", "rm_item", rm1)["required_qty"]), n("20"));
    assert_eq!(row(&sco, "supplied_items", "rm_item", rm2)["source_warehouse"], json!(s(stores)));
    let sco = uid(&sco);
    let sco_doc = t.submit(&a, "SubcontractingOrder", sco).await;
    assert_eq!(sco_doc["status"], json!("Open"));
    let service_po = t.get("PurchaseOrder", sco_doc["purchase_order"].as_str().unwrap().parse().unwrap()).await;
    assert_eq!(
        (service_po["supplier"].clone(), service_po["items"][0]["item"].clone(), dec(&service_po["grand_total"])),
        (json!(s(bolt)), json!(s(plating)), n("150"))
    );
    t.action(&a, "SubcontractingOrder", sco, "make_material_transfer", json!({})).await;
    assert_eq!((t.stock(rm1, bolt_store).await, t.stock(rm2, bolt_store).await), ((n("20"), n("120")), (n("10"), n("20"))));
    assert_eq!(t.get("SubcontractingOrder", sco).await["status"], json!("Material Transferred"));
    let resend = t.k.run_action(&a, "SubcontractingOrder", sco, "make_material_transfer", json!({})).await;
    assert!(matches!(&resend, Err(e) if says(e, "already sent")), "{resend:?}");
    let stock_value_before = t.gl("Stock In Hand").await;

    // First receipt: 6 parts with 30 of freight.
    let r1 = vid(&t.action(&a, "SubcontractingOrder", sco, "make_subcontracting_receipt", json!({})).await);
    let draft = t.get("SubcontractingReceipt", r1).await;
    let r1_line = draft["items"][0]["id"].clone();
    let draft = t
        .update(
            &a,
            "SubcontractingReceipt",
            r1,
            json!({"items": [{"id": r1_line, "qty": "6"}], "additional_costs": [{"description": "Freight", "account": s(freight), "amount": "30"}]}),
        )
        .await;
    assert_eq!(
        (dec(&row(&draft, "supplied_items", "rm_item", rm1)["required_qty"]), dec(&row(&draft, "supplied_items", "rm_item", rm2)["required_qty"])),
        (n("12"), n("6"))
    );
    let done = t.submit(&a, "SubcontractingReceipt", r1).await;
    let got = &done["items"][0];
    // 12 × 6 + 6 × 2 materials + 6 × 15 service + 30 freight.
    assert_eq!(
        (dec(&got["rm_cost"]), dec(&got["service_cost"]), dec(&got["additional_cost"]), dec(&got["amount"]), dec(&got["valuation_rate"])),
        (n("84"), n("90"), n("30"), n("204"), n("34"))
    );
    assert_eq!(t.stock(part, fg).await, (n("6"), n("204")));
    assert_eq!((t.stock(rm1, bolt_store).await, t.stock(rm2, bolt_store).await), ((n("8"), n("48")), (n("4"), n("8"))));
    assert_eq!((t.gl("Administrative Expenses").await, t.gl("Freight and Forwarding").await), (n("-90"), n("-30")), "service and freight move into stock");
    assert_eq!(t.gl("Stock In Hand").await - stock_value_before, n("120"));
    let order = t.get("SubcontractingOrder", sco).await;
    assert_eq!((order["status"].clone(), dec(&order["per_received"]), dec(&order["items"][0]["received_qty"])), (json!("Partially Received"), n("60"), n("6")));
    assert_eq!(dec(&row(&order, "supplied_items", "rm_item", rm1)["consumed_qty"]), n("12"));
    let none_spare = t.k.run_action(&a, "SubcontractingOrder", sco, "return_excess_materials", json!({})).await;
    assert!(matches!(&none_spare, Err(e) if says(e, "holds nothing beyond")), "8 held is what the 4 to come need: {none_spare:?}");

    // Refusals: receiving more than ordered, consuming more than was sent.
    let r_over = vid(&t.action(&a, "SubcontractingOrder", sco, "make_subcontracting_receipt", json!({})).await);
    let over_line = t.get("SubcontractingReceipt", r_over).await["items"][0]["id"].clone();
    t.update(&a, "SubcontractingReceipt", r_over, json!({"items": [{"id": over_line, "qty": "5"}]})).await;
    let over = t.k.submit(&a, "SubcontractingReceipt", r_over, None).await;
    assert!(matches!(&over, Err(e) if says(e, "11 of 10")), "{over:?}");
    t.k.delete(&a, "SubcontractingReceipt", r_over, None).await.expect("drop the draft");
    let r2 = vid(&t.action(&a, "SubcontractingOrder", sco, "make_subcontracting_receipt", json!({})).await);
    let draft = t.get("SubcontractingReceipt", r2).await;
    let mats: Vec<Value> = draft["supplied_items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| if r["rm_item"] == json!(s(rm1)) { json!({"id": r["id"], "consumed_qty": "9"}) } else { json!({"id": r["id"]}) })
        .collect();
    t.update(&a, "SubcontractingReceipt", r2, json!({"supplied_items": mats})).await;
    let greedy = t.k.submit(&a, "SubcontractingReceipt", r2, None).await;
    assert!(matches!(&greedy, Err(e) if says(e, "holds only 8")), "{greedy:?}");
    // The supplier used only 6 of the 8 for the last 4 parts.
    let draft = t.get("SubcontractingReceipt", r2).await;
    let mats: Vec<Value> = draft["supplied_items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| if r["rm_item"] == json!(s(rm1)) { json!({"id": r["id"], "consumed_qty": "6"}) } else { json!({"id": r["id"]}) })
        .collect();
    t.update(&a, "SubcontractingReceipt", r2, json!({"supplied_items": mats})).await;
    let done = t.submit(&a, "SubcontractingReceipt", r2).await;
    assert_eq!(dec(&done["total_amount"]), n("104"), "6 × 6 + 4 × 2 + 4 × 15");
    assert_eq!(t.stock(part, fg).await, (n("10"), n("308")));
    let order = t.get("SubcontractingOrder", sco).await;
    assert_eq!((order["status"].clone(), dec(&order["per_received"])), (json!("Completed"), n("100")));
    assert_eq!(t.stock(rm1, bolt_store).await, (n("2"), n("12")), "two left over at the supplier");

    // The excess comes back. A return is checked against what this order holds at the supplier,
    // even when the warehouse physically holds more (stock of other business there).
    let other = t
        .insert(&a, "StockEntry", json!({"company": s(cid), "purpose": "Material Receipt", "items": [{"item": s(rm1), "qty": "5", "basic_rate": "6", "target_warehouse": s(bolt_store)}]}))
        .await;
    t.submit(&a, "StockEntry", uid(&other)).await;
    let draft_return = vid(&t.action(&a, "SubcontractingOrder", sco, "return_excess_materials", json!({"submit": false})).await);
    let ret_line = t.get("StockEntry", draft_return).await["items"][0]["id"].clone();
    t.update(&a, "StockEntry", draft_return, json!({"items": [{"id": ret_line, "qty": "4"}]})).await;
    let overdrawn = t.k.submit(&a, "StockEntry", draft_return, None).await;
    assert!(matches!(&overdrawn, Err(e) if says(e, "holds only 2")), "{overdrawn:?}");
    t.k.delete(&a, "StockEntry", draft_return, None).await.expect("drop the draft");
    let out = t
        .insert(
            &a,
            "StockEntry",
            json!({"company": s(cid), "purpose": "Material Issue", "items": [{"item": s(rm1), "qty": "5", "source_warehouse": s(bolt_store)}]}),
        )
        .await;
    t.submit(&a, "StockEntry", uid(&out)).await;
    t.action(&a, "SubcontractingOrder", sco, "return_excess_materials", json!({})).await;
    assert_eq!(t.stock(rm1, bolt_store).await, (Decimal::ZERO, Decimal::ZERO));
    let order = t.get("SubcontractingOrder", sco).await;
    let m = row(&order, "supplied_items", "rm_item", rm1);
    assert_eq!((dec(&m["supplied_qty"]), dec(&m["consumed_qty"]), dec(&m["returned_qty"])), (n("20"), n("18"), n("2")));
    let sc_report = t.report(&a, "subcontracting_status", &[("company", s(cid)), ("subcontracting_order", s(sco))]).await;
    let r = sc_report["rows"].as_array().unwrap().iter().find(|r| r["material"] == json!("SC-RM")).cloned().unwrap();
    assert_eq!((dec(&r["consumed_qty"]), dec(&r["returned_qty"]), dec(&r["at_supplier"]), dec(&r["received_qty"])), (n("18"), n("2"), Decimal::ZERO, n("10")));
    let closed = t.k.cancel(&a, "SubcontractingOrder", sco, None).await;
    assert!(matches!(&closed, Err(e) if says(e, "goods received")), "{closed:?}");

    // Cancelling the first receipt reverses it exactly.
    t.cancel(&a, "SubcontractingReceipt", r1).await;
    assert_eq!(t.stock(part, fg).await, (n("4"), n("104")));
    assert_eq!((t.stock(rm1, bolt_store).await, t.stock(rm2, bolt_store).await), ((n("12"), n("72")), (n("6"), n("12"))));
    assert_eq!((t.gl("Administrative Expenses").await, t.gl("Freight and Forwarding").await), (n("-60"), Decimal::ZERO));
    let order = t.get("SubcontractingOrder", sco).await;
    assert_eq!((order["status"].clone(), dec(&order["items"][0]["received_qty"])), (json!("Partially Received"), n("4")));
    assert_eq!(dec(&row(&order, "supplied_items", "rm_item", rm1)["consumed_qty"]), n("6"));
    t.books_balance(cid).await;
}
