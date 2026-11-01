//! Whole business journeys across apps, against a real PostgreSQL (`FERRUM_JOURNEYS_TEST_DATABASE_URL`,
//! default `postgres://ferrum:ferrum@localhost:5432/ferrum_journeys_test`; the database is reset) with
//! every app this build compiles in. Each app's own suite proves its part; these prove the parts meet.
//! Expected figures are worked out by hand in the comments beside them, not by the code under test.
//! Each journey also goes the wrong way: what must be refused is, and what is undone comes back.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
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

fn uid(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn made(v: &Value) -> Uuid {
    v["id"].as_str().unwrap().parse().unwrap()
}

fn n(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn dec(v: &Value) -> Decimal {
    Decimal::from_str(v.as_str().unwrap_or_else(|| panic!("{v} is not a decimal"))).unwrap()
}

struct As {
    k: Arc<Kernel>,
    ctx: Ctx,
}

impl As {
    async fn insert(&self, entity: &str, v: Value) -> Doc {
        self.k.insert(&self.ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }
    async fn update(&self, entity: &str, id: Uuid, v: Value) -> Doc {
        self.k.update(&self.ctx, entity, id, obj(v), None).await.unwrap_or_else(|e| panic!("update {entity}: {e:?}"))
    }
    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.ctx, entity, id, false).await.unwrap_or_else(|e| panic!("get {entity}: {e:?}"))
    }
    async fn submit(&self, entity: &str, id: Uuid) -> Doc {
        self.k.submit(&self.ctx, entity, id, None).await.unwrap_or_else(|e| panic!("submit {entity}: {e:?}"))
    }
    async fn cancel(&self, entity: &str, id: Uuid) -> Doc {
        self.k.cancel(&self.ctx, entity, id, None).await.unwrap_or_else(|e| panic!("cancel {entity}: {e:?}"))
    }
    async fn action(&self, entity: &str, id: Uuid, name: &str, params: Value) -> Value {
        self.k.run_action(&self.ctx, entity, id, name, params).await.unwrap_or_else(|e| panic!("{entity}.{name}: {e:?}"))
    }
    async fn one(&self, entity: &str, filters: Value) -> Doc {
        let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), limit: Some(2), ..Default::default() };
        let page = self.k.list(&self.ctx, entity, &q).await.unwrap();
        assert_eq!(page.rows.len(), 1, "{entity} {filters}");
        page.rows[0].clone()
    }
    async fn report(&self, name: &str, params: &[(&str, String)]) -> Value {
        let p: HashMap<String, String> = params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        ferrum_domain::reports::run(&self.k, &self.ctx, name, &p).await.unwrap_or_else(|e| panic!("report {name}: {e:?}"))
    }
}

async fn kernel() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_JOURNEYS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_journeys_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 20, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = ferrum_server::packages::base_catalog(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/no-extra-packages")).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_server::packages::register(&mut hooks);
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("maintenance");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    k
}

async fn workspace(k: &Arc<Kernel>, slug: &str, packages: &[&str]) -> As {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: packages.iter().map(|p| p.to_string()).collect(),
            settings: Some(json!({"fiscal_year_start_month": 4})),
        })
        .await
        .expect("workspace");
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.expect("sign in");
    As { k: k.clone(), ctx: k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx") }
}

async fn person(admin: &As, email: &str, roles: &[&str]) -> As {
    let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
    let id = admin.k.create_user(&admin.ctx, email, email, None, &roles, false).await.expect("user");
    As { k: admin.k.clone(), ctx: admin.k.load_ctx(admin.ctx.tenant, id, None, "it".into()).await.expect("ctx") }
}

#[test]
fn journeys() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(32 << 20).build().unwrap();
    rt.block_on(async {
        tokio::spawn(async {
            let k = kernel().await;
            lead_to_cash(&k).await;
            procure_to_pay(&k).await;
        })
        .await
        .unwrap()
    });
}

/// CRM → quotation → approval → order → delivery → invoice → payment → reporting, and back.
async fn lead_to_cash(k: &Arc<Kernel>) {
    let admin = workspace(k, "ltc", &["crm", "crm_erp"]).await;
    let inr = admin.one("Currency", json!({"code": "INR"})).await;
    let company = admin.insert("Company", json!({"company_name": "Lead To Cash Ltd", "abbr": "LC", "default_currency": inr["id"]})).await;
    let cid = uid(&company).to_string();
    let company_param = ("company", cid.clone());
    let output_tax = admin.one("Account", json!({"account_name": "Output Tax", "company": cid})).await;
    admin
        .insert("TaxTemplate", json!({"title": "VAT 10%", "company": cid, "kind": "Sales", "is_default": true, "rows": [{"charge_type": "On Net Total", "account": output_tax["id"], "description": "VAT 10%", "rate": "10"}]}))
        .await;
    let products = admin.one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = admin.one("Uom", json!({"uom_name": "Nos"})).await;
    let widget = admin
        .insert("Item", json!({"item_code": "WIDGET", "item_name": "Widget", "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "100"}))
        .await;
    let widget_id = widget["id"].clone();
    let stores = admin.one("Warehouse", json!({"warehouse_name": "Stores", "company": cid})).await;
    let opening = admin.insert("StockEntry", json!({"company": cid, "purpose": "Opening Stock", "items": [{"item": widget_id, "qty": "50", "basic_rate": "60", "target_warehouse": stores["id"]}]})).await;
    admin.submit("StockEntry", uid(&opening)).await;
    admin.insert("QuotationApprovalRule", json!({"rule_name": "Discounts", "discount_over": "10", "approver_role": "Sales Manager"})).await;

    let rep = person(&admin, "rep@ltc.test", &["Sales User"]).await;
    let manager = person(&admin, "manager@ltc.test", &["Sales Manager"]).await;

    // ───────────── lead, opportunity, quotation ─────────────
    let lead = rep
        .insert("Lead", json!({"first_name": "Jane", "last_name": "Doe", "organization_name": "Acme Corp", "email": "jane@acme.test", "company": cid}))
        .await;
    let opportunity = made(&rep.action("Lead", uid(&lead), "make_opportunity", json!({})).await);
    let quotation = made(&rep.action("Opportunity", opportunity, "make_quotation", json!({})).await);
    // 10 at 100, less 20%: 800. VAT 10%: 80. Total 880.
    let q = rep
        .update("Quotation", quotation, json!({"items": [{"item": widget_id, "qty": "10", "rate": "100"}], "discount_percent": "20", "tax_template": null}))
        .await;
    assert_eq!((dec(&q["net_total"]), dec(&q["grand_total"])), (n("800"), n("880")), "{q:?}");

    // A discount past the limit waits for someone else, and the customer has to exist.
    assert!(matches!(rep.k.submit(&rep.ctx, "Quotation", quotation, None).await, Err(Error::Invalid(_))), "not without approval");
    assert!(
        matches!(rep.k.run_action(&rep.ctx, "Quotation", quotation, "approve_quotation", json!({})).await, Err(Error::Forbidden(_))),
        "not by whoever raised it"
    );
    manager.action("Quotation", quotation, "approve_quotation", json!({})).await;
    rep.submit("Quotation", quotation).await;
    assert!(rep.k.run_action(&rep.ctx, "Quotation", quotation, "make_sales_order", json!({})).await.is_err(), "a lead cannot order");
    let customer = rep.action("Lead", uid(&lead), "convert_to_customer", json!({})).await;
    let customer = made(&customer);
    assert_eq!(admin.get("Quotation", quotation).await["customer"], json!(customer.to_string()), "the quotation follows the lead to its customer");

    // ───────────── order, delivery, invoice ─────────────
    let order = made(&rep.action("Quotation", quotation, "make_sales_order", json!({})).await);
    let so = admin.get("SalesOrder", order).await;
    assert_eq!(dec(&so["grand_total"]), n("880"), "the order prices exactly like the quotation");
    admin.update("SalesOrder", order, json!({"set_warehouse": stores["id"], "delivery_date": so["posting_date"]})).await;
    admin.submit("SalesOrder", order).await;

    let note = made(&admin.action("SalesOrder", order, "make_delivery_note", json!({})).await);
    let dn = admin.get("DeliveryNote", note).await;
    let mut lines = dn["items"].as_array().unwrap().clone();
    lines[0]["qty"] = json!("11");
    assert!(
        admin.k.update(&admin.ctx, "DeliveryNote", note, obj(json!({"items": lines})), None).await.is_err()
            || admin.k.submit(&admin.ctx, "DeliveryNote", note, None).await.is_err(),
        "not more than was ordered"
    );
    let dn = admin.get("DeliveryNote", note).await;
    if dn["items"][0]["qty"] != json!("10") {
        let mut lines = dn["items"].as_array().unwrap().clone();
        lines[0]["qty"] = json!("10");
        admin.update("DeliveryNote", note, json!({"items": lines})).await;
    }
    admin.submit("DeliveryNote", note).await;
    let stock = admin.report("stock_balance", std::slice::from_ref(&company_param)).await;
    let row = stock["rows"].as_array().unwrap().iter().find(|r| r["item_code"] == json!("WIDGET") || r["item"] == json!("WIDGET")).cloned();
    assert!(row.is_some_and(|r| dec(&r["qty"]) == n("40") || dec(&r["balance_qty"]) == n("40")), "50 less 10: {stock}");

    let invoice = made(&admin.action("DeliveryNote", note, "make_sales_invoice", json!({})).await);
    let si = admin.submit("SalesInvoice", invoice).await;
    assert_eq!(dec(&si["grand_total"]), n("880"), "the invoice bills what was quoted");
    assert_eq!(dec(&si["outstanding_amount"]), n("880"));
    assert_eq!(admin.get("SalesOrder", order).await["status"], json!("Completed"));

    let balanced = |report: Value| {
        let t = &report["totals"];
        assert_eq!(dec(&t["closing_debit"]), dec(&t["closing_credit"]), "the books balance: {report}");
    };
    let today = chrono::Utc::now().date_naive();
    let year = [("from_date", (today - chrono::Duration::days(300)).to_string()), ("to_date", (today + chrono::Duration::days(1)).to_string())];
    let window = |extra: &[(&'static str, String)]| [std::slice::from_ref(&company_param), &year[..], extra].concat();
    balanced(admin.report("trial_balance", &window(&[])).await);
    let owed = admin.report("receivables", std::slice::from_ref(&company_param)).await;
    assert_eq!(owed["rows"].as_array().unwrap().len(), 1, "{owed}");

    // ───────────── payment, and what it closes ─────────────
    let payment = made(&admin.action("SalesInvoice", invoice, "make_payment", json!({})).await);
    admin.submit("PaymentEntry", payment).await;
    let si = admin.get("SalesInvoice", invoice).await;
    assert_eq!((dec(&si["outstanding_amount"]), si["status"].as_str()), (Decimal::ZERO, Some("Paid")));
    assert!(admin.report("receivables", std::slice::from_ref(&company_param)).await["rows"].as_array().unwrap().is_empty(), "nothing is owed");
    balanced(admin.report("trial_balance", &window(&[])).await);
    let register = admin.report("sales_register", &window(&[])).await;
    assert_eq!(register["rows"].as_array().unwrap().len(), 1, "{register}");

    // ───────────── and back ─────────────
    assert!(matches!(admin.k.cancel(&admin.ctx, "SalesInvoice", invoice, None).await, Err(Error::Invalid(_))), "not a paid invoice");
    admin.cancel("PaymentEntry", payment).await;
    let si = admin.get("SalesInvoice", invoice).await;
    assert_eq!(dec(&si["outstanding_amount"]), n("880"), "owed again");
    assert_eq!(admin.report("receivables", std::slice::from_ref(&company_param)).await["rows"].as_array().unwrap().len(), 1);
    admin.cancel("SalesInvoice", invoice).await;
    admin.cancel("DeliveryNote", note).await;
    let stock = admin.report("stock_balance", std::slice::from_ref(&company_param)).await;
    let row = stock["rows"].as_array().unwrap().iter().find(|r| r["item_code"] == json!("WIDGET") || r["item"] == json!("WIDGET")).cloned();
    assert!(row.is_some_and(|r| dec(&r["qty"]) == n("50") || dec(&r["balance_qty"]) == n("50")), "the stock came back: {stock}");
    balanced(admin.report("trial_balance", &window(&[])).await);
    assert_ne!(admin.get("SalesOrder", order).await["status"], json!("Completed"), "the order is open again");
}

/// Request → RFQ → purchase order → receipt → supplier invoice → payment → reconciliation, and back.
async fn procure_to_pay(k: &Arc<Kernel>) {
    let admin = workspace(k, "ptp", &["procurement", "finance"]).await;
    let inr = admin.one("Currency", json!({"code": "INR"})).await;
    let company = admin.insert("Company", json!({"company_name": "Procure To Pay Ltd", "abbr": "PP", "default_currency": inr["id"]})).await;
    let cid = uid(&company).to_string();
    let company = admin.get("Company", uid(&company)).await;
    let company_param = ("company", cid.clone());
    let stores = admin.one("Warehouse", json!({"warehouse_name": "Stores", "company": cid})).await;
    let group = admin.one("ItemGroup", json!({"group_name": "Raw Material"})).await;
    let nos = admin.one("Uom", json!({"uom_name": "Nos"})).await;
    let steel = admin.insert("Supplier", json!({"supplier_name": "Steel Co", "email": "sales@steel.test"})).await;
    let other = admin.insert("Supplier", json!({"supplier_name": "Dear Steel", "email": "sales@dear.test"})).await;
    let sheet = admin.insert("Item", json!({"item_code": "SHEET", "item_name": "Sheet", "item_group": group["id"], "stock_uom": nos["id"]})).await;
    let today = chrono::Utc::now().date_naive();

    // ───────────── a request, and who should supply it ─────────────
    let request = admin
        .insert("MaterialRequest", json!({"company": cid, "schedule_date": (today + chrono::Duration::days(10)).to_string(), "set_warehouse": stores["id"], "items": [{"item": sheet["id"], "qty": "100"}]}))
        .await;
    admin.submit("MaterialRequest", uid(&request)).await;
    let rfq = admin
        .insert(
            "RequestForQuotation",
            json!({"company": cid, "items": [{"item": sheet["id"], "qty": "100"}], "suppliers": [{"supplier": steel["id"]}, {"supplier": other["id"]}]}),
        )
        .await;
    admin.submit("RequestForQuotation", uid(&rfq)).await;

    // The better offer becomes the order: 100 at 12.
    let order = admin
        .insert("PurchaseOrder", json!({"company": cid, "supplier": steel["id"], "items": [{"item": sheet["id"], "qty": "100", "price_list_rate": "12", "warehouse": stores["id"]}]}))
        .await;
    assert_eq!(dec(&order["grand_total"]), n("1200"));
    let po = uid(&order);
    admin.submit("PurchaseOrder", po).await;

    // ───────────── receipt, and what it owes ─────────────
    let receipt = made(&admin.action("PurchaseOrder", po, "make_purchase_receipt", json!({})).await);
    admin.submit("PurchaseReceipt", receipt).await;
    assert_eq!(admin.get("PurchaseOrder", po).await["status"], json!("To Bill"));
    let invoice = made(&admin.action("PurchaseReceipt", receipt, "make_purchase_invoice", json!({})).await);
    admin.update("PurchaseInvoice", invoice, json!({"bill_no": "STEEL/778"})).await;
    let pi = admin.submit("PurchaseInvoice", invoice).await;
    assert_eq!((dec(&pi["grand_total"]), dec(&pi["outstanding_amount"])), (n("1200"), n("1200")));
    assert_eq!(admin.get("PurchaseOrder", po).await["status"], json!("Completed"));

    // A supplier's invoice is booked once.
    let twice = admin
        .k
        .insert(
            &admin.ctx,
            "PurchaseInvoice",
            obj(json!({"company": cid, "supplier": steel["id"], "bill_no": "STEEL/778", "items": [{"item": sheet["id"], "qty": "1", "rate": "1"}]})),
        )
        .await;
    assert!(matches!(twice, Err(Error::Validation(ref e)) if e.iter().any(|f| f.field == "bill_no")), "{twice:?}");

    // ───────────── paid, and matched to the bank's statement ─────────────
    let payment = made(&admin.action("PurchaseInvoice", invoice, "make_payment", json!({})).await);
    admin.update("PaymentEntry", payment, json!({"reference_no": "STEEL-778"})).await;
    admin.submit("PaymentEntry", payment).await;
    assert_eq!(admin.get("PurchaseInvoice", invoice).await["status"], json!("Paid"));
    let bank_gl = company["default_bank_account"].clone();
    let bank = admin.insert("BankAccount", json!({"account_name": "Operating", "company": cid, "account": bank_gl, "currency": inr["id"]})).await;
    let statement = format!("Date,Description,Reference,Debit,Credit\n{today},Steel Co STEEL-778,STEEL-778,1200,\n");
    let imported = admin.action("BankAccount", uid(&bank), "import_transactions", json!({"csv": statement, "mapping": {"date": "Date", "description": "Description", "reference": "Reference", "withdrawal": "Debit", "deposit": "Credit"}})).await;
    assert_eq!(imported["imported"], json!(1));
    let again = admin.action("BankAccount", uid(&bank), "import_transactions", json!({"csv": format!("Date,Description,Reference,Debit,Credit\n{today},Steel Co STEEL-778,STEEL-778,1200,\n"), "mapping": {"date": "Date", "description": "Description", "reference": "Reference", "withdrawal": "Debit", "deposit": "Credit"}})).await;
    assert_eq!((again["imported"].clone(), again["duplicates"].clone()), (json!(0), json!(1)), "the same statement twice is one statement");
    let line = admin.one("BankTransaction", json!({"bank_account": bank["id"]})).await;
    let suggested = admin.action("BankTransaction", uid(&line), "get_suggestions", json!({})).await;
    assert_eq!(suggested["suggestions"][0]["payment_entry"], json!(payment.to_string()), "by its reference: {suggested}");
    let done = admin
        .action("BankTransaction", uid(&line), "reconcile", json!({"vouchers": [{"payment_document": "PaymentEntry", "payment_entry": payment.to_string()}]}))
        .await;
    assert_eq!(done["status"], json!("Reconciled"), "{done}");
    assert!(admin.report("payables", std::slice::from_ref(&company_param)).await["rows"].as_array().unwrap().is_empty(), "nothing is owed");

    // ───────────── and back ─────────────
    assert!(admin.k.cancel(&admin.ctx, "PaymentEntry", payment, None).await.is_err(), "not a reconciled payment");
    admin.action("BankTransaction", uid(&line), "unreconcile", json!({"payment_entry": payment.to_string()})).await;
    admin.cancel("PaymentEntry", payment).await;
    assert_eq!(dec(&admin.get("PurchaseInvoice", invoice).await["outstanding_amount"]), n("1200"), "owed again");
    assert_eq!(admin.report("payables", std::slice::from_ref(&company_param)).await["rows"].as_array().unwrap().len(), 1);
    admin.cancel("PurchaseInvoice", invoice).await;
    assert_ne!(admin.get("PurchaseOrder", po).await["status"], json!("Completed"));
}
