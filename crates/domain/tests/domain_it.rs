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
    // Secret fields (the IRP credentials) need a key; the fake IRP listens on a local address.
    let settings = KernelSettings { secret_key: Some(ferrum_kernel::secret::SecretKey::new([5u8; 32])), webhooks_allow_private: true, ..Default::default() };
    let k = Kernel::boot(db, catalog, hooks, settings, None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("maintenance");
    (k, plugins)
}

// ───────────────────────────── a fake IRP ─────────────────────────────

/// The IRP's API as NIC documents it, on a local port: an RSA-encrypted sign-in returning a
/// session key under the caller's app key, AES-encrypted payloads and answers, RS256-signed QR
/// codes, duplicate-IRN answers for invoices it registered before, and refusals on request.
/// Started with headers of its own, it is a GSP passing NIC's API through: those headers are
/// required on every call, once each, and a call without them is refused in the GSP's own form.
mod fake_irp {
    use axum::extract::{Path, Request, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::middleware::{self, Next};
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as B64;
    use ferrum_domain::irp::{ecb_decrypt, ecb_encrypt};
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use rsa::pkcs8::{EncodePublicKey, LineEnding};
    use rsa::rand_core::{OsRng, RngCore};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub struct Books {
        pub sessions: HashMap<String, [u8; 32]>,
        /// Answers by document number, and by IRN.
        pub registered: HashMap<String, Value>,
        pub by_irn: HashMap<String, Value>,
        pub refuse_next: bool,
        pub sign_ins: usize,
        /// Calls carrying a session token it never issued.
        pub stale_tokens: usize,
        /// As a GSP: the headers it requires, and the paths that came with them.
        pub gsp: Vec<(String, String)>,
        pub through_gsp: Vec<String>,
    }

    pub struct Irp {
        pub url: String,
        pub public_pem: String,
        pub qr_pem: String,
        pub books: Arc<Mutex<Books>>,
        key: Arc<rsa::RsaPrivateKey>,
        qr_der: Arc<Vec<u8>>,
    }

    #[derive(Clone)]
    struct S {
        books: Arc<Mutex<Books>>,
        key: Arc<rsa::RsaPrivateKey>,
        qr_der: Arc<Vec<u8>>,
    }

    fn now_ist() -> String {
        (chrono::Utc::now() + chrono::Duration::minutes(330)).format("%Y-%m-%d %H:%M:%S").to_string()
    }

    fn sealed(sek: &[u8; 32], v: &Value) -> Json<Value> {
        Json(json!({"Status": 1, "Data": B64.encode(ecb_encrypt(sek, v.to_string().as_bytes()))}))
    }

    fn refused(code: &str, message: &str, info: Value) -> Json<Value> {
        Json(json!({"Status": 0, "ErrorDetails": [{"ErrorCode": code, "ErrorMessage": message}], "InfoDtls": info}))
    }

    fn session(s: &S, h: &HeaderMap) -> Option<[u8; 32]> {
        let token = h.get("authtoken")?.to_str().ok()?;
        let mut b = s.books.lock().unwrap();
        let sek = b.sessions.get(token).copied();
        b.stale_tokens += usize::from(sek.is_none());
        sek
    }

    /// As a GSP: its headers on every call, each once; refused without them as Azure API
    /// Management (in front of several GSPs) refuses.
    async fn gsp_gate(State(s): State<S>, req: Request, next: Next) -> Response {
        let required = s.books.lock().unwrap().gsp.clone();
        if !required.is_empty() {
            let once = |name: &str, value: &str| {
                let mut all = req.headers().get_all(name).iter();
                all.next().and_then(|v| v.to_str().ok()) == Some(value) && all.next().is_none()
            };
            if !required.iter().all(|(n, v)| once(n, v)) {
                return (StatusCode::UNAUTHORIZED, Json(json!({"statusCode": 401, "message": "Access denied due to missing subscription key."})))
                    .into_response();
            }
            s.books.lock().unwrap().through_gsp.push(req.uri().path().to_string());
        }
        next.run(req).await
    }

    fn open(sek: &[u8; 32], body: &Value) -> Value {
        let plain = ecb_decrypt(sek, &B64.decode(body["Data"].as_str().unwrap()).unwrap()).unwrap();
        serde_json::from_slice(&plain).unwrap()
    }

    /// What the IRP answers for an invoice it registers: IRN, acknowledgement and signed QR code.
    pub fn answer(qr_der: &[u8], seller: &str, buyer: &str, doc_no: &str) -> Value {
        let irn = hex::encode(Sha256::digest(format!("{seller}|{doc_no}").as_bytes()));
        let data = json!({"SellerGstin": seller, "BuyerGstin": buyer, "DocNo": doc_no, "DocTyp": "INV", "Irn": irn, "IrnDt": now_ist()}).to_string();
        let key = jsonwebtoken::EncodingKey::from_rsa_der(qr_der);
        let qr = jsonwebtoken::encode(&jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256), &json!({"data": data, "iss": "NIC"}), &key).unwrap();
        json!({"AckNo": 112_610_000_000_042_u64, "AckDt": now_ist(), "Irn": irn, "SignedInvoice": qr, "SignedQRCode": qr, "Status": "ACT"})
    }

    async fn auth(State(s): State<S>, h: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
        if h.get("client_id").is_none() || h.get("client_secret").is_none() {
            return refused("1001", "client credentials missing", Value::Null);
        }
        let raw = B64.decode(body["Data"].as_str().unwrap()).unwrap();
        let inner = s.key.decrypt(rsa::Pkcs1v15Encrypt, &raw).expect("encrypted with our public key");
        let creds: Value = serde_json::from_slice(&B64.decode(inner).unwrap()).unwrap();
        if creds["UserName"] != "ti_api" || creds["Password"] != "Api-Passw0rd" {
            return refused("1002", "Invalid UserName or Password", Value::Null);
        }
        let app_key: [u8; 32] = B64.decode(creds["AppKey"].as_str().unwrap()).unwrap().try_into().unwrap();
        let mut sek = [0u8; 32];
        OsRng.fill_bytes(&mut sek);
        let mut b = s.books.lock().unwrap();
        b.sign_ins += 1;
        let token = format!("token-{}", b.sign_ins);
        b.sessions.insert(token.clone(), sek);
        let expiry = (chrono::Utc::now() + chrono::Duration::minutes(330 + 360)).format("%Y-%m-%d %H:%M:%S").to_string();
        Json(
            json!({"Status": 1, "Data": {"ClientId": "x", "UserName": "ti_api", "AuthToken": token, "Sek": B64.encode(ecb_encrypt(&app_key, &sek)), "TokenExpiry": expiry}}),
        )
    }

    async fn invoice(State(s): State<S>, h: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
        let Some(sek) = session(&s, &h) else { return refused("1005", "Invalid Token", Value::Null) };
        let inv = open(&sek, &body);
        let (no, seller, buyer) =
            (inv["DocDtls"]["No"].as_str().unwrap().to_string(), inv["SellerDtls"]["Gstin"].as_str().unwrap(), inv["BuyerDtls"]["Gstin"].as_str().unwrap());
        assert_eq!(inv["Version"], "1.1", "schema 1.1");
        let mut b = s.books.lock().unwrap();
        if std::mem::take(&mut b.refuse_next) {
            return refused("2265", "Recipient GSTIN state code does not match the state code passed in recipient details", Value::Null);
        }
        if let Some(prior) = b.registered.get(&no) {
            let info = json!([{"InfCd": "DUPIRN", "Desc": {"AckNo": prior["AckNo"], "AckDt": prior["AckDt"], "Irn": prior["Irn"]}}]);
            return refused("2150", "Duplicate IRN", info);
        }
        let a = answer(&s.qr_der, seller, buyer, &no);
        b.registered.insert(no, a.clone());
        b.by_irn.insert(a["Irn"].as_str().unwrap().to_string(), a.clone());
        sealed(&sek, &a)
    }

    async fn by_irn(State(s): State<S>, h: HeaderMap, Path(irn): Path<String>) -> Json<Value> {
        let Some(sek) = session(&s, &h) else { return refused("1005", "Invalid Token", Value::Null) };
        match s.books.lock().unwrap().by_irn.get(&irn) {
            Some(a) => sealed(&sek, a),
            None => refused("2283", "IRN details are not found", Value::Null),
        }
    }

    async fn cancel(State(s): State<S>, h: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
        let Some(sek) = session(&s, &h) else { return refused("1005", "Invalid Token", Value::Null) };
        let c = open(&sek, &body);
        sealed(&sek, &json!({"Irn": c["Irn"], "CancelDate": now_ist()}))
    }

    async fn ewb(State(s): State<S>, h: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
        let Some(sek) = session(&s, &h) else { return refused("1005", "Invalid Token", Value::Null) };
        let e = open(&sek, &body);
        assert!(e["Irn"].as_str().is_some_and(|i| i.len() == 64) && e["Distance"].is_number(), "{e}");
        sealed(&sek, &json!({"EwbNo": 331_009_876_543_u64, "EwbDt": now_ist(), "EwbValidTill": "2026-12-31 23:59:00"}))
    }

    pub async fn start() -> Irp {
        start_with(&[]).await
    }

    /// A GSP in front of the IRP, requiring these headers (lower-case names).
    pub async fn start_with(gsp: &[(&str, &str)]) -> Irp {
        // NIC's key is 2048-bit: the sign-in payload does not fit a smaller one.
        let key = rsa::RsaPrivateKey::new(&mut OsRng, 2048).unwrap();
        let qr = rsa::RsaPrivateKey::new(&mut OsRng, 1024).unwrap();
        let public_pem = key.to_public_key().to_public_key_pem(LineEnding::LF).unwrap();
        let qr_pem = qr.to_public_key().to_public_key_pem(LineEnding::LF).unwrap();
        let qr_der = Arc::new(qr.to_pkcs1_der().unwrap().as_bytes().to_vec());
        let gsp = gsp.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect();
        let books = Arc::new(Mutex::new(Books { gsp, ..Books::default() }));
        let key = Arc::new(key);
        let state = S { books: books.clone(), key: key.clone(), qr_der: qr_der.clone() };
        let app = Router::new()
            .route("/eivital/v1.04/auth", post(auth))
            .route("/eicore/v1.03/Invoice", post(invoice))
            .route("/eicore/v1.03/Invoice/irn/{irn}", get(by_irn))
            .route("/eicore/v1.03/Invoice/Cancel", post(cancel))
            .route("/eiewb/v1.03/ewaybill", post(ewb))
            .layer(middleware::from_fn_with_state(state.clone(), gsp_gate))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Irp { url, public_pem, qr_pem, books, key, qr_der }
    }

    impl Irp {
        /// Registers an invoice as if an earlier answer had been lost on the way back.
        pub fn registered_before(&self, seller: &str, buyer: &str, doc_no: &str) -> String {
            let a = answer(&self.qr_der, seller, buyer, doc_no);
            let mut b = self.books.lock().unwrap();
            b.registered.insert(doc_no.to_string(), a.clone());
            b.by_irn.insert(a["Irn"].as_str().unwrap().to_string(), a.clone());
            a["Irn"].as_str().unwrap().to_string()
        }
        pub fn forged_qr(&self, seller: &str, buyer: &str, doc_no: &str) -> Value {
            let other = rsa::RsaPrivateKey::new(&mut OsRng, 1024).unwrap();
            let _ = &self.key;
            answer(other.to_pkcs1_der().unwrap().as_bytes(), seller, buyer, doc_no)
        }
    }
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

    // ───────────── GST e-invoice and e-way bill ─────────────
    let ein_doc = t
        .insert("SalesInvoice", json!({"company": cid.to_string(), "customer": globex.to_string(), "items": [{"item": widget.to_string(), "qty": "2"}]}))
        .await;
    let ein = uid(&ein_doc);
    t.submit("SalesInvoice", ein).await;
    let refused = t.k.run_action(&t.ctx, "SalesInvoice", ein, "e_invoice_json", json!({})).await;
    let Err(Error::Invalid(m)) = refused else { panic!("{refused:?}") };
    for part in ["PIN code", "needs an address", "city"] {
        assert!(m.contains(part), "{part}: {m}");
    }
    t.k.update(&t.ctx, "Company", cid, obj(json!({"in_address": "Plot 7, MIDC\nAndheri East", "in_city": "Mumbai", "in_pincode": "400093"})), None)
        .await
        .expect("company address");
    t.k.update(&t.ctx, "Customer", globex, obj(json!({"billing_address": "88 Residency Road", "in_city": "Bengaluru", "in_pincode": "560025"})), None)
        .await
        .expect("customer address");
    // India numbers Sales Invoices the way GST wants them: at most 16 characters.
    let number = t.get("SalesInvoice", ein).await["number"].as_str().unwrap().to_string();
    assert!(number.starts_with("SI/") && ferrum_domain::einvoice::valid_doc_no(&number), "{number}");
    let json_out = t.action("SalesInvoice", ein, "e_invoice_json", json!({})).await;
    let p = &json_out["payload"];
    assert_eq!((&p["TranDtls"]["SupTyp"], &p["DocDtls"]["No"], &p["BuyerDtls"]["Gstin"]), (&json!("B2B"), &json!(number), &json!(globex_gstin)));
    assert_eq!((&p["BuyerDtls"]["Pos"], &p["BuyerDtls"]["Pin"], &p["SellerDtls"]["Loc"]), (&json!("29"), &json!(560_025), &json!("Mumbai")));
    assert_eq!((&p["ItemList"][0]["HsnCd"], &p["ItemList"][0]["Unit"], &p["ItemList"][0]["GstRt"]), (&json!("8479"), &json!("NOS"), &json!(18.0)));
    assert_eq!((&p["ValDtls"]["AssVal"], &p["ValDtls"]["IgstVal"], &p["ValDtls"]["TotInvVal"]), (&json!(300.0), &json!(54.0), &json!(354.0)));
    assert!(json_out["file"]["content"].as_str().unwrap().trim_start().starts_with('['), "a list, as the portal's bulk upload takes");

    assert_eq!(t.action("SalesInvoice", ein, "request_e_invoice", json!({})).await["status"], json!("Pending"));
    let mut tx = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
    let queued: i64 = sqlx::query_scalar("select count(*) from outbox where tenant_id = $1 and topic = 'india.e_invoice.requested' and record_id = $2")
        .bind(t.ctx.tenant)
        .bind(ein)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(queued, 1, "for the GSP connector");
    let held = t.k.cancel(&t.ctx, "SalesInvoice", ein, None).await;
    assert!(matches!(held, Err(Error::Invalid(ref m)) if m.contains("e-invoice")), "{held:?}");

    // The IRP's answer, as a connector (or someone pasting the portal's JSON) records it. A code
    // claiming NIC is checked against NIC's published keys: one no published key signed is
    // refused when entered by hand. This IRP signs with its own key, which the company names.
    let irn = "a1".repeat(32);
    let ist = chrono::Utc::now() + chrono::Duration::minutes(330);
    let ack_at = ist.format("%Y-%m-%d %H:%M:%S").to_string();
    let irp_key = {
        use rsa::pkcs1::EncodeRsaPrivateKey;
        use rsa::pkcs8::{EncodePublicKey, LineEnding};
        let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 1024).unwrap();
        (key.to_pkcs1_der().unwrap().as_bytes().to_vec(), key.to_public_key().to_public_key_pem(LineEnding::LF).unwrap())
    };
    let qr_for = |irn: &str, doc_no: &str| {
        let data = json!({"Irn": irn, "DocNo": doc_no, "SellerGstin": "27AAPFU0939F1ZV", "BuyerGstin": globex_gstin}).to_string();
        let key = jsonwebtoken::EncodingKey::from_rsa_der(&irp_key.0);
        jsonwebtoken::encode(&jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256), &json!({"data": data, "iss": "NIC"}), &key).unwrap()
    };
    let answer = |irn: &str, doc_no: &str| json!({"response": json!({"data": {"Irn": irn, "AckNo": 112_010_036_563_310_u64, "AckDt": ack_at, "SignedQRCode": qr_for(irn, doc_no)}}).to_string()});
    let unvouched = t.k.run_action(&t.ctx, "SalesInvoice", ein, "record_e_invoice", answer(&irn, &number)).await;
    assert!(matches!(unvouched, Err(Error::Validation(ref f)) if f[0].message.contains("NIC key this release does not know")), "{unvouched:?}");
    t.k.update(&t.ctx, "Company", cid, obj(json!({"in_irp_qr_key": irp_key.1})), None).await.expect("the IRP's QR key");
    let wrong_doc = t.k.run_action(&t.ctx, "SalesInvoice", ein, "record_e_invoice", answer(&irn, "SI/1999-00/00099")).await;
    assert!(matches!(wrong_doc, Err(Error::Validation(ref f)) if f[0].message.contains("another invoice number")), "{wrong_doc:?}");
    assert_eq!(t.action("SalesInvoice", ein, "record_e_invoice", answer(&irn, &number)).await["status"], json!("Generated"));
    assert_eq!(t.action("SalesInvoice", ein, "record_e_invoice", answer(&irn, &number)).await["replayed"], json!(true));
    let other = t.k.run_action(&t.ctx, "SalesInvoice", ein, "record_e_invoice", answer(&"b2".repeat(32), &number)).await;
    assert!(matches!(other, Err(Error::Conflict(_))), "{other:?}");
    let doc = t.get("SalesInvoice", ein).await;
    assert_eq!((&doc["in_irn"], &doc["in_ack_no"], &doc["in_e_invoice_status"]), (&json!(irn), &json!("112010036563310"), &json!("Generated")));
    assert!(matches!(t.k.cancel(&t.ctx, "SalesInvoice", ein, None).await, Err(Error::Invalid(_))), "a live e-invoice holds the invoice");
    let printed = t.k.render_print(&t.ctx, "SalesInvoice", ein).await.expect("print");
    for part in ["Tax Invoice", number.rsplit('/').next().unwrap(), irn.as_str(), "<svg", "27AAPFU0939F1ZV", "Andheri East", "8479", "Bengaluru", "354.00"] {
        assert!(printed.contains(part), "{part} on the tax invoice");
    }

    // Transport details after invoicing, then the e-way bill by IRN.
    let no_vehicle = t.k.run_action(&t.ctx, "SalesInvoice", ein, "e_way_bill_json", json!({})).await;
    assert!(matches!(no_vehicle, Err(Error::Invalid(ref m)) if m.contains("vehicle")), "{no_vehicle:?}");
    let bad_mode = t.k.run_action(&t.ctx, "SalesInvoice", ein, "set_transport", json!({"in_transport_mode": "Teleport"})).await;
    assert!(matches!(bad_mode, Err(Error::Validation(_))), "{bad_mode:?}");
    t.action("SalesInvoice", ein, "set_transport", json!({"in_transport_mode": "Road", "in_distance_km": "320", "in_vehicle_no": "ka01ab1234"})).await;
    let ewb = t.action("SalesInvoice", ein, "e_way_bill_json", json!({})).await;
    assert_eq!(
        (&ewb["by_irn"], &ewb["payload"]["Irn"], &ewb["payload"]["Distance"], &ewb["payload"]["VehNo"]),
        (&json!(true), &json!(irn), &json!(320), &json!("KA01AB1234"))
    );
    let made =
        t.action("SalesInvoice", ein, "record_e_way_bill", json!({"EwbNo": "331001234567", "EwbDt": ack_at, "EwbValidTill": "2026-12-31 23:59:00"})).await;
    assert_eq!(made["status"], json!("Generated"));
    let first_ewb = t.k.run_action(&t.ctx, "SalesInvoice", ein, "cancel_e_invoice", json!({"reason": "Order cancelled", "remark": "Customer withdrew"})).await;
    assert!(matches!(first_ewb, Err(Error::Invalid(ref m)) if m.contains("e-way bill")), "{first_ewb:?}");

    // Cancelling: the e-way bill, the e-invoice, then the invoice itself.
    assert_eq!(
        t.action("SalesInvoice", ein, "cancel_e_way_bill", json!({"reason": "Order cancelled", "remark": "Customer withdrew"})).await["status"],
        json!("Cancelling")
    );
    assert_eq!(t.action("SalesInvoice", ein, "record_e_way_bill_cancellation", json!({})).await["status"], json!("Cancelled"));
    let cancel = t.action("SalesInvoice", ein, "cancel_e_invoice", json!({"reason": "Order cancelled", "remark": "Customer withdrew"})).await;
    assert_eq!((&cancel["status"], &cancel["payload"]["CnlRsn"], &cancel["payload"]["Irn"]), (&json!("Cancelling"), &json!("3"), &json!(irn)));
    assert_eq!(t.action("SalesInvoice", ein, "record_e_invoice_cancellation", json!({})).await["status"], json!("Cancelled"));
    t.k.cancel(&t.ctx, "SalesInvoice", ein, None).await.expect("cancelled once its e-invoice is");

    // ───────────── e-invoices through a direct IRP connection ─────────────
    let irp = fake_irp::start().await;
    t.k.update(
        &t.ctx,
        "Company",
        cid,
        obj(json!({"in_irp_mode": "Sandbox", "in_irp_url": irp.url, "in_irp_client_id": "TI-ERP", "in_irp_client_secret": "client-secret",
                   "in_irp_username": "ti_api", "in_irp_password": "Api-Passw0rd", "in_irp_public_key": irp.public_pem, "in_irp_qr_key": irp.qr_pem})),
        None,
    )
    .await
    .expect("IRP connection");
    let drain = || async { while t.k.dispatch_outbox_once().await.expect("dispatch") > 0 {} };
    let invoice_for_irp = || async {
        let d = t
            .insert("SalesInvoice", json!({"company": cid.to_string(), "customer": globex.to_string(), "items": [{"item": widget.to_string(), "qty": "1"}]}))
            .await;
        let id = uid(&d);
        let number = t.submit("SalesInvoice", id).await["number"].as_str().unwrap().to_string();
        (id, number)
    };
    let (sent, _) = invoice_for_irp().await;
    t.action("SalesInvoice", sent, "request_e_invoice", json!({})).await;
    drain().await;
    let doc = t.get("SalesInvoice", sent).await;
    assert_eq!((&doc["in_e_invoice_status"], &doc["in_ack_no"]), (&json!("Generated"), &json!("112610000000042")), "{:?}", doc.get("in_e_invoice_error"));
    assert_eq!(doc["in_irn"].as_str().unwrap().len(), 64);

    // An answer lost on the way back: the retry learns of the duplicate and fetches it by IRN.
    let (lost, lost_no) = invoice_for_irp().await;
    let irn = irp.registered_before("27AAPFU0939F1ZV", &globex_gstin, &lost_no);
    t.action("SalesInvoice", lost, "request_e_invoice", json!({})).await;
    drain().await;
    let doc = t.get("SalesInvoice", lost).await;
    assert_eq!((&doc["in_e_invoice_status"], &doc["in_irn"]), (&json!("Generated"), &json!(irn)));

    // The IRP's refusal is the request's failure, with its reason.
    let (refused, _) = invoice_for_irp().await;
    irp.books.lock().unwrap().refuse_next = true;
    t.action("SalesInvoice", refused, "request_e_invoice", json!({})).await;
    drain().await;
    let doc = t.get("SalesInvoice", refused).await;
    assert_eq!(doc["in_e_invoice_status"], json!("Failed"));
    assert!(doc["in_e_invoice_error"].as_str().unwrap().contains("2265"), "{:?}", doc["in_e_invoice_error"]);
    assert_eq!(irp.books.lock().unwrap().sign_ins, 1, "one session for all of it");

    // A cancellation, and an e-way bill by IRN, through the same connection.
    t.action("SalesInvoice", sent, "cancel_e_invoice", json!({"reason": "Data entry mistake", "remark": "Wrong rate"})).await;
    drain().await;
    assert_eq!(t.get("SalesInvoice", sent).await["in_e_invoice_status"], json!("Cancelled"));
    t.action("SalesInvoice", lost, "set_transport", json!({"in_transport_mode": "Road", "in_distance_km": 120, "in_vehicle_no": "KA01AB1234"})).await;
    t.action("SalesInvoice", lost, "request_e_way_bill", json!({})).await;
    drain().await;
    let doc = t.get("SalesInvoice", lost).await;
    assert_eq!((&doc["in_e_way_bill_status"], &doc["in_ewaybill_no"]), (&json!("Generated"), &json!("331009876543")));

    // With the IRP's signing key set, an answer signed by another key is refused when recorded by hand.
    let (by_hand, by_hand_no) = invoice_for_irp().await;
    let forged = irp.forged_qr("27AAPFU0939F1ZV", &globex_gstin, &by_hand_no);
    let refused_qr = t.k.run_action(&t.ctx, "SalesInvoice", by_hand, "record_e_invoice", json!({"response": forged.to_string()})).await;
    assert!(matches!(refused_qr, Err(Error::Validation(ref f)) if f[0].message.contains("signature")), "{refused_qr:?}");
    // Undone again: the e-way bill (cancelled on its portal, recorded here), then the e-invoices
    // (through the connection), then the invoices.
    t.action("SalesInvoice", lost, "cancel_e_way_bill", json!({"reason": "Order cancelled", "remark": "Withdrawn"})).await;
    drain().await;
    assert_eq!(t.get("SalesInvoice", lost).await["in_e_way_bill_status"], json!("Cancelling"), "the IRP does not cancel e-way bills");
    t.action("SalesInvoice", lost, "record_e_way_bill_cancellation", json!({})).await;
    t.action("SalesInvoice", lost, "cancel_e_invoice", json!({"reason": "Order cancelled", "remark": "Withdrawn"})).await;
    drain().await;
    for inv in [sent, lost, refused, by_hand] {
        t.k.cancel(&t.ctx, "SalesInvoice", inv, None).await.expect("cancelled");
    }

    // ───────────── the same through a GSP in front of the IRP ─────────────
    // The GSP's own credentials (the company's GSP headers) go on every call, each once, also one
    // the IRP takes too (gstin); the new address signs in afresh rather than reusing the old token.
    let gsp = fake_irp::start_with(&[("ocp-apim-subscription-key", "gsp-sub-key"), ("x-gsp-user", "ti-erp"), ("gstin", "27AAPFU0939F1ZV")]).await;
    t.k.update(
        &t.ctx,
        "Company",
        cid,
        obj(json!({"in_irp_url": gsp.url, "in_irp_public_key": gsp.public_pem, "in_irp_qr_key": gsp.qr_pem,
                   "in_irp_gsp_headers": json!({"Ocp-Apim-Subscription-Key": "gsp-sub-key", "X-GSP-User": "ti-erp", "GSTIN": "27AAPFU0939F1ZV"}).to_string()})),
        None,
    )
    .await
    .expect("through a GSP");
    let (via_gsp, _) = invoice_for_irp().await;
    t.action("SalesInvoice", via_gsp, "request_e_invoice", json!({})).await;
    drain().await;
    let doc = t.get("SalesInvoice", via_gsp).await;
    assert_eq!(doc["in_e_invoice_status"], json!("Generated"), "{:?}", doc.get("in_e_invoice_error"));
    let (lost_at_gsp, lost_at_gsp_no) = invoice_for_irp().await;
    let irn = gsp.registered_before("27AAPFU0939F1ZV", &globex_gstin, &lost_at_gsp_no);
    t.action("SalesInvoice", lost_at_gsp, "request_e_invoice", json!({})).await;
    drain().await;
    assert_eq!(t.get("SalesInvoice", lost_at_gsp).await["in_irn"], json!(irn), "fetched by IRN through the GSP");
    t.action("SalesInvoice", lost_at_gsp, "set_transport", json!({"in_transport_mode": "Road", "in_distance_km": 120, "in_vehicle_no": "KA01AB1234"})).await;
    t.action("SalesInvoice", lost_at_gsp, "request_e_way_bill", json!({})).await;
    drain().await;
    assert_eq!(t.get("SalesInvoice", lost_at_gsp).await["in_e_way_bill_status"], json!("Generated"));
    t.action("SalesInvoice", via_gsp, "cancel_e_invoice", json!({"reason": "Data entry mistake", "remark": "Wrong rate"})).await;
    drain().await;
    assert_eq!(t.get("SalesInvoice", via_gsp).await["in_e_invoice_status"], json!("Cancelled"));
    {
        let b = gsp.books.lock().unwrap();
        let through = |path: &str| b.through_gsp.iter().any(|p| p == path || (path.ends_with('/') && p.starts_with(path)));
        for path in ["/eivital/v1.04/auth", "/eicore/v1.03/Invoice", "/eicore/v1.03/Invoice/irn/", "/eicore/v1.03/Invoice/Cancel", "/eiewb/v1.03/ewaybill"] {
            assert!(through(path), "{path} without the GSP's headers: {:?}", b.through_gsp);
        }
        assert_eq!((b.sign_ins, b.stale_tokens), (1, 0), "one fresh session at the new address");
    }
    // Undone: the e-way bill (cancelled on its portal, recorded here), the e-invoice through the GSP.
    t.action("SalesInvoice", lost_at_gsp, "cancel_e_way_bill", json!({"reason": "Order cancelled", "remark": "Withdrawn"})).await;
    t.action("SalesInvoice", lost_at_gsp, "record_e_way_bill_cancellation", json!({})).await;
    t.action("SalesInvoice", lost_at_gsp, "cancel_e_invoice", json!({"reason": "Order cancelled", "remark": "Withdrawn"})).await;
    drain().await;
    assert_eq!(t.get("SalesInvoice", lost_at_gsp).await["in_e_invoice_status"], json!("Cancelled"));
    // The GSP's refusal without its credentials (in its own form, not NIC's) is the request's
    // failure, with its status and words; so is a header that cannot be sent, rather than a retry.
    let cases: [(Value, &[&str]); 3] = [
        (json!({"X-GSP-User": "ti-erp"}), &["401", "subscription key"]),
        (json!({"Bad Header": "x"}), &["Bad Header", "not a valid HTTP header"]),
        (json!({"X-Retries": {"n": 1}}), &["X-Retries", "text value"]),
    ];
    for (headers, said) in cases {
        t.k.update(&t.ctx, "Company", cid, obj(json!({"in_irp_gsp_headers": headers.to_string()})), None).await.expect("GSP headers");
        let (inv, _) = invoice_for_irp().await;
        t.action("SalesInvoice", inv, "request_e_invoice", json!({})).await;
        drain().await;
        let doc = t.get("SalesInvoice", inv).await;
        let error = doc["in_e_invoice_error"].as_str().unwrap_or_default();
        assert!(doc["in_e_invoice_status"] == json!("Failed") && said.iter().all(|s| error.contains(s)), "{said:?}: {error}");
        t.k.cancel(&t.ctx, "SalesInvoice", inv, None).await.expect("cancelled");
    }
    for inv in [via_gsp, lost_at_gsp] {
        t.k.cancel(&t.ctx, "SalesInvoice", inv, None).await.expect("cancelled");
    }
    t.k.update(&t.ctx, "Company", cid, obj(json!({"in_irp_mode": "Off"})), None).await.expect("connection off");

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
    // Paying with tax withheld: 167 leaves the bank, 10 is owed to the tax office, all 177 is settled.
    let bank_before = t.gl("Primary Bank").await;
    let tds = t.find_one("Account", json!({"account_name": "TDS Payable"})).await;
    let pay = t
        .insert(
            "PaymentEntry",
            json!({"company": cid.to_string(), "payment_type": "Pay", "supplier": acme.to_string(), "bank_account": bank, "currency": inr["id"], "paid_amount": "167",
                   "deductions": [{"account": tds["id"], "amount": "-10", "remarks": "TDS u/s 194C"}],
                   "references": [{"invoice_type": "Purchase Invoice", "invoice": bill["id"], "allocated_amount": "177"}]}),
        )
        .await;
    assert_eq!((dec(&pay["total_deductions"]), dec(&pay["unallocated_amount"])), (n("-10"), Decimal::ZERO));
    let over = t.k.update(&t.ctx, "PaymentEntry", uid(&pay), obj(json!({"paid_amount": "150"})), None).await;
    assert!(matches!(over, Err(Error::Validation(ref e)) if e.iter().any(|f| f.field == "references")), "over-allocation: {over:?}");
    t.submit("PaymentEntry", uid(&pay)).await;
    assert_eq!(dec(&t.get("PurchaseInvoice", uid(&bill)).await["outstanding_amount"]), Decimal::ZERO);
    assert_eq!(t.gl("Creditors").await, creditors_before);
    assert_eq!(t.gl("TDS Payable").await, n("-10"));
    assert_eq!(t.gl("Primary Bank").await - bank_before, n("-167"));
    // An opening balance entered as a journal entry line is offered and paid like a bill.
    let creditors = t.find_one("Account", json!({"account_name": "Creditors"})).await;
    let equity = t.find_one("Account", json!({"account_name": "Opening Balance Equity"})).await;
    let opening = t
        .insert(
            "JournalEntry",
            json!({"company": cid.to_string(), "accounts": [{"account": equity["id"], "debit": "500"},
                   {"account": creditors["id"], "credit": "500", "party_type": "Supplier", "supplier": acme.to_string()}]}),
        )
        .await;
    let opening = t.submit("JournalEntry", uid(&opening)).await;
    let line = opening["accounts"].as_array().unwrap().iter().find(|l| l["party_type"] == json!("Supplier")).unwrap()["id"].clone();
    let offer = t
        .insert(
            "PaymentEntry",
            json!({"company": cid.to_string(), "payment_type": "Pay", "supplier": acme.to_string(), "bank_account": bank, "currency": inr["id"], "paid_amount": "1000000"}),
        )
        .await;
    let offered = t.action("PaymentEntry", uid(&offer), "get_outstanding_invoices", json!({})).await;
    let row = offered["references"].as_array().unwrap().iter().find(|r| r["invoice"] == line).cloned().expect("the opening balance is offered");
    assert_eq!((row["invoice_type"].clone(), dec(&row["outstanding"])), (json!("Journal Entry"), n("500")));
    t.k.delete(&t.ctx, "PaymentEntry", uid(&offer), None).await.expect("discard the draft");
    let pay = t
        .insert(
            "PaymentEntry",
            json!({"company": cid.to_string(), "payment_type": "Pay", "supplier": acme.to_string(), "bank_account": bank, "currency": inr["id"], "paid_amount": "500",
                   "references": [{"invoice_type": "Purchase Invoice", "invoice": line, "allocated_amount": "500"}]}),
        )
        .await;
    assert_eq!(pay["references"][0]["invoice_type"], json!("Journal Entry"), "the type comes from the open item, not the request");
    t.submit("PaymentEntry", uid(&pay)).await;
    assert_eq!(t.gl("Creditors").await, creditors_before, "the opening balance is settled");

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
    let charges = t.find_one("Account", json!({"account_name": "Bank Charges"})).await;
    let paid = t
        .insert(
            "PaymentEntry",
            json!({"company": cid.to_string(), "payment_type": "Receive", "customer": abroad.to_string(), "bank_account": bank, "currency": usd["id"],
                   "exchange_rate": "84", "paid_amount": (usd_total - n("5")).to_string(),
                   "deductions": [{"account": charges["id"], "amount": "5", "remarks": "Correspondent bank fee"}],
                   "references": [{"invoice_type": "Sales Invoice", "invoice": export["id"], "allocated_amount": usd_total.to_string()}]}),
        )
        .await;
    t.submit("PaymentEntry", uid(&paid)).await;
    assert_eq!(dec(&t.get("SalesInvoice", uid(&export)).await["outstanding_amount"]), Decimal::ZERO);
    assert_eq!(t.gl("Bank Charges").await, n("420"), "5 dollars of charges at 84");
    let gain = (usd_total * n("84")).round_dp(2) - inr_total;
    assert_eq!(t.gl("Exchange Gain or Loss").await - gain_before, -gain, "the extra rupees are an exchange gain");
    // An import bill booked at 83 and paid at 84 costs more rupees: an exchange loss.
    let import = t
        .insert(
            "PurchaseInvoice",
            json!({"company": cid.to_string(), "supplier": acme.to_string(), "bill_no": "ACME/USD-1", "currency": usd["id"], "update_stock": true,
                   "items": [{"item": raw.to_string(), "qty": "1", "price_list_rate": "10", "warehouse": stores.to_string()}]}),
        )
        .await;
    assert_eq!(dec(&import["conversion_rate"]), n("83"));
    let import = t.submit("PurchaseInvoice", uid(&import)).await;
    let usd_bill = dec(&import["grand_total"]);
    let loss_before = t.gl("Exchange Gain or Loss").await;
    let settle = t
        .insert(
            "PaymentEntry",
            json!({"company": cid.to_string(), "payment_type": "Pay", "supplier": acme.to_string(), "bank_account": bank, "currency": usd["id"],
                   "exchange_rate": "84", "paid_amount": usd_bill.to_string(),
                   "references": [{"invoice_type": "Purchase Invoice", "invoice": import["id"], "allocated_amount": usd_bill.to_string()}]}),
        )
        .await;
    t.submit("PaymentEntry", uid(&settle)).await;
    assert_eq!(dec(&t.get("PurchaseInvoice", uid(&import)).await["outstanding_amount"]), Decimal::ZERO);
    let loss = (usd_bill * n("84")).round_dp(2) - dec(&import["base_grand_total"]);
    assert!(loss > Decimal::ZERO);
    assert_eq!(t.gl("Exchange Gain or Loss").await - loss_before, loss, "the extra rupees paid are an exchange loss");

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
    // Someone who may read only their own invoices gets no report that sums up everyone's.
    let own: ferrum_meta::types::PermissionDef = serde_json::from_value(json!({"entity": "SalesInvoice", "read": true, "if_owner": true})).unwrap();
    t.k.set_role_permission(&t.ctx, "Own Invoices", &own).await.expect("role");
    let rep = t.k.create_user(&t.ctx, "rep@reports.test", "Field Rep", Some("Correct-Horse-9"), &["Own Invoices".into()], false).await.unwrap();
    let rep_ctx = t.k.load_ctx(tenant, rep, None, "it".into()).await.unwrap();
    assert!(!names(ferrum_domain::reports::catalog_for(&rep_ctx)).contains(&"sales_register".to_string()));
    let params: HashMap<String, String> = company_param.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    let refused = ferrum_domain::reports::run(&t.k, &rep_ctx, "sales_register", &params).await;
    assert!(matches!(refused, Err(Error::Forbidden(_))), "own-records access cannot sum up everyone's invoices: {refused:?}");
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

    // ───────────── units: sold and bought by the box, stocked by the piece ─────────────
    let boxes = t.find_one("Uom", json!({"uom_name": "Box"})).await;
    let kg = t.find_one("Uom", json!({"uom_name": "Kg"})).await;
    let twice =
        t.k.insert(
            &t.ctx,
            "Item",
            obj(json!({"item_code": "BOLT-2", "item_name": "Bolt 2", "item_group": products["id"], "stock_uom": nos["id"],
                       "uoms": [{"uom": boxes["id"], "conversion_factor": "12"}, {"uom": boxes["id"], "conversion_factor": "10"}]})),
        )
        .await;
    assert!(matches!(twice, Err(Error::Validation(ref e)) if e.iter().any(|f| f.field == "uom")), "{twice:?}");
    let bolt = uid(&t
        .insert(
            "Item",
            json!({"item_code": "BOLT", "item_name": "Bolt", "item_group": products["id"], "stock_uom": nos["id"], "hsn_code": "7318", "standard_rate": "10",
                   "uoms": [{"uom": boxes["id"], "conversion_factor": "12"}]}),
        )
        .await);
    let opening = t
        .insert(
            "StockEntry",
            json!({"company": cid.to_string(), "purpose": "Opening Stock", "items": [{"item": bolt.to_string(), "qty": "100", "basic_rate": "4", "target_warehouse": stores.to_string()}]}),
        )
        .await;
    t.submit("StockEntry", uid(&opening)).await;
    let line = |v: Value| json!({"company": cid.to_string(), "customer": globex.to_string(), "set_warehouse": stores.to_string(), "items": [v]});
    let in_kg = t.k.insert(&t.ctx, "SalesOrder", obj(line(json!({"item": bolt.to_string(), "qty": "1", "uom": kg["id"]})))).await;
    assert!(matches!(in_kg, Err(Error::Validation(ref e)) if e.iter().any(|f| f.field == "uom")), "no conversion from Kg: {in_kg:?}");
    let so = t.insert("SalesOrder", line(json!({"item": bolt.to_string(), "qty": "2", "uom": boxes["id"]}))).await;
    let l = &so["items"][0];
    assert_eq!(
        (dec(&l["conversion_factor"]), dec(&l["stock_qty"]), dec(&l["price_list_rate"])),
        (n("12"), n("24"), n("120")),
        "a box is 12 pieces at the piece price"
    );
    let so_id = uid(&so);
    t.submit("SalesOrder", so_id).await;
    assert_eq!(t.reserved(bolt, stores).await, n("24"), "reserved in pieces");
    // Half a box delivered as 6 pieces: the box price becomes a piece price, the order counts boxes.
    let dn_id = vid(&t.action("SalesOrder", so_id, "make_delivery_note", json!({})).await);
    let dn = t.get("DeliveryNote", dn_id).await;
    assert_eq!(dn["items"][0]["uom"], boxes["id"], "the order's unit carries over");
    let pieces = json!({"items": [{"id": dn["items"][0]["id"], "item": bolt.to_string(), "uom": nos["id"], "qty": "6", "warehouse": stores.to_string()}]});
    let dn = t.k.update(&t.ctx, "DeliveryNote", dn_id, obj(pieces), None).await.expect("deliver in pieces");
    assert_eq!((dec(&dn["items"][0]["price_list_rate"]), dec(&dn["items"][0]["stock_qty"])), (n("10"), n("6")));
    t.submit("DeliveryNote", dn_id).await;
    assert_eq!(t.stock(bolt, stores).await, (n("94"), n("376")));
    assert_eq!(dec(&t.get("SalesOrder", so_id).await["items"][0]["delivered_qty"]), n("0.5"), "6 pieces are half a box");
    assert_eq!(t.reserved(bolt, stores).await, n("18"));
    let rest = vid(&t.action("SalesOrder", so_id, "make_delivery_note", json!({})).await);
    let rest_doc = t.get("DeliveryNote", rest).await;
    assert_eq!((dec(&rest_doc["items"][0]["qty"]), dec(&rest_doc["items"][0]["stock_qty"])), (n("1.5"), n("18")));
    t.submit("DeliveryNote", rest).await;
    assert_eq!(t.stock(bolt, stores).await.0, n("76"));
    assert_eq!(t.reserved(bolt, stores).await, Decimal::ZERO);
    assert_eq!(t.get("SalesOrder", so_id).await["status"], json!("To Bill"));
    let si = vid(&t.action("SalesOrder", so_id, "make_sales_invoice", json!({})).await);
    assert_eq!(dec(&t.submit("SalesInvoice", si).await["net_total"]), n("240"));
    assert_eq!(t.get("SalesOrder", so_id).await["status"], json!("Completed"));
    // Bought by the box at 36: stock and valuation count pieces at 3.
    let srbnb = t.gl("Stock Received But Not Billed").await;
    let po = t
        .insert(
            "PurchaseOrder",
            json!({"company": cid.to_string(), "supplier": acme.to_string(), "items": [{"item": bolt.to_string(), "qty": "2", "uom": boxes["id"], "price_list_rate": "36", "warehouse": stores.to_string()}]}),
        )
        .await;
    assert_eq!(dec(&po["items"][0]["stock_qty"]), n("24"));
    t.submit("PurchaseOrder", uid(&po)).await;
    let pr = vid(&t.action("PurchaseOrder", uid(&po), "make_purchase_receipt", json!({})).await);
    t.submit("PurchaseReceipt", pr).await;
    assert_eq!(t.stock(bolt, stores).await, (n("100"), n("376")));
    let pi = vid(&t.action("PurchaseReceipt", pr, "make_purchase_invoice", json!({})).await);
    t.k.update(&t.ctx, "PurchaseInvoice", pi, obj(json!({"bill_no": "ACME/BOLTS"})), None).await.expect("bill no");
    t.submit("PurchaseInvoice", pi).await;
    assert_eq!(t.gl("Stock Received But Not Billed").await, srbnb, "cleared at the receipt's value");
    assert_eq!(dec(&t.get("Item", bolt).await["last_purchase_rate"]), n("3"), "per piece");
    // A saved line given another item takes that item's price and unit.
    let draft = t.insert("SalesOrder", line(json!({"item": bolt.to_string(), "qty": "1", "uom": boxes["id"]}))).await;
    let other = json!({"items": [{"id": draft["items"][0]["id"], "item": widget.to_string(), "qty": "1"}]});
    let changed = t.k.update(&t.ctx, "SalesOrder", uid(&draft), obj(other), None).await.expect("change the item");
    assert_eq!((changed["items"][0]["uom"].clone(), dec(&changed["items"][0]["price_list_rate"])), (nos["id"].clone(), n("150")));
}
