//! End-to-end domain test against a real PostgreSQL (`FERRUM_DOMAIN_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_domain_test`). The database is reset first.
//!
//! Walks a company through opening stock, procure-to-pay, order-to-cash with GST, payments,
//! credit notes, manufacturing, MRP, period closing and concurrent stock issues, checking the
//! ledgers after each step and reconciling stock value with the general ledger at the end.

use ferrum_domain::india::gstin_check_char;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::Catalog;
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

fn gstin(first14: &str) -> String {
    format!("{first14}{}", gstin_check_char(first14).unwrap())
}

async fn fresh_kernel() -> (Arc<Kernel>, Arc<ferrum_plugins::PluginHost>) {
    let url = std::env::var("FERRUM_DOMAIN_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_domain_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    let plugins = ferrum_plugins::PluginHost::new(false).expect("plugin host");
    hooks.register("*", Arc::new(ferrum_plugins::PluginHook(plugins.clone())));
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("maintenance");
    (k, plugins)
}

struct T {
    k: Arc<Kernel>,
    ctx: Ctx,
}

impl T {
    async fn insert(&self, entity: &str, v: Value) -> Doc {
        match self.k.insert(&self.ctx, entity, obj(v)).await {
            Ok(d) => d,
            Err(e) => panic!("insert {entity}: {e:?}"),
        }
    }
    async fn submit(&self, entity: &str, id: Uuid) -> Doc {
        match self.k.submit(&self.ctx, entity, id, None).await {
            Ok(d) => d,
            Err(e) => panic!("submit {entity}: {e:?}"),
        }
    }
    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.ctx, entity, id, false).await.unwrap_or_else(|e| panic!("get {entity}: {e:?}"))
    }
    async fn action(&self, entity: &str, id: Uuid, action: &str, params: Value) -> Value {
        match self.k.run_action(&self.ctx, entity, id, action, params).await {
            Ok(v) => v,
            Err(e) => panic!("{entity}.{action}: {e:?}"),
        }
    }
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let page =
            self.k.list(&self.ctx, entity, &ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() }).await.unwrap();
        assert_eq!(page.rows.len(), 1, "{entity} {filters}");
        page.rows[0].clone()
    }
    /// Debit minus credit on an account (by name) across all postings.
    async fn gl(&self, account: &str) -> Decimal {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        let v: Decimal = sqlx::query_scalar(
            "select coalesce(sum(l.debit - l.credit), 0) from journal_line l join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id \
             where l.tenant_id = $1 and a.account_name = $2",
        )
        .bind(self.ctx.tenant)
        .bind(account)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        v
    }
    async fn reserved(&self, item: Uuid, wh: Uuid) -> Decimal {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        let v: Option<Decimal> = sqlx::query_scalar("select reserved_qty from stock_balance where tenant_id = $1 and item_id = $2 and warehouse_id = $3")
            .bind(self.ctx.tenant)
            .bind(item)
            .bind(wh)
            .fetch_optional(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        v.unwrap_or_default()
    }
    async fn stock(&self, item: Uuid, wh: Uuid) -> (Decimal, Decimal) {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        sqlx::query_as::<_, (Decimal, Decimal)>("select qty, value from stock_balance where tenant_id = $1 and item_id = $2 and warehouse_id = $3")
            .bind(self.ctx.tenant)
            .bind(item)
            .bind(wh)
            .fetch_optional(&mut *tx)
            .await
            .unwrap()
            .unwrap_or_default()
    }
    async fn report(&self, name: &str, params: &[(&str, String)]) -> Value {
        let p: HashMap<String, String> = params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        ferrum_domain::reports::run(&self.k, &self.ctx, name, &p).await.unwrap_or_else(|e| panic!("report {name}: {e:?}"))
    }
}

#[test]
fn domain_end_to_end() {
    // Same runtime shape as the server: unoptimized async hook chains need more than 2 MiB.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let (k, plugins) = fresh_kernel().await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "ti".into(),
            name: "Test Industries".into(),
            admin_email: "admin@ti.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying", "manufacturing", "india"].iter().map(|s| s.to_string()).collect(),
            settings: Some(json!({"fiscal_year_start_month": 4})),
        })
        .await
        .expect("tenant");
    let session = k.login("ti", "admin@ti.test", "Correct-Horse-9").await.expect("login");
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    // Engine contexts number documents in the same fiscal-year series as users do.
    assert_eq!(Ctx::system(tenant, k.registry.schema(tenant).await.unwrap()).fy_start_month, ctx.fy_start_month);
    assert_eq!(ctx.fy_start_month, 4);
    // A custom line field on orders, deliveries and invoices: it travels with the lines.
    for line in ["SalesOrderItem", "DeliveryNoteItem", "SalesInvoiceItem"] {
        let op = serde_json::from_value(json!({"op": "add_field", "field": {"name": "x_po_line", "type": "data"}})).unwrap();
        k.add_customization(&ctx, line, op, None).await.unwrap_or_else(|e| panic!("custom field on {line}: {e:?}"));
    }
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    let t = T { k: k.clone(), ctx };

    // ───────────── company, chart of accounts, GST registration ─────────────
    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let bad =
        t.k.insert(&t.ctx, "Company", obj(json!({"company_name": "Bad", "abbr": "BAD", "default_currency": inr["id"], "in_gstin": "27AAPFU0939F1ZX"}))).await;
    assert!(matches!(bad, Err(Error::Validation(ref e)) if e.iter().any(|f| f.field == "in_gstin")), "{bad:?}");
    let company = t
        .insert("Company", json!({"company_name": "Test Industries Pvt Ltd", "abbr": "TI", "default_currency": inr["id"], "in_gstin": " 27aapfu0939f1zv "}))
        .await;
    let cid = uid(&company);
    let company = t.get("Company", cid).await;
    assert_eq!(company["in_gstin"], json!("27AAPFU0939F1ZV"), "GSTIN normalized");
    assert_eq!(company["in_state_code"], json!("27"), "state derived from GSTIN");
    for f in [
        "default_receivable_account",
        "default_payable_account",
        "srbnb_account",
        "cogs_account",
        "opening_balance_account",
        "overhead_absorbed_account",
        "default_cost_center",
    ] {
        assert!(company[f].is_string(), "company default {f} set by setup");
    }
    let stores = uid(&t.find_one("Warehouse", json!({"warehouse_name": "Stores", "company": cid.to_string()})).await);
    let fg = uid(&t.find_one("Warehouse", json!({"warehouse_name": "Finished Goods", "company": cid.to_string()})).await);
    let templates = t.k.list(&t.ctx, "TaxTemplate", &ListQuery::default()).await.unwrap();
    assert_eq!(templates.rows.len(), 5, "GST templates created");
    let bank = company["default_bank_account"].as_str().unwrap().to_string();

    // ───────────── masters ─────────────
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let raw_group = t.find_one("ItemGroup", json!({"group_name": "Raw Material"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let globex_gstin = gstin("29AAGCG1234A1Z");
    let globex = uid(&t.insert("Customer", json!({"customer_name": "Globex", "in_gstin": globex_gstin})).await);
    let globex_doc = t.get("Customer", globex).await;
    assert_eq!(globex_doc["in_gst_category"], json!("Registered"), "GSTIN makes the party registered");
    let local = uid(&t.insert("Customer", json!({"customer_name": "Local Traders", "in_gstin": gstin("27AAACL1234B1Z")})).await);
    let wrong = t.k.insert(&t.ctx, "Customer", obj(json!({"customer_name": "Typo", "in_gstin": "29AAGCG1234A1Z0"}))).await;
    assert!(wrong.is_err() || globex_gstin.ends_with('0'), "a wrong check digit is refused");
    let acme = uid(&t.insert("Supplier", json!({"supplier_name": "Acme Supplies", "in_gstin": gstin("27AAACA1111C1Z")})).await);
    let raw = uid(&t
        .insert(
            "Item",
            json!({"item_code": "RAW-A", "item_name": "Steel Sheet", "item_group": raw_group["id"], "stock_uom": nos["id"], "hsn_code": "7208",
                   "suppliers": [{"supplier": acme.to_string(), "priority": 1}]}),
        )
        .await);
    let widget = uid(&t
        .insert(
            "Item",
            json!({"item_code": "WIDGET", "item_name": "Widget", "item_group": products["id"], "stock_uom": nos["id"], "hsn_code": "8479",
                   "standard_rate": "150", "is_manufactured": true, "is_purchased": false}),
        )
        .await);

    // ───────────── opening stock ─────────────
    let ose = t
        .insert(
            "StockEntry",
            json!({"company": cid.to_string(), "purpose": "Opening Stock", "items": [
                {"item": raw.to_string(), "qty": "100", "basic_rate": "10", "target_warehouse": stores.to_string()},
                {"item": widget.to_string(), "qty": "50", "basic_rate": "100", "target_warehouse": stores.to_string()}]}),
        )
        .await;
    assert_eq!(dec(&ose["total_amount"]), n("6000"));
    t.submit("StockEntry", uid(&ose)).await;
    assert_eq!(t.stock(raw, stores).await, (n("100"), n("1000")));
    assert_eq!(t.gl("Stock In Hand").await, n("6000"));
    assert_eq!(t.gl("Opening Balance Equity").await, n("-6000"));

    // ───────────── procure to pay ─────────────
    let po = t
        .insert(
            "PurchaseOrder",
            json!({"company": cid.to_string(), "supplier": acme.to_string(), "items": [{"item": raw.to_string(), "qty": "200", "price_list_rate": "12", "warehouse": stores.to_string()}]}),
        )
        .await;
    assert_eq!(dec(&po["net_total"]), n("2400"));
    assert_eq!(dec(&po["grand_total"]), n("2832"), "intra-state CGST 9% + SGST 9%");
    let po_id = uid(&po);
    t.submit("PurchaseOrder", po_id).await;
    let pr = t.action("PurchaseOrder", po_id, "make_purchase_receipt", json!({})).await;
    let pr_id = vid(&pr);
    t.submit("PurchaseReceipt", pr_id).await;
    assert_eq!(t.stock(raw, stores).await, (n("300"), n("3400")));
    assert_eq!(t.gl("Stock Received But Not Billed").await, n("-2400"));
    assert_eq!(t.get("PurchaseOrder", po_id).await["status"], json!("To Bill"));
    let pi = t.action("PurchaseReceipt", pr_id, "make_purchase_invoice", json!({})).await;
    let pi_id = vid(&pi);
    t.k.update(&t.ctx, "PurchaseInvoice", pi_id, obj(json!({"bill_no": "ACME/778"})), None).await.expect("bill no");
    let pi_doc = t.submit("PurchaseInvoice", pi_id).await;
    assert_eq!(dec(&pi_doc["grand_total"]), n("2832"));
    assert_eq!(t.gl("Stock Received But Not Billed").await, Decimal::ZERO, "SRBNB cleared");
    assert_eq!(t.gl("CGST Input").await, n("216"));
    assert_eq!(t.gl("SGST Input").await, n("216"));
    assert_eq!(t.gl("Creditors").await, n("-2832"));
    assert_eq!(t.get("PurchaseOrder", po_id).await["status"], json!("Completed"));
    assert_eq!(t.get("PurchaseReceipt", pr_id).await["status"], json!("Completed"));
    assert_eq!(dec(&t.get("Item", raw).await["last_purchase_rate"]), n("12"));
    let dup = t
        .k
        .insert(&t.ctx, "PurchaseInvoice", obj(json!({"company": cid.to_string(), "supplier": acme.to_string(), "bill_no": "ACME/778", "items": [{"item": raw.to_string(), "qty": "1", "price_list_rate": "12"}]})))
        .await;
    assert!(matches!(dup, Err(Error::Validation(ref e)) if e.iter().any(|f| f.field == "bill_no")), "duplicate supplier invoice: {dup:?}");

    // ───────────── order to cash (inter-state GST) ─────────────
    let so = t
        .insert(
            "SalesOrder",
            json!({"company": cid.to_string(), "customer": globex.to_string(), "set_warehouse": stores.to_string(), "items": [{"item": widget.to_string(), "qty": "10", "x_po_line": "PO-7/1"}]}),
        )
        .await;
    assert_eq!(dec(&so["net_total"]), n("1500"), "standard rate used");
    assert_eq!(dec(&so["grand_total"]), n("1770"), "IGST 18% on an inter-state supply");
    let so_id = uid(&so);
    let reserved_before = t.reserved(widget, stores).await;
    t.submit("SalesOrder", so_id).await;
    assert_eq!(t.reserved(widget, stores).await - reserved_before, n("10"), "a submitted order reserves its stock");
    let dn = t.action("SalesOrder", so_id, "make_delivery_note", json!({})).await;
    let dn_id = vid(&dn);
    assert_eq!(t.get("DeliveryNote", dn_id).await["items"][0]["x_po_line"], json!("PO-7/1"), "custom line fields carry over");
    t.submit("DeliveryNote", dn_id).await;
    assert_eq!(t.reserved(widget, stores).await, reserved_before, "delivery releases the reservation");
    assert_eq!(t.stock(widget, stores).await, (n("40"), n("4000")));
    assert_eq!(t.gl("Cost of Goods Sold").await, n("1000"));
    assert_eq!(t.get("SalesOrder", so_id).await["status"], json!("To Bill"));
    let si = t.action("DeliveryNote", dn_id, "make_sales_invoice", json!({})).await;
    let si_id = vid(&si);
    assert_eq!(t.get("SalesInvoice", si_id).await["items"][0]["x_po_line"], json!("PO-7/1"));
    let si_doc = t.submit("SalesInvoice", si_id).await;
    assert_eq!(si_doc["in_place_of_supply"], json!("29"));
    assert_eq!(dec(&si_doc["outstanding_amount"]), n("1770"));
    assert_eq!(si_doc["status"], json!("Unpaid"));
    assert_eq!(t.gl("Debtors").await, n("1770"));
    assert_eq!(t.gl("Sales").await, n("-1500"));
    assert_eq!(t.gl("IGST Output").await, n("-270"));
    assert_eq!(t.get("SalesOrder", so_id).await["status"], json!("Completed"));
    assert_eq!(t.get("DeliveryNote", dn_id).await["status"], json!("Completed"));

    // Payment, refusal to cancel a paid invoice, payment cancellation.
    let pe = t.action("SalesInvoice", si_id, "make_payment", json!({})).await;
    let pe_id = vid(&pe);
    t.submit("PaymentEntry", pe_id).await;
    let si_doc = t.get("SalesInvoice", si_id).await;
    assert_eq!(dec(&si_doc["outstanding_amount"]), Decimal::ZERO);
    assert_eq!(si_doc["status"], json!("Paid"));
    assert_eq!(t.gl("Primary Bank").await, n("1770"));
    let refused = t.k.cancel(&t.ctx, "SalesInvoice", si_id, None).await;
    assert!(matches!(refused, Err(Error::Invalid(_))), "{refused:?}");
    t.k.cancel(&t.ctx, "PaymentEntry", pe_id, None).await.expect("cancel payment");
    let si_doc = t.get("SalesInvoice", si_id).await;
    assert_eq!(dec(&si_doc["outstanding_amount"]), n("1770"));
    assert_eq!(si_doc["status"], json!("Unpaid"));
    assert_eq!(t.gl("Primary Bank").await, Decimal::ZERO);
    let part = t
        .insert(
            "PaymentEntry",
            json!({"company": cid.to_string(), "payment_type": "Receive", "customer": globex.to_string(), "bank_account": bank, "currency": inr["id"], "paid_amount": "770",
                   "references": [{"invoice_type": "Sales Invoice", "invoice": si_id.to_string(), "allocated_amount": "770"}]}),
        )
        .await;
    t.submit("PaymentEntry", uid(&part)).await;
    let si_doc = t.get("SalesInvoice", si_id).await;
    assert_eq!(dec(&si_doc["outstanding_amount"]), n("1000"));
    assert_eq!(si_doc["status"], json!("Partly Paid"));

    // Credit note settles what is still open; the rest is a customer credit.
    let cn = t.action("SalesInvoice", si_id, "make_credit_note", json!({})).await;
    let cn_doc = t.submit("SalesInvoice", vid(&cn)).await;
    assert_eq!(dec(&cn_doc["grand_total"]), n("-1770"));
    assert_eq!(dec(&t.get("SalesInvoice", si_id).await["outstanding_amount"]), Decimal::ZERO);
    assert_eq!(dec(&t.get("SalesInvoice", vid(&cn)).await["outstanding_amount"]), n("-770"));
    assert_eq!(t.gl("Debtors").await, n("-770"));

    // Credit limit.
    t.k.update(&t.ctx, "Customer", local, obj(json!({"credit_limit": "1000"})), None).await.expect("limit");
    let big = t.insert("SalesInvoice", json!({"company": cid.to_string(), "customer": local.to_string(), "items": [{"item": widget.to_string(), "qty": "10", "warehouse": stores.to_string()}]})).await;
    assert_eq!(dec(&big["grand_total"]), n("1770"), "intra-state CGST+SGST");
    let over = t.k.submit(&t.ctx, "SalesInvoice", uid(&big), None).await;
    assert!(matches!(over, Err(Error::Invalid(ref m)) if m.contains("credit limit")), "{over:?}");

    // ───────────── manufacturing ─────────────
    let bom = t
        .insert("Bom", json!({"company": cid.to_string(), "item": widget.to_string(), "quantity": "1", "operating_cost": "5", "items": [{"item": raw.to_string(), "qty": "2"}]}))
        .await;
    assert_eq!(dec(&bom["raw_material_cost"]), n("22.67"), "costed at the moving average 11.3333");
    let bom_id = uid(&bom);
    t.submit("Bom", bom_id).await;
    assert_eq!(t.get("Bom", bom_id).await["is_default"], json!(true));
    let circular =
        t.k.insert(&t.ctx, "Bom", obj(json!({"company": cid.to_string(), "item": raw.to_string(), "items": [{"item": widget.to_string(), "qty": "1"}]}))).await;
    assert!(matches!(circular, Err(Error::Invalid(ref m)) if m.contains("circular")), "{circular:?}");
    let wo = t
        .insert("WorkOrder", json!({"company": cid.to_string(), "item": widget.to_string(), "qty": "10", "source_warehouse": stores.to_string(), "target_warehouse": fg.to_string()}))
        .await;
    assert_eq!(wo["bom"], json!(bom_id.to_string()), "default BOM picked");
    let wo_id = uid(&wo);
    t.submit("WorkOrder", wo_id).await;
    let made = t.action("WorkOrder", wo_id, "manufacture", json!({"qty": "4"})).await;
    let made_id = vid(&made);
    let wo_doc = t.get("WorkOrder", wo_id).await;
    assert_eq!(dec(&wo_doc["produced_qty"]), n("4"));
    assert_eq!(wo_doc["status"], json!("In Process"));
    assert_eq!(dec(&wo_doc["actual_cost"]), n("110.67"), "8 × 11.3333 consumed + 20 operating cost");
    assert_eq!(t.stock(widget, fg).await, (n("4"), n("110.67")));
    assert_eq!(t.gl("Manufacturing Overheads Absorbed").await, n("-20"));
    let too_many = t.k.run_action(&t.ctx, "WorkOrder", wo_id, "manufacture", json!({"qty": "7"})).await;
    assert!(too_many.is_err(), "cannot produce more than ordered");
    t.k.cancel(&t.ctx, "StockEntry", made_id, None).await.expect("cancel production");
    let wo_doc = t.get("WorkOrder", wo_id).await;
    assert_eq!(dec(&wo_doc["produced_qty"]), Decimal::ZERO);
    assert_eq!(wo_doc["status"], json!("Not Started"));
    assert_eq!(t.stock(widget, fg).await.0, Decimal::ZERO);
    t.action("WorkOrder", wo_id, "manufacture", json!({})).await;
    assert_eq!(t.get("WorkOrder", wo_id).await["status"], json!("Completed"));
    assert_eq!(t.stock(raw, stores).await.0, n("280"));

    // ───────────── MRP ─────────────
    t.k.update(&t.ctx, "Item", raw, obj(json!({"safety_stock": "500"})), None).await.expect("safety stock");
    let so2 = t
        .insert("SalesOrder", json!({"company": cid.to_string(), "customer": local.to_string(), "set_warehouse": stores.to_string(), "items": [{"item": widget.to_string(), "qty": "100"}]}))
        .await;
    t.submit("SalesOrder", uid(&so2)).await;
    let held = t.reserved(widget, stores).await;
    t.action("SalesOrder", uid(&so2), "close", json!({})).await;
    assert_eq!(held - t.reserved(widget, stores).await, n("100"), "closing an order releases its reservation");
    t.action("SalesOrder", uid(&so2), "reopen", json!({})).await;
    assert_eq!(t.reserved(widget, stores).await, held, "re-opening reserves again");
    let mrp = t.insert("MrpRun", json!({"company": cid.to_string(), "warehouse": stores.to_string()})).await;
    let mrp_id = uid(&mrp);
    t.action("MrpRun", mrp_id, "run", json!({})).await;
    let mrp_doc = t.get("MrpRun", mrp_id).await;
    assert_eq!(mrp_doc["status"], json!("Planned"));
    let lines = mrp_doc["lines"].as_array().unwrap();
    let line = |item: Uuid| lines.iter().find(|l| l["item"] == json!(item.to_string())).cloned().unwrap_or_else(|| panic!("MRP line for {item}: {lines:?}"));
    let w = line(widget);
    assert_eq!(w["action"], json!("Manufacture"));
    assert_eq!(dec(&w["suggested_qty"]), n("60"), "100 ordered - 40 in stores");
    let r = line(raw);
    assert_eq!(r["action"], json!("Purchase"));
    assert_eq!(dec(&r["suggested_qty"]), n("340"), "120 for production + 500 safety - 280 available");
    assert_eq!(r["supplier"], json!(acme.to_string()));
    let created = t.action("MrpRun", mrp_id, "create_orders", json!({})).await;
    assert_eq!(created["created"].as_array().unwrap().len(), 2);
    assert_eq!(t.get("MrpRun", mrp_id).await["status"], json!("Orders Created"));

    // ───────────── period closing ─────────────
    let fy = t.insert("FiscalYear", json!({"year_name": "2026-27", "company": cid.to_string(), "start_date": "2026-04-01", "end_date": "2027-03-31"})).await;
    t.action("FiscalYear", uid(&fy), "close_period", json!({"period_start": "2026-05-01"})).await;
    let je = t
        .insert(
            "JournalEntry",
            json!({"company": cid.to_string(), "posting_date": "2026-05-15", "accounts": [
                {"account": company["default_expense_account"], "debit": "100"}, {"account": company["default_cash_account"], "credit": "100"}]}),
        )
        .await;
    let closed = t.k.submit(&t.ctx, "JournalEntry", uid(&je), None).await;
    assert!(matches!(closed, Err(Error::Invalid(ref m)) if m.contains("closed")), "{closed:?}");
    t.action("FiscalYear", uid(&fy), "reopen_period", json!({"period_start": "2026-05-01"})).await;
    t.submit("JournalEntry", uid(&je)).await;

    // ───────────── concurrent issues never overdraw stock ─────────────
    let mut drafts = Vec::new();
    for _ in 0..2 {
        let d = t
            .insert("StockEntry", json!({"company": cid.to_string(), "purpose": "Material Issue", "items": [{"item": widget.to_string(), "qty": "30", "source_warehouse": stores.to_string()}]}))
            .await;
        drafts.push(uid(&d));
    }
    let (a, b) = tokio::join!(t.k.submit(&t.ctx, "StockEntry", drafts[0], None), t.k.submit(&t.ctx, "StockEntry", drafts[1], None));
    assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|x| **x).count(), 1, "exactly one issue fits: {a:?} / {b:?}");
    assert_eq!(t.stock(widget, stores).await.0, n("10"));

    // ───────────── a burst of numbered submits never deadlocks the connection pool ─────────────
    // 30 submits against a 10-connection pool: each holds a connection while it takes a document
    // number on the side pool, so none can starve the others.
    let mut drafts = Vec::new();
    for _ in 0..30 {
        let d = t
            .insert("StockEntry", json!({"company": cid.to_string(), "purpose": "Material Receipt", "items": [{"item": raw.to_string(), "qty": "1", "basic_rate": "12", "target_warehouse": stores.to_string()}]}))
            .await;
        drafts.push(uid(&d));
    }
    let gate = Arc::new(tokio::sync::Barrier::new(drafts.len()));
    let started = std::time::Instant::now();
    let burst: Vec<_> = drafts
        .into_iter()
        .map(|id| {
            let (k, ctx, gate) = (t.k.clone(), t.ctx.clone(), gate.clone());
            tokio::spawn(async move {
                gate.wait().await;
                k.submit(&ctx, "StockEntry", id, None).await
            })
        })
        .collect();
    for h in burst {
        h.await.unwrap().expect("burst submit");
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(15), "burst took {:?}", started.elapsed());

    // ───────────── a sales return comes back at cost, not at the selling price ─────────────
    let (_, before) = t.stock(widget, stores).await;
    let ret = t
        .insert(
            "DeliveryNote",
            json!({"company": cid.to_string(), "customer": globex.to_string(), "is_return": true, "return_against": dn_id.to_string(),
                   "items": [{"item": widget.to_string(), "qty": "2", "warehouse": stores.to_string()}]}),
        )
        .await;
    t.submit("DeliveryNote", uid(&ret)).await;
    let (qty_after, value_after) = t.stock(widget, stores).await;
    assert_eq!(qty_after, n("12"));
    assert_eq!(value_after - before, n("200"), "returned at the delivery's cost of 100, not the price of 150");

    // ───────────── cancelling a receipt after part of it was issued ─────────────
    // Moving average: 10 @ 10 + 10 @ 100 = 20 for 1,100; issuing 10 takes 550. Cancelling the
    // second receipt can only remove the 550 left, so 450 goes to stock adjustment and the stock
    // accounts still equal the stock ledger (checked by the reconciliation below).
    let avg = uid(&t.insert("Item", json!({"item_code": "AVG-X", "item_name": "Average X", "item_group": raw_group["id"], "stock_uom": nos["id"]})).await);
    let receive = |rate: &str| json!({"company": cid.to_string(), "purpose": "Material Receipt", "items": [{"item": avg.to_string(), "qty": "10", "basic_rate": rate, "target_warehouse": stores.to_string()}]});
    let r1 = t.insert("StockEntry", receive("10")).await;
    t.submit("StockEntry", uid(&r1)).await;
    let r2 = t.insert("StockEntry", receive("100")).await;
    t.submit("StockEntry", uid(&r2)).await;
    let issue = t
        .insert("StockEntry", json!({"company": cid.to_string(), "purpose": "Material Issue", "items": [{"item": avg.to_string(), "qty": "10", "source_warehouse": stores.to_string()}]}))
        .await;
    let issue = t.submit("StockEntry", uid(&issue)).await;
    assert_eq!(dec(&issue["total_amount"]), n("550"));
    let adjustment_before = t.gl("Stock Adjustment").await;
    t.k.cancel(&t.ctx, "StockEntry", uid(&r2), None).await.expect("cancel the second receipt");
    assert_eq!(t.stock(avg, stores).await, (Decimal::ZERO, Decimal::ZERO));
    // +1,000 undoing the receipt's own offset, -450 for the value the reversal could not take out.
    assert_eq!(t.gl("Stock Adjustment").await - adjustment_before, n("550"));
    let mut tx = t.k.db.tenant_tx(tenant).await.unwrap();
    let booked: Decimal =
        sqlx::query_scalar("select total_debit from journal_entry where tenant_id = $1 and source_entity = 'StockValueAdjustment' and source_id = $2")
            .bind(tenant)
            .bind(uid(&r2))
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    drop(tx);
    assert_eq!(booked, n("450"));

    // ───────────── FIFO valuation ─────────────
    let fifo = uid(&t
        .insert(
            "Item",
            json!({"item_code": "FIFO-B", "item_name": "Fifo B", "item_group": raw_group["id"], "stock_uom": nos["id"], "valuation_method": "FIFO"}),
        )
        .await);
    for rate in ["10", "20"] {
        let r = t
            .insert("StockEntry", json!({"company": cid.to_string(), "purpose": "Material Receipt", "items": [{"item": fifo.to_string(), "qty": "10", "basic_rate": rate, "target_warehouse": stores.to_string()}]}))
            .await;
        t.submit("StockEntry", uid(&r)).await;
    }
    let out = t
        .insert("StockEntry", json!({"company": cid.to_string(), "purpose": "Material Issue", "items": [{"item": fifo.to_string(), "qty": "15", "source_warehouse": stores.to_string()}]}))
        .await;
    let out = t.submit("StockEntry", uid(&out)).await;
    assert_eq!(dec(&out["total_amount"]), n("200"), "10 × 10 from the first layer + 5 × 20 from the second");
    assert_eq!(t.stock(fifo, stores).await, (n("5"), n("100")));

    // ───────────── serial numbers ─────────────
    let serial = uid(&t
        .insert("Item", json!({"item_code": "SER-1", "item_name": "Serialized", "item_group": products["id"], "stock_uom": nos["id"], "has_serial_no": true}))
        .await);
    let r = t
        .insert("StockEntry", json!({"company": cid.to_string(), "purpose": "Material Receipt", "items": [{"item": serial.to_string(), "qty": "2", "basic_rate": "500", "serial_no": "SN-1\nSN-2", "target_warehouse": stores.to_string()}]}))
        .await;
    t.submit("StockEntry", uid(&r)).await;
    let issue_serial = |sn: &str| json!({"company": cid.to_string(), "purpose": "Material Issue", "items": [{"item": serial.to_string(), "qty": "1", "serial_no": sn, "source_warehouse": stores.to_string()}]});
    let wrong = t.insert("StockEntry", issue_serial("SN-9")).await;
    let wrong = t.k.submit(&t.ctx, "StockEntry", uid(&wrong), None).await;
    assert!(matches!(wrong, Err(Error::Invalid(ref m)) if m.contains("SN-9")), "{wrong:?}");
    let right = t.insert("StockEntry", issue_serial("SN-1")).await;
    t.submit("StockEntry", uid(&right)).await;
    let twice = t.insert("StockEntry", issue_serial("SN-1")).await;
    assert!(t.k.submit(&t.ctx, "StockEntry", uid(&twice), None).await.is_err(), "a serial cannot leave twice");

    // ───────────── invoices that move stock themselves ─────────────
    let (_, widget_value) = t.stock(widget, stores).await;
    let cogs_before = t.gl("Cost of Goods Sold").await;
    let direct = t
        .insert(
            "SalesInvoice",
            json!({"company": cid.to_string(), "customer": globex.to_string(), "update_stock": true, "items": [{"item": widget.to_string(), "qty": "2", "warehouse": stores.to_string()}]}),
        )
        .await;
    t.submit("SalesInvoice", uid(&direct)).await;
    let (_, widget_after) = t.stock(widget, stores).await;
    assert_eq!(t.gl("Cost of Goods Sold").await - cogs_before, widget_value - widget_after, "COGS equals the stock value that left");
    let (raw_qty, _) = t.stock(raw, stores).await;
    let creditors_before = t.gl("Creditors").await;
    let bill = t
        .insert(
            "PurchaseInvoice",
            json!({"company": cid.to_string(), "supplier": acme.to_string(), "bill_no": "ACME/901", "update_stock": true,
                   "items": [{"item": raw.to_string(), "qty": "10", "price_list_rate": "15", "warehouse": stores.to_string()}]}),
        )
        .await;
    t.submit("PurchaseInvoice", uid(&bill)).await;
    assert_eq!(t.stock(raw, stores).await.0, raw_qty + n("10"));
    assert_eq!(t.gl("Creditors").await - creditors_before, n("-177"), "150 + CGST 13.5 + SGST 13.5");

    // ───────────── foreign currency: invoiced at 83, paid at 84 ─────────────
    let usd = t.find_one("Currency", json!({"code": "USD"})).await;
    let yesterday = (chrono::Utc::now().date_naive() - chrono::Duration::days(1)).to_string();
    t.insert("ExchangeRate", json!({"from_currency": usd["id"], "to_currency": inr["id"], "rate": "83", "valid_from": yesterday})).await;
    let abroad = uid(&t.insert("Customer", json!({"customer_name": "Overseas Inc", "in_gst_category": "Overseas", "default_currency": usd["id"]})).await);
    let export = t
        .insert("SalesInvoice", json!({"company": cid.to_string(), "customer": abroad.to_string(), "items": [{"item": widget.to_string(), "qty": "100"}]}))
        .await;
    assert_eq!(export["in_place_of_supply"], json!("96"), "exports have the foreign place of supply");
    assert_eq!(dec(&export["conversion_rate"]), n("83"));
    let export = t.submit("SalesInvoice", uid(&export)).await;
    let (usd_total, inr_total) = (dec(&export["grand_total"]), dec(&export["base_grand_total"]));
    assert_eq!(dec(&export["total_taxes"]), Decimal::ZERO, "zero-rated export under LUT");
    let gain_before = t.gl("Exchange Gain or Loss").await;
    let paid = t
        .insert(
            "PaymentEntry",
            json!({"company": cid.to_string(), "payment_type": "Receive", "customer": abroad.to_string(), "bank_account": bank, "currency": usd["id"],
                   "exchange_rate": "84", "paid_amount": usd_total.to_string(),
                   "references": [{"invoice_type": "Sales Invoice", "invoice": export["id"], "allocated_amount": usd_total.to_string()}]}),
        )
        .await;
    t.submit("PaymentEntry", uid(&paid)).await;
    assert_eq!(dec(&t.get("SalesInvoice", uid(&export)).await["outstanding_amount"]), Decimal::ZERO);
    let gain = (usd_total * n("84")).round_dp(2) - inr_total;
    assert_eq!(t.gl("Exchange Gain or Loss").await - gain_before, -gain, "the extra rupees are an exchange gain");

    // ───────────── reports and reconciliation ─────────────
    let company_param = [("company", cid.to_string())];
    let tb = t.report("trial_balance", &company_param).await;
    assert_eq!(tb["meta"]["balanced"], json!(true), "{}", tb["totals"]);
    let bs = t.report("balance_sheet", &company_param).await;
    assert_eq!(bs["meta"]["balanced"], json!(true), "difference {}", bs["meta"]["difference"]);
    let pl = t.report("profit_and_loss", &company_param).await;
    assert!(pl["rows"].as_array().unwrap().iter().any(|r| r["account"] == json!("Net Profit") || r["account"] == json!("Net Loss")));
    // Closing the year into Retained Earnings keeps the income statement's profit and the books balanced.
    let sales = t.gl("Sales").await;
    assert!(sales < Decimal::ZERO, "sales carry a credit balance");
    let mut tx = t.k.db.tenant_tx(tenant).await.unwrap();
    let ids: Vec<(String, Uuid)> = sqlx::query_as("select account_name, id from e_account where tenant_id = $1 and company = $2 and account_name = any($3)")
        .bind(tenant)
        .bind(cid)
        .bind(vec!["Sales", "Retained Earnings"])
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let id_of = |name: &str| ids.iter().find(|(n, _)| n == name).map(|(_, id)| *id).unwrap();
    let (sales_acc, retained) = (id_of("Sales"), id_of("Retained Earnings"));
    let mut closing = ferrum_domain::ledger::Entry::new(cid, t.ctx.today(), "PeriodClosingVoucher", Uuid::now_v7(), None);
    closing.lines.push(ferrum_domain::ledger::Line::new(sales_acc, -sales, "INR"));
    closing.lines.push(ferrum_domain::ledger::Line::new(retained, sales, "INR"));
    let mut tx = t.k.db.tenant_tx(tenant).await.unwrap();
    ferrum_domain::ledger::post(&mut tx, &t.ctx, closing, 2).await.unwrap();
    tx.commit().await.unwrap();
    let after = t.report("profit_and_loss", &company_param).await;
    assert_eq!(after["totals"]["net_profit"], pl["totals"]["net_profit"], "closing entries never change the income statement");
    assert_eq!(t.report("balance_sheet", &company_param).await["meta"]["balanced"], json!(true));
    assert_eq!(t.report("trial_balance", &company_param).await["meta"]["balanced"], json!(true));
    // The report list only offers what the caller may run.
    let names = |v: Value| v.as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert!(names(ferrum_domain::reports::catalog_for(&t.ctx)).contains(&"trial_balance".to_string()));
    let nobody = t.k.create_user(&t.ctx, "nobody@reports.test", "No Body", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let nobody_ctx = t.k.load_ctx(tenant, nobody, None, "it".into()).await.unwrap();
    assert!(names(ferrum_domain::reports::catalog_for(&nobody_ctx)).is_empty(), "no permissions, no reports");
    let ar = t.report("receivables", &company_param).await;
    assert_eq!(dec(&ar["totals"]["outstanding_base"]), n("-770") + dec(&direct["grand_total"]), "customer credit plus the unpaid direct sale");
    let sb = t.report("stock_balance", &company_param).await;
    let stock_gl = t.gl("Stock In Hand").await + t.gl("Work In Progress").await;
    assert_eq!(dec(&sb["totals"]["value"]), stock_gl, "perpetual inventory: stock value equals the stock accounts");
    let sr = t.report("sales_register", &company_param).await;
    assert_eq!(sr["totals"]["count"], json!(4), "invoice, credit note, direct sale, export");
    let gl = t.report("general_ledger", &[("company", cid.to_string()), ("customer", globex.to_string())]).await;
    assert!(gl["rows"].as_array().unwrap().len() > 3);
    let sl = t.report("stock_ledger", &[("company", cid.to_string()), ("item", widget.to_string())]).await;
    assert!(sl["rows"].as_array().unwrap().iter().any(|r| r["is_reversal"] == json!(true)));

    // Every journal entry balances, and stock balances equal the sum of their moves.
    let mut tx = t.k.db.tenant_tx(tenant).await.unwrap();
    let unbalanced: i64 = sqlx::query_scalar(
        "select count(*) from (select entry_id from journal_line where tenant_id = $1 group by entry_id having sum(debit) <> sum(credit)) x",
    )
    .bind(tenant)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(unbalanced, 0);
    let drift: i64 = sqlx::query_scalar(
        "select count(*) from stock_balance b where b.tenant_id = $1 and (b.qty, b.value) <> \
         (select coalesce(sum(m.qty), 0), coalesce(sum(m.value), 0) from stock_move m where m.tenant_id = b.tenant_id and m.item_id = b.item_id and m.warehouse_id = b.warehouse_id)",
    )
    .bind(tenant)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(drift, 0, "stock balances match their moves");
    drop(tx);

    // A role without accounting access cannot read financial reports.
    let clerk = t.k.create_user(&t.ctx, "clerk@ti.test", "Sales Clerk", Some("Correct-Horse-9"), &["Sales User".into()], false).await.expect("user");
    let clerk_ctx = t.k.load_ctx(tenant, clerk, None, "it".into()).await.expect("clerk ctx");
    let denied = ferrum_domain::reports::run(&t.k, &clerk_ctx, "trial_balance", &HashMap::new()).await;
    assert!(matches!(denied, Err(Error::Forbidden(_))), "{denied:?}");

    // ───────────── sandboxed plugins ─────────────
    let plugin = |name: &str, entity: &str, stage: &str| -> ferrum_plugins::NewPlugin {
        serde_json::from_value(json!({"name": name, "hooks": [{"entity": entity, "stage": stage}]})).unwrap()
    };
    let guard = ferrum_plugins::constant_plugin_wat(r#"{"error":"Invoices need sign-off this week"}"#);
    let refused = plugins.install(&t.k, &clerk_ctx, plugin("guard", "SalesInvoice", "before_submit"), guard.as_bytes()).await;
    assert!(matches!(refused, Err(Error::Forbidden(_))), "only administrators install plugins");
    let guard_id = plugins.install(&t.k, &t.ctx, plugin("guard", "SalesInvoice", "before_submit"), guard.as_bytes()).await.expect("install");
    let small = t
        .insert("SalesInvoice", json!({"company": cid.to_string(), "customer": globex.to_string(), "items": [{"item": widget.to_string(), "qty": "1"}]}))
        .await;
    let vetoed = t.k.submit(&t.ctx, "SalesInvoice", uid(&small), None).await;
    assert!(matches!(vetoed, Err(Error::Invalid(ref m)) if m.contains("sign-off")), "{vetoed:?}");
    assert_eq!(t.get("SalesInvoice", uid(&small)).await["docstatus"], json!(0), "the veto rolled the submit back");
    plugins.set_enabled(&t.k, &t.ctx, guard_id, false).await.expect("disable");
    t.submit("SalesInvoice", uid(&small)).await;
    let stamp = ferrum_plugins::constant_plugin_wat(r#"{"patch":{"remarks":"checked by plugin"}}"#);
    plugins.install(&t.k, &t.ctx, plugin("stamp", "SalesInvoice", "before_validate"), stamp.as_bytes()).await.expect("install stamp");
    let stamped = t
        .insert("SalesInvoice", json!({"company": cid.to_string(), "customer": globex.to_string(), "items": [{"item": widget.to_string(), "qty": "1"}]}))
        .await;
    assert_eq!(stamped["remarks"], json!("checked by plugin"));
    let bad = plugins.install(&t.k, &t.ctx, plugin("bad", "NoSuchThing", "validate"), stamp.as_bytes()).await;
    assert!(matches!(bad, Err(Error::Validation(_))), "{bad:?}");
    assert_eq!(plugins.list(&t.k, &t.ctx).await.unwrap().len(), 2);
}
