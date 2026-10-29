//! End-to-end stock operations against a real PostgreSQL (`FERRUM_STOCK_OPS_TEST_DATABASE_URL`,
//! default `postgres://ferrum:ferrum@localhost:5432/ferrum_stock_ops_test`). The database is reset first.
//!
//! One company runs the stock module the way its users would through the API: counts and opening
//! stock (plain, batched, serialized), landed costs on partly consumed receipts (moving average and
//! FIFO) and their cancellation, pick lists from sales orders with expiry-ordered batches and
//! serials (and every over-picking refusal), packing slips, quality inspections gating receipts and
//! deliveries, item variants, product bundles delivered, returned and invoiced, and stock ageing.
//! Throughout, stock quantities and values, ledger balances, statuses and refusals are checked,
//! and at the end the books balance and the stock accounts equal the stock ledger.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::Catalog;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
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

fn rows(doc: &Doc, k: &str) -> Vec<Doc> {
    doc[k].as_array().unwrap().iter().map(|r| r.as_object().unwrap().clone()).collect()
}

/// Every message of an error, for matching refusals.
fn text(e: &Error) -> String {
    match e {
        Error::Validation(v) => v.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

struct T {
    k: Arc<Kernel>,
    tenant: Uuid,
    admin: Ctx,
    company: String,
}

impl T {
    async fn insert(&self, ctx: &Ctx, entity: &str, v: Value) -> Doc {
        self.k.insert(ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }
    async fn submit(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Doc {
        self.k.submit(ctx, entity, id, None).await.unwrap_or_else(|e| panic!("submit {entity}: {e:?}"))
    }
    async fn cancel(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Doc {
        self.k.cancel(ctx, entity, id, None).await.unwrap_or_else(|e| panic!("cancel {entity}: {e:?}"))
    }
    async fn update(&self, ctx: &Ctx, entity: &str, id: Uuid, v: Value) -> Doc {
        self.k.update(ctx, entity, id, obj(v), None).await.unwrap_or_else(|e| panic!("update {entity}: {e:?}"))
    }
    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.admin, entity, id, false).await.unwrap_or_else(|e| panic!("get {entity}: {e:?}"))
    }
    async fn action(&self, ctx: &Ctx, entity: &str, id: Uuid, name: &str, params: Value) -> Value {
        self.k.run_action(ctx, entity, id, name, params).await.unwrap_or_else(|e| panic!("{entity}.{name}: {e:?}"))
    }
    /// Inserts and submits; returns the refusal of either step.
    async fn refused(&self, ctx: &Ctx, entity: &str, v: Value) -> String {
        match self.k.insert(ctx, entity, obj(v)).await {
            Err(e) => text(&e),
            Ok(d) => text(&self.k.submit(ctx, entity, uid(&d), None).await.expect_err("should be refused")),
        }
    }
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let page = self
            .k
            .list(&self.admin, entity, &ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(page.rows.len(), 1, "{entity} {filters}");
        page.rows[0].clone()
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
    async fn batch(&self, item: Uuid, batch: &str) -> Decimal {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        sqlx::query_scalar("select coalesce(sum(qty), 0) from stock_move where tenant_id = $1 and item_id = $2 and batch_no = $3")
            .bind(self.tenant)
            .bind(item)
            .bind(batch)
            .fetch_one(&mut *tx)
            .await
            .unwrap()
    }
    /// Where a serial is in stock (`None`: delivered or consumed).
    async fn serial_in(&self, item: Uuid, serial: &str) -> Option<Uuid> {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        sqlx::query_scalar("select warehouse_id from serial_no where tenant_id = $1 and item_id = $2 and serial = $3")
            .bind(self.tenant)
            .bind(item)
            .bind(serial)
            .fetch_one(&mut *tx)
            .await
            .unwrap()
    }
    async fn report(&self, ctx: &Ctx, name: &str, params: &[(&str, String)]) -> Value {
        let p: HashMap<String, String> = params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        ferrum_domain::reports::run(&self.k, ctx, name, &p).await.unwrap_or_else(|e| panic!("report {name}: {e:?}"))
    }
    async fn user(&self, email: &str, roles: &[&str]) -> Ctx {
        let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
        let id = self.k.create_user(&self.admin, email, email, Some("Correct-Horse-9"), &roles, false).await.expect("user");
        self.k.load_ctx(self.tenant, id, None, "it".into()).await.expect("ctx")
    }
    /// A submitted material receipt.
    async fn receive(&self, item: Uuid, wh: Uuid, qty: &str, rate: &str, extra: Value) -> Doc {
        let mut line = json!({"item": item.to_string(), "qty": qty, "basic_rate": rate, "target_warehouse": wh.to_string()});
        let mut head = json!({"company": self.company, "purpose": "Material Receipt"});
        for (k, v) in extra.as_object().unwrap() {
            if k == "posting_date" { head[k] = v.clone() } else { line[k] = v.clone() }
        }
        head["items"] = json!([line]);
        let d = self.insert(&self.admin, "StockEntry", head).await;
        self.submit(&self.admin, "StockEntry", uid(&d)).await
    }
    /// A submitted material issue; returns its value.
    async fn issue(&self, item: Uuid, wh: Uuid, qty: &str) -> Decimal {
        let d = self
            .insert(
                &self.admin,
                "StockEntry",
                json!({"company": self.company, "purpose": "Material Issue", "items": [{"item": item.to_string(), "qty": qty, "source_warehouse": wh.to_string()}]}),
            )
            .await;
        dec(&self.submit(&self.admin, "StockEntry", uid(&d)).await["total_amount"])
    }
}

fn line(item: Uuid, qty: &str, wh: Uuid) -> Value {
    json!({"item": item.to_string(), "qty": qty, "warehouse": wh.to_string()})
}

#[test]
fn stock_operations_end_to_end() {
    // Same runtime shape as the server: unoptimized async hook chains need more than 2 MiB.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_STOCK_OPS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_stock_ops_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("platform packages");
    packages.extend(ferrum_stock_ops::packages().expect("app packages"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_stock_ops::register(&mut hooks);
    for source in ferrum_stock_ops::reports() {
        ferrum_domain::reports::add_source(source);
    }
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("partitions");
    ferrum_stock_ops::maintenance(&k).await.expect("maintenance");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "stockops".into(),
            name: "Stock Ops Co".into(),
            admin_email: "admin@stock.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["stock_ops".into()],
            settings: Some(json!({"fiscal_year_start_month": 4})),
        })
        .await
        .expect("tenant");
    let session = k.login("stockops", "admin@stock.test", "Correct-Horse-9").await.expect("login");
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    let a = admin.clone();
    let mut t = T { k: k.clone(), tenant, admin, company: String::new() };

    // ───────────── company, warehouses, parties, users ─────────────
    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let company = t.insert(&a, "Company", json!({"company_name": "Stock Ops Co", "abbr": "SOC", "default_currency": inr["id"]})).await;
    let cid = uid(&company);
    t.company = cid.to_string();
    let co = t.company.clone();
    let wh = |name: &'static str| {
        let t = &t;
        async move { uid(&t.find_one("Warehouse", json!({"warehouse_name": name, "company": cid.to_string()})).await) }
    };
    let (stores, fg) = (wh("Stores").await, wh("Finished Goods").await);
    let all_wh = wh("All Warehouses").await;
    let overflow = uid(&t.insert(&a, "Warehouse", json!({"warehouse_name": "Overflow", "company": co, "parent_warehouse": all_wh.to_string()})).await);
    let account = |name: &'static str| {
        let t = &t;
        async move { uid(&t.find_one("Account", json!({"account_name": name, "company": cid.to_string()})).await) }
    };
    let freight = account("Freight and Forwarding").await;
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let item = |code: &'static str, extra: Value| {
        let (t, products, nos) = (&t, &products, &nos);
        async move {
            let mut v = json!({"item_code": code, "item_name": code.to_lowercase(), "item_group": products["id"], "stock_uom": nos["id"]});
            for (k, x) in extra.as_object().unwrap() {
                v[k] = x.clone();
            }
            uid(&t.insert(&t.admin, "Item", v).await)
        }
    };
    let customer = uid(&t.insert(&a, "Customer", json!({"customer_name": "Acme Retail"})).await);
    let supplier = uid(&t.insert(&a, "Supplier", json!({"supplier_name": "Bolt Supplies"})).await);
    let stock_user = t.user("counter@stock.test", &["Stock User"]).await;
    let stock_manager = t.user("manager@stock.test", &["Stock Manager", "Stock User"]).await;
    let seller = t.user("seller@stock.test", &["Sales User"]).await;
    let inspector = t.user("qa@stock.test", &["Quality Manager"]).await;
    let buyer = t.user("buyer@stock.test", &["Purchase Manager", "Purchase User"]).await;
    let today = chrono::Utc::now().date_naive();
    let days_ago = |d: i64| (today - chrono::Duration::days(d)).to_string();

    // ═════════════ stock reconciliation ═════════════
    let reco_a = item("RECO-A", json!({"standard_rate": "20"})).await;
    let opening = t
        .insert(
            &stock_user,
            "StockReconciliation",
            json!({"company": co, "purpose": "Opening Stock", "items": [{"item": reco_a.to_string(), "warehouse": stores.to_string(), "qty": "10", "valuation_rate": "10"}]}),
        )
        .await;
    assert_eq!(dec(&opening["difference_amount"]), n("100"), "draft shows the change");
    let opening = t.submit(&stock_user, "StockReconciliation", uid(&opening)).await;
    assert!(opening["number"].as_str().unwrap().starts_with("MAT-RECO-"));
    assert_eq!(t.stock(reco_a, stores).await, (n("10"), n("100")));
    assert_eq!(t.gl("Opening Balance Equity").await, n("-100"), "opening stock goes to the opening balance account");
    let reco = |qty: &'static str, rate: Option<&'static str>| {
        let mut l = json!({"item": reco_a.to_string(), "warehouse": stores.to_string(), "qty": qty});
        if let Some(r) = rate {
            l["valuation_rate"] = json!(r);
        }
        json!({"company": co, "items": [l]})
    };
    // Up with a rate: the counted stock is revalued.
    let up = t.insert(&stock_user, "StockReconciliation", reco("12", Some("11"))).await;
    let up = t.submit(&stock_user, "StockReconciliation", uid(&up)).await;
    assert_eq!(t.stock(reco_a, stores).await, (n("12"), n("132")));
    assert_eq!(t.gl("Stock Adjustment").await, n("-32"), "the gain is credited to stock adjustment");
    let up_line = &rows(&up, "items")[0];
    assert_eq!((dec(&up_line["current_qty"]), dec(&up_line["current_valuation_rate"]), dec(&up_line["amount_difference"])), (n("10"), n("10"), n("32")));
    assert_eq!(dec(&up["difference_amount"]), n("32"));
    // Down without a rate: only the quantity changes, at the current rate.
    let down = t.insert(&stock_user, "StockReconciliation", reco("7", None)).await;
    let down = t.submit(&stock_user, "StockReconciliation", uid(&down)).await;
    assert_eq!(t.stock(reco_a, stores).await, (n("7"), n("77")));
    assert_eq!(dec(&down["difference_amount"]), n("-55"));
    assert_eq!(t.gl("Stock Adjustment").await, n("23"));
    // Down with a rate, then cancelled: everything comes back exactly.
    let reset = t.insert(&stock_user, "StockReconciliation", reco("5", Some("9"))).await;
    let reset = t.submit(&stock_user, "StockReconciliation", uid(&reset)).await;
    assert_eq!(t.stock(reco_a, stores).await, (n("5"), n("45")));
    assert_eq!(t.gl("Stock Adjustment").await, n("55"));
    let not_allowed = t.k.cancel(&stock_user, "StockReconciliation", uid(&reset), None).await;
    assert!(matches!(not_allowed, Err(Error::Forbidden(_))), "stock users count, managers cancel: {not_allowed:?}");
    t.cancel(&stock_manager, "StockReconciliation", uid(&reset)).await;
    assert_eq!(t.stock(reco_a, stores).await, (n("7"), n("77")));
    assert_eq!(t.gl("Stock Adjustment").await, n("23"));
    // Revalued without moving quantity: the same count at a new rate.
    let reval = t.insert(&stock_user, "StockReconciliation", reco("7", Some("12"))).await;
    let reval = t.submit(&stock_user, "StockReconciliation", uid(&reval)).await;
    assert_eq!(t.stock(reco_a, stores).await, (n("7"), n("84")));
    assert_eq!((dec(&reval["difference_amount"]), t.gl("Stock Adjustment").await), (n("7"), n("16")));
    t.cancel(&stock_manager, "StockReconciliation", uid(&reval)).await;
    assert_eq!(t.stock(reco_a, stores).await, (n("7"), n("77")));
    let twice = t
        .refused(
            &stock_user,
            "StockReconciliation",
            json!({"company": co, "items": [{"item": reco_a.to_string(), "warehouse": stores.to_string(), "qty": "1"}, {"item": reco_a.to_string(), "warehouse": stores.to_string(), "qty": "2"}]}),
        )
        .await;
    assert!(twice.contains("counted twice"), "{twice}");

    // Batches: one line per batch.
    let reco_b = item("RECO-B", json!({"has_batch_no": true})).await;
    let batch_line = |batch: &str, qty: &str, rate: Option<&str>| {
        let mut l = json!({"item": reco_b.to_string(), "warehouse": stores.to_string(), "qty": qty, "batch_no": batch});
        if let Some(r) = rate {
            l["valuation_rate"] = json!(r);
        }
        l
    };
    let ob = t
        .insert(
            &stock_user,
            "StockReconciliation",
            json!({"company": co, "purpose": "Opening Stock", "items": [batch_line("B1", "5", Some("20")), batch_line("B2", "3", Some("20"))]}),
        )
        .await;
    t.submit(&stock_user, "StockReconciliation", uid(&ob)).await;
    assert_eq!(t.stock(reco_b, stores).await, (n("8"), n("160")));
    let no_batch = t
        .refused(
            &stock_user,
            "StockReconciliation",
            json!({"company": co, "items": [{"item": reco_b.to_string(), "warehouse": stores.to_string(), "qty": "1"}]}),
        )
        .await;
    assert!(no_batch.contains("batch_no"), "{no_batch}");
    let count_b1 = t.insert(&stock_user, "StockReconciliation", json!({"company": co, "items": [batch_line("B1", "4", None)]})).await;
    t.submit(&stock_user, "StockReconciliation", uid(&count_b1)).await;
    assert_eq!(t.stock(reco_b, stores).await, (n("7"), n("140")));
    assert_eq!((t.batch(reco_b, "B1").await, t.batch(reco_b, "B2").await), (n("4"), n("3")), "only batch B1 lost a unit");

    // Serials: the counted set replaces the book set.
    let reco_s = item("RECO-S", json!({"has_serial_no": true})).await;
    let ser = |qty: &str, serials: &str, rate: Option<&str>, purpose: &str| {
        let mut l = json!({"item": reco_s.to_string(), "warehouse": stores.to_string(), "qty": qty, "serial_no": serials});
        if let Some(r) = rate {
            l["valuation_rate"] = json!(r);
        }
        json!({"company": co, "purpose": purpose, "items": [l]})
    };
    let os = t.insert(&stock_user, "StockReconciliation", ser("3", "S1\nS2\nS3", Some("100"), "Opening Stock")).await;
    t.submit(&stock_user, "StockReconciliation", uid(&os)).await;
    assert_eq!(t.stock(reco_s, stores).await, (n("3"), n("300")));
    let wrong = t.refused(&stock_user, "StockReconciliation", ser("2", "S1\nS2\nS4", None, "Stock Reconciliation")).await;
    assert!(wrong.contains("exactly 2 serial"), "{wrong}");
    let swap = t.insert(&stock_user, "StockReconciliation", ser("3", "S1\nS2\nS4", None, "Stock Reconciliation")).await;
    let swap = t.submit(&stock_user, "StockReconciliation", uid(&swap)).await;
    assert_eq!(rows(&swap, "items")[0]["current_serial_no"], json!("S1\nS2\nS3"));
    assert_eq!(t.stock(reco_s, stores).await, (n("3"), n("300")));
    assert_eq!((t.serial_in(reco_s, "S3").await, t.serial_in(reco_s, "S4").await), (None, Some(stores)), "S3 is gone, S4 was found");
    let variance = t.report(&stock_manager, "stock_reconciliation_variance", &[("company", co.clone()), ("item", reco_a.to_string())]).await;
    assert_eq!(variance["rows"].as_array().unwrap().len(), 3, "the cancelled count is not a variance");
    assert_eq!(dec(&variance["totals"]["value_difference"]), n("77") - n("0"), "opening 100 + 32 - 55 = 77");

    // ═════════════ landed cost, moving average ═════════════
    let lc = item("LC-MA", json!({"standard_rate": "150"})).await;
    let pr = t
        .insert(&buyer, "PurchaseReceipt", json!({"company": co, "supplier": supplier.to_string(), "items": [{"item": lc.to_string(), "qty": "10", "price_list_rate": "100", "warehouse": stores.to_string()}]}))
        .await;
    let pr_id = uid(&t.submit(&buyer, "PurchaseReceipt", uid(&pr)).await);
    let deliver = |item: Uuid, qty: &'static str| {
        let t = &t;
        async move {
            let d =
                t.insert(&t.admin, "DeliveryNote", json!({"company": t.company, "customer": customer.to_string(), "items": [line(item, qty, stores)]})).await;
            t.submit(&t.admin, "DeliveryNote", uid(&d)).await
        }
    };
    let cogs0 = t.gl("Cost of Goods Sold").await;
    deliver(lc, "4").await;
    assert_eq!(t.gl("Cost of Goods Sold").await - cogs0, n("400"));
    assert_eq!(t.stock(lc, stores).await, (n("6"), n("600")));
    let voucher = |amount: &str| {
        json!({"company": co, "receipts": [{"receipt_type": "Purchase Receipt", "purchase_receipt": pr_id.to_string()}],
               "charges": [{"description": "Freight", "expense_account": freight.to_string(), "amount": amount}]})
    };
    let lcv = t.insert(&buyer, "LandedCostVoucher", voucher("200")).await;
    let lcv_items = rows(&lcv, "items");
    assert_eq!(lcv_items.len(), 1);
    assert_eq!((dec(&lcv_items[0]["qty"]), dec(&lcv_items[0]["amount"]), dec(&lcv_items[0]["applicable_charges"])), (n("10"), n("1000"), n("200")));
    let lcv = t.submit(&buyer, "LandedCostVoucher", uid(&lcv)).await;
    let lcv_line = &rows(&lcv, "items")[0];
    assert_eq!(
        (dec(&lcv_line["on_hand_qty"]), dec(&lcv_line["capitalized_amount"]), dec(&lcv_line["consumed_amount"])),
        (n("6"), n("120"), n("80")),
        "6 of 10 still in stock: 120 capitalized, 80 consumed"
    );
    assert_eq!(t.stock(lc, stores).await, (n("6"), n("720")));
    assert_eq!(t.gl("Cost of Goods Sold").await - cogs0, n("480"), "the consumed share raises the cost of what was sold");
    assert_eq!(t.gl("Freight and Forwarding").await, n("-200"));
    t.cancel(&buyer, "LandedCostVoucher", uid(&lcv)).await;
    assert_eq!(t.stock(lc, stores).await, (n("6"), n("600")), "cancelled at once: exactly as before");
    assert_eq!(t.gl("Cost of Goods Sold").await - cogs0, n("400"));
    assert_eq!(t.gl("Freight and Forwarding").await, Decimal::ZERO);
    let lcv2 = t.insert(&buyer, "LandedCostVoucher", voucher("200")).await;
    t.submit(&buyer, "LandedCostVoucher", uid(&lcv2)).await;
    assert_eq!(t.stock(lc, stores).await, (n("6"), n("720")));
    let locked = t.k.cancel(&buyer, "PurchaseReceipt", pr_id, None).await;
    assert!(locked.as_ref().is_err_and(|e| text(e).contains("landed cost voucher")), "{locked:?}");
    deliver(lc, "2").await;
    assert_eq!(t.gl("Cost of Goods Sold").await - cogs0, n("720"), "sold at the landed rate of 120");
    t.cancel(&buyer, "LandedCostVoucher", uid(&lcv2)).await;
    assert_eq!(t.stock(lc, stores).await, (n("4"), n("400")), "the remaining stock is back at its receipt cost");
    assert_eq!(t.gl("Cost of Goods Sold").await - cogs0, n("600"), "six units sold at 100, as if there had been no voucher");
    assert_eq!(t.gl("Freight and Forwarding").await, Decimal::ZERO);
    // Distribution over two receipt lines.
    let lx = item("LC-X", json!({})).await;
    let pr2 = t
        .insert(
            &buyer,
            "PurchaseReceipt",
            json!({"company": co, "supplier": supplier.to_string(), "items": [
                {"item": lc.to_string(), "qty": "5", "price_list_rate": "100", "warehouse": stores.to_string()},
                {"item": lx.to_string(), "qty": "10", "price_list_rate": "10", "warehouse": stores.to_string()}]}),
        )
        .await;
    let pr2_id = uid(&t.submit(&buyer, "PurchaseReceipt", uid(&pr2)).await);
    let v = json!({"company": co, "distribute_charges_based_on": "Qty", "receipts": [{"purchase_receipt": pr2_id.to_string()}],
                       "charges": [{"description": "Duty", "expense_account": freight.to_string(), "amount": "60"}]});
    let by_qty = t.insert(&buyer, "LandedCostVoucher", v.clone()).await;
    let shares: Vec<Decimal> = rows(&by_qty, "items").iter().map(|r| dec(&r["applicable_charges"])).collect();
    assert_eq!(shares, vec![n("20"), n("40")], "60 over 5 and 10 units");
    let by_amount = t.update(&buyer, "LandedCostVoucher", uid(&by_qty), json!({"distribute_charges_based_on": "Amount"})).await;
    let shares: Vec<Decimal> = rows(&by_amount, "items").iter().map(|r| dec(&r["applicable_charges"])).collect();
    assert_eq!(shares, vec![n("50"), n("10")], "60 over 500 and 100 of value");
    let mut manual = rows(&by_amount, "items");
    manual[0].insert("applicable_charges".into(), json!("30"));
    manual[1].insert("applicable_charges".into(), json!("20"));
    let off =
        t.k.update(&buyer, "LandedCostVoucher", uid(&by_qty), obj(json!({"distribute_charges_based_on": "Distribute Manually", "items": manual})), None).await;
    assert!(off.as_ref().is_err_and(|e| text(e).contains("must add up")), "{off:?}");
    t.k.delete(&buyer, "LandedCostVoucher", uid(&by_qty), None).await.expect("drop the draft");

    // ═════════════ landed cost, FIFO ═════════════
    let lf = item("LC-FIFO", json!({"valuation_method": "FIFO"})).await;
    let fifo_pr = |qty: &'static str, rate: &'static str| {
        let t = &t;
        async move {
            let d = t
                .insert(
                    &t.admin,
                    "PurchaseReceipt",
                    json!({"company": t.company, "supplier": supplier.to_string(), "items": [{"item": lf.to_string(), "qty": qty, "price_list_rate": rate, "warehouse": stores.to_string()}]}),
                )
                .await;
            uid(&t.submit(&t.admin, "PurchaseReceipt", uid(&d)).await)
        }
    };
    fifo_pr("5", "10").await;
    let pr_b = fifo_pr("5", "20").await;
    assert_eq!(t.issue(lf, stores, "6").await, n("70"), "5 x 10 + 1 x 20");
    fifo_pr("5", "40").await;
    assert_eq!(t.stock(lf, stores).await, (n("9"), n("280")));
    let fifo_lcv = t
        .insert(
            &buyer,
            "LandedCostVoucher",
            json!({"company": co, "receipts": [{"purchase_receipt": pr_b.to_string()}], "charges": [{"description": "Freight", "expense_account": freight.to_string(), "amount": "50"}]}),
        )
        .await;
    let fifo_lcv = t.submit(&buyer, "LandedCostVoucher", uid(&fifo_lcv)).await;
    let fl = &rows(&fifo_lcv, "items")[0];
    assert_eq!((dec(&fl["on_hand_qty"]), dec(&fl["capitalized_amount"]), dec(&fl["consumed_amount"])), (n("4"), n("40"), n("10")));
    assert_eq!(t.stock(lf, stores).await, (n("9"), n("320")));
    assert_eq!(t.issue(lf, stores, "2").await, n("60"), "the receipt's layer keeps its place in the queue, now at 30");
    t.cancel(&buyer, "LandedCostVoucher", uid(&fifo_lcv)).await;
    assert_eq!(t.stock(lf, stores).await, (n("7"), n("240")), "2 x 20 + 5 x 40");
    assert_eq!(t.issue(lf, stores, "3").await, n("80"), "2 x 20 + 1 x 40: the layers are intact");
    assert_eq!(t.gl("Freight and Forwarding").await, Decimal::ZERO);
    // Batches and serials come back exactly as they were, with the extra value.
    let lb = item("LC-BATCH", json!({"has_batch_no": true})).await;
    let ls = item("LC-SER", json!({"has_serial_no": true})).await;
    let tracked = t
        .insert(
            &buyer,
            "PurchaseReceipt",
            json!({"company": co, "supplier": supplier.to_string(), "items": [
                {"item": lb.to_string(), "qty": "4", "price_list_rate": "10", "warehouse": stores.to_string(), "batch_no": "LB1"},
                {"item": lb.to_string(), "qty": "2", "price_list_rate": "10", "warehouse": stores.to_string(), "batch_no": "LB2"},
                {"item": ls.to_string(), "qty": "2", "price_list_rate": "100", "warehouse": stores.to_string(), "serial_no": "LS1\nLS2"}]}),
        )
        .await;
    let tracked = uid(&t.submit(&buyer, "PurchaseReceipt", uid(&tracked)).await);
    let tlcv = t
        .insert(
            &buyer,
            "LandedCostVoucher",
            json!({"company": co, "distribute_charges_based_on": "Qty", "receipts": [{"purchase_receipt": tracked.to_string()}],
                   "charges": [{"description": "Freight", "expense_account": freight.to_string(), "amount": "16"}]}),
        )
        .await;
    t.submit(&buyer, "LandedCostVoucher", uid(&tlcv)).await;
    assert_eq!((t.stock(lb, stores).await, t.stock(ls, stores).await), ((n("6"), n("72")), (n("2"), n("204"))), "8 units share 16");
    assert_eq!((t.batch(lb, "LB1").await, t.batch(lb, "LB2").await, t.serial_in(ls, "LS2").await), (n("4"), n("2"), Some(stores)));
    t.cancel(&buyer, "LandedCostVoucher", uid(&tlcv)).await;
    assert_eq!((t.stock(lb, stores).await, t.stock(ls, stores).await), ((n("6"), n("60")), (n("2"), n("200"))));
    assert_eq!((t.batch(lb, "LB1").await, t.serial_in(ls, "LS1").await), (n("4"), Some(stores)));

    // ═════════════ pick lists ═════════════
    let pk_plain = item("PK-PLAIN", json!({"standard_rate": "10"})).await;
    let pk_batch = item("PK-BATCH", json!({"standard_rate": "10", "has_batch_no": true})).await;
    let pk_ser = item("PK-SER", json!({"standard_rate": "10", "has_serial_no": true})).await;
    t.receive(pk_plain, stores, "6", "5", json!({})).await;
    t.receive(pk_plain, overflow, "10", "5", json!({})).await;
    for (batch, expiry) in [("BY", 100), ("BX", 10), ("BZ", -1)] {
        t.insert(&a, "Batch", json!({"batch_id": batch, "item": pk_batch.to_string(), "expiry_date": (today + chrono::Duration::days(expiry)).to_string()}))
            .await;
    }
    t.receive(pk_batch, stores, "5", "5", json!({"batch_no": "BY"})).await;
    t.receive(pk_batch, stores, "3", "5", json!({"batch_no": "BX"})).await;
    t.receive(pk_batch, stores, "4", "5", json!({"batch_no": "BZ"})).await;
    t.receive(pk_ser, stores, "3", "5", json!({"serial_no": "P1\nP2\nP3"})).await;
    let so = t
        .insert(
            &seller,
            "SalesOrder",
            json!({"company": co, "customer": customer.to_string(), "items": [line(pk_plain, "8", stores), line(pk_batch, "6", stores), line(pk_ser, "2", stores)]}),
        )
        .await;
    let so_id = uid(&t.submit(&seller, "SalesOrder", uid(&so)).await);
    let pl1 = vid(&t.action(&seller, "SalesOrder", so_id, "make_pick_list", json!({})).await);
    let pl2 = vid(&t.action(&seller, "SalesOrder", so_id, "make_pick_list", json!({})).await);
    let pl1_doc = t.get("PickList", pl1).await;
    let loc = rows(&pl1_doc, "locations");
    let spots: Vec<(String, String, Option<String>, Option<String>, Decimal)> = loc
        .iter()
        .map(|r| {
            (
                r["item"].as_str().unwrap().to_string(),
                r["warehouse"].as_str().unwrap_or_default().to_string(),
                r["batch_no"].as_str().map(str::to_string),
                r["serial_no"].as_str().map(str::to_string),
                dec(&r["picked_qty"]),
            )
        })
        .collect();
    assert_eq!(
        spots,
        vec![
            (pk_plain.to_string(), stores.to_string(), None, None, n("6")),
            (pk_plain.to_string(), overflow.to_string(), None, None, n("2")),
            (pk_batch.to_string(), stores.to_string(), Some("BX".into()), None, n("3")),
            (pk_batch.to_string(), stores.to_string(), Some("BY".into()), None, n("3")),
            (pk_ser.to_string(), stores.to_string(), None, Some("P1\nP2".into()), n("2")),
        ],
        "the order's warehouse first; earliest expiry first, expired batch skipped; oldest serials first"
    );
    assert_eq!(pl1_doc["customer"], json!(customer.to_string()));
    let mut over = loc.clone();
    over[0].insert("picked_qty".into(), json!("7"));
    let over = t.k.update(&seller, "PickList", pl1, obj(json!({"locations": over})), None).await;
    assert!(over.as_ref().is_err_and(|e| text(e).contains("over-picking")), "{over:?}");
    let pl1_doc = t.submit(&seller, "PickList", pl1).await;
    assert_eq!(pl1_doc["status"], json!("Open"));
    let second = t.k.submit(&seller, "PickList", pl2, None).await;
    assert!(second.as_ref().is_err_and(|e| text(e).contains("over-picking") && text(e).contains("not enough")), "{second:?}");
    let again = t.k.run_action(&seller, "SalesOrder", so_id, "make_pick_list", json!({})).await;
    assert!(again.as_ref().is_err_and(|e| text(e).contains("nothing is left to pick")), "{again:?}");
    t.k.delete(&seller, "PickList", pl2, None).await.expect("drop the duplicate draft");
    // A transfer cannot take what the first list holds; from the other warehouse it can.
    let transfer = t
        .insert(
            &stock_user,
            "PickList",
            json!({"company": co, "purpose": "Material Transfer", "target_warehouse": fg.to_string(),
                   "locations": [{"item": pk_plain.to_string(), "qty": "3", "warehouse": stores.to_string()}]}),
        )
        .await;
    let held = t.k.submit(&stock_user, "PickList", uid(&transfer), None).await;
    assert!(held.as_ref().is_err_and(|e| text(e).contains("not enough PK-PLAIN in Stores: 0 available")), "{held:?}");
    let mut moved = rows(&transfer, "locations");
    moved[0].insert("warehouse".into(), json!(overflow.to_string()));
    t.update(&stock_user, "PickList", uid(&transfer), json!({"locations": moved})).await;
    t.submit(&stock_user, "PickList", uid(&transfer)).await;
    let se = vid(&t.action(&stock_user, "PickList", uid(&transfer), "make_stock_entry", json!({})).await);
    t.submit(&stock_user, "StockEntry", se).await;
    assert_eq!(t.get("PickList", uid(&transfer)).await["status"], json!("Completed"));
    assert_eq!((t.stock(pk_plain, overflow).await.0, t.stock(pk_plain, fg).await.0), (n("7"), n("3")));
    // Placing a transfer's needs never picks from its own target, nor what other lists hold.
    let free = t
        .insert(
            &stock_user,
            "PickList",
            json!({"company": co, "purpose": "Material Transfer", "target_warehouse": fg.to_string(), "locations": [{"item": pk_plain.to_string(), "qty": "4"}]}),
        )
        .await;
    let placed = t.action(&stock_user, "PickList", uid(&free), "get_item_locations", json!({})).await;
    assert_eq!(placed["shortages"], json!([]));
    let placed = rows(&t.get("PickList", uid(&free)).await, "locations");
    assert_eq!(
        placed.iter().map(|r| (r["warehouse"].as_str().unwrap().to_string(), dec(&r["picked_qty"]))).collect::<Vec<_>>(),
        vec![(overflow.to_string(), n("4"))],
        "Stores is fully held and Finished Goods is the target"
    );
    t.k.delete(&stock_user, "PickList", uid(&free), None).await.expect("drop the draft");
    // Deliver what was picked.
    let made = t.action(&seller, "PickList", pl1, "make_delivery_note", json!({})).await;
    let dn = vid(&made);
    let dn_doc = t.get("DeliveryNote", dn).await;
    assert_eq!(rows(&dn_doc, "items").len(), 5);
    assert!(rows(&dn_doc, "items").iter().all(|r| r["so_line"].is_string() && r["stk_pick_list"] == json!(pl1.to_string())));
    t.submit(&seller, "DeliveryNote", dn).await;
    assert_eq!((t.stock(pk_plain, stores).await.0, t.stock(pk_plain, overflow).await.0), (n("0"), n("5")));
    assert_eq!((t.batch(pk_batch, "BX").await, t.batch(pk_batch, "BY").await, t.batch(pk_batch, "BZ").await), (n("0"), n("2"), n("4")));
    assert_eq!((t.serial_in(pk_ser, "P1").await, t.serial_in(pk_ser, "P3").await), (None, Some(stores)));
    let pl1_doc = t.get("PickList", pl1).await;
    assert_eq!((pl1_doc["status"].clone(), dec(&pl1_doc["per_delivered"])), (json!("Completed"), n("100")));
    assert_eq!(t.get("SalesOrder", so_id).await["status"], json!("To Bill"));
    let status = t.report(&seller, "pick_list_status", &[("company", co.clone())]).await;
    let row = status["rows"].as_array().unwrap().iter().find(|r| r["pick_list_id"] == json!(pl1.to_string())).cloned().unwrap();
    assert_eq!((row["status"].clone(), dec(&row["picked_qty"]), dec(&row["delivered_qty"])), (json!("Completed"), n("16"), n("16")));
    t.cancel(&seller, "DeliveryNote", dn).await;
    assert_eq!(t.get("PickList", pl1).await["status"], json!("Open"), "a cancelled delivery reopens the list");
    let busy = t.k.cancel(&seller, "PickList", uid(&transfer), None).await;
    assert!(busy.is_err(), "the transfer was moved; it cannot be cancelled first");

    // ═════════════ packing slips ═════════════
    let ps_a = item("PS-ITEM", json!({"standard_rate": "5"})).await;
    let ps_b = item("PS-ITEM2", json!({"standard_rate": "5"})).await;
    t.receive(ps_a, stores, "20", "2", json!({})).await;
    t.receive(ps_b, stores, "10", "2", json!({})).await;
    let dn = t
        .insert(&seller, "DeliveryNote", json!({"company": co, "customer": customer.to_string(), "items": [line(ps_a, "10", stores), line(ps_b, "5", stores)]}))
        .await;
    let dn_id = uid(&dn);
    let ps1 = t
        .insert(
            &seller,
            "PackingSlip",
            json!({"delivery_note": dn_id.to_string(), "from_case_no": 1, "to_case_no": 2, "gross_weight_pkg": "3.5",
                   "items": [{"item": ps_a.to_string(), "qty": "6", "net_weight": "0.5"}]}),
        )
        .await;
    assert_eq!(dec(&ps1["net_weight_pkg"]), n("3"));
    assert_eq!(rows(&ps1, "items")[0]["dn_line"], rows(&dn, "items")[0]["id"], "the note's only line for the item");
    assert_eq!(ps1["company"], json!(co));
    let overlap = t
        .refused(
            &seller,
            "PackingSlip",
            json!({"delivery_note": dn_id.to_string(), "from_case_no": 2, "to_case_no": 3, "items": [{"item": ps_b.to_string(), "qty": "5"}]}),
        )
        .await;
    assert!(overlap.contains("packages 1-2 are already on packing slip"), "{overlap}");
    let too_much = t
        .refused(&seller, "PackingSlip", json!({"delivery_note": dn_id.to_string(), "from_case_no": 3, "items": [{"item": ps_a.to_string(), "qty": "5"}]}))
        .await;
    assert!(too_much.contains("packing more") && too_much.contains("11 packed of 10"), "{too_much}");
    let ps2 = t
        .insert(
            &seller,
            "PackingSlip",
            json!({"delivery_note": dn_id.to_string(), "from_case_no": 3, "items": [{"item": ps_a.to_string(), "qty": "4"}, {"item": ps_b.to_string(), "qty": "5"}]}),
        )
        .await;
    assert_eq!(ps2["to_case_no"], json!(3));
    let mut fewer = rows(&dn, "items");
    fewer[0].insert("qty".into(), json!("8"));
    let fewer = t.k.update(&seller, "DeliveryNote", dn_id, obj(json!({"items": fewer})), None).await;
    assert!(fewer.as_ref().is_err_and(|e| text(e).contains("already packed")), "{fewer:?}");
    let early = t.k.submit(&seller, "DeliveryNote", dn_id, None).await;
    assert!(early.as_ref().is_err_and(|e| text(e).contains("still a draft")), "{early:?}");
    let ps1 = t.submit(&seller, "PackingSlip", uid(&ps1)).await;
    t.submit(&seller, "PackingSlip", uid(&ps2)).await;
    t.submit(&seller, "DeliveryNote", dn_id).await;
    let html = t.k.render_print(&seller, "PackingSlip", uid(&ps1)).await.expect("print");
    assert!(html.contains("Packages 1 to 2") && html.contains(ps1["number"].as_str().unwrap()) && html.contains("ps-item"), "{html}");
    t.cancel(&seller, "DeliveryNote", dn_id).await;
    assert_eq!(t.get("PackingSlip", uid(&ps1)).await["docstatus"], json!(2), "cancelling the note cancels its slips");
    let dn2 = t.insert(&seller, "DeliveryNote", json!({"company": co, "customer": customer.to_string(), "items": [line(ps_a, "3", stores)]})).await;
    let slip = t.action(&seller, "DeliveryNote", uid(&dn2), "make_packing_slip", json!({})).await;
    let slip = t.get("PackingSlip", vid(&slip)).await;
    assert_eq!((dec(&rows(&slip, "items")[0]["qty"]), slip["from_case_no"].clone()), (n("3"), json!(1)));

    // ═════════════ quality inspection ═════════════
    let bad_tpl = t
        .refused(
            &inspector,
            "QualityInspectionTemplate",
            json!({"template_name": "Broken", "parameters": [{"parameter": "Length", "numeric": true, "formula": "doc.mean >"}]}),
        )
        .await;
    assert!(bad_tpl.contains("formula"), "{bad_tpl}");
    let tpl = t
        .insert(
            &inspector,
            "QualityInspectionTemplate",
            json!({"template_name": "Rod QC", "parameters": [
                {"parameter": "Length", "numeric": true, "min_value": "9.5", "max_value": "10.5"},
                {"parameter": "Finish", "numeric": false, "accepted_values": "Smooth\nPolished"},
                {"parameter": "Weight", "numeric": true, "formula": "doc.mean >= 2.0 && doc.max <= 2.5"}]}),
        )
        .await;
    let rod = item(
        "QI-ROD",
        json!({"standard_rate": "30", "stk_inspection_required_before_purchase": true, "stk_inspection_required_before_delivery": true,
               "stk_quality_inspection_template": uid(&tpl).to_string()}),
    )
    .await;
    let qpr = t
        .insert(&buyer, "PurchaseReceipt", json!({"company": co, "supplier": supplier.to_string(), "items": [{"item": rod.to_string(), "qty": "10", "price_list_rate": "20", "warehouse": stores.to_string()}]}))
        .await;
    let qpr_id = uid(&qpr);
    let gated = t.k.submit(&buyer, "PurchaseReceipt", qpr_id, None).await;
    assert!(gated.as_ref().is_err_and(|e| text(e).contains("needs an accepted quality inspection before purchase")), "{gated:?}");
    let made = t.action(&inspector, "PurchaseReceipt", qpr_id, "make_quality_inspections", json!({})).await;
    let qi1 = vid(&made["created"][0]);
    let qi1_doc = t.get("QualityInspection", qi1).await;
    assert_eq!((qi1_doc["inspection_type"].clone(), dec(&qi1_doc["sample_size"]), rows(&qi1_doc, "readings").len()), (json!("Incoming"), n("10"), 3));
    let read = |doc: &Doc, values: [&str; 3]| -> Value {
        let mut r = rows(doc, "readings");
        for (row, v) in r.iter_mut().zip(values) {
            row.insert("readings".into(), json!(v));
        }
        json!({"readings": r})
    };
    let qi1_doc = t.update(&inspector, "QualityInspection", qi1, read(&qi1_doc, ["9.4, 10.0", "Smooth", "2.1, 2.2"])).await;
    assert_eq!(qi1_doc["status"], json!("Rejected"));
    assert_eq!(rows(&qi1_doc, "readings")[0]["status"], json!("Rejected"), "9.4 is below the minimum");
    t.submit(&inspector, "QualityInspection", qi1).await;
    let rejected = t.k.submit(&buyer, "PurchaseReceipt", qpr_id, None).await;
    assert!(rejected.as_ref().is_err_and(|e| text(e).contains("rejected by its quality inspection")), "{rejected:?}");
    let qi2 = t
        .insert(
            &inspector,
            "QualityInspection",
            json!({"company": co, "reference_type": "Purchase Receipt", "purchase_receipt": qpr_id.to_string(), "reference_line": rows(&qpr, "items")[0]["id"],
                   "item": rod.to_string(), "sample_size": "2", "template": uid(&tpl).to_string()}),
        )
        .await;
    let too_many = t.k.update(&inspector, "QualityInspection", uid(&qi2), obj(json!({"sample_size": "11"})), None).await;
    assert!(too_many.as_ref().is_err_and(|e| text(e).contains("sample_size")), "{too_many:?}");
    let qi2 = t.update(&inspector, "QualityInspection", uid(&qi2), read(&qi2, ["9.8, 10.2", "Polished", "2.1, 2.3"])).await;
    assert_eq!(qi2["status"], json!("Accepted"));
    let qi2 = t.submit(&inspector, "QualityInspection", uid(&qi2)).await;
    t.submit(&buyer, "PurchaseReceipt", qpr_id).await;
    let keep = t.k.cancel(&inspector, "QualityInspection", uid(&qi2), None).await;
    assert!(keep.as_ref().is_err_and(|e| text(e).contains("was submitted on this inspection")), "{keep:?}");
    let qdn = t.insert(&seller, "DeliveryNote", json!({"company": co, "customer": customer.to_string(), "items": [line(rod, "2", stores)]})).await;
    let blocked = t.k.submit(&seller, "DeliveryNote", uid(&qdn), None).await;
    assert!(blocked.as_ref().is_err_and(|e| text(e).contains("before delivery")), "{blocked:?}");
    let qi3 = t
        .insert(
            &inspector,
            "QualityInspection",
            json!({"company": co, "reference_type": "Delivery Note", "delivery_note": uid(&qdn).to_string(), "item": rod.to_string(),
                   "sample_size": "1", "manual_inspection": true, "status": "Accepted", "remarks": "Visual check"}),
        )
        .await;
    assert_eq!(qi3["inspection_type"], json!("Outgoing"));
    t.submit(&inspector, "QualityInspection", uid(&qi3)).await;
    t.submit(&seller, "DeliveryNote", uid(&qdn)).await;
    let summary = t.report(&inspector, "quality_inspection_summary", &[("company", co.clone())]).await;
    let incoming = summary["rows"].as_array().unwrap().iter().find(|r| r["inspection_type"] == json!("Incoming")).cloned().unwrap();
    assert_eq!(
        (incoming["inspections"].clone(), incoming["accepted"].clone(), incoming["rejected"].clone(), incoming["rejected_parameters"].clone()),
        (json!(2), json!(1), json!(1), json!("Length (1)"))
    );
    assert_eq!(dec(&incoming["acceptance_rate"]), n("50"));

    // ═════════════ item variants ═════════════
    let colour = t
        .insert(
            &stock_manager,
            "ItemAttribute",
            json!({"attribute_name": "Colour", "values": [{"attribute_value": "Red", "abbr": "R"}, {"attribute_value": "Blue", "abbr": "B"}]}),
        )
        .await;
    let size = t
        .insert(
            &stock_manager,
            "ItemAttribute",
            json!({"attribute_name": "Size", "numeric_values": true, "from_range": "1", "to_range": "3", "increment": "1"}),
        )
        .await;
    let dup = t
        .refused(
            &stock_manager,
            "ItemAttribute",
            json!({"attribute_name": "Shade", "values": [{"attribute_value": "Red", "abbr": "R"}, {"attribute_value": "red", "abbr": "X"}]}),
        )
        .await;
    assert!(dup.contains("listed twice"), "{dup}");
    let shirt = item(
        "TSHIRT",
        json!({"item_name": "T-Shirt", "standard_rate": "20", "stk_has_variants": true,
               "stk_attributes": [{"attribute": uid(&colour).to_string()}, {"attribute": uid(&size).to_string()}]}),
    )
    .await;
    let made = t.action(&stock_manager, "Item", shirt, "make_variants", json!({})).await;
    let mut codes: Vec<String> = made["created"].as_array().unwrap().iter().map(|v| v["item_code"].as_str().unwrap().to_string()).collect();
    codes.sort();
    assert_eq!(codes, ["TSHIRT-B-1", "TSHIRT-B-2", "TSHIRT-B-3", "TSHIRT-R-1", "TSHIRT-R-2", "TSHIRT-R-3"]);
    let red2 = t.get("Item", uid(&t.find_one("Item", json!({"item_code": "TSHIRT-R-2"})).await)).await;
    assert_eq!(
        (red2["item_name"].clone(), dec(&red2["standard_rate"]), red2["stk_variant_of"].clone()),
        (json!("T-Shirt-R-2"), n("20"), json!(shirt.to_string()))
    );
    assert_eq!((red2["item_group"].clone(), red2["stk_has_variants"].clone()), (products["id"].clone(), json!(false)));
    let values: Vec<String> = rows(&red2, "stk_attributes").iter().map(|r| r["attribute_value"].as_str().unwrap().to_string()).collect();
    assert_eq!(values, ["Red", "2"]);
    let again = t.action(&stock_manager, "Item", shirt, "make_variants", json!({})).await;
    assert_eq!((again["created"].as_array().unwrap().len(), again["existing"].as_array().unwrap().len()), (0, 6), "idempotent");
    let chosen = t.action(&stock_manager, "Item", shirt, "make_variants", json!({"attributes": {"Colour": ["Red"], "Size": [2]}})).await;
    assert_eq!((chosen["created"].as_array().unwrap().len(), chosen["existing"][0]["item_code"].clone()), (0, json!("TSHIRT-R-2")));
    let off_grid = t.k.run_action(&stock_manager, "Item", shirt, "make_variants", json!({"attributes": {"Size": ["2.5"]}})).await;
    assert!(off_grid.is_err(), "2.5 is not a size");
    for (entity, input) in [
        ("SalesOrder", json!({"company": co, "customer": customer.to_string(), "items": [line(shirt, "1", stores)]})),
        (
            "PurchaseReceipt",
            json!({"company": co, "supplier": supplier.to_string(), "items": [{"item": shirt.to_string(), "qty": "1", "price_list_rate": "5", "warehouse": stores.to_string()}]}),
        ),
        (
            "StockEntry",
            json!({"company": co, "purpose": "Material Receipt", "items": [{"item": shirt.to_string(), "qty": "1", "basic_rate": "5", "target_warehouse": stores.to_string()}]}),
        ),
        (
            "StockReconciliation",
            json!({"company": co, "items": [{"item": shirt.to_string(), "warehouse": stores.to_string(), "qty": "1", "valuation_rate": "5"}]}),
        ),
    ] {
        let refused = t.refused(&a, entity, input).await;
        assert!(refused.contains("TSHIRT is a template"), "{entity}: {refused}");
    }
    let keep_blue = t.k.update(&stock_manager, "ItemAttribute", uid(&colour), obj(json!({"values": [{"attribute_value": "Red", "abbr": "R"}]})), None).await;
    assert!(keep_blue.as_ref().is_err_and(|e| text(e).contains("uses Blue")), "{keep_blue:?}");
    let stay = t.k.update(&a, "Item", shirt, obj(json!({"stk_has_variants": false})), None).await;
    assert!(stay.as_ref().is_err_and(|e| text(e).contains("must stay a template")), "{stay:?}");
    let blue2 = uid(&t.find_one("Item", json!({"item_code": "TSHIRT-B-2"})).await);
    let red1 = uid(&t.find_one("Item", json!({"item_code": "TSHIRT-R-1"})).await);
    t.receive(red1, stores, "5", "8", json!({})).await;
    t.receive(blue2, stores, "3", "8", json!({})).await;
    t.receive(blue2, overflow, "2", "8", json!({})).await;
    let matrix = t.report(&stock_manager, "variant_stock_matrix", &[("company", co.clone()), ("template", shirt.to_string())]).await;
    assert_eq!(matrix["rows"].as_array().unwrap().len(), 6);
    let b2 = matrix["rows"].as_array().unwrap().iter().find(|r| r["item_code"] == json!("TSHIRT-B-2")).cloned().unwrap();
    assert_eq!(
        (b2["attribute_0"].clone(), b2["attribute_1"].clone(), dec(&b2["total_qty"]), dec(&b2["total_value"])),
        (json!("Blue"), json!("2"), n("5"), n("40"))
    );
    assert_eq!(dec(&matrix["totals"]["total_qty"]), n("10"));

    // ═════════════ product bundles ═════════════
    let kit = item("KIT", json!({"is_stock_item": false, "standard_rate": "100"})).await;
    let ka = item("KA", json!({"standard_rate": "15"})).await;
    let ks = item("KS", json!({"has_serial_no": true})).await;
    t.receive(ka, stores, "20", "10", json!({})).await;
    t.receive(ks, stores, "4", "50", json!({"serial_no": "K1\nK2\nK3\nK4"})).await;
    let stocked = t.refused(&a, "ProductBundle", json!({"item": ka.to_string(), "items": [{"item": ks.to_string(), "qty": "1"}]})).await;
    assert!(stocked.contains("is a stock item"), "{stocked}");
    t.insert(&a, "ProductBundle", json!({"item": kit.to_string(), "items": [{"item": ka.to_string(), "qty": "2"}, {"item": ks.to_string(), "qty": "1"}]}))
        .await;
    let kit2 = item("KIT2", json!({"is_stock_item": false})).await;
    let nested = t.refused(&a, "ProductBundle", json!({"item": kit2.to_string(), "items": [{"item": kit.to_string(), "qty": "1"}]})).await;
    assert!(nested.contains("itself a product bundle"), "{nested}");
    let set_serials = |doc: &Doc, serials: &str| -> Value {
        let mut p = rows(doc, "stk_packed_items");
        for r in p.iter_mut() {
            if r["item"] == json!(ks.to_string()) {
                r.insert("serial_no".into(), json!(serials));
            }
        }
        json!({"stk_packed_items": p})
    };
    let kdn = t.insert(&seller, "DeliveryNote", json!({"company": co, "customer": customer.to_string(), "items": [line(kit, "2", stores)]})).await;
    let packed: Vec<(String, Decimal, String)> = rows(&kdn, "stk_packed_items")
        .iter()
        .map(|r| (r["item"].as_str().unwrap().to_string(), dec(&r["qty"]), r["warehouse"].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(packed, vec![(ka.to_string(), n("4"), stores.to_string()), (ks.to_string(), n("2"), stores.to_string())]);
    let kdn = t.update(&seller, "DeliveryNote", uid(&kdn), set_serials(&kdn, "K1\nK2")).await;
    let cogs = t.gl("Cost of Goods Sold").await;
    let kdn = t.submit(&seller, "DeliveryNote", uid(&kdn)).await;
    assert_eq!((t.stock(ka, stores).await, t.stock(ks, stores).await), ((n("16"), n("160")), (n("2"), n("100"))));
    assert_eq!((t.serial_in(ks, "K1").await, t.serial_in(ks, "K3").await), (None, Some(stores)));
    assert_eq!(t.gl("Cost of Goods Sold").await - cogs, n("140"), "4 x 10 + 2 x 50");
    assert_eq!(dec(&rows(&kdn, "items")[0]["valuation_rate"]), n("70"), "a kit costs 70");
    let rates: Vec<Decimal> = rows(&kdn, "stk_packed_items").iter().map(|r| dec(&r["valuation_rate"])).collect();
    assert_eq!(rates, vec![n("10"), n("50")]);
    let back = t
        .insert(
            &seller,
            "DeliveryNote",
            json!({"company": co, "customer": customer.to_string(), "is_return": true, "return_against": uid(&kdn).to_string(), "items": [line(kit, "1", stores)]}),
        )
        .await;
    let back = t.update(&seller, "DeliveryNote", uid(&back), set_serials(&back, "K1")).await;
    t.submit(&seller, "DeliveryNote", uid(&back)).await;
    assert_eq!(
        (t.stock(ka, stores).await, t.stock(ks, stores).await),
        ((n("18"), n("180")), (n("3"), n("150"))),
        "the return brings the components back at cost"
    );
    assert_eq!((t.serial_in(ks, "K1").await, t.gl("Cost of Goods Sold").await - cogs), (Some(stores), n("70")));
    let sinv =
        t.insert(&a, "SalesInvoice", json!({"company": co, "customer": customer.to_string(), "update_stock": true, "items": [line(kit, "1", stores)]})).await;
    let sinv = t.update(&a, "SalesInvoice", uid(&sinv), set_serials(&sinv, "K3")).await;
    t.submit(&a, "SalesInvoice", uid(&sinv)).await;
    assert_eq!((t.stock(ka, stores).await, t.stock(ks, stores).await), ((n("16"), n("160")), (n("2"), n("100"))));
    t.cancel(&a, "SalesInvoice", uid(&sinv)).await;
    assert_eq!((t.stock(ka, stores).await, t.stock(ks, stores).await), ((n("18"), n("180")), (n("3"), n("150"))));
    assert_eq!(t.serial_in(ks, "K3").await, Some(stores));
    // A kit picked for an order ships from where its components were picked.
    let kso = t.insert(&seller, "SalesOrder", json!({"company": co, "customer": customer.to_string(), "items": [line(kit, "1", stores)]})).await;
    let kso = uid(&t.submit(&seller, "SalesOrder", uid(&kso)).await);
    let kpl = vid(&t.action(&seller, "SalesOrder", kso, "make_pick_list", json!({})).await);
    let comps: Vec<(String, Decimal, Option<String>, Option<String>)> = rows(&t.get("PickList", kpl).await, "locations")
        .iter()
        .map(|r| {
            (
                r["item"].as_str().unwrap().to_string(),
                dec(&r["qty"]),
                r["serial_no"].as_str().map(str::to_string),
                r["product_bundle_item"].as_str().map(str::to_string),
            )
        })
        .collect();
    assert_eq!(
        comps,
        vec![(ka.to_string(), n("2"), None, Some(kit.to_string())), (ks.to_string(), n("1"), Some("K4".into()), Some(kit.to_string()))],
        "the kit's components, oldest serial first"
    );
    t.submit(&seller, "PickList", kpl).await;
    let kdn2 = vid(&t.action(&seller, "PickList", kpl, "make_delivery_note", json!({})).await);
    let kdn2_doc = t.get("DeliveryNote", kdn2).await;
    assert_eq!((rows(&kdn2_doc, "items").len(), dec(&rows(&kdn2_doc, "items")[0]["qty"])), (1, n("1")), "one whole kit");
    let packed: Vec<(String, Decimal, Option<String>)> = rows(&kdn2_doc, "stk_packed_items")
        .iter()
        .map(|r| (r["item"].as_str().unwrap().to_string(), dec(&r["qty"]), r["serial_no"].as_str().map(str::to_string)))
        .collect();
    assert_eq!(packed, vec![(ka.to_string(), n("2"), None), (ks.to_string(), n("1"), Some("K4".into()))]);
    t.submit(&seller, "DeliveryNote", kdn2).await;
    assert_eq!(t.get("PickList", kpl).await["status"], json!("Completed"));
    assert_eq!((t.stock(ka, stores).await, t.serial_in(ks, "K4").await), ((n("16"), n("160")), None));

    // ═════════════ stock ageing ═════════════
    let aged = item("AGE", json!({})).await;
    t.receive(aged, stores, "5", "10", json!({"posting_date": days_ago(100)})).await;
    t.receive(aged, stores, "5", "10", json!({"posting_date": days_ago(40)})).await;
    let gone = t.receive(aged, stores, "3", "10", json!({"posting_date": days_ago(10)})).await;
    t.cancel(&a, "StockEntry", uid(&gone)).await;
    t.receive(aged, stores, "5", "10", json!({})).await;
    t.issue(aged, stores, "3").await;
    let mv = t
        .insert(
            &a,
            "StockEntry",
            json!({"company": co, "purpose": "Material Transfer", "items": [{"item": aged.to_string(), "qty": "2", "source_warehouse": stores.to_string(), "target_warehouse": overflow.to_string()}]}),
        )
        .await;
    t.submit(&a, "StockEntry", uid(&mv)).await;
    let ageing = t.report(&stock_manager, "stock_ageing", &[("company", co.clone()), ("item", aged.to_string())]).await;
    let by_wh: HashMap<String, Value> = ageing["rows"].as_array().unwrap().iter().map(|r| (r["warehouse"].as_str().unwrap().to_string(), r.clone())).collect();
    let s = &by_wh["Stores"];
    assert_eq!(
        (dec(&s["qty"]), dec(&s["range1"]), dec(&s["range2"]), dec(&s["range3"]), dec(&s["older"])),
        (n("10"), n("5"), n("5"), n("0"), n("0")),
        "the oldest units left first; the cancelled receipt never counted"
    );
    let o = &by_wh["Overflow"];
    assert_eq!((dec(&o["qty"]), dec(&o["older"]), o["oldest"].clone()), (n("2"), n("2"), json!(days_ago(100))), "moved stock keeps its receipt date");

    // ═════════════ the books ═════════════
    let lcv_report = t.report(&buyer, "landed_cost_allocation", &[("company", co.clone())]).await;
    assert_eq!(lcv_report["rows"].as_array().unwrap().len(), 0, "cancelled vouchers allocate nothing");
    let live = t
        .insert(
            &buyer,
            "LandedCostVoucher",
            json!({"company": co, "receipts": [{"purchase_receipt": pr2_id.to_string()}],
        "charges": [{"description": "Insurance", "expense_account": freight.to_string(), "amount": "30"}]}),
        )
        .await;
    t.submit(&buyer, "LandedCostVoucher", uid(&live)).await;
    let lcv_report = t.report(&buyer, "landed_cost_allocation", &[("company", co.clone())]).await;
    assert_eq!(dec(&lcv_report["totals"]["applicable_charges"]), n("30"));
    assert_eq!(dec(&lcv_report["totals"]["capitalized"]) + dec(&lcv_report["totals"]["consumed"]), n("30"));
    let tb = t.report(&a, "trial_balance", &[("company", co.clone())]).await;
    assert_eq!(tb["meta"]["balanced"], json!(true), "{}", tb["totals"]);
    let unbalanced: i64 = {
        let mut tx = t.k.db.tenant_tx(tenant).await.unwrap();
        sqlx::query_scalar("select count(*) from (select entry_id from journal_line where tenant_id = $1 group by entry_id having sum(debit) <> sum(credit)) x")
            .bind(tenant)
            .fetch_one(&mut *tx)
            .await
            .unwrap()
    };
    assert_eq!(unbalanced, 0);
    let sb = t.report(&a, "stock_balance", &[("company", co.clone())]).await;
    assert_eq!(dec(&sb["totals"]["value"]), t.gl("Stock In Hand").await, "perpetual inventory: stock value equals the stock account");

    // A workspace without this package is untouched by its hooks.
    let plain_tenant = k
        .create_tenant(NewTenant {
            slug: "plainshop".into(),
            name: "Plain Shop".into(),
            admin_email: "admin@plain.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["selling".into(), "buying".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let ps = k.login("plainshop", "admin@plain.test", "Correct-Horse-9").await.expect("login");
    let pa = k.load_ctx(plain_tenant, ps.user, Some(ps.token_version), "it".into()).await.expect("ctx");
    let mut p = T { k: k.clone(), tenant: plain_tenant, admin: pa.clone(), company: String::new() };
    let pinr = p.find_one("Currency", json!({"code": "INR"})).await;
    let pco = uid(&p.insert(&pa, "Company", json!({"company_name": "Plain Shop", "abbr": "PS", "default_currency": pinr["id"]})).await);
    p.company = pco.to_string();
    let pstores = uid(&p.find_one("Warehouse", json!({"warehouse_name": "Stores", "company": pco.to_string()})).await);
    let (pg, pn) = (p.find_one("ItemGroup", json!({"group_name": "Products"})).await, p.find_one("Uom", json!({"uom_name": "Nos"})).await);
    let pitem = uid(&p
        .insert(&pa, "Item", json!({"item_code": "PLAIN", "item_name": "Plain", "item_group": pg["id"], "stock_uom": pn["id"], "standard_rate": "9"}))
        .await);
    p.receive(pitem, pstores, "5", "3", json!({})).await;
    let pcust = uid(&p.insert(&pa, "Customer", json!({"customer_name": "Walk-in"})).await);
    let pdn = p.insert(&pa, "DeliveryNote", json!({"company": p.company, "customer": pcust.to_string(), "items": [line(pitem, "2", pstores)]})).await;
    assert!(pdn.get("stk_packed_items").is_none(), "no overlay fields without the package");
    p.submit(&pa, "DeliveryNote", uid(&pdn)).await;
    assert_eq!(p.stock(pitem, pstores).await, (n("3"), n("9")));
}
