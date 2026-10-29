//! End-to-end procurement test against a real PostgreSQL (`FERRUM_PROCUREMENT_TEST_DATABASE_URL`,
//! default `postgres://ferrum:ferrum@localhost:5432/ferrum_procurement_test`). The database is reset.
//!
//! A manufacturer's store keeper requests goods; buyers order them from each item's preferred
//! supplier in two goes, over-order, cancel, stop and resume; receipts close the requests.
//! Transfers, issues and customer-provided goods move through stock entries. A request for
//! quotation is mailed to suppliers who answer on the signed page (tampered and expired links are
//! refused), quotations are compared, one expires, the other is ordered. Re-order levels raise
//! requests exactly once. A supplier with a poor history is scored, blocked, warned about and
//! released. Reports and the books are checked at the end.

use async_trait::async_trait;
use chrono::{Datelike, Duration, Months};
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, Mailer, SiteRequest, SiteResponse};
use ferrum_meta::Catalog;
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
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

fn rows(doc: &Doc, table: &str) -> Vec<Doc> {
    doc[table].as_array().unwrap().iter().map(|r| r.as_object().unwrap().clone()).collect()
}

/// Mail the kernel would send, captured.
#[derive(Default)]
struct Mails(Mutex<Vec<(String, String, String)>>);

#[async_trait]
impl Mailer for Mails {
    async fn send(&self, to: &str, subject: &str, html: &str) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().push((to.to_string(), subject.to_string(), html.to_string()));
        Ok(())
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
        let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), limit: Some(500), ..Default::default() };
        self.k.list(&self.admin, entity, &q).await.unwrap().rows
    }
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let found = self.find(entity, filters.clone()).await;
        assert_eq!(found.len(), 1, "{entity} {filters}");
        found[0].clone()
    }
    async fn sql_scalar<R>(&self, sql: &str) -> R
    where
        R: Send + Unpin + for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
    {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(self.tenant).fetch_one(&mut *tx).await.unwrap()
    }
    async fn exec(&self, sql: &str, id: Uuid, value: &str) {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string())).bind(self.tenant).bind(id).bind(value).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
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
    async fn upkeep(&self) {
        ferrum_procurement::maintenance(&self.k).await.expect("procurement upkeep");
    }
    async fn site(&self, method: &str, token: &str, query: &[(&str, &str)], content_type: &str, body: &str) -> SiteResponse {
        let site = self.k.hooks.site("procurement").expect("procurement site").clone();
        let mut headers = Map::new();
        if !content_type.is_empty() {
            headers.insert("content-type".into(), json!(content_type));
        }
        let req = SiteRequest {
            tenant: self.tenant,
            user: None,
            method: method.into(),
            path: format!("/rfq/{token}"),
            query: query.iter().map(|(k, v)| (k.to_string(), json!(v))).collect(),
            headers,
            body: body.as_bytes().to_vec(),
            client_ip: "203.0.113.7".into(),
        };
        site.handle(&self.k, req).await.expect("site")
    }
    async fn deliver_mail(&self) {
        while self.k.dispatch_outbox_once().await.expect("outbox") > 0 {}
    }
}

fn body(r: &SiteResponse) -> String {
    String::from_utf8(r.body.clone()).unwrap()
}

fn json_body(r: &SiteResponse) -> Value {
    serde_json::from_slice(&r.body).unwrap()
}

/// The material request line for `item`.
fn line_of(mr: &Doc, item: Uuid) -> Doc {
    rows(mr, "items").into_iter().find(|l| l["item"] == json!(item.to_string())).expect("line")
}

fn assert_refused<T: std::fmt::Debug>(r: Result<T, Error>, needle: &str) {
    let msg = match &r {
        Err(Error::Validation(errs)) => errs.iter().map(|e| e.message.clone()).collect::<Vec<_>>().join(" | "),
        Err(e) => e.to_string(),
        Ok(v) => panic!("expected a refusal containing '{needle}', got {v:?}"),
    };
    assert!(msg.contains(needle), "expected '{needle}' in: {msg}");
}

#[test]
fn procurement_end_to_end() {
    // Same runtime shape as the server: unoptimized async hook chains need more than 2 MiB.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_PROCUREMENT_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_procurement_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("platform packages");
    packages.extend(ferrum_procurement::packages().expect("procurement packages"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_procurement::register(&mut hooks);
    for source in ferrum_procurement::reports() {
        ferrum_domain::reports::add_source(source);
    }
    let mails = Arc::new(Mails::default());
    let settings = KernelSettings { secret_key: Some(SecretKey::new([7u8; 32])), ..Default::default() };
    let k = Kernel::boot(db, catalog, hooks, settings, Some(mails.clone() as Arc<dyn Mailer>)).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("partitions");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme Industries".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["procurement".into()],
            settings: Some(json!({"fiscal_year_start_month": 4})),
        })
        .await
        .expect("tenant");
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.expect("login");
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    let t = T { k: k.clone(), tenant, admin };
    let a = t.admin.clone();
    let today = chrono::Utc::now().date_naive();
    let day = |offset: i64| (today + Duration::days(offset)).to_string();

    // ───────────── masters ─────────────
    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let company = t.insert(&a, "Company", json!({"company_name": "Acme Industries", "abbr": "AI", "default_currency": inr["id"]})).await;
    let cid = uid(&company);
    let co = cid.to_string();
    let wh = |name: &'static str| {
        let t = &t;
        async move { uid(&t.find_one("Warehouse", json!({"warehouse_name": name, "company": cid.to_string()})).await) }
    };
    let (stores, finished) = (wh("Stores").await, wh("Finished Goods").await);
    let raw = t.find_one("ItemGroup", json!({"group_name": "Raw Material"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let boxes = t.find_one("Uom", json!({"uom_name": "Box"})).await;
    t.insert(&a, "ProcurementSettings", json!({"public_url": "https://erp.example.test/"})).await;
    let supplier = |name: &str, email: &str| json!({"supplier_name": name, "email": email});
    let steel = uid(&t.insert(&a, "Supplier", supplier("Steel Co", "sales@steel.test")).await);
    let fast = uid(&t.insert(&a, "Supplier", supplier("Fasteners Ltd", "quotes@fasteners.test")).await);
    let backup = uid(&t.insert(&a, "Supplier", supplier("Backup Supplies", "hello@backup.test")).await);
    let slow = uid(&t.insert(&a, "Supplier", supplier("Slow Metals", "info@slow.test")).await);
    let item = |code: &str, extra: Value| {
        let mut v = json!({"item_code": code, "item_name": code.to_lowercase(), "item_group": raw["id"], "stock_uom": nos["id"]});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    };
    let prefer = |list: &[(Uuid, i64)]| json!({"suppliers": list.iter().map(|(s, p)| json!({"supplier": s.to_string(), "priority": p})).collect::<Vec<_>>()});
    let bolt = uid(&t.insert(&a, "Item", item("BOLT", prefer(&[(steel, 1), (fast, 2)]))).await);
    let nut = uid(&t.insert(&a, "Item", item("NUT", prefer(&[(fast, 1)]))).await);
    let washer = uid(&t.insert(&a, "Item", item("WASHER", json!({}))).await);
    let paint = uid(&t.insert(&a, "Item", item("PAINT", json!({}))).await);
    let mould = uid(&t.insert(&a, "Item", item("MOULD", json!({}))).await);
    let rod = uid(&t.insert(&a, "Item", item("ROD", prefer(&[(slow, 1), (steel, 2)]))).await);
    let opening = t
        .insert(
            &a,
            "StockEntry",
            json!({"company": co, "purpose": "Opening Stock", "items": [{"item": paint.to_string(), "qty": "100", "basic_rate": "5", "target_warehouse": stores.to_string()}]}),
        )
        .await;
    t.submit(&a, "StockEntry", uid(&opening)).await;

    let keeper = t.user("keeper@acme.test", "Stock User").await;
    let buyer = t.user("buyer@acme.test", "Purchase User").await;
    let manager = t.user("manager@acme.test", "Purchase Manager").await;

    // ───────────── a purchase request ─────────────
    let mr = t
        .insert(
            &keeper,
            "MaterialRequest",
            json!({"company": co, "schedule_date": day(10), "set_warehouse": stores.to_string(), "items": [
                {"item": bolt.to_string(), "qty": "100"},
                {"item": nut.to_string(), "qty": "5", "uom": boxes["id"], "conversion_factor": "12"},
                {"item": washer.to_string(), "qty": "20"}]}),
        )
        .await;
    let mr_id = uid(&mr);
    assert_eq!(mr["status"], json!("Draft"));
    assert_eq!(mr["requested_by"], json!("keeper@acme.test"));
    assert_eq!(dec(&line_of(&mr, nut)["stock_qty"]), n("60"), "5 boxes of 12");
    assert_eq!(line_of(&mr, bolt)["uom"], nos["id"], "the stock unit by default");
    assert_eq!(line_of(&mr, washer)["warehouse"], json!(stores.to_string()));
    let bad = t.k.insert(&keeper, "MaterialRequest", obj(json!({"company": co, "schedule_date": day(-1), "transaction_date": day(0), "items": [{"item": bolt.to_string(), "qty": "1", "warehouse": stores.to_string()}]}))).await;
    assert_refused(bad, "Required By cannot be before");
    let mr = t.submit(&keeper, "MaterialRequest", mr_id).await;
    assert_eq!(mr["status"], json!("Pending"));
    assert!(mr["number"].as_str().unwrap().starts_with("MAT-REQ-"));
    assert!(
        t.k.run_action(&keeper, "MaterialRequest", mr_id, "make_stock_entry", json!({})).await.is_err(),
        "purchase requests are not moved by stock entries"
    );

    // Each line goes to its item's preferred supplier; the washer has none.
    assert_refused(t.k.run_action(&buyer, "MaterialRequest", mr_id, "make_purchase_order", json!({})).await, "no usable approved supplier for washer");
    let made = t.action(&buyer, "MaterialRequest", mr_id, "make_purchase_order", json!({"supplier": backup.to_string()})).await;
    let docs = made["documents"].as_array().unwrap();
    assert_eq!(docs.len(), 3, "{made}");
    let po_for = |sup: Uuid| vid(docs.iter().find(|d| d["supplier"] == json!(sup.to_string())).expect("po per supplier"));
    let (po_steel, po_fast, po_backup) = (po_for(steel), po_for(fast), po_for(backup));
    let po1 = t.get("PurchaseOrder", po_steel).await;
    let po1_line = rows(&po1, "items")[0].clone();
    assert_eq!((po1_line["item"].clone(), dec(&po1_line["qty"])), (json!(bolt.to_string()), n("100")));
    assert_eq!(po1_line["proc_material_request"], json!(mr_id.to_string()));
    assert_eq!(po1_line["proc_mr_line"], line_of(&mr, bolt)["id"]);
    assert_eq!(dec(&rows(&t.get("PurchaseOrder", po_fast).await, "items")[0]["qty"]), n("60"), "ordered in stock units");
    let tamper =
        t.k.update(&buyer, "PurchaseOrder", po_steel, obj(json!({"items": [{"id": po1_line["id"], "proc_mr_line": Uuid::now_v7().to_string()}]})), None).await;
    let po1 = t.get("PurchaseOrder", po_steel).await;
    assert!(tamper.is_ok() && rows(&po1, "items")[0]["proc_mr_line"] == po1_line["proc_mr_line"], "links are read-only to users");

    // Partial ordering across two orders.
    t.submit(&buyer, "PurchaseOrder", po_fast).await;
    t.submit(&buyer, "PurchaseOrder", po_backup).await;
    t.update(&buyer, "PurchaseOrder", po_steel, json!({"items": [{"id": po1_line["id"], "qty": "60"}]})).await;
    t.submit(&buyer, "PurchaseOrder", po_steel).await;
    let mr = t.get("MaterialRequest", mr_id).await;
    assert_eq!(dec(&line_of(&mr, bolt)["ordered_qty"]), n("60"));
    assert_eq!((mr["status"].clone(), dec(&mr["per_ordered"])), (json!("Partially Ordered"), n("77.78")), "140 of 180 units");
    let second = t.action(&buyer, "MaterialRequest", mr_id, "make_purchase_order", json!({})).await;
    assert_eq!(second["documents"].as_array().unwrap().len(), 1, "only the bolt is left");
    let po2 = vid(&second);
    assert_eq!(dec(&rows(&t.get("PurchaseOrder", po2).await, "items")[0]["qty"]), n("40"));
    t.submit(&buyer, "PurchaseOrder", po2).await;
    let mr = t.get("MaterialRequest", mr_id).await;
    assert_eq!((mr["status"].clone(), dec(&mr["per_ordered"])), (json!("Ordered"), n("100")));
    assert_refused(t.k.run_action(&buyer, "MaterialRequest", mr_id, "make_purchase_order", json!({})).await, "not available");
    assert_refused(t.k.cancel(&manager, "MaterialRequest", mr_id, None).await, "cancel the purchase orders");

    // Cancelling an order gives the quantity back to the request.
    t.cancel(&buyer, "PurchaseOrder", po2).await;
    let mr = t.get("MaterialRequest", mr_id).await;
    assert_eq!((mr["status"].clone(), dec(&line_of(&mr, bolt)["ordered_qty"])), (json!("Partially Ordered"), n("60")));

    // Over-ordering: two drafts for the same 40 units; a draft above the open quantity.
    let po_a = vid(&t.action(&buyer, "MaterialRequest", mr_id, "make_purchase_order", json!({})).await);
    let po_b = vid(&t.action(&buyer, "MaterialRequest", mr_id, "make_purchase_order", json!({})).await);
    let b_line = rows(&t.get("PurchaseOrder", po_b).await, "items")[0]["id"].clone();
    assert_refused(t.k.update(&buyer, "PurchaseOrder", po_b, obj(json!({"items": [{"id": b_line, "qty": "41"}]})), None).await, "while 100 was requested");
    t.submit(&buyer, "PurchaseOrder", po_a).await;
    assert_refused(t.k.submit(&buyer, "PurchaseOrder", po_b, None).await, "brings the quantity ordered against Material Request");
    // An allowance lets a little more through.
    let ps = t.find_one("ProcurementSettings", json!({})).await;
    t.update(&manager, "ProcurementSettings", uid(&ps), json!({"over_order_allowance": "10"})).await;
    t.update(&buyer, "PurchaseOrder", po_b, json!({"items": [{"id": b_line, "qty": "10"}]})).await;
    t.submit(&buyer, "PurchaseOrder", po_b).await;
    assert_eq!(dec(&line_of(&t.get("MaterialRequest", mr_id).await, bolt)["ordered_qty"]), n("110"));
    t.cancel(&buyer, "PurchaseOrder", po_b).await;
    t.update(&manager, "ProcurementSettings", uid(&ps), json!({"over_order_allowance": "0"})).await;
    let mr = t.get("MaterialRequest", mr_id).await;
    assert_eq!((mr["status"].clone(), dec(&line_of(&mr, bolt)["ordered_qty"])), (json!("Ordered"), n("100")));

    // A stopped request refuses new orders until resumed.
    t.cancel(&buyer, "PurchaseOrder", po_a).await;
    let po_x = vid(&t.action(&buyer, "MaterialRequest", mr_id, "make_purchase_order", json!({})).await);
    assert_eq!(t.action(&manager, "MaterialRequest", mr_id, "stop", json!({})).await["status"], json!("Stopped"));
    assert_refused(t.k.submit(&buyer, "PurchaseOrder", po_x, None).await, "is stopped");
    assert_refused(t.k.run_action(&buyer, "MaterialRequest", mr_id, "make_purchase_order", json!({})).await, "not available");
    assert_refused(t.k.run_action(&buyer, "MaterialRequest", mr_id, "make_rfq", json!({})).await, "not available");
    assert_eq!(t.action(&manager, "MaterialRequest", mr_id, "resume", json!({})).await["status"], json!("Partially Ordered"));
    t.submit(&buyer, "PurchaseOrder", po_x).await;
    assert_eq!(t.get("MaterialRequest", mr_id).await["status"], json!("Ordered"));

    // Receipts close the request; cancelling one reopens it.
    let mut receipts = Vec::new();
    for po in [po_steel, po_fast, po_backup, po_x] {
        let pr = vid(&t.action(&buyer, "PurchaseOrder", po, "make_purchase_receipt", json!({})).await);
        let pr_doc = t.get("PurchaseReceipt", pr).await;
        assert_eq!(rows(&pr_doc, "items")[0]["proc_material_request"], json!(mr_id.to_string()), "receipt lines inherit the request link");
        if po == po_steel {
            t.submit(&buyer, "PurchaseReceipt", pr).await;
            let mr = t.get("MaterialRequest", mr_id).await;
            assert_eq!((mr["status"].clone(), dec(&line_of(&mr, bolt)["received_qty"])), (json!("Partially Received"), n("60")));
        } else {
            t.submit(&buyer, "PurchaseReceipt", pr).await;
        }
        receipts.push(pr);
    }
    let mr = t.get("MaterialRequest", mr_id).await;
    assert_eq!((mr["status"].clone(), dec(&mr["per_received"])), (json!("Received"), n("100")));
    assert_eq!(t.stock(bolt, stores).await.0, n("100"));
    t.cancel(&buyer, "PurchaseReceipt", receipts[2]).await;
    let mr = t.get("MaterialRequest", mr_id).await;
    assert_eq!((mr["status"].clone(), dec(&line_of(&mr, washer)["received_qty"])), (json!("Partially Received"), n("0")));
    let again = t.k.amend(&buyer, "PurchaseReceipt", receipts[2]).await.expect("amend");
    t.submit(&buyer, "PurchaseReceipt", uid(&again)).await;
    assert_eq!(t.get("MaterialRequest", mr_id).await["status"], json!("Received"));

    // A purchase invoice that updates stock receives against the request too.
    let mr3 = t
        .insert(
            &keeper,
            "MaterialRequest",
            json!({"company": co, "schedule_date": day(6), "items": [{"item": nut.to_string(), "qty": "10", "warehouse": stores.to_string()}]}),
        )
        .await;
    t.submit(&keeper, "MaterialRequest", uid(&mr3)).await;
    let po3 = vid(&t.action(&buyer, "MaterialRequest", uid(&mr3), "make_purchase_order", json!({})).await);
    t.submit(&buyer, "PurchaseOrder", po3).await;
    let pi = vid(&t.action(&buyer, "PurchaseOrder", po3, "make_purchase_invoice", json!({})).await);
    t.update(&buyer, "PurchaseInvoice", pi, json!({"bill_no": "FL-1001", "update_stock": true})).await;
    t.submit(&manager, "PurchaseInvoice", pi).await;
    assert_eq!(t.get("MaterialRequest", uid(&mr3)).await["status"], json!("Received"));
    t.cancel(&manager, "PurchaseInvoice", pi).await;
    assert_eq!(t.get("MaterialRequest", uid(&mr3)).await["status"], json!("Ordered"));

    // ───────────── transfers, issues, customer-provided goods ─────────────
    let transfer = t
        .insert(
            &keeper,
            "MaterialRequest",
            json!({"company": co, "material_request_type": "Material Transfer", "schedule_date": day(3), "set_from_warehouse": stores.to_string(),
                   "items": [{"item": paint.to_string(), "qty": "30", "warehouse": finished.to_string()}]}),
        )
        .await;
    let transfer_id = uid(&transfer);
    assert_eq!(line_of(&transfer, paint)["from_warehouse"], json!(stores.to_string()));
    t.submit(&keeper, "MaterialRequest", transfer_id).await;
    assert_refused(t.k.run_action(&buyer, "MaterialRequest", transfer_id, "make_purchase_order", json!({})).await, "not available");
    let se1 = vid(&t.action(&keeper, "MaterialRequest", transfer_id, "make_stock_entry", json!({})).await);
    let se1_doc = t.get("StockEntry", se1).await;
    assert_eq!(se1_doc["purpose"], json!("Material Transfer"));
    let se_line = rows(&se1_doc, "items")[0].clone();
    assert_eq!((se_line["source_warehouse"].clone(), se_line["target_warehouse"].clone()), (json!(stores.to_string()), json!(finished.to_string())));
    assert_refused(t.k.update(&keeper, "StockEntry", se1, obj(json!({"items": [{"id": se_line["id"], "qty": "31"}]})), None).await, "while 30 was requested");
    t.update(&keeper, "StockEntry", se1, json!({"items": [{"id": se_line["id"], "qty": "20"}]})).await;
    t.submit(&keeper, "StockEntry", se1).await;
    let tr = t.get("MaterialRequest", transfer_id).await;
    assert_eq!((tr["status"].clone(), dec(&line_of(&tr, paint)["ordered_qty"])), (json!("Partially Received"), n("20")));
    let se2 = vid(&t.action(&keeper, "MaterialRequest", transfer_id, "make_stock_entry", json!({})).await);
    t.submit(&keeper, "StockEntry", se2).await;
    assert_eq!(t.get("MaterialRequest", transfer_id).await["status"], json!("Transferred"));
    assert_eq!((t.stock(paint, stores).await.0, t.stock(paint, finished).await.0), (n("70"), n("30")));
    t.cancel(&a, "StockEntry", se2).await;
    assert_eq!(t.get("MaterialRequest", transfer_id).await["status"], json!("Partially Received"));
    let se3 = vid(&t.action(&keeper, "MaterialRequest", transfer_id, "make_stock_entry", json!({})).await);
    t.submit(&keeper, "StockEntry", se3).await;
    assert_eq!(t.get("MaterialRequest", transfer_id).await["status"], json!("Transferred"));

    let issue = t
        .insert(&keeper, "MaterialRequest", json!({"company": co, "material_request_type": "Material Issue", "schedule_date": day(1), "items": [{"item": paint.to_string(), "qty": "15", "warehouse": stores.to_string()}]}))
        .await;
    t.submit(&keeper, "MaterialRequest", uid(&issue)).await;
    let se = vid(&t.action(&keeper, "MaterialRequest", uid(&issue), "make_stock_entry", json!({})).await);
    assert_eq!(t.get("StockEntry", se).await["purpose"], json!("Material Issue"));
    t.submit(&keeper, "StockEntry", se).await;
    assert_eq!(t.get("MaterialRequest", uid(&issue)).await["status"], json!("Issued"));
    assert_eq!(t.stock(paint, stores).await.0, n("55"));

    let customer = uid(&t.insert(&a, "Customer", json!({"customer_name": "Tooling Customer"})).await);
    let provided = t.k.insert(&keeper, "MaterialRequest", obj(json!({"company": co, "material_request_type": "Customer Provided", "schedule_date": day(5), "items": [{"item": mould.to_string(), "qty": "5", "warehouse": stores.to_string()}]}))).await;
    assert_refused(provided, "Customer is required");
    let provided = t
        .insert(
            &keeper,
            "MaterialRequest",
            json!({"company": co, "material_request_type": "Customer Provided", "customer": customer.to_string(), "schedule_date": day(5),
                   "items": [{"item": mould.to_string(), "qty": "5", "warehouse": stores.to_string()}]}),
        )
        .await;
    t.submit(&keeper, "MaterialRequest", uid(&provided)).await;
    let se = vid(&t.action(&keeper, "MaterialRequest", uid(&provided), "make_stock_entry", json!({})).await);
    let se_doc = t.get("StockEntry", se).await;
    assert_eq!((se_doc["purpose"].clone(), dec(&rows(&se_doc, "items")[0]["basic_rate"])), (json!("Material Receipt"), n("0")), "customer goods cost nothing");
    t.submit(&keeper, "StockEntry", se).await;
    assert_eq!(t.get("MaterialRequest", uid(&provided)).await["status"], json!("Received"));
    assert_eq!(t.stock(mould, stores).await, (n("5"), n("0")));

    // ───────────── request for quotation ─────────────
    let unaddressed = t.insert(&buyer, "RequestForQuotation", json!({"company": co, "items": [{"item": washer.to_string(), "qty": "5"}]})).await;
    assert_refused(t.k.submit(&buyer, "RequestForQuotation", uid(&unaddressed), None).await, "Invite at least one supplier");
    t.k.delete(&buyer, "RequestForQuotation", uid(&unaddressed), None).await.expect("drop the draft");
    let mr2 = t
        .insert(
            &keeper,
            "MaterialRequest",
            json!({"company": co, "schedule_date": day(20), "items": [{"item": bolt.to_string(), "qty": "200", "warehouse": stores.to_string()}]}),
        )
        .await;
    let mr2_id = uid(&mr2);
    t.submit(&keeper, "MaterialRequest", mr2_id).await;
    let rfq_id = vid(&t.action(&buyer, "MaterialRequest", mr2_id, "make_rfq", json!({})).await);
    let rfq = t.get("RequestForQuotation", rfq_id).await;
    let invited: Vec<Value> = rows(&rfq, "suppliers").iter().map(|r| r["supplier"].clone()).collect();
    assert_eq!(invited, vec![json!(steel.to_string()), json!(fast.to_string())], "the item's approved suppliers, in priority order");
    assert_eq!(rows(&rfq, "suppliers")[0]["email"], json!("sales@steel.test"), "email from the supplier");
    assert_eq!(rfq["response_deadline"], json!(day(7)), "default response window");
    let rfq_line = rows(&rfq, "items")[0].clone();
    assert_eq!((dec(&rfq_line["qty"]), rfq_line["material_request"].clone()), (n("200"), json!(mr2_id.to_string())));
    assert_refused(t.k.run_action(&buyer, "RequestForQuotation", rfq_id, "send", json!({})).await, "not available");
    t.submit(&buyer, "RequestForQuotation", rfq_id).await;
    let sent = t.action(&buyer, "RequestForQuotation", rfq_id, "send", json!({})).await;
    assert_eq!(sent["emailed"], json!(true));
    let links = sent["links"].as_array().unwrap().clone();
    assert_eq!(links.len(), 2);
    let link_of = |sup: Uuid| links.iter().find(|l| l["supplier"] == json!(sup.to_string())).unwrap()["link"].as_str().unwrap().to_string();
    let prefix = "https://erp.example.test/api/site/acme/procurement/rfq/";
    let (steel_link, fast_link) = (link_of(steel), link_of(fast));
    assert!(steel_link.starts_with(prefix), "{steel_link}");
    let (steel_token, fast_token) = (steel_link[prefix.len()..].to_string(), fast_link[prefix.len()..].to_string());
    assert_eq!(rows(&t.get("RequestForQuotation", rfq_id).await, "suppliers")[0]["email_status"], json!("Queued"));
    assert!(mails.0.lock().unwrap().is_empty(), "nothing is mailed inside the transaction");
    t.deliver_mail().await;
    {
        let sent_mail = mails.0.lock().unwrap();
        assert_eq!(sent_mail.len(), 2);
        let to_steel = sent_mail.iter().find(|m| m.0 == "sales@steel.test").expect("steel mailed");
        assert!(to_steel.1.contains("Request for Quotation RFQ-") && to_steel.1.contains("Acme Industries"), "{}", to_steel.1);
        assert!(to_steel.2.contains(&steel_link) && to_steel.2.contains("bolt"), "{}", to_steel.2);
    }
    t.deliver_mail().await;
    assert_eq!(mails.0.lock().unwrap().len(), 2, "never mailed twice");
    let rfq = t.get("RequestForQuotation", rfq_id).await;
    assert!(rows(&rfq, "suppliers").iter().all(|r| r["email_status"] == json!("Sent") && r["last_sent_at"].is_string()));

    // The supplier page.
    let page = t.site("GET", &steel_token, &[], "", "").await;
    assert_eq!(page.status, 200);
    let html = body(&page);
    assert!(html.contains("bolt") && html.contains(rfq["number"].as_str().unwrap()) && html.contains("Steel Co"), "{html}");
    assert!(page.headers.iter().any(|(k, v)| k == "referrer-policy" && v == "no-referrer"));
    let data = json_body(&t.site("GET", &steel_token, &[("format", "json")], "", "").await);
    let line = data["items"][0]["line"].as_str().unwrap().to_string();
    assert_eq!((data["currency"].clone(), data["quote_status"].clone()), (json!("INR"), json!("Pending")));
    // Tampered and expired links.
    let mut forged = steel_token.clone();
    let last = forged.pop().unwrap();
    forged.push(if last == '0' { '1' } else { '0' });
    assert_eq!(t.site("GET", &forged, &[], "", "").await.status, 403);
    let other_row = fast_token.split('.').nth(1).unwrap();
    let swapped = steel_token.replacen(steel_token.split('.').nth(1).unwrap(), other_row, 1);
    assert_eq!(t.site("POST", &swapped, &[], "application/json", "{\"decline\": true}").await.status, 403, "a token cannot be moved to another invitation");
    let key = {
        let mut tx = t.k.db.tenant_tx(tenant).await.unwrap();
        let k2 = ferrum_procurement::rfq::signing_key(&t.k, &t.admin, &mut tx, false).await.unwrap().unwrap();
        tx.rollback().await.unwrap();
        k2
    };
    let row_id: Uuid = rows(&rfq, "suppliers")[0]["id"].as_str().unwrap().parse().unwrap();
    let stale = ferrum_procurement::rfq::sign_link(&key, tenant, rfq_id, row_id, chrono::Utc::now().timestamp() - 60);
    let expired = t.site("GET", &stale, &[], "", "").await;
    assert_eq!(expired.status, 410);
    assert!(body(&expired).contains("expired"));
    assert_eq!(t.site("GET", &steel_token, &[], "", "").await.status, 200, "the real link still works");
    // Answers: steel in JSON, fasteners first declines, then quotes through the form.
    let answer =
        json!({"items": [{"line": line, "rate": "9.50", "lead_time_days": 5}], "valid_till": day(30), "supplier_reference": "SC-77", "notes": "Ex works"});
    let r = t.site("POST", &steel_token, &[], "application/json", &answer.to_string()).await;
    assert_eq!(r.status, 200, "{}", body(&r));
    let sq_steel: Uuid = json_body(&r)["supplier_quotation"].as_str().unwrap().parse().unwrap();
    let sq = t.get("SupplierQuotation", sq_steel).await;
    assert_eq!((sq["status"].clone(), sq["supplier"].clone(), sq["supplier_reference"].clone()), (json!("Draft"), json!(steel.to_string()), json!("SC-77")));
    let sq_line = rows(&sq, "items")[0].clone();
    assert_eq!((dec(&sq_line["price_list_rate"]), dec(&sq_line["amount"]), sq_line["lead_time_days"].clone()), (n("9.5"), n("1900"), json!(5)));
    assert_eq!(sq_line["mr_line"], line_of(&mr2, bolt)["id"]);
    let bad_rate = t.site("POST", &steel_token, &[], "application/json", &json!({"items": [{"line": line, "rate": "-3"}]}).to_string()).await;
    assert_eq!(bad_rate.status, 422);
    let r = t.site("POST", &fast_token, &[], "application/json", "{\"decline\": true}").await;
    assert_eq!(json_body(&r)["quote_status"], json!("No Quote"));
    let r = t.site("POST", &fast_token, &[], "application/x-www-form-urlencoded", &format!("rate_{line}=9.25&lead_{line}=10&action=quote")).await;
    assert_eq!(r.status, 200);
    assert!(body(&r).contains("Thank you"));
    let rfq = t.get("RequestForQuotation", rfq_id).await;
    let statuses: Vec<Value> = rows(&rfq, "suppliers").iter().map(|r| r["quote_status"].clone()).collect();
    assert_eq!(statuses, vec![json!("Received"), json!("Received")]);
    let sq_fast: Uuid = rows(&rfq, "suppliers")[1]["supplier_quotation"].as_str().unwrap().parse().unwrap();
    let answers_again = t.site("POST", &steel_token, &[], "application/json", &json!({"items": [{"line": line, "rate": "9.40"}]}).to_string()).await;
    assert_eq!(answers_again.status, 200, "a draft answer can be revised");
    assert_eq!(t.find("SupplierQuotation", json!({"supplier": steel.to_string()})).await.len(), 1, "revised in place");

    // Comparison: the lowest rate per item is highlighted.
    let cmp = t.report(&buyer, "supplier_quotation_comparison", &[("company", co.clone()), ("request_for_quotation", rfq_id.to_string())]).await;
    let row = &cmp["rows"][0];
    assert_eq!(
        (row["item_code"].clone(), dec(&row["lowest_rate"]), row["lowest_supplier"].clone(), row["quotes"].clone()),
        (json!("BOLT"), n("9.25"), json!("Fasteners Ltd"), json!(2))
    );
    let fast_col = cmp["meta"]["suppliers"].as_array().unwrap().iter().find(|s| s["supplier"] == json!(fast.to_string())).unwrap()["column"].clone();
    assert_eq!(row["_highlight"], json!([fast_col]));

    t.submit(&buyer, "SupplierQuotation", sq_steel).await;
    t.submit(&buyer, "SupplierQuotation", sq_fast).await;
    assert_eq!(t.site("GET", &steel_token, &[("format", "json")], "", "").await.status, 200);
    let locked = t.site("POST", &steel_token, &[], "application/json", &answer.to_string()).await;
    assert_eq!(locked.status, 409, "a submitted quotation is the buyer's");
    assert_refused(t.k.cancel(&buyer, "RequestForQuotation", rfq_id, None).await, "cancel the supplier quotations");

    // Answers taken by phone: a draft quotation from one invited supplier.
    assert_refused(t.k.run_action(&buyer, "RequestForQuotation", rfq_id, "make_supplier_quotation", json!({})).await, "choose which supplier quoted");
    assert_refused(
        t.k.run_action(&buyer, "RequestForQuotation", rfq_id, "make_supplier_quotation", json!({"supplier": backup.to_string()})).await,
        "was not invited",
    );
    let manual = vid(&t.action(&buyer, "RequestForQuotation", rfq_id, "make_supplier_quotation", json!({"supplier": steel.to_string()})).await);
    let manual_doc = t.get("SupplierQuotation", manual).await;
    assert_eq!(
        (manual_doc["request_for_quotation"].clone(), rows(&manual_doc, "items")[0]["mr_line"].clone()),
        (json!(rfq_id.to_string()), line_of(&mr2, bolt)["id"].clone())
    );
    let seen = json_body(&t.site("GET", &steel_token, &[("format", "json")], "", "").await);
    assert_eq!(seen["quotation"]["id"], json!(sq_steel.to_string()), "the supplier sees their own answer, never the buyer's draft");
    t.k.delete(&buyer, "SupplierQuotation", manual, None).await.expect("drop the draft");
    assert_eq!(
        rows(&t.get("RequestForQuotation", rfq_id).await, "suppliers")[0]["supplier_quotation"],
        json!(sq_steel.to_string()),
        "deleting another draft leaves the answer alone"
    );

    // An expired quotation refuses orders, even a draft made before it expired.
    let po_s = vid(&t.action(&buyer, "SupplierQuotation", sq_steel, "make_purchase_order", json!({})).await);
    t.exec("update e_supplier_quotation set valid_till = $3::date where tenant_id = $1 and id = $2", sq_steel, &day(-1)).await;
    t.upkeep().await;
    assert_eq!(t.get("SupplierQuotation", sq_steel).await["status"], json!("Expired"));
    assert_refused(t.k.submit(&buyer, "PurchaseOrder", po_s, None).await, "expired on");
    t.k.delete(&buyer, "PurchaseOrder", po_s, None).await.expect("drop the draft");
    assert_refused(t.k.run_action(&buyer, "SupplierQuotation", sq_steel, "make_purchase_order", json!({})).await, "not available");
    // The cheaper, valid one is ordered at its quoted rate and fulfils the request.
    let po_f = vid(&t.action(&buyer, "SupplierQuotation", sq_fast, "make_purchase_order", json!({})).await);
    let po_f_doc = t.get("PurchaseOrder", po_f).await;
    let l = rows(&po_f_doc, "items")[0].clone();
    assert_eq!((po_f_doc["supplier"].clone(), dec(&l["rate"]), dec(&l["qty"])), (json!(fast.to_string()), n("9.25"), n("200")));
    assert_eq!((l["proc_supplier_quotation"].clone(), l["proc_material_request"].clone()), (json!(sq_fast.to_string()), json!(mr2_id.to_string())));
    t.submit(&buyer, "PurchaseOrder", po_f).await;
    assert_eq!(
        (t.get("SupplierQuotation", sq_fast).await["status"].clone(), t.get("MaterialRequest", mr2_id).await["status"].clone()),
        (json!("Ordered"), json!("Ordered"))
    );
    assert_refused(t.k.cancel(&buyer, "SupplierQuotation", sq_fast, None).await, "cancel the purchase orders");
    let tracker = t.report(&buyer, "procurement_tracker", &[("company", co.clone()), ("material_request", mr2_id.to_string())]).await;
    let tr = &tracker["rows"][0];
    assert_eq!((tr["stage"].clone(), dec(&tr["best_quoted_rate"])), (json!("Ordered"), n("9.25")));
    assert!(tr["rfqs"].as_str().unwrap().starts_with("RFQ-") && tr["purchase_orders"].as_str().unwrap().starts_with("PO-"), "{tr}");
    assert_eq!(tr["supplier_quotations"].as_str().unwrap().split(", ").count(), 2);
    // Withdrawn requests close their links.
    t.cancel(&buyer, "PurchaseOrder", po_f).await;
    t.cancel(&buyer, "SupplierQuotation", sq_fast).await;
    t.cancel(&buyer, "SupplierQuotation", sq_steel).await;
    assert_eq!(
        rows(&t.get("RequestForQuotation", rfq_id).await, "suppliers")[1]["quote_status"],
        json!("Pending"),
        "a cancelled quotation reopens the invitation"
    );
    t.cancel(&buyer, "RequestForQuotation", rfq_id).await;
    assert_eq!(t.site("GET", &fast_token, &[], "", "").await.status, 410);

    // ───────────── auto-reorder ─────────────
    let gasket = uid(&t
        .insert(
            &a,
            "Item",
            item(
                "GASKET",
                json!({"suppliers": [{"supplier": steel.to_string()}], "lead_time_days": 4,
                "proc_reorder_levels": [{"warehouse": stores.to_string(), "reorder_level": "50", "reorder_qty": "100"}]}),
            ),
        )
        .await);
    let dup = t
        .k
        .insert(&a, "Item", obj(item("GASKET2", json!({"proc_reorder_levels": [{"warehouse": stores.to_string(), "reorder_level": "1", "reorder_qty": "1"}, {"warehouse": stores.to_string(), "reorder_level": "2", "reorder_qty": "1"}]}))))
        .await;
    assert_refused(dup, "has a re-order level already");
    let auto = || {
        let t = &t;
        async move { t.find("MaterialRequest", json!({"auto_reorder": true, "items.item": gasket.to_string()})).await }
    };
    t.upkeep().await;
    let raised = auto().await;
    assert_eq!(raised.len(), 1);
    let auto1 = t.get("MaterialRequest", uid(&raised[0])).await;
    assert_eq!((auto1["status"].clone(), auto1["material_request_type"].clone()), (json!("Pending"), json!("Purchase")));
    assert_eq!((dec(&line_of(&auto1, gasket)["qty"]), auto1["schedule_date"].clone()), (n("100"), json!(day(4))));
    t.upkeep().await;
    t.upkeep().await;
    assert_eq!(auto().await.len(), 1, "never two open requests");
    let po = vid(&t.action(&buyer, "MaterialRequest", uid(&auto1), "make_purchase_order", json!({})).await);
    t.submit(&buyer, "PurchaseOrder", po).await;
    t.upkeep().await;
    assert_eq!(auto().await.len(), 1, "ordered quantity counts as projected");
    let pr = vid(&t.action(&buyer, "PurchaseOrder", po, "make_purchase_receipt", json!({})).await);
    t.submit(&buyer, "PurchaseReceipt", pr).await;
    t.upkeep().await;
    assert_eq!(auto().await.len(), 1);
    let issue = t
        .insert(
            &keeper,
            "StockEntry",
            json!({"company": co, "purpose": "Material Issue", "items": [{"item": gasket.to_string(), "qty": "80", "source_warehouse": stores.to_string()}]}),
        )
        .await;
    t.submit(&keeper, "StockEntry", uid(&issue)).await;
    t.upkeep().await;
    let raised = auto().await;
    assert_eq!(raised.len(), 2, "20 on hand is below 50");
    let auto2 = raised.iter().find(|m| uid(m) != uid(&auto1)).unwrap().clone();
    assert_eq!(dec(&line_of(&t.get("MaterialRequest", uid(&auto2)).await, gasket)["qty"]), n("100"));
    t.action(&manager, "MaterialRequest", uid(&auto2), "stop", json!({})).await;
    t.upkeep().await;
    assert_eq!(auto().await.len(), 3, "a stopped request no longer covers the need");
    t.upkeep().await;
    assert_eq!(auto().await.len(), 3);
    // Reservations by sales orders lower the projection; transfers come from a source warehouse.
    let valve = uid(&t
        .insert(&a, "Item", item("VALVE", json!({"proc_reorder_levels": [{"warehouse": stores.to_string(), "reorder_level": "10", "reorder_qty": "5"}]})))
        .await);
    let oil = uid(&t
        .insert(
            &a,
            "Item",
            item("OIL", json!({"proc_reorder_levels": [{"warehouse": finished.to_string(), "reorder_level": "10", "reorder_qty": "20", "request_type": "Material Transfer", "from_warehouse": stores.to_string()}]})),
        )
        .await);
    let stock_in = t
        .insert(&a, "StockEntry", json!({"company": co, "purpose": "Material Receipt", "items": [{"item": valve.to_string(), "qty": "30", "basic_rate": "8", "target_warehouse": stores.to_string()}, {"item": oil.to_string(), "qty": "40", "basic_rate": "3", "target_warehouse": finished.to_string()}]}))
        .await;
    t.submit(&a, "StockEntry", uid(&stock_in)).await;
    t.upkeep().await;
    assert!(t.find("MaterialRequest", json!({"items.item": valve.to_string()})).await.is_empty(), "30 on hand is above 10");
    let so = t
        .insert(&a, "SalesOrder", json!({"company": co, "customer": customer.to_string(), "items": [{"item": valve.to_string(), "qty": "25", "warehouse": stores.to_string(), "price_list_rate": "20"}]}))
        .await;
    t.submit(&a, "SalesOrder", uid(&so)).await;
    t.upkeep().await;
    let valve_mr = t.find_one("MaterialRequest", json!({"items.item": valve.to_string()})).await;
    assert_eq!(dec(&line_of(&t.get("MaterialRequest", uid(&valve_mr)).await, valve)["qty"]), n("5"), "30 − 25 reserved = 5 projected: 5 more reach the level");
    let drain = t
        .insert(
            &a,
            "StockEntry",
            json!({"company": co, "purpose": "Material Issue", "items": [{"item": oil.to_string(), "qty": "35", "source_warehouse": finished.to_string()}]}),
        )
        .await;
    t.submit(&a, "StockEntry", uid(&drain)).await;
    t.upkeep().await;
    let oil_mr = t.get("MaterialRequest", uid(&t.find_one("MaterialRequest", json!({"items.item": oil.to_string()})).await)).await;
    assert_eq!(
        (oil_mr["material_request_type"].clone(), line_of(&oil_mr, oil)["from_warehouse"].clone()),
        (json!("Material Transfer"), json!(stores.to_string()))
    );
    t.update(&manager, "ProcurementSettings", uid(&ps), json!({"auto_reorder": false})).await;
    t.action(&manager, "MaterialRequest", uid(&oil_mr), "stop", json!({})).await;
    t.upkeep().await;
    assert_eq!(t.find("MaterialRequest", json!({"items.item": oil.to_string()})).await.len(), 1, "switched off");

    // ───────────── supplier scorecard ─────────────
    let m_start = today.with_day(1).unwrap().checked_sub_months(Months::new(2)).unwrap();
    let in_m = |d: i64| (m_start + Duration::days(d - 1)).to_string();
    let old_quote = t
        .insert(&buyer, "SupplierQuotation", json!({"company": co, "supplier": slow.to_string(), "posting_date": in_m(1), "items": [{"item": bolt.to_string(), "qty": "100", "price_list_rate": "10"}]}))
        .await;
    t.submit(&buyer, "SupplierQuotation", uid(&old_quote)).await;
    let late = t
        .insert(
            &buyer,
            "PurchaseOrder",
            json!({"company": co, "supplier": slow.to_string(), "posting_date": in_m(5),
                   "items": [{"item": bolt.to_string(), "qty": "100", "price_list_rate": "12", "warehouse": stores.to_string(), "schedule_date": in_m(10)}]}),
        )
        .await;
    t.submit(&buyer, "PurchaseOrder", uid(&late)).await;
    let pr = vid(&t.action(&buyer, "PurchaseOrder", uid(&late), "make_purchase_receipt", json!({})).await);
    let pr_line = rows(&t.get("PurchaseReceipt", pr).await, "items")[0]["id"].clone();
    t.update(&buyer, "PurchaseReceipt", pr, json!({"posting_date": in_m(15), "items": [{"id": pr_line, "qty": "80", "rejected_qty": "20"}]})).await;
    t.submit(&buyer, "PurchaseReceipt", pr).await;
    let ignored = t
        .insert(
            &buyer,
            "RequestForQuotation",
            json!({"company": co, "transaction_date": in_m(2), "response_deadline": in_m(7),
            "items": [{"item": bolt.to_string(), "qty": "10"}], "suppliers": [{"supplier": slow.to_string()}]}),
        )
        .await;
    t.submit(&buyer, "RequestForQuotation", uid(&ignored)).await;
    let standings = json!([
        {"standing_name": "Very Poor", "min_score": "0", "max_score": "40", "prevent_rfqs": true, "prevent_pos": true},
        {"standing_name": "Poor", "min_score": "40", "max_score": "60", "warn_rfqs": true, "warn_pos": true},
        {"standing_name": "Good", "min_score": "60", "max_score": "80"},
        {"standing_name": "Excellent", "min_score": "80", "max_score": "100"}]);
    let bad_card =
        t.k.insert(
            &manager,
            "SupplierScorecard",
            obj(json!({"supplier": slow.to_string(), "criteria": [{"criterion_name": "x", "weight": "50", "formula": "doc.on_time_pct"}]})),
        )
        .await;
    assert_refused(bad_card, "add up to 100");
    let card = t.insert(&manager, "SupplierScorecard", json!({"supplier": slow.to_string(), "start_date": m_start.to_string(), "standings": standings})).await;
    assert_eq!(rows(&card, "criteria").len(), 4, "default criteria");
    assert!(t.k.insert(&buyer, "SupplierScorecard", obj(json!({"supplier": fast.to_string()}))).await.is_err(), "buyers read scorecards, managers keep them");
    t.upkeep().await;
    let card = t.get("SupplierScorecard", uid(&card)).await;
    assert_eq!((dec(&card["current_score"]), card["current_standing"].clone(), card["prevent_pos"].clone()), (n("36"), json!("Very Poor"), json!(true)));
    let last_month_end = today.with_day(1).unwrap().pred_opt().unwrap();
    assert_eq!(card["scored_until"], json!(last_month_end.to_string()));
    let periods = t.find("SupplierScorecardPeriod", json!({"scorecard": uid(&card).to_string()})).await;
    assert_eq!(periods.len(), 2, "two ended months");
    let first = periods.iter().find(|p| p["period_start"] == json!(m_start.to_string())).unwrap();
    let metrics: Vec<Decimal> = ["on_time_pct", "quality_pct", "price_variance_pct", "response_rate_pct", "score"].iter().map(|f| dec(&first[*f])).collect();
    assert_eq!(metrics, vec![n("0"), n("80"), n("20"), n("0"), n("36")], "late, 20% rejected, 20% above its quote, silent");
    let quiet = periods.iter().find(|p| p["period_start"] != json!(m_start.to_string())).unwrap();
    assert_eq!((quiet["no_activity"].clone(), dec(&quiet["score"])), (json!(true), n("36")), "silence carries the score forward");
    t.upkeep().await;
    assert_eq!(t.find("SupplierScorecardPeriod", json!({})).await.len(), 2, "scoring is idempotent");
    let refreshed = t.action(&manager, "SupplierScorecard", uid(&card), "refresh", json!({})).await;
    assert_eq!(refreshed["periods_scored"], json!(0));

    // Blocked: no orders, no RFQs; the request's lines go to the next approved supplier.
    let po_slow = json!({"company": co, "supplier": slow.to_string(), "items": [{"item": rod.to_string(), "qty": "5", "price_list_rate": "3", "warehouse": stores.to_string()}]});
    assert_refused(t.k.insert(&buyer, "PurchaseOrder", obj(po_slow.clone())).await, "blocks new purchase orders");
    let rfq_slow = json!({"company": co, "items": [{"item": rod.to_string(), "qty": "5"}], "suppliers": [{"supplier": slow.to_string()}]});
    assert_refused(t.k.insert(&buyer, "RequestForQuotation", obj(rfq_slow.clone())).await, "blocks new requests for quotation");
    let rod_mr = t
        .insert(
            &keeper,
            "MaterialRequest",
            json!({"company": co, "schedule_date": day(9), "items": [{"item": rod.to_string(), "qty": "5", "warehouse": stores.to_string()}]}),
        )
        .await;
    t.submit(&keeper, "MaterialRequest", uid(&rod_mr)).await;
    let rod_po = t.action(&buyer, "MaterialRequest", uid(&rod_mr), "make_purchase_order", json!({})).await;
    assert_eq!(t.get("PurchaseOrder", vid(&rod_po)).await["supplier"], json!(steel.to_string()), "the blocked preferred supplier is skipped");
    // Softer standings: the same score now only warns.
    let softer = json!([
        {"standing_name": "Very Poor", "min_score": "0", "max_score": "30", "prevent_rfqs": true, "prevent_pos": true},
        {"standing_name": "Poor", "min_score": "30", "max_score": "60", "warn_rfqs": true, "warn_pos": true},
        {"standing_name": "Good", "min_score": "60", "max_score": "100"}]);
    let card = t.update(&manager, "SupplierScorecard", uid(&card), json!({"standings": softer})).await;
    assert_eq!((card["current_standing"].clone(), card["prevent_pos"].clone(), card["warn_pos"].clone()), (json!("Poor"), json!(false), json!(true)));
    let warned = t.insert(&buyer, "PurchaseOrder", po_slow.clone()).await;
    assert_eq!(warned["proc_supplier_standing"], json!("Poor (36)"));
    let warned_rfq = t.insert(&buyer, "RequestForQuotation", rfq_slow.clone()).await;
    assert_eq!(rows(&warned_rfq, "suppliers")[0]["scorecard_standing"], json!("Poor (36)"));
    let ranking = t.report(&buyer, "supplier_scorecard_ranking", &[]).await;
    assert_eq!(
        (ranking["rows"][0]["supplier"].clone(), ranking["rows"][0]["rank"].clone(), ranking["rows"][0]["effect"].clone()),
        (json!("Slow Metals"), json!(1), json!("Warns"))
    );
    t.update(&manager, "SupplierScorecard", uid(&card), json!({"disabled": true})).await;
    let released = t.update(&buyer, "PurchaseOrder", uid(&warned), json!({"items": [{"id": rows(&warned, "items")[0]["id"], "qty": "6"}]})).await;
    assert_eq!(released["proc_supplier_standing"], Value::Null, "a disabled scorecard neither warns nor blocks");

    // ───────────── reports and books ─────────────
    let status = t.report(&keeper, "material_request_status", &[("company", co.clone()), ("from_date", day(-1)), ("to_date", day(0))]).await;
    let gasket_row = status["rows"].as_array().unwrap().iter().find(|r| r["item_code"] == json!("GASKET")).expect("gasket pending");
    assert_eq!((dec(&gasket_row["pending_order_qty"]), gasket_row["requests"].clone()), (n("100"), json!(1)), "only the running request counts");
    assert!(status["rows"].as_array().unwrap().iter().all(|r| r["item_code"] != json!("WASHER")), "fulfilled lines drop out");
    let analytics = t.report(&manager, "purchase_analytics", &[("company", co.clone()), ("from_date", m_start.to_string()), ("to_date", day(0))]).await;
    let slow_row = analytics["rows"].as_array().unwrap().iter().find(|r| r["name"] == json!("Slow Metals")).expect("slow metals");
    assert_eq!(dec(&slow_row["total"]), n("1200"));
    assert_eq!(dec(&slow_row[&m_start.format("%Y-%m").to_string()]), n("1200"));
    let by_item = t
        .report(
            &manager,
            "purchase_analytics",
            &[
                ("company", co.clone()),
                ("from_date", m_start.to_string()),
                ("group_by", "Item".into()),
                ("value_or_qty", "Quantity".into()),
                ("based_on", "Purchase Receipt".into()),
            ],
        )
        .await;
    assert!(
        by_item["rows"].as_array().unwrap().iter().any(|r| r["name"].as_str().unwrap().starts_with("BOLT") && dec(&r["total"]) == n("180")),
        "{}",
        by_item["rows"]
    );
    let tb = t.report(&a, "trial_balance", &[("company", co.clone())]).await;
    assert_eq!(tb["meta"]["balanced"], json!(true));
    let sb = t.report(&a, "stock_balance", &[("company", co.clone())]).await;
    assert_eq!(dec(&sb["totals"]["value"]), t.gl("Stock In Hand").await, "stock value equals the stock account");
    let unbalanced: i64 = t
        .sql_scalar("select count(*) from (select entry_id from journal_line where tenant_id = $1 group by entry_id having sum(debit) <> sum(credit)) x")
        .await;
    assert_eq!(unbalanced, 0);
}
