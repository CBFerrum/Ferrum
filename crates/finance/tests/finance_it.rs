//! End-to-end finance test against a real PostgreSQL (`FERRUM_FINANCE_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_finance_test`). The database is reset first.
//!
//! A company runs a year of finance the way users drive the API: budgets refuse at their exact
//! boundary, phase by month and count purchase-order and material-request commitments; a bank
//! statement is imported (twice, and overlapping), reconciled partly across two payments and
//! against vouchers made from its lines, and the reconciliation statement explains the bank
//! balance; payment terms split invoices exactly; advances settle invoices (with an exchange
//! difference); a dunning charges interest across a leap year and emails its letter; a fiscal year
//! is closed, refuses back-dated postings, is reopened and closed again; subscriptions bill their
//! periods once, prorate plan changes and cancellations; the upkeep drafts dunnings idempotently.

use chrono::{Datelike, NaiveDate};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, Mailer};
use ferrum_meta::Catalog;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
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

/// Captures what the dunning letters send.
struct Outbox(Mutex<Vec<(String, String, String)>>);

#[async_trait::async_trait]
impl Mailer for Outbox {
    async fn send(&self, to: &str, subject: &str, html: &str) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().push((to.into(), subject.into(), html.into()));
        Ok(())
    }
}

/// A material request shaped like ERPNext's (the procurement package's own may differ): the
/// budget checks read it through the tenant's schema.
const MR_PACKAGE: &str = "package: mrtest\nversion: 0.0.1\ndepends: [buying]\n";
const MR_ENTITIES: &str = r#"
- name: MaterialRequestItem
  label: Material Request Item
  kind: child
  fields:
    - {name: item, type: link, to: Item, required: true}
    - {name: qty, type: decimal, required: true}
    - {name: ordered_qty, type: decimal, default: 0}
    - {name: rate, type: currency}
- name: MaterialRequest
  label: Material Request
  kind: document
  submittable: true
  company_scoped: true
  fields:
    - {name: company, type: link, to: Company, required: true}
    - {name: posting_date, type: date, required: true}
    - {name: material_request_type, type: select, options: [Purchase, Material Transfer], default: Purchase}
    - {name: status, type: select, options: [Draft, Pending, Stopped], default: Draft}
    - {name: project, type: link, to: Project}
    - {name: items, type: table, of: MaterialRequestItem, required: true}
"#;

struct T {
    k: Arc<Kernel>,
    tenant: Uuid,
    admin: Ctx,
    company: Uuid,
}

impl T {
    async fn insert(&self, ctx: &Ctx, entity: &str, v: Value) -> Doc {
        self.k.insert(ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }
    async fn submit(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Doc {
        self.k.submit(ctx, entity, id, None).await.unwrap_or_else(|e| panic!("submit {entity}: {e:?}"))
    }
    /// Create and submit.
    async fn make(&self, ctx: &Ctx, entity: &str, v: Value) -> Doc {
        let d = self.insert(ctx, entity, v).await;
        self.submit(ctx, entity, uid(&d)).await
    }
    /// Create, then fail to submit; the draft is deleted and nothing it would post remains.
    async fn refused(&self, ctx: &Ctx, entity: &str, v: Value) -> Error {
        let d = self.insert(ctx, entity, v).await;
        let e = self.k.submit(ctx, entity, uid(&d), None).await.expect_err("the submit should be refused");
        let posted = self.count("select count(*) from journal_entry where tenant_id = $1 and source_id = $2", Some(uid(&d))).await;
        assert_eq!(posted, 0, "a refused document posts nothing");
        self.k.delete(&self.admin, entity, uid(&d), None).await.expect("drop the refused draft");
        e
    }
    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.admin, entity, id, false).await.unwrap_or_else(|e| panic!("get {entity}: {e:?}"))
    }
    async fn action(&self, ctx: &Ctx, entity: &str, id: Uuid, name: &str, params: Value) -> Value {
        self.k.run_action(ctx, entity, id, name, params).await.unwrap_or_else(|e| panic!("{entity}.{name}: {e:?}"))
    }
    async fn list(&self, entity: &str, filters: Value) -> Vec<Doc> {
        let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), limit: Some(500), ..Default::default() };
        self.k.list(&self.admin, entity, &q).await.unwrap().rows
    }
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let rows = self.list(entity, filters.clone()).await;
        assert_eq!(rows.len(), 1, "{entity} {filters}");
        rows[0].clone()
    }
    async fn count(&self, sql: &str, id: Option<Uuid>) -> i64 {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(self.tenant).bind(id).fetch_one(&mut *tx).await.unwrap()
    }
    async fn amount(&self, sql: &str, id: Option<Uuid>) -> Decimal {
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
    async fn report(&self, ctx: &Ctx, name: &str, params: &[(&str, String)]) -> Value {
        let p: HashMap<String, String> = params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        ferrum_domain::reports::run(&self.k, ctx, name, &p).await.unwrap_or_else(|e| panic!("report {name}: {e:?}"))
    }
    async fn user(&self, email: &str, role: &str) -> Ctx {
        let id = self.k.create_user(&self.admin, email, email, Some("Correct-Horse-9"), &[role.into()], false).await.expect("user");
        self.k.load_ctx(self.tenant, id, None, "it".into()).await.expect("ctx")
    }
    async fn account(&self, name: &str) -> Uuid {
        uid(&self.find_one("Account", json!({"account_name": name, "company": self.company.to_string()})).await)
    }
    fn journal(&self, date: &str, lines: Value) -> Value {
        json!({"company": self.company.to_string(), "posting_date": date, "accounts": lines})
    }
    /// The trial balance balances and every journal entry balances.
    async fn books_balance(&self) {
        let tb = self
            .report(
                &self.admin,
                "trial_balance",
                &[("company", self.company.to_string()), ("from_date", "2024-01-01".into()), ("to_date", "2029-12-31".into())],
            )
            .await;
        assert_eq!(tb["meta"]["balanced"], json!(true), "{}", tb["totals"]);
        let unbalanced = self
            .count("select count(*) from (select entry_id from journal_line where tenant_id = $1 and $2::uuid is null group by entry_id having sum(debit) <> sum(credit)) x", None)
            .await;
        assert_eq!(unbalanced, 0);
    }
}

fn dr(account: Uuid, amount: &str, cost_center: Option<Uuid>, project: Option<Uuid>) -> Value {
    json!({"account": account.to_string(), "debit": amount, "cost_center": cost_center.map(|c| c.to_string()), "project": project.map(|p| p.to_string())})
}

fn cr(account: Uuid, amount: &str) -> Value {
    json!({"account": account.to_string(), "credit": amount})
}

fn invalid(e: &Error, needle: &str) -> bool {
    match e {
        Error::Invalid(m) => m.contains(needle),
        Error::Validation(fs) => fs.iter().any(|f| f.message.contains(needle)),
        _ => false,
    }
}

#[test]
fn finance_end_to_end() {
    // Same runtime shape as the server: unoptimized async hook chains need more than 2 MiB.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_FINANCE_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_finance_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("platform packages");
    packages.extend(ferrum_finance::packages().expect("finance packages"));
    let files = BTreeMap::from([("package.yaml".to_string(), MR_PACKAGE.to_string()), ("entities/mr.yaml".to_string(), MR_ENTITIES.to_string())]);
    packages.push(ferrum_meta::catalog::load_package_files(&files).expect("material request package"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_finance::register(&mut hooks);
    for source in ferrum_finance::reports() {
        ferrum_domain::reports::add_source(source);
    }
    let outbox = Arc::new(Outbox(Mutex::new(vec![])));
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), Some(outbox.clone() as Arc<dyn Mailer>)).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("partitions");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "fin".into(),
            name: "Fin Co".into(),
            admin_email: "admin@fin.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["finance".into(), "mrtest".into()],
            settings: Some(json!({"fiscal_year_start_month": 1})),
        })
        .await
        .expect("tenant");
    let session = k.login("fin", "admin@fin.test", "Correct-Horse-9").await.expect("login");
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    let mut t = T { k: k.clone(), tenant, admin: admin.clone(), company: Uuid::nil() };
    let a = admin.clone();
    let today = chrono::Utc::now().date_naive();

    // ───────────── company, years, cost centers, accounts, parties ─────────────
    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let eur = t.find_one("Currency", json!({"code": "EUR"})).await;
    let company = t.insert(&a, "Company", json!({"company_name": "Fin Co", "abbr": "FC", "default_currency": inr["id"]})).await;
    t.company = uid(&company);
    let cid = t.company.to_string();
    let mut years = HashMap::new();
    for y in [2024, 2025, 2026] {
        let fy = t
            .insert(&a, "FiscalYear", json!({"year_name": y.to_string(), "company": cid, "start_date": format!("{y}-01-01"), "end_date": format!("{y}-12-31")}))
            .await;
        years.insert(y, uid(&fy));
    }
    let main = uid(&t.find_one("CostCenter", json!({"cost_center_name": "Main", "company": cid})).await);
    let operations = uid(&t.insert(&a, "CostCenter", json!({"cost_center_name": "Operations", "company": cid, "is_group": true})).await);
    let marketing =
        uid(&t.insert(&a, "CostCenter", json!({"cost_center_name": "Marketing", "company": cid, "parent_cost_center": operations.to_string()})).await);
    let sales_team =
        uid(&t.insert(&a, "CostCenter", json!({"cost_center_name": "Sales Team", "company": cid, "parent_cost_center": operations.to_string()})).await);
    let website = uid(&t.insert(&a, "Project", json!({"project_name": "Website", "company": cid})).await);
    let admin_exp = t.account("Administrative Expenses").await;
    let rent = t.account("Rent").await;
    let utilities = t.account("Utilities").await;
    let cash = t.account("Cash").await;
    let bank_gl = t.account("Primary Bank").await;
    let sales = t.account("Sales").await;
    let other_income = t.account("Other Income").await;
    let bank_charges = t.account("Bank Charges").await;
    let debtors = t.account("Debtors").await;
    let retained = t.account("Retained Earnings").await;
    let services = t.find_one("ItemGroup", json!({"group_name": "Services"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let item = |code: &str, expense: Option<Uuid>| {
        json!({"item_code": code, "item_name": code, "item_group": services["id"], "stock_uom": nos["id"], "is_stock_item": false,
               "expense_account": expense.map(|e| e.to_string())})
    };
    // Sales lines are priced by the selling engine: one unit of UNIT costs 1, so quantities are amounts.
    let mut unit_item = item("UNIT", None);
    unit_item["standard_rate"] = json!("1");
    let unit = uid(&t.insert(&a, "Item", unit_item).await);
    let mut eur_item = item("EU-SVC", None);
    eur_item["standard_rate"] = json!("9200");
    let eu_svc = uid(&t.insert(&a, "Item", eur_item).await);
    let rent_svc = uid(&t.insert(&a, "Item", item("RENT-SVC", Some(rent))).await);
    let util_svc = uid(&t.insert(&a, "Item", item("UTIL-SVC", Some(utilities))).await);
    let pro_item = uid(&t.insert(&a, "Item", item("SUB-PRO", None)).await);
    let seat_item = uid(&t.insert(&a, "Item", item("SUB-SEAT", None)).await);
    let year_item = uid(&t.insert(&a, "Item", item("SUB-YEAR", None)).await);
    let customer = |name: &str, extra: Value| {
        let mut v = json!({"customer_name": name});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    };
    let acme = uid(&t.insert(&a, "Customer", customer("Acme Corp", json!({"email": "ap@acme.test"}))).await);
    let globex = uid(&t.insert(&a, "Customer", customer("Globex", json!({}))).await);
    let initech = uid(&t.insert(&a, "Customer", customer("Initech", json!({}))).await);
    let hooli = uid(&t.insert(&a, "Customer", customer("Hooli", json!({"email": "billing@hooli.test"}))).await);
    let umbrella = uid(&t.insert(&a, "Customer", customer("Umbrella", json!({}))).await);
    let euro = uid(&t.insert(&a, "Customer", customer("Euro GmbH", json!({"default_currency": eur["id"]}))).await);
    let cyberdyne = uid(&t.insert(&a, "Customer", customer("Cyberdyne", json!({}))).await);
    let tyrell = uid(&t.insert(&a, "Customer", customer("Tyrell", json!({}))).await);
    let supplies = uid(&t.insert(&a, "Supplier", json!({"supplier_name": "Supplies Co"})).await);
    let am = t.user("am@fin.test", "Accounts Manager").await;
    let au = t.user("au@fin.test", "Accounts User").await;
    let am_id = am.user;

    // ───────────── monthly distributions ─────────────
    let even = t.insert(&a, "MonthlyDistribution", json!({"distribution_name": "Even"})).await;
    assert_eq!(even["percentages"].as_array().unwrap().len(), 12, "an empty distribution splits evenly");
    assert_eq!((dec(&even["total_percentage"]), dec(&even["percentages"][11]["percentage_allocation"])), (n("100"), n("8.37")));
    let months = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    let pcts: Vec<Value> =
        months.iter().map(|m| json!({"month": m, "percentage_allocation": match *m { "January" => "50", "February" => "10", _ => "4" }})).collect();
    let front = uid(&t.insert(&a, "MonthlyDistribution", json!({"distribution_name": "Front loaded", "percentages": pcts})).await);
    let mut short = pcts.clone();
    short[0]["percentage_allocation"] = json!("49");
    let e = t.k.insert(&a, "MonthlyDistribution", obj(json!({"distribution_name": "Short", "percentages": short}))).await.unwrap_err();
    assert!(invalid(&e, "99%"), "{e:?}");
    let mut twice = pcts.clone();
    twice[1]["month"] = json!("January");
    let e = t.k.insert(&a, "MonthlyDistribution", obj(json!({"distribution_name": "Twice", "percentages": twice}))).await.unwrap_err();
    assert!(invalid(&e, "twice"), "{e:?}");

    // ───────────── budget against a cost center tree: Stop at the exact boundary ─────────────
    let fy26 = years[&2026].to_string();
    let not_pl = t
        .k
        .insert(&am, "Budget", obj(json!({"company": cid, "fiscal_year": fy26, "cost_center": operations.to_string(), "accounts": [{"account": debtors.to_string(), "budget_amount": "5"}]})))
        .await
        .unwrap_err();
    assert!(invalid(&not_pl, "income or expense"), "{not_pl:?}");
    let po_only =
        t.k.insert(
            &am,
            "Budget",
            obj(json!({"company": cid, "fiscal_year": fy26, "cost_center": operations.to_string(), "applicable_on_purchase_order": true,
                       "applicable_on_booking_actual_expenses": false, "accounts": [{"account": admin_exp.to_string(), "budget_amount": "5"}]})),
        )
        .await
        .unwrap_err();
    assert!(invalid(&po_only, "actual expenses too"), "{po_only:?}");
    let e = t.k.insert(&au, "Budget", obj(json!({"company": cid, "fiscal_year": fy26, "cost_center": main.to_string(), "accounts": []}))).await.unwrap_err();
    assert!(matches!(e, Error::Forbidden(_)), "accounts users read budgets only: {e:?}");
    let b1 = t
        .insert(
            &am,
            "Budget",
            json!({"company": cid, "fiscal_year": fy26, "budget_against": "Cost Center", "cost_center": operations.to_string(),
                   "action_if_accumulated_monthly_budget_exceeded": "Ignore", "accounts": [{"account": admin_exp.to_string(), "budget_amount": "1000"}]}),
        )
        .await;
    assert_eq!((b1["applicable_on_booking_actual_expenses"].clone(), b1["action_if_annual_budget_exceeded"].clone()), (json!(true), json!("Stop")));
    assert_eq!(dec(&b1["total_budget"]), n("1000"));
    t.submit(&am, "Budget", uid(&b1)).await;
    let dup = t
        .k
        .insert(&am, "Budget", obj(json!({"company": cid, "fiscal_year": fy26, "cost_center": operations.to_string(), "accounts": [{"account": admin_exp.to_string(), "budget_amount": "5"}]})))
        .await
        .unwrap_err();
    assert!(invalid(&dup, "already covers"), "{dup:?}");
    t.make(&am, "JournalEntry", t.journal("2026-03-10", json!([dr(admin_exp, "600", Some(marketing), None), cr(cash, "600")]))).await;
    t.make(&am, "JournalEntry", t.journal("2026-03-11", json!([dr(admin_exp, "400", Some(sales_team), None), cr(cash, "400")]))).await;
    let over = t.refused(&am, "JournalEntry", t.journal("2026-03-12", json!([dr(admin_exp, "0.01", Some(marketing), None), cr(cash, "0.01")]))).await;
    assert!(invalid(&over, "Annual budget of 1,000.00") && invalid(&over, "would be exceeded by 0.01"), "one cent over the boundary: {over:?}");
    let back =
        json!([{"account": cash.to_string(), "debit": "100"}, {"account": admin_exp.to_string(), "credit": "100", "cost_center": marketing.to_string()}]);
    t.make(&am, "JournalEntry", t.journal("2026-03-13", back)).await;
    t.make(&am, "JournalEntry", t.journal("2026-03-13", json!([dr(admin_exp, "5000", Some(main), None), cr(cash, "5000")]))).await;

    // ───────────── monthly phasing, commitments of purchase orders, Warn ─────────────
    let b2 = t
        .insert(
            &a,
            "Budget",
            json!({"company": cid, "fiscal_year": fy26, "cost_center": main.to_string(), "monthly_distribution": front.to_string(),
                   "budget_owner": "am@fin.test", "applicable_on_purchase_order": true, "action_if_annual_budget_exceeded_on_po": "Stop",
                   "action_if_accumulated_monthly_budget_exceeded_on_po": "Warn", "action_if_accumulated_monthly_budget_exceeded": "Stop",
                   "accounts": [{"account": rent.to_string(), "budget_amount": "12000"}]}),
        )
        .await;
    t.submit(&a, "Budget", uid(&b2)).await;
    t.make(&am, "JournalEntry", t.journal("2026-01-15", json!([dr(rent, "6000", Some(main), None), cr(cash, "6000")]))).await;
    let january = t.refused(&am, "JournalEntry", t.journal("2026-01-20", json!([dr(rent, "1", Some(main), None), cr(cash, "1")]))).await;
    assert!(invalid(&january, "Accumulated monthly (through 2026-01-31) budget of 6,000.00") && invalid(&january, "exceeded by 1.00"), "{january:?}");
    t.make(&am, "JournalEntry", t.journal("2026-02-05", json!([dr(rent, "1", Some(main), None), cr(cash, "1")]))).await;
    let warned_before = t.count("select count(*) from notification where tenant_id = $1 and user_id = $2 and title like 'Budget%'", Some(am_id)).await;
    let po = t
        .insert(
            &a,
            "PurchaseOrder",
            json!({"company": cid, "supplier": supplies.to_string(), "posting_date": "2026-02-10",
            "items": [{"item": rent_svc.to_string(), "qty": "1", "price_list_rate": "1500"}]}),
        )
        .await;
    assert_eq!((po["items"][0]["fin_expense_account"].clone(), po["items"][0]["fin_cost_center"].clone()), (json!(rent.to_string()), json!(main.to_string())));
    let po = t.submit(&a, "PurchaseOrder", uid(&po)).await;
    assert_eq!(po["docstatus"], json!(1), "Warn lets the order through");
    let warned = t.count("select count(*) from notification where tenant_id = $1 and user_id = $2 and title like 'Budget%'", Some(am_id)).await;
    assert_eq!(warned, warned_before + 1, "the budget owner is told");
    let committed = t.refused(&am, "JournalEntry", t.journal("2026-02-20", json!([dr(rent, "100", Some(main), None), cr(cash, "100")]))).await;
    assert!(invalid(&committed, "ordered 1,500.00"), "the open order's commitment counts: {committed:?}");
    let pi = t.action(&am, "PurchaseOrder", uid(&po), "make_purchase_invoice", json!({})).await;
    t.k.update(&am, "PurchaseInvoice", vid(&pi), obj(json!({"posting_date": "2026-04-02", "bill_no": "SUP-1"})), None).await.expect("bill details");
    let pi = t.submit(&am, "PurchaseInvoice", vid(&pi)).await;
    assert_eq!((pi["items"][0]["cost_center"].clone(), pi["items"][0]["expense_account"].clone()), (json!(main.to_string()), json!(rent.to_string())));
    assert_eq!(t.gl("Rent").await, n("7501"), "billing moved the commitment into actual expense");

    // ───────────── a project budget with material requests ─────────────
    let b3 = t
        .insert(
            &a,
            "Budget",
            json!({"company": cid, "fiscal_year": fy26, "budget_against": "Project", "project": website.to_string(), "cost_center": main.to_string(),
                   "applicable_on_material_request": true, "applicable_on_purchase_order": true, "action_if_annual_budget_exceeded_on_mr": "Stop",
                   "accounts": [{"account": utilities.to_string(), "budget_amount": "500"}]}),
        )
        .await;
    assert_eq!(b3["cost_center"], Value::Null, "a project budget has no cost center");
    t.submit(&a, "Budget", uid(&b3)).await;
    let mr = |qty: &str| json!({"company": cid, "posting_date": "2026-05-01", "project": website.to_string(), "items": [{"item": util_svc.to_string(), "qty": qty, "rate": "100"}]});
    t.make(&a, "MaterialRequest", mr("4")).await;
    let requested = t.refused(&a, "MaterialRequest", mr("2")).await;
    assert!(invalid(&requested, "requested 600.00"), "{requested:?}");
    t.make(&am, "JournalEntry", t.journal("2026-05-05", json!([dr(utilities, "50", Some(main), Some(website)), cr(cash, "50")]))).await;
    let over = t.refused(&am, "JournalEntry", t.journal("2026-05-06", json!([dr(utilities, "60", Some(main), Some(website)), cr(cash, "60")]))).await;
    assert!(invalid(&over, "booked 110.00") && invalid(&over, "requested 400.00"), "requests count against actual spending: {over:?}");

    // ───────────── bank account and statement import ─────────────
    let bad = t
        .k
        .insert(&am, "BankAccount", obj(json!({"account_name": "Bad", "company": cid, "account": bank_gl.to_string(), "iban": "GB82 WEST 1234 5698 7654 33"})))
        .await;
    assert!(matches!(bad, Err(ref e) if invalid(e, "IBAN")), "{bad:?}");
    let till = t.k.insert(&am, "BankAccount", obj(json!({"account_name": "Till", "company": cid, "account": cash.to_string()}))).await;
    assert!(matches!(till, Err(ref e) if invalid(e, "type Bank")), "{till:?}");
    let ba = t
        .insert(
            &am,
            "BankAccount",
            json!({"account_name": "Main Current", "company": cid, "account": bank_gl.to_string(), "bank": "West Bank", "iban": "gb82 west 1234 5698 7654 32",
                   "account_number": "98765432", "is_default": true, "opening_balance": "0", "opening_date": "2026-02-28"}),
        )
        .await;
    let ba_id = uid(&ba);
    assert_eq!(
        (ba["iban"].clone(), ba["masked_number"].clone(), ba["currency"].clone()),
        (json!("GB82WEST12345698765432"), json!("•••• 5432"), inr["id"].clone())
    );
    let seen = t.k.get(&au, "BankAccount", ba_id, false).await.unwrap();
    assert!(!seen.contains_key("iban") && !seen.contains_key("account_number"), "accounts users see the masked number only");
    assert_eq!(seen["masked_number"], json!("•••• 5432"));
    let csv1 = "Date,Description,Reference,Amount\n05/03/2026,Payment from Acme Corp,INV-1001,1500.00\n06/03/2026,Bank charges,,-25.00\n\
                06/03/2026,Coffee,,-3.50\n06/03/2026,Coffee,,-3.50\n10/03/2026,\"Transfer, Globex\",G-77,\"2,000.00\"\n";
    let mapping = json!({"date": "Date", "description": "Description", "reference": "Reference", "amount": "Amount"});
    let e = t.k.run_action(&au, "BankAccount", ba_id, "import_transactions", json!({"csv": csv1, "mapping": mapping})).await.unwrap_err();
    assert!(matches!(e, Error::Forbidden(_)), "{e:?}");
    let first = t.action(&am, "BankAccount", ba_id, "import_transactions", json!({"csv": csv1, "mapping": mapping})).await;
    assert_eq!((first["imported"].clone(), first["duplicates"].clone()), (json!(5), json!(0)), "both coffees are real: {first}");
    let again = t.action(&am, "BankAccount", ba_id, "import_transactions", json!({"csv": csv1, "mapping": mapping})).await;
    assert_eq!((again["imported"].clone(), again["duplicates"].clone()), (json!(0), json!(5)), "{again}");
    let csv2 = "Txn Date;Narration;Ref;Debit;Credit\n06.03.2026;Coffee;;3,50;\n10.03.2026;Transfer, Globex;G-77;;2.000,00\n12.03.2026;Rent March;R-3;800,00;\n";
    let overlap = t
        .action(
            &am,
            "BankAccount",
            ba_id,
            "import_transactions",
            json!({"csv": csv2, "decimal_comma": true, "mapping": {"date": 1, "description": "Narration", "reference": "Ref", "withdrawal": "Debit", "deposit": "Credit"}}),
        )
        .await;
    assert_eq!((overlap["imported"].clone(), overlap["duplicates"].clone()), (json!(1), json!(2)), "an overlapping statement adds only what is new: {overlap}");
    let broken =
        t.k.run_action(
            &am,
            "BankAccount",
            ba_id,
            "import_transactions",
            json!({"csv": "Date,Description,Amount\nyesterday,Oops,5\n", "mapping": {"date": "Date", "amount": "Amount"}}),
        )
        .await
        .unwrap_err();
    assert!(invalid(&broken, "line 2"), "{broken:?}");
    assert_eq!(t.list("BankTransaction", json!({"bank_account": ba_id.to_string()})).await.len(), 6);

    // ───────────── reconciling one deposit across two payments ─────────────
    let si_acme = t
        .make(
            &am,
            "SalesInvoice",
            json!({"company": cid, "customer": acme.to_string(), "posting_date": "2026-03-01",
            "items": [{"item": unit.to_string(), "qty": "1500"}]}),
        )
        .await;
    let pay = |party: Uuid, kind: &str, date: &str, amount: &str, reference: &str, invoice: Option<Uuid>| {
        let party_field = if kind == "Pay" { "supplier" } else { "customer" };
        let mut v = json!({"company": cid, "payment_type": kind, party_field: party.to_string(), "posting_date": date, "bank_account": bank_gl.to_string(),
                           "currency": inr["id"], "paid_amount": amount, "reference_no": reference});
        if let Some(inv) = invoice {
            v["references"] = json!([{"invoice_type": "Sales Invoice", "invoice": inv.to_string(), "allocated_amount": amount}]);
        }
        v
    };
    let pe1 = uid(&t.make(&am, "PaymentEntry", pay(acme, "Receive", "2026-03-04", "1000", "INV-1001", Some(uid(&si_acme)))).await);
    let pe2 = uid(&t.make(&am, "PaymentEntry", pay(acme, "Receive", "2026-03-04", "500", "ACME-2", Some(uid(&si_acme)))).await);
    let t1 = uid(&t.find_one("BankTransaction", json!({"description": "Payment from Acme Corp"})).await);
    let sug = t.action(&au, "BankTransaction", t1, "get_suggestions", json!({})).await;
    let found: Vec<&Value> = sug["suggestions"].as_array().unwrap().iter().collect();
    assert_eq!(found[0]["payment_entry"], json!(pe1.to_string()), "the reference match ranks first: {sug}");
    assert_eq!((found[0]["reference_match"].clone(), found[0]["exact_amount"].clone()), (json!(true), json!(false)));
    assert!(found.iter().any(|s| s["payment_entry"] == json!(pe2.to_string())));
    let r =
        t.action(&au, "BankTransaction", t1, "reconcile", json!({"vouchers": [{"payment_document": "PaymentEntry", "payment_entry": pe1.to_string()}]})).await;
    assert_eq!((r["status"].clone(), dec(&r["unallocated_amount"])), (json!("Partially Reconciled"), n("500")));
    assert_eq!(t.get("PaymentEntry", pe1).await["fin_clearance_date"], json!("2026-03-05"));
    let over =
        t.k.run_action(
            &au,
            "BankTransaction",
            t1,
            "reconcile",
            json!({"vouchers": [{"payment_document": "PaymentEntry", "payment_entry": pe2.to_string(), "allocated_amount": "600"}]}),
        )
        .await
        .unwrap_err();
    assert!(invalid(&over, "500 left to clear"), "{over:?}");
    let r =
        t.action(&au, "BankTransaction", t1, "reconcile", json!({"vouchers": [{"payment_document": "PaymentEntry", "payment_entry": pe2.to_string()}]})).await;
    assert_eq!((r["status"].clone(), r["allocations"].as_array().unwrap().len()), (json!("Reconciled"), 2));
    let e = t.k.cancel(&am, "PaymentEntry", pe1, None).await.unwrap_err();
    assert!(invalid(&e, "reconciled with bank transaction"), "{e:?}");
    let r = t.action(&au, "BankTransaction", t1, "unreconcile", json!({"payment_entry": pe2.to_string()})).await;
    assert_eq!(r["status"], json!("Partially Reconciled"));
    assert_eq!(t.get("PaymentEntry", pe2).await["fin_clearance_date"], Value::Null);
    t.action(&au, "BankTransaction", t1, "reconcile", json!({"vouchers": [{"payment_document": "PaymentEntry", "payment_entry": pe2.to_string()}]})).await;
    assert_eq!(t.get("PaymentEntry", pe2).await["fin_clearance_date"], json!("2026-03-05"));

    // ───────────── vouchers made from unmatched lines ─────────────
    let charges = uid(&t.find_one("BankTransaction", json!({"description": "Bank charges"})).await);
    let e = t.k.run_action(&au, "BankTransaction", charges, "make_journal_entry", json!({"account": bank_charges.to_string()})).await.unwrap_err();
    assert!(matches!(e, Error::Forbidden(_)), "accounts users cannot submit journals: {e:?}");
    t.action(&am, "BankTransaction", charges, "make_journal_entry", json!({"account": bank_charges.to_string()})).await;
    assert_eq!(t.get("BankTransaction", charges).await["status"], json!("Reconciled"));
    assert_eq!(t.gl("Bank Charges").await, n("25"));
    let coffees = t.list("BankTransaction", json!({"description": "Coffee"})).await;
    assert_eq!(coffees.len(), 2);
    t.action(&am, "BankTransaction", uid(&coffees[0]), "make_journal_entry", json!({"account": admin_exp.to_string(), "cost_center": main.to_string()})).await;
    let t_globex = uid(&t.find_one("BankTransaction", json!({"description": "Transfer, Globex"})).await);
    let made = t.action(&au, "BankTransaction", t_globex, "make_payment_entry", json!({"party": globex.to_string()})).await;
    let pe_globex = vid(&made);
    let pe_doc = t.get("PaymentEntry", pe_globex).await;
    assert_eq!(
        (pe_doc["docstatus"].clone(), dec(&pe_doc["unallocated_amount"]), pe_doc["fin_clearance_date"].clone()),
        (json!(1), n("2000"), json!("2026-03-10"))
    );
    assert_eq!(t.get("BankTransaction", t_globex).await["status"], json!("Reconciled"));
    t.make(&am, "PaymentEntry", pay(supplies, "Pay", "2026-03-20", "300", "CHQ-9", None)).await;
    t.make(&am, "PaymentEntry", pay(acme, "Receive", "2026-03-30", "250", "ACME-ADV", None)).await;

    let st = t.report(&am, "bank_reconciliation_statement", &[("bank_account", ba_id.to_string()), ("as_of", "2026-03-31".into())]).await;
    let tot = &st["totals"];
    assert_eq!(
        (dec(&tot["book_balance"]), dec(&tot["uncleared_deposits"]), dec(&tot["uncleared_payments"]), dec(&tot["unrecorded"])),
        (n("3421.5"), n("250"), n("300"), n("-803.5")),
        "{st}"
    );
    assert_eq!((dec(&tot["expected_bank_balance"]), dec(&tot["statement_balance"]), dec(&tot["difference"])), (n("2668"), n("2668"), Decimal::ZERO));
    assert_eq!(st["meta"]["reconciled"], json!(true));

    // An order's cost center reaches the invoice made from it; a supplier advance settles a purchase invoice.
    let po2 = t
        .make(
            &a,
            "PurchaseOrder",
            json!({"company": cid, "supplier": supplies.to_string(), "posting_date": "2026-03-21", "fin_cost_center": sales_team.to_string(),
            "items": [{"item": util_svc.to_string(), "qty": "2", "price_list_rate": "100"}]}),
        )
        .await;
    assert_eq!(po2["items"][0]["fin_cost_center"], json!(sales_team.to_string()));
    let pi2 = t.action(&am, "PurchaseOrder", uid(&po2), "make_purchase_invoice", json!({})).await;
    t.k.update(&am, "PurchaseInvoice", vid(&pi2), obj(json!({"posting_date": "2026-03-25", "bill_no": "SUP-2"})), None).await.expect("bill details");
    let pi2 = t.submit(&am, "PurchaseInvoice", vid(&pi2)).await;
    assert_eq!(pi2["items"][0]["cost_center"], json!(sales_team.to_string()), "booked where the order committed it");
    let r = t.action(&am, "PurchaseInvoice", uid(&pi2), "allocate_advances", json!({})).await;
    assert_eq!((dec(&r["allocated"]), dec(&r["outstanding"])), (n("200"), Decimal::ZERO), "the 300 paid ahead covers the 200 bill");
    assert_eq!(t.get("PurchaseInvoice", uid(&pi2)).await["status"], json!("Paid"));
    let supplier_left = t
        .amount(
            "select coalesce(sum(outstanding), 0) from open_item where tenant_id = $1 and supplier_id = $2 and entity = 'PaymentEntry' and status = 'open'",
            Some(supplies),
        )
        .await;
    assert_eq!(supplier_left, n("-100"));

    // ───────────── payment terms and schedules ─────────────
    let bad_terms =
        t.k.insert(&am, "PaymentTermsTemplate", obj(json!({"template_name": "Bad", "terms": [{"invoice_portion": "50"}, {"invoice_portion": "49"}]}))).await;
    assert!(matches!(bad_terms, Err(ref e) if invalid(e, "99%")), "{bad_terms:?}");
    let thirds = uid(&t
        .insert(
            &am,
            "PaymentTermsTemplate",
            json!({"template_name": "Thirds", "terms": [
                {"description": "First third", "invoice_portion": "33.333", "due_date_based_on": "Day(s) after invoice date", "credit_days": 30,
                 "discount_percent": "2", "discount_days": 10},
                {"description": "Second third", "invoice_portion": "33.333", "due_date_based_on": "Month(s) after the end of the invoice month", "credit_months": 1},
                {"description": "Balance", "invoice_portion": "33.334", "due_date_based_on": "Day of the month", "day_of_month": 15, "credit_months": 2}]}),
        )
        .await);
    t.k.update(&a, "Customer", initech, obj(json!({"fin_payment_terms_template": thirds.to_string()})), None).await.expect("customer terms");
    let si_i = t
        .insert(
            &am,
            "SalesInvoice",
            json!({"company": cid, "customer": initech.to_string(), "posting_date": "2026-01-31",
            "items": [{"item": unit.to_string(), "qty": "1000.01"}]}),
        )
        .await;
    assert_eq!(si_i["fin_payment_terms_template"], json!(thirds.to_string()), "the customer's terms apply");
    let sched = si_i["fin_payment_schedule"].as_array().unwrap().clone();
    let row = |d: &str| sched.iter().find(|r| r["description"] == json!(d)).unwrap().clone();
    assert_eq!((row("First third")["due_date"].clone(), dec(&row("First third")["payment_amount"])), (json!("2026-03-02"), n("333.33")));
    assert_eq!((row("First third")["discount_date"].clone(), dec(&row("First third")["discount_amount"])), (json!("2026-02-10"), n("6.67")));
    assert_eq!((row("Second third")["due_date"].clone(), dec(&row("Second third")["payment_amount"])), (json!("2026-02-28"), n("333.33")));
    assert_eq!(
        (row("Balance")["due_date"].clone(), dec(&row("Balance")["payment_amount"])),
        (json!("2026-03-15"), n("333.35")),
        "the last absorbs the rounding"
    );
    assert_eq!(si_i["due_date"], json!("2026-03-15"), "due when the last installment is");
    let repriced = t.k.update(&am, "SalesInvoice", uid(&si_i), obj(json!({"items": [{"item": unit.to_string(), "qty": "900"}]})), None).await.unwrap();
    let total: Decimal = repriced["fin_payment_schedule"].as_array().unwrap().iter().map(|r| dec(&r["payment_amount"])).sum();
    assert_eq!(total, n("900"), "the schedule follows the total");
    let si_i = t.k.update(&am, "SalesInvoice", uid(&si_i), obj(json!({"items": [{"item": unit.to_string(), "qty": "1000.01"}]})), None).await.unwrap();
    t.submit(&am, "SalesInvoice", uid(&si_i)).await;
    let manual =
        t.k.insert(
            &am,
            "SalesInvoice",
            obj(json!({"company": cid, "customer": globex.to_string(), "posting_date": "2026-02-01",
            "items": [{"item": unit.to_string(), "qty": "150"}], "fin_payment_schedule": [{"due_date": "2026-02-15", "payment_amount": "100"}]})),
        )
        .await;
    assert!(matches!(manual, Err(ref e) if invalid(e, "adds up to 100")), "{manual:?}");
    let so = t
        .insert(
            &a,
            "SalesOrder",
            json!({"company": cid, "customer": initech.to_string(), "posting_date": "2026-02-01",
            "items": [{"item": unit.to_string(), "qty": "300"}]}),
        )
        .await;
    assert_eq!(so["fin_payment_schedule"].as_array().unwrap().len(), 3, "orders carry the schedule too");
    t.make(&am, "PaymentEntry", pay(initech, "Receive", "2026-03-05", "400", "INI-1", Some(uid(&si_i)))).await;
    let due = t.report(&am, "receivables_by_due_date", &[("company", cid.clone()), ("as_of", "2026-03-10".into()), ("customer", initech.to_string())]).await;
    let rows = due["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "the earliest installment is paid: {due}");
    assert_eq!((rows[0]["due_date"].clone(), dec(&rows[0]["outstanding"]), dec(&rows[0]["d1_30"])), (json!("2026-03-02"), n("266.66"), n("266.66")));
    assert_eq!((rows[1]["due_date"].clone(), dec(&rows[1]["not_due"])), (json!("2026-03-15"), n("333.35")));
    assert_eq!(dec(&due["totals"]["outstanding_base"]), n("600.01"));

    // ───────────── advances settle invoices ─────────────
    let invoice = |customer: Uuid, date: &str, amount: &str| json!({"company": cid, "customer": customer.to_string(), "posting_date": date, "items": [{"item": unit.to_string(), "qty": amount}]});
    let si_g = uid(&t.make(&am, "SalesInvoice", invoice(globex, "2026-03-15", "1500")).await);
    let r = t.action(&am, "SalesInvoice", si_g, "allocate_advances", json!({})).await;
    assert_eq!((dec(&r["allocated"]), dec(&r["outstanding"])), (n("1500"), Decimal::ZERO));
    assert_eq!(t.get("SalesInvoice", si_g).await["status"], json!("Paid"));
    let si_g2 = uid(&t.make(&am, "SalesInvoice", invoice(globex, "2026-03-20", "800")).await);
    let r = t.action(&am, "SalesInvoice", si_g2, "allocate_advances", json!({})).await;
    assert_eq!(dec(&r["allocated"]), n("500"), "only what is left of the advance");
    let si_g2_doc = t.get("SalesInvoice", si_g2).await;
    assert_eq!((dec(&si_g2_doc["outstanding_amount"]), si_g2_doc["status"].clone()), (n("300"), json!("Partly Paid")));
    let none_left = t.k.run_action(&am, "SalesInvoice", si_g2, "allocate_advances", json!({})).await.unwrap_err();
    assert!(invalid(&none_left, "no unallocated payments"), "{none_left:?}");
    let advance_left = "select outstanding from open_item where tenant_id = $1 and id = $2";
    assert_eq!(t.amount(advance_left, Some(pe_globex)).await, Decimal::ZERO);
    t.k.cancel(&am, "SalesInvoice", si_g2, None).await.expect("cancel undoes its pairing");
    assert_eq!(t.amount(advance_left, Some(pe_globex)).await, n("-500"));
    let e = t.k.cancel(&am, "PaymentEntry", pe_globex, None).await.unwrap_err();
    assert!(invalid(&e, "reconciled"), "{e:?}");
    t.action(&am, "BankTransaction", t_globex, "unreconcile", json!({})).await;
    t.k.cancel(&am, "PaymentEntry", pe_globex, None).await.expect("cancel the advance");
    let si_g_doc = t.get("SalesInvoice", si_g).await;
    assert_eq!((dec(&si_g_doc["outstanding_amount"]), si_g_doc["status"].clone()), (n("1500"), json!("Unpaid")), "the invoice is open again");
    // In a foreign currency the two rates differ: the difference goes to exchange gain/loss.
    for (rate, from) in [("90", "2026-01-01"), ("92", "2026-04-01")] {
        t.insert(&a, "ExchangeRate", json!({"from_currency": eur["id"], "to_currency": inr["id"], "rate": rate, "valid_from": from})).await;
    }
    let pe_eur = t
        .make(
            &am,
            "PaymentEntry",
            json!({"company": cid, "payment_type": "Receive", "customer": euro.to_string(), "posting_date": "2026-03-01",
            "bank_account": cash.to_string(), "currency": eur["id"], "paid_amount": "100"}),
        )
        .await;
    assert_eq!(dec(&pe_eur["base_paid_amount"]), n("9000"));
    let si_eur = t
        .make(
            &am,
            "SalesInvoice",
            json!({"company": cid, "customer": euro.to_string(), "posting_date": "2026-04-05", "items": [{"item": eu_svc.to_string(), "qty": "1"}]}),
        )
        .await;
    assert_eq!((si_eur["currency"].clone(), dec(&si_eur["base_grand_total"])), (eur["id"].clone(), n("9200")));
    t.action(&am, "SalesInvoice", uid(&si_eur), "allocate_advances", json!({})).await;
    assert_eq!(t.get("SalesInvoice", uid(&si_eur)).await["status"], json!("Paid"));
    assert_eq!(t.gl("Exchange Gain or Loss").await, n("200"), "received at 90, invoiced at 92");
    let euro_balance = t.amount("select coalesce(sum(debit - credit), 0) from journal_line where tenant_id = $1 and customer_id = $2", Some(euro)).await;
    assert_eq!(euro_balance, Decimal::ZERO, "nothing is left on the customer's account in base currency");

    // ───────────── dunning with interest across a leap year ─────────────
    let bad_type =
        t.k.insert(
            &am,
            "DunningType",
            obj(json!({"dunning_type": "Bad", "company": cid, "levels": [{"level": 1, "days_overdue": 20}, {"level": 2, "days_overdue": 10}]})),
        )
        .await;
    assert!(matches!(bad_type, Err(ref e) if invalid(e, "more days overdue")), "{bad_type:?}");
    let letter = "Dear {{ customer_name }}, you owe {{ grand_total }} {{ currency }} including interest of {{ interest_amount }}.{% for r in overdue %} [{{ r.overdue_days }}d]{% endfor %}";
    let dtype = uid(&t
        .insert(
            &am,
            "DunningType",
            json!({"dunning_type": "Standard", "company": cid, "is_default": true, "income_account": other_income.to_string(), "levels": [
                {"level": 1, "days_overdue": 1, "subject": "Friendly reminder"},
                {"level": 2, "days_overdue": 30, "dunning_fee": "10", "interest_rate": "10"},
                {"level": 3, "days_overdue": 90, "dunning_fee": "25", "interest_rate": "10", "subject": "Final notice {{ dunning_level }} for {{ customer_name }}",
                 "letter_text": letter}]}),
        )
        .await);
    let si_h = uid(&t
        .make(
            &am,
            "SalesInvoice",
            json!({"company": cid, "customer": hooli.to_string(), "posting_date": "2027-11-01", "due_date": "2027-12-01",
            "items": [{"item": unit.to_string(), "qty": "1000"}]}),
        )
        .await);
    let dn = t.insert(&am, "Dunning", json!({"company": cid, "customer": hooli.to_string(), "posting_date": "2028-03-01"})).await;
    assert_eq!((dn["dunning_type"].clone(), dn["dunning_level"].clone()), (json!(dtype.to_string()), json!(3)));
    let od = &dn["overdue_payments"][0];
    assert_eq!((od["overdue_days"].clone(), dec(&od["outstanding"]), dec(&od["interest"])), (json!(91), n("1000"), n("24.89")), "30/365 + 61/366 of 10%");
    assert_eq!((dec(&dn["dunning_fee"]), dec(&dn["dunning_amount"]), dec(&dn["grand_total"])), (n("25"), n("49.89"), n("1049.89")));
    let text = dn["letter"].as_str().unwrap();
    assert!(text.contains("Dear Hooli, you owe 1,049.89 INR including interest of 24.89.") && text.contains("[91d]"), "{text}");
    assert_eq!(dn["letter_subject"], json!("Final notice 3 for Hooli"));
    let dn = t.submit(&am, "Dunning", uid(&dn)).await;
    assert_eq!(dn["status"], json!("Unresolved"));
    assert_eq!(t.gl("Other Income").await, n("-49.89"));
    t.action(&am, "Dunning", uid(&dn), "send_letter", json!({})).await;
    let sent = outbox.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!((sent[0].0.as_str(), sent[0].1.as_str()), ("billing@hooli.test", "Final notice 3 for Hooli"));
    assert!(sent[0].2.contains("24.89"));
    assert_eq!(t.get("Dunning", uid(&dn)).await["letter_sent_to"], json!("billing@hooli.test"));
    t.make(
        &am,
        "PaymentEntry",
        json!({"company": cid, "payment_type": "Receive", "customer": hooli.to_string(), "posting_date": "2028-03-05",
        "bank_account": bank_gl.to_string(), "currency": inr["id"], "paid_amount": "49.89",
        "references": [{"invoice_type": "Sales Invoice", "invoice": uid(&dn).to_string(), "allocated_amount": "49.89"}]}),
    )
    .await;
    let e = t.k.cancel(&am, "Dunning", uid(&dn), None).await.unwrap_err();
    assert!(invalid(&e, "payments are allocated"), "a paid fee keeps its dunning: {e:?}");

    // ───────────── closing fiscal year 2025 ─────────────
    t.make(&am, "JournalEntry", t.journal("2025-06-30", json!([dr(admin_exp, "300", Some(main), None), cr(cash, "300")]))).await;
    let income =
        json!([{"account": cash.to_string(), "debit": "1000"}, {"account": sales.to_string(), "credit": "1000", "cost_center": marketing.to_string()}]);
    t.make(&am, "JournalEntry", t.journal("2025-07-15", income)).await;
    t.make(&am, "SalesInvoice", invoice(acme, "2025-08-01", "200")).await;
    let draft = t.insert(&am, "JournalEntry", t.journal("2025-09-01", json!([dr(admin_exp, "1", Some(main), None), cr(cash, "1")]))).await;
    let early = t.insert(&am, "PeriodClosingVoucher", json!({"company": cid, "fiscal_year": years[&2026].to_string()})).await;
    let e = t.k.submit(&am, "PeriodClosingVoucher", uid(&early), None).await.unwrap_err();
    assert!(invalid(&e, "close the previous fiscal year 2025 first"), "{e:?}");
    t.k.delete(&am, "PeriodClosingVoucher", uid(&early), None).await.unwrap();
    let pcv = t.insert(&am, "PeriodClosingVoucher", json!({"company": cid, "fiscal_year": years[&2025].to_string()})).await;
    assert_eq!((pcv["posting_date"].clone(), pcv["closing_account"].clone()), (json!("2025-12-31"), json!(retained.to_string())));
    let e = t.k.submit(&am, "PeriodClosingVoucher", uid(&pcv), None).await.unwrap_err();
    assert!(invalid(&e, "unposted drafts: 1 Journal Entry"), "{e:?}");
    t.k.delete(&am, "JournalEntry", uid(&draft), None).await.unwrap();
    let pcv = t.submit(&am, "PeriodClosingVoucher", uid(&pcv)).await;
    assert_eq!(dec(&pcv["net_profit"]), n("900"), "1000 + 200 income - 300 expense");
    let pl_left = "select count(*) from (select l.account_id, l.cost_center_id from journal_line l join e_account a on a.tenant_id = l.tenant_id \
                   and a.id = l.account_id where l.tenant_id = $1 and a.root_type in ('Income', 'Expense') and l.posting_date between '2025-01-01' \
                   and '2025-12-31' and $2::uuid is null group by 1, 2 having sum(l.debit - l.credit) <> 0) x";
    assert_eq!(t.count(pl_left, None).await, 0, "every income and expense balance of the year is closed, per cost center");
    assert_eq!(t.gl("Retained Earnings").await, n("-900"));
    let marketing_re = "select coalesce(sum(l.debit - l.credit), 0) from journal_line l where l.tenant_id = $1 and l.cost_center_id = $2 and l.account_id = \
                        (select id from e_account where tenant_id = $1 and account_name = 'Retained Earnings')";
    assert_eq!(t.amount(marketing_re, Some(marketing)).await, n("-1000"), "the result is closed per cost center");
    let late = t.insert(&am, "JournalEntry", t.journal("2025-12-15", json!([dr(admin_exp, "50", Some(main), None), cr(cash, "50")]))).await;
    let e = t.k.submit(&am, "JournalEntry", uid(&late), None).await.unwrap_err();
    assert!(invalid(&e, "closed"), "the closed year refuses back-dated postings: {e:?}");
    let e = t.k.insert(&am, "PeriodClosingVoucher", obj(json!({"company": cid, "fiscal_year": years[&2024].to_string()}))).await.unwrap();
    let e = t.k.submit(&am, "PeriodClosingVoucher", uid(&e), None).await.unwrap_err();
    assert!(invalid(&e, "a later year is already closed"), "{e:?}");
    t.k.cancel(&am, "PeriodClosingVoucher", uid(&pcv), None).await.expect("reopen the year");
    assert_eq!(t.gl("Retained Earnings").await, Decimal::ZERO);
    let reversal_day: NaiveDate =
        sqlx::query_scalar("select posting_date from journal_entry where tenant_id = $1 and source_id = $2 and reversal_of is not null")
            .bind(tenant)
            .bind(uid(&pcv))
            .fetch_one(&mut *k.db.tenant_tx(tenant).await.unwrap())
            .await
            .unwrap();
    assert_eq!(reversal_day, NaiveDate::from_ymd_opt(2025, 12, 31).unwrap(), "the reversal stays in the closed year");
    t.submit(&am, "JournalEntry", uid(&late)).await;
    t.action(&am, "FiscalYear", years[&2025], "close_period", json!({"period_start": "2025-12-01"})).await;
    let again = t.k.amend(&am, "PeriodClosingVoucher", uid(&pcv)).await.unwrap();
    let again = t.submit(&am, "PeriodClosingVoucher", uid(&again)).await;
    assert_eq!(dec(&again["net_profit"]), n("850"), "posted on the last day even though December was already closed");
    assert_eq!(again["closed_periods"].as_array().unwrap().len(), 11, "December was closed before; cancelling leaves it closed");
    let open_2025 = t.count("select count(*) from accounting_period where tenant_id = $1 and status = 'open' and period_start between '2025-01-01' and '2025-12-31' and $2::uuid is null", None).await;
    assert_eq!(open_2025, 0);
    assert_eq!(t.gl("Retained Earnings").await, n("-850"));

    // ───────────── subscriptions ─────────────
    let plan = |name: &str, item: Uuid, price: &str, interval: &str| json!({"plan_name": name, "item": item.to_string(), "currency": inr["id"], "price": price, "billing_interval": interval, "interval_count": 1});
    let pro = uid(&t.insert(&am, "SubscriptionPlan", plan("Pro Monthly", pro_item, "310", "Month")).await);
    let seat = uid(&t.insert(&am, "SubscriptionPlan", plan("Seat", seat_item, "31", "Month")).await);
    let annual = uid(&t.insert(&am, "SubscriptionPlan", plan("Annual", year_item, "1200", "Year")).await);
    let mixed =
        t.k.insert(
            &am,
            "Subscription",
            obj(json!({"company": cid, "customer": umbrella.to_string(), "start_date": "2026-01-10",
            "plans": [{"plan": pro.to_string(), "qty": "1"}, {"plan": annual.to_string(), "qty": "1"}]})),
        )
        .await;
    assert!(matches!(mixed, Err(ref e) if invalid(e, "same interval")), "{mixed:?}");
    let s1 = t
        .insert(
            &am,
            "Subscription",
            json!({"company": cid, "customer": umbrella.to_string(), "start_date": "2026-01-10", "trial_end": "2026-01-19",
            "billing_anchor": "2026-02-01", "plans": [{"plan": pro.to_string(), "qty": "1"}]}),
        )
        .await;
    let s1 = uid(&s1);
    assert_eq!(t.get("Subscription", s1).await["next_invoice_date"], json!("2026-01-20"), "billing starts after the trial");
    let r = t.action(&am, "Subscription", s1, "generate_invoices", json!({"as_of": "2026-01-19"})).await;
    assert_eq!(r["invoices"].as_array().unwrap().len(), 0, "nothing during the trial");
    let r = t.action(&am, "Subscription", s1, "generate_invoices", json!({"as_of": "2026-02-15"})).await;
    let made: Vec<Uuid> = r["invoices"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().parse().unwrap()).collect();
    assert_eq!(made.len(), 2);
    let stub = t.get("SalesInvoice", made[0]).await;
    assert_eq!(
        (stub["posting_date"].clone(), dec(&stub["grand_total"]), stub["fin_subscription_period_start"].clone(), stub["fin_subscription_period_end"].clone()),
        (json!("2026-01-20"), n("120"), json!("2026-01-20"), json!("2026-01-31")),
        "12 of January's 31 days up to the anchor"
    );
    assert_eq!(stub["docstatus"], json!(1));
    let february = t.get("SalesInvoice", made[1]).await;
    assert_eq!((february["posting_date"].clone(), dec(&february["grand_total"])), (json!("2026-02-01"), n("310")));
    let r = t.action(&am, "Subscription", s1, "generate_invoices", json!({"as_of": "2026-02-15"})).await;
    assert_eq!(r["invoices"].as_array().unwrap().len(), 0, "a period is billed once");
    assert_eq!(t.count("select count(*) from fin_subscription_period where tenant_id = $1 and subscription_id = $2", Some(s1)).await, 2);
    let e = t.k.update(&am, "Subscription", s1, obj(json!({"plans": [{"plan": pro.to_string(), "qty": "2"}]})), None).await.unwrap_err();
    assert!(invalid(&e, "Change Plans"), "{e:?}");
    let up = t
        .action(
            &am,
            "Subscription",
            s1,
            "change_plans",
            json!({"effective_date": "2026-02-15",
            "plans": [{"plan": pro.to_string(), "qty": "1"}, {"plan": seat.to_string(), "qty": "5"}]}),
        )
        .await;
    let up_inv = t.get("SalesInvoice", up["invoices"][0].as_str().unwrap().parse().unwrap()).await;
    assert_eq!(
        (dec(&up_inv["grand_total"]), dec(&up_inv["items"][0]["rate"]), up_inv["posting_date"].clone()),
        (n("77.5"), n("15.5"), json!("2026-02-15")),
        "5 seats for 14 of 28 days"
    );
    let down = t
        .action(
            &am,
            "Subscription",
            s1,
            "change_plans",
            json!({"effective_date": "2026-02-22",
            "plans": [{"plan": pro.to_string(), "qty": "1"}, {"plan": seat.to_string(), "qty": "2"}]}),
        )
        .await;
    let credit = t.get("SalesInvoice", down["invoices"][0].as_str().unwrap().parse().unwrap()).await;
    assert_eq!(
        (credit["is_return"].clone(), credit["return_against"].clone(), dec(&credit["grand_total"])),
        (json!(true), json!(made[1].to_string()), n("-23.25"))
    );
    assert_eq!(dec(&t.get("SalesInvoice", made[1]).await["outstanding_amount"]), n("286.75"), "the credit settles February's invoice");
    let r = t.action(&am, "Subscription", s1, "generate_invoices", json!({"as_of": "2026-03-05"})).await;
    let march = t.get("SalesInvoice", r["invoices"][0].as_str().unwrap().parse().unwrap()).await;
    assert_eq!(dec(&march["grand_total"]), n("372"), "1 × 310 + 2 seats × 31");
    let r = t.action(&am, "Subscription", s1, "cancel_subscription", json!({"date": "2026-03-20"})).await;
    let refund = t.get("SalesInvoice", r["invoices"][0].as_str().unwrap().parse().unwrap()).await;
    assert_eq!((dec(&refund["grand_total"]), refund["return_against"].clone()), (n("-132"), json!(uid(&march).to_string())), "11 unused days of 31");
    let s1_doc = t.get("Subscription", s1).await;
    assert_eq!(
        (s1_doc["status"].clone(), s1_doc["cancellation_date"].clone(), s1_doc["next_invoice_date"].clone()),
        (json!("Cancelled"), json!("2026-03-20"), Value::Null)
    );
    let r = t.action(&am, "Subscription", s1, "generate_invoices", json!({})).await;
    assert_eq!(r["invoices"].as_array().unwrap().len(), 0, "a cancelled subscription bills nothing more");

    let s2 = uid(&t
        .insert(
            &am,
            "Subscription",
            json!({"company": cid, "customer": cyberdyne.to_string(), "start_date": "2026-02-01", "generate_invoice_days_before": 5,
            "plans": [{"plan": annual.to_string(), "qty": "1"}]}),
        )
        .await);
    assert_eq!(t.get("Subscription", s2).await["next_invoice_date"], json!("2026-01-27"));
    let r = t.action(&am, "Subscription", s2, "generate_invoices", json!({"as_of": "2026-01-26"})).await;
    assert_eq!(r["invoices"].as_array().unwrap().len(), 0);
    let r = t.action(&am, "Subscription", s2, "generate_invoices", json!({"as_of": "2026-01-27"})).await;
    let yearly = t.get("SalesInvoice", r["invoices"][0].as_str().unwrap().parse().unwrap()).await;
    assert_eq!(
        (yearly["posting_date"].clone(), dec(&yearly["grand_total"]), yearly["fin_subscription_period_end"].clone()),
        (json!("2026-01-27"), n("1200"), json!("2027-01-31")),
        "billed five days ahead for the whole year"
    );
    assert_eq!(t.get("Subscription", s2).await["status"], json!("Past Due"), "the yearly invoice is unpaid past its due date");
    t.make(
        &am,
        "PaymentEntry",
        json!({"company": cid, "payment_type": "Receive", "customer": cyberdyne.to_string(), "posting_date": "2026-02-02",
        "bank_account": bank_gl.to_string(), "currency": inr["id"], "paid_amount": "1200",
        "references": [{"invoice_type": "Sales Invoice", "invoice": uid(&yearly).to_string(), "allocated_amount": "1200"}]}),
    )
    .await;
    t.action(&am, "Subscription", s2, "cancel_subscription", json!({"at_period_end": true})).await;
    let s2_doc = t.get("Subscription", s2).await;
    assert_eq!((s2_doc["cancel_at_period_end"].clone(), s2_doc["status"].clone()), (json!(true), json!("Active")), "it runs to the end of the paid period");
    assert!(s2_doc["cancellation_date"].as_str().unwrap() >= today.to_string().as_str());

    // ───────────── upkeep: billing what is due, drafting dunnings ─────────────
    let s3 = uid(&t
        .insert(
            &am,
            "Subscription",
            json!({"company": cid, "customer": tyrell.to_string(), "start_date": "2026-06-01", "plans": [{"plan": pro.to_string(), "qty": "2"}]}),
        )
        .await);
    ferrum_finance::maintenance(&k).await.expect("upkeep");
    let mut expected = 0;
    let mut m = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
    while m <= today {
        expected += 1;
        m = if m.month() == 12 { NaiveDate::from_ymd_opt(m.year() + 1, 1, 1).unwrap() } else { NaiveDate::from_ymd_opt(m.year(), m.month() + 1, 1).unwrap() };
    }
    let billed = "select count(*) from e_sales_invoice where tenant_id = $1 and (ext->>'fin_subscription') = $2::text and docstatus = 1";
    assert_eq!(t.count(billed, Some(s3)).await, expected, "one invoice per month since June, the current one included");
    let totals =
        t.amount("select coalesce(sum(grand_total), 0) from e_sales_invoice where tenant_id = $1 and (ext->>'fin_subscription') = $2::text", Some(s3)).await;
    assert_eq!(totals, Decimal::from(expected) * n("620"));
    ferrum_finance::maintenance(&k).await.expect("upkeep again");
    assert_eq!(t.count(billed, Some(s3)).await, expected, "upkeep never bills a period twice");
    let drafts = t.list("Dunning", json!({"customer": initech.to_string()})).await;
    assert_eq!(drafts.len(), 1, "the upkeep drafted a dunning for Initech's overdue installments");
    assert_eq!((drafts[0]["docstatus"].clone(), drafts[0]["dunning_level"].clone()), (json!(0), json!(3)));
    assert_eq!(dec(&drafts[0]["total_outstanding"]), n("600.01"));
    // Pay Hooli's invoice, then let the daily run go again: the dunning resolves, nothing is drafted twice.
    t.make(
        &am,
        "PaymentEntry",
        json!({"company": cid, "payment_type": "Receive", "customer": hooli.to_string(), "posting_date": "2028-03-10",
        "bank_account": bank_gl.to_string(), "currency": inr["id"], "paid_amount": "1000",
        "references": [{"invoice_type": "Sales Invoice", "invoice": si_h.to_string(), "allocated_amount": "1000"}]}),
    )
    .await;
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("delete from fin_daily_run where tenant_id = $1").bind(tenant).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    ferrum_finance::maintenance(&k).await.expect("daily upkeep again");
    assert_eq!(t.list("Dunning", json!({"customer": initech.to_string()})).await.len(), 1, "idempotent drafting");
    let hooli_dn = t.get("Dunning", uid(&dn)).await;
    assert_eq!((hooli_dn["status"].clone(), dec(&hooli_dn["outstanding_amount"])), (json!("Resolved"), Decimal::ZERO));
    let draft = t.submit(&am, "Dunning", uid(&drafts[0])).await;
    assert_eq!(draft["status"], json!("Unresolved"));

    // ───────────── reports ─────────────
    let bv = t.report(&am, "budget_variance", &[("company", cid.clone()), ("fiscal_year", years[&2026].to_string())]).await;
    let rows = bv["rows"].as_array().unwrap();
    let pos = rows.iter().position(|r| r["period"].as_str().unwrap().starts_with("5230 - Rent")).expect("the rent budget");
    let rent_row = &rows[pos];
    assert_eq!((dec(&rent_row["budget"]), dec(&rent_row["actual"]), dec(&rent_row["committed"])), (n("12000"), n("7501"), Decimal::ZERO), "{rent_row}");
    assert_eq!((dec(&rows[pos + 1]["budget"]), dec(&rows[pos + 1]["actual"])), (n("6000"), n("6000")), "January");
    assert_eq!((dec(&rows[pos + 2]["budget"]), dec(&rows[pos + 2]["actual"])), (n("1200"), n("1")), "February");
    assert_eq!((dec(&rows[pos + 4]["budget"]), dec(&rows[pos + 4]["actual"])), (n("480"), n("1500")), "April");
    let month_sum: Decimal = rows[pos + 1..pos + 13].iter().map(|r| dec(&r["budget"])).sum();
    assert_eq!(month_sum, n("12000"), "the months add up to the year exactly");
    let admin_row = rows.iter().find(|r| r["period"].as_str().unwrap().starts_with("5210 - Administrative Expenses")).unwrap();
    assert_eq!(dec(&admin_row["actual"]), n("900"), "Marketing and Sales Team roll up to Operations, the credit counted");
    let util_row = rows.iter().find(|r| r["period"].as_str().unwrap().starts_with("5240 - Utilities")).unwrap();
    assert_eq!((dec(&util_row["actual"]), dec(&util_row["committed"]), dec(&util_row["variance"])), (n("50"), n("400"), n("50")));
    let mrr = t.report(&am, "subscription_metrics", &[("company", cid.clone()), ("from_date", "2026-02-01".into()), ("to_date", "2026-03-31".into())]).await;
    let mt = &mrr["totals"];
    assert_eq!((dec(&mt["mrr_start"]), dec(&mt["mrr_end"]), dec(&mt["churned_mrr"]), dec(&mt["arr_end"])), (n("472"), n("100"), n("372"), n("1200")), "{mrr}");
    assert_eq!((dec(&mt["customer_churn_pct"]), dec(&mt["revenue_churn_pct"])), (n("50"), n("78.81")));
    let ds = t
        .report(
            &am,
            "dunning_summary",
            &[("company", cid.clone()), ("from_date", "2028-01-01".into()), ("to_date", "2028-12-31".into()), ("customer", hooli.to_string())],
        )
        .await;
    assert_eq!((ds["totals"]["count"].clone(), dec(&ds["totals"]["charges"])), (json!(1), n("49.89")));
    let horizon = today + chrono::Duration::days(90);
    let cf = t.report(&am, "cash_flow_forecast", &[("company", cid.clone()), ("from_date", today.to_string()), ("to_date", horizon.to_string())]).await;
    let net: Decimal = cf["rows"].as_array().unwrap().iter().map(|r| dec(&r["net"])).sum();
    assert_eq!(dec(&cf["totals"]["opening_balance"]) + net, dec(&cf["totals"]["closing_balance"]), "{cf}");
    let opening = t
        .amount(
            "select coalesce(sum(l.debit - l.credit), 0) from journal_line l join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id \
             where l.tenant_id = $1 and a.account_type in ('Bank', 'Cash') and l.posting_date < current_date and $2::uuid is null",
            None,
        )
        .await;
    assert_eq!(dec(&cf["totals"]["opening_balance"]), opening);
    assert!(dec(&cf["totals"]["receivables"]) > Decimal::ZERO && dec(&cf["totals"]["subscriptions"]) > Decimal::ZERO, "{cf}");
    t.books_balance().await;
}
