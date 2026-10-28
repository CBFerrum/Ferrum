//! End-to-end CRM test against a real PostgreSQL (`FERRUM_CRM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_crm_test`). The database is reset first.
//!
//! Two sales reps and their manager work leads from a trade-show campaign through activities,
//! opportunities and quotations to sales orders, with the refusal paths along the way: duplicate
//! emails, converting twice, ordering a lead's or an expired quotation, losing without a reason,
//! out-of-range probabilities, over-ordering, and what a sales user may not see or do.

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

const PASSWORD: &str = "Correct-Horse-9";

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

fn today() -> chrono::NaiveDate {
    chrono::Utc::now().date_naive()
}

/// A refusal of the expected kind whose text mentions `needle` (field errors: field or message).
fn refused<T: std::fmt::Debug>(r: Result<T, Error>, needle: &str) {
    match &r {
        Err(Error::Invalid(m)) | Err(Error::Forbidden(m)) if m.contains(needle) => {}
        Err(Error::Validation(errs)) if errs.iter().any(|e| e.field == needle || e.message.contains(needle)) => {}
        Err(Error::NotFound(_)) if needle == "not found" => {}
        other => panic!("expected a refusal mentioning '{needle}', got {other:?}"),
    }
}

async fn fresh_kernel() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_CRM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_crm_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_pkg_crm::register(&mut hooks);
    for source in ferrum_pkg_crm::reports() {
        ferrum_domain::reports::add_source(source);
    }
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("maintenance");
    k
}

/// One signed-in user.
struct U {
    k: Arc<Kernel>,
    ctx: Ctx,
}

impl U {
    async fn insert(&self, entity: &str, v: Value) -> Doc {
        self.k.insert(&self.ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }
    async fn update(&self, entity: &str, id: Uuid, v: Value) -> Doc {
        self.k.update(&self.ctx, entity, id, obj(v), None).await.unwrap_or_else(|e| panic!("update {entity}: {e:?}"))
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
    async fn action(&self, entity: &str, id: Uuid, action: &str, params: Value) -> Value {
        self.k.run_action(&self.ctx, entity, id, action, params).await.unwrap_or_else(|e| panic!("{entity}.{action}: {e:?}"))
    }
    async fn list(&self, entity: &str, filters: Value) -> Vec<Doc> {
        let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), limit: Some(500), ..Default::default() };
        self.k.list(&self.ctx, entity, &q).await.unwrap_or_else(|e| panic!("list {entity}: {e:?}")).rows
    }
    async fn one(&self, entity: &str, filters: Value) -> Doc {
        let rows = self.list(entity, filters.clone()).await;
        assert_eq!(rows.len(), 1, "{entity} {filters}");
        rows[0].clone()
    }
    async fn report(&self, name: &str, params: &[(&str, String)]) -> Value {
        let p: HashMap<String, String> = params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        ferrum_domain::reports::run(&self.k, &self.ctx, name, &p).await.unwrap_or_else(|e| panic!("report {name}: {e:?}"))
    }
}

#[test]
fn crm_end_to_end() {
    // Same runtime shape as the server: unoptimized async hook chains need more than 2 MiB.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let k = fresh_kernel().await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "crm".into(),
            name: "CRM Test Ltd".into(),
            admin_email: "admin@crm.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec!["crm".into()],
            settings: Some(json!({"fiscal_year_start_month": 4})),
        })
        .await
        .expect("tenant");
    let session = k.login("crm", "admin@crm.test", PASSWORD).await.expect("login");
    let admin = U { k: k.clone(), ctx: k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx") };

    // ───────────── company, tax, items, team ─────────────
    let inr = admin.one("Currency", json!({"code": "INR"})).await;
    let company = admin.insert("Company", json!({"company_name": "CRM Test Ltd", "abbr": "CT", "default_currency": inr["id"]})).await;
    let cid = uid(&company).to_string();
    let output_tax = admin.one("Account", json!({"account_name": "Output Tax", "company": cid})).await;
    admin
        .insert(
            "TaxTemplate",
            json!({"title": "VAT 10%", "company": cid, "kind": "Sales", "is_default": true,
                   "rows": [{"charge_type": "On Net Total", "account": output_tax["id"], "description": "VAT 10%", "rate": "10"}]}),
        )
        .await;
    let products = admin.one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = admin.one("Uom", json!({"uom_name": "Nos"})).await;
    let widget = uid(&admin
        .insert("Item", json!({"item_code": "WIDGET", "item_name": "Widget", "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "150"}))
        .await)
    .to_string();
    let service = uid(&admin
        .insert(
            "Item",
            json!({"item_code": "INSTALL", "item_name": "Installation", "item_group": products["id"], "stock_uom": nos["id"], "is_stock_item": false, "standard_rate": "500"}),
        )
        .await)
    .to_string();
    let user = |email: &'static str, name: &'static str, roles: &'static [&'static str]| {
        let (k, admin_ctx) = (k.clone(), admin.ctx.clone());
        async move {
            let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
            let id = k.create_user(&admin_ctx, email, name, Some(PASSWORD), &roles, false).await.expect("user");
            U { ctx: k.load_ctx(tenant, id, None, "it".into()).await.expect("user ctx"), k }
        }
    };
    let anna = user("anna@crm.test", "Anna Rep", &["Sales User"]).await;
    let ben = user("ben@crm.test", "Ben Rep", &["Sales User"]).await;
    let maria = user("maria@crm.test", "Maria Manager", &["Sales Manager"]).await;
    assert_eq!(anna.ctx.default_company.map(|c| c.to_string()), Some(cid.clone()), "a single company is everyone's default");

    // Seeded masters.
    let stages = admin.list("SalesStage", json!({})).await;
    assert_eq!(stages.len(), 7);
    let stage = |name: &str| stages.iter().find(|s| s["stage_name"] == json!(name)).map(uid).unwrap().to_string();
    let admin_ref = &admin;
    let reason = |name: &'static str| async move { admin_ref.one("LostReason", json!({"reason": name})).await["id"].clone() };
    let price_reason = reason("Price").await;

    let expo = maria
        .insert("Campaign", json!({"campaign_name": "Expo 2026", "campaign_type": "Trade Show", "status": "Active", "start_date": today().to_string(), "budget": "1000", "actual_cost": "800"}))
        .await;
    refused(anna.k.delete(&anna.ctx, "Campaign", uid(&expo), None).await, "not permitted");
    refused(anna.k.insert(&anna.ctx, "SalesStage", obj(json!({"stage_name": "Demo", "probability": "30"}))).await, "not permitted");

    // ───────────── leads ─────────────
    let l1 = anna
        .insert(
            "Lead",
            json!({"first_name": "Jane", "last_name": "Doe", "organization_name": "Acme Corp", "job_title": "Purchasing Head", "email": "Jane@Acme.test",
                   "phone": "+91 80 1234 5678", "mobile": "+91 98450 00000", "source": "Trade Show", "campaign": expo["id"],
                   "address_line1": "12 MG Road", "city": "Bengaluru", "state": "Karnataka", "postal_code": "560001", "country": "India"}),
        )
        .await;
    let l1_id = uid(&l1);
    assert_eq!(l1["lead_name"], json!("Jane Doe"));
    assert_eq!(l1["email"], json!("jane@acme.test"));
    assert_eq!(l1["status"], json!("New"));
    assert_eq!(l1["assigned_to"], json!("anna@crm.test"), "the creator owns the lead");
    assert!(l1["number"].as_str().unwrap().starts_with("LEAD-"));
    let dup = ben.k.insert(&ben.ctx, "Lead", obj(json!({"first_name": "J", "email": "jane@acme.test"}))).await;
    refused(dup, "already exists");
    refused(anna.k.insert(&anna.ctx, "Lead", obj(json!({"email": "nobody@x.test"}))).await, "first_name");
    let l2 = anna
        .insert(
            "Lead",
            json!({"first_name": "Bob", "last_name": "Stone", "organization_name": "Stone Builders", "email": "bob@stone.test", "source": "Website"}),
        )
        .await;
    let l2_id = uid(&l2);
    let l3 = ben.insert("Lead", json!({"first_name": "Carla", "last_name": "Lee", "email": "carla@lee.test", "source": "Referral"})).await;
    assert_eq!(l3["lead_name"], json!("Carla Lee"));

    // Sales users work their own leads; the manager sees the team's.
    refused(ben.k.get(&ben.ctx, "Lead", l1_id, false).await, "not found");
    refused(ben.k.update(&ben.ctx, "Lead", l1_id, obj(json!({"next_follow_up": today().to_string()})), None).await, "not found");
    assert_eq!(ben.list("Lead", json!({})).await.len(), 1);
    assert_eq!(anna.list("Lead", json!({})).await.len(), 2);
    assert_eq!(maria.list("Lead", json!({})).await.len(), 3);
    refused(maria.k.update(&maria.ctx, "Lead", uid(&l3), obj(json!({"assigned_to": "stranger@x.test"})), None).await, "assigned_to");

    // ───────────── activities ─────────────
    let call = anna.insert("Activity", json!({"activity_type": "Call", "subject": "Intro call", "lead": l1_id.to_string()})).await;
    assert_eq!(call["status"], json!("Open"));
    assert_eq!(anna.get("Lead", l1_id).await["status"], json!("New"), "an open activity is not contact yet");
    let done = anna.action("Activity", uid(&call), "complete", json!({})).await;
    assert_eq!(done["status"], json!("Done"));
    assert!(done["completed_at"].is_string());
    let l1_doc = anna.get("Lead", l1_id).await;
    assert_eq!(l1_doc["status"], json!("Contacted"), "completing an activity on a new lead");
    assert_eq!(l1_doc["last_contacted_on"], json!(today().to_string()));
    let note = anna.insert("Activity", json!({"activity_type": "Note", "subject": "Met at the booth", "lead": l2_id.to_string()})).await;
    assert_eq!(note["status"], json!("Done"), "notes are done when written");
    assert_eq!(anna.get("Lead", l2_id).await["status"], json!("New"), "a note is not an interaction");

    // ───────────── opportunity from a lead ─────────────
    let made = anna.action("Lead", l1_id, "make_opportunity", json!({})).await;
    assert_eq!(made["entity"], json!("Opportunity"));
    let o1 = vid(&made);
    let opp = anna.get("Opportunity", o1).await;
    assert_eq!(opp["party_name"], json!("Acme Corp"));
    assert_eq!(opp["title"], json!("Acme Corp"));
    assert_eq!(opp["sales_stage"], json!(stage("Prospecting")), "first open stage");
    assert_eq!(dec(&opp["probability"]), n("10"), "the stage's probability");
    assert_eq!(opp["campaign"], expo["id"]);
    assert_eq!(opp["source"], json!("Trade Show"));
    assert_eq!(opp["currency"], inr["id"]);
    let l1_doc = anna.get("Lead", l1_id).await;
    assert_eq!(l1_doc["status"], json!("Qualified"), "an opportunity qualifies the lead");
    assert_eq!(l1_doc["qualified_by"], json!("anna@crm.test"));

    let opp = anna
        .update(
            "Opportunity",
            o1,
            json!({"sales_stage": stage("Proposal"), "expected_close_date": today().to_string(),
                   "items": [{"item": widget, "qty": "10", "rate": "150"}, {"item": service, "qty": "1", "rate": "500"}],
                   "competitors": [{"competitor": "Globex", "their_price": "1900"}]}),
        )
        .await;
    assert_eq!(dec(&opp["amount"]), n("2000"), "amount from the items");
    assert_eq!(dec(&opp["probability"]), n("60"));
    assert_eq!(dec(&opp["weighted_amount"]), n("1200"));
    assert_eq!(dec(&opp["base_weighted_amount"]), n("1200"));
    refused(anna.k.update(&anna.ctx, "Opportunity", o1, obj(json!({"probability": "120"})), None).await, "probability");
    let opp = anna.update("Opportunity", o1, json!({"sales_stage": stage("Negotiation"), "probability": "75"})).await;
    assert_eq!(dec(&opp["probability"]), n("75"), "a probability set with the stage change stands");
    assert_eq!(dec(&opp["weighted_amount"]), n("1500"));
    // Choosing the Lost stage closes the opportunity as lost, which needs a reason.
    refused(anna.k.update(&anna.ctx, "Opportunity", o1, obj(json!({"sales_stage": stage("Lost")})), None).await, "lost_reason");
    refused(ben.k.get(&ben.ctx, "Opportunity", o1, false).await, "not found");

    // ───────────── quotation for the lead ─────────────
    let q1 = vid(&anna.action("Opportunity", o1, "make_quotation", json!({})).await);
    let q = anna.get("Quotation", q1).await;
    assert_eq!(q["party_type"], json!("Lead"));
    assert_eq!(q["lead"], json!(l1_id.to_string()));
    assert_eq!(q["opportunity"], json!(o1.to_string()));
    assert_eq!(q["party_name"], json!("Acme Corp"));
    assert_eq!(q["valid_till"], json!((today() + chrono::Duration::days(30)).to_string()), "30 days validity by default");
    assert_eq!(dec(&q["net_total"]), n("2000"));
    assert_eq!(dec(&q["total_taxes"]), n("200"), "the default sales tax template applies");
    assert_eq!(dec(&q["grand_total"]), n("2200"));
    assert_eq!(q["status"], json!("Draft"));
    assert_eq!(q["assigned_to"], json!("anna@crm.test"), "the opportunity's owner owns its quotation");
    let q = anna.submit("Quotation", q1).await;
    assert_eq!(q["status"], json!("Open"));
    assert!(q["number"].as_str().unwrap().starts_with("QTN-"));
    refused(anna.k.run_action(&anna.ctx, "Quotation", q1, "make_sales_order", json!({})).await, "convert the lead");
    refused(ben.k.get(&ben.ctx, "Quotation", q1, false).await, "not found");
    let printed = k.render_print(&anna.ctx, "Quotation", q1).await.expect("print");
    for text in ["CRM Test Ltd", "Acme Corp", q["number"].as_str().unwrap(), "Installation", "VAT 10%", "2,200.00", "INR"] {
        assert!(printed.contains(text), "the printed quotation shows {text}");
    }

    // ───────────── lead conversion ─────────────
    let converted = anna.action("Lead", l1_id, "convert_to_customer", json!({})).await;
    assert_eq!(converted["entity"], json!("Customer"));
    let acme = vid(&converted).to_string();
    let customer = anna.get("Customer", vid(&converted)).await;
    assert_eq!(customer["customer_name"], json!("Acme Corp"));
    assert_eq!(customer["customer_type"], json!("Company"));
    assert_eq!(customer["email"], json!("jane@acme.test"));
    assert_eq!(customer["billing_address"], json!("12 MG Road\nBengaluru, Karnataka 560001\nIndia"), "the primary address lands on the customer");
    let contact = anna.get("Contact", vid(&json!({"id": converted["contact"]}))).await;
    assert_eq!(contact["full_name"], json!("Jane Doe"));
    assert_eq!(contact["email"], json!("jane@acme.test"));
    assert_eq!(contact["designation"], json!("Purchasing Head"));
    assert_eq!(contact["customer"], json!(acme));
    assert_eq!(contact["phones"].as_array().unwrap().len(), 2);
    let address = anna.get("Address", vid(&json!({"id": converted["address"]}))).await;
    assert_eq!(address["is_primary"], json!(true));
    assert_eq!(address["address_title"], json!("Acme Corp"));
    let l1_doc = anna.get("Lead", l1_id).await;
    assert_eq!(l1_doc["status"], json!("Converted"));
    assert_eq!(l1_doc["customer"], json!(acme));
    assert_eq!(l1_doc["contact"], converted["contact"]);
    assert_eq!(l1_doc["converted_on"], json!(today().to_string()));
    assert_eq!(anna.get("Opportunity", o1).await["customer"], json!(acme), "the lead's opportunity follows it");
    assert_eq!(anna.get("Quotation", q1).await["customer"], json!(acme), "so does its quotation");
    refused(anna.k.run_action(&anna.ctx, "Lead", l1_id, "convert_to_customer", json!({})).await, "not available");
    refused(anna.k.update(&anna.ctx, "Lead", l1_id, obj(json!({"status": "Qualified"})), None).await, "converted_via_action");

    // ───────────── ordering the quotation ─────────────
    let so1 = vid(&anna.action("Quotation", q1, "make_sales_order", json!({})).await);
    let so = anna.get("SalesOrder", so1).await;
    assert_eq!(so["customer"], json!(acme));
    assert_eq!(so["crm_quotation"], json!(q1.to_string()));
    assert_eq!(dec(&so["grand_total"]), n("2200"), "the order prices exactly like the quotation");
    let lines = so["items"].as_array().unwrap().clone();
    assert!(lines.iter().all(|l| l["crm_quotation"] == json!(q1.to_string()) && l["crm_quotation_line"].is_string()));
    // The customer takes 6 widgets for now.
    let so = anna.update("SalesOrder", so1, json!({"items": [{"id": lines[0]["id"], "qty": "6"}, {"id": lines[1]["id"]}]})).await;
    assert_eq!(dec(&so["grand_total"]), n("1540"));
    anna.submit("SalesOrder", so1).await;
    let q = anna.get("Quotation", q1).await;
    assert_eq!(q["status"], json!("Partially Ordered"));
    assert_eq!(dec(&q["per_ordered"]), n("63.64"), "7 of 11 units");
    assert_eq!(dec(&q["items"][0]["ordered_qty"]), n("6"));
    let opp = anna.get("Opportunity", o1).await;
    assert_eq!(opp["status"], json!("Won"), "an ordered quotation wins its opportunity");
    assert_eq!(opp["sales_stage"], json!(stage("Won")));
    assert_eq!(dec(&opp["weighted_amount"]), n("2000"));
    assert_eq!(opp["closed_on"], json!(today().to_string()));
    refused(anna.k.cancel(&anna.ctx, "Quotation", q1, None).await, "cancel the sales orders");

    // Cancelling the order gives the quantities back and reopens the deal.
    anna.cancel("SalesOrder", so1).await;
    let q = anna.get("Quotation", q1).await;
    assert_eq!(q["status"], json!("Open"));
    assert_eq!(dec(&q["per_ordered"]), Decimal::ZERO);
    assert_eq!(dec(&q["items"][0]["ordered_qty"]), Decimal::ZERO);
    let opp = anna.get("Opportunity", o1).await;
    assert_eq!(opp["status"], json!("Open"));
    assert_eq!(opp["sales_stage"], json!(stage("Negotiation")), "back in the last open stage");
    assert_eq!(dec(&opp["probability"]), n("80"));
    assert!(opp["closed_on"].is_null());

    // The amendment still counts against the quotation.
    let amended = k.amend(&anna.ctx, "SalesOrder", so1).await.expect("amend");
    assert_eq!(amended["crm_quotation"], json!(q1.to_string()));
    assert!(amended["items"].as_array().unwrap().iter().all(|l| l["crm_quotation_line"].is_string()));
    anna.submit("SalesOrder", uid(&amended)).await;
    assert_eq!(anna.get("Quotation", q1).await["status"], json!("Partially Ordered"));
    assert_eq!(anna.get("Opportunity", o1).await["status"], json!("Won"));

    // The rest of the quotation: only the 4 open widgets.
    let so2 = vid(&anna.action("Quotation", q1, "make_sales_order", json!({})).await);
    let so = anna.get("SalesOrder", so2).await;
    let lines = so["items"].as_array().unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["item"], json!(widget));
    assert_eq!(dec(&lines[0]["qty"]), n("4"));
    assert_eq!(dec(&so["grand_total"]), n("660"));
    anna.submit("SalesOrder", so2).await;
    let q = anna.get("Quotation", q1).await;
    assert_eq!(q["status"], json!("Ordered"));
    assert_eq!(dec(&q["per_ordered"]), n("100"));
    refused(anna.k.run_action(&anna.ctx, "Quotation", q1, "make_sales_order", json!({})).await, "not available");

    // Two drafts for the same quotation cannot both be ordered.
    let q3 = anna.insert("Quotation", json!({"party_type": "Customer", "customer": acme, "items": [{"item": widget, "qty": "2"}]})).await;
    assert_eq!(dec(&q3["total_taxes"]), n("30"), "the default tax template applies on the first save (no india package)");
    assert_eq!(dec(&q3["grand_total"]), n("330"), "standard rate 150 × 2 + 10% VAT");
    let q3 = uid(&q3);
    anna.submit("Quotation", q3).await;
    let so_a = vid(&anna.action("Quotation", q3, "make_sales_order", json!({})).await);
    let so_b = vid(&anna.action("Quotation", q3, "make_sales_order", json!({})).await);
    anna.submit("SalesOrder", so_a).await;
    refused(anna.k.submit(&anna.ctx, "SalesOrder", so_b, None).await, "exceed");
    assert_eq!(anna.get("Quotation", q3).await["status"], json!("Ordered"));

    // ───────────── losing ─────────────
    let o2 = vid(&anna.action("Lead", l2_id, "make_opportunity", json!({})).await);
    refused(anna.k.run_action(&anna.ctx, "Opportunity", o2, "mark_lost", json!({})).await, "lost_reason");
    refused(anna.k.run_action(&anna.ctx, "Opportunity", o2, "mark_lost", json!({"lost_reason": "Too Expensive"})).await, "lost_reason");
    let lost = anna.action("Opportunity", o2, "mark_lost", json!({"lost_reason": "Price", "lost_detail": "Went with a cheaper local firm"})).await;
    assert_eq!(lost["status"], json!("Lost"));
    let opp = anna.get("Opportunity", o2).await;
    assert_eq!(opp["sales_stage"], json!(stage("Lost")));
    assert_eq!(dec(&opp["probability"]), Decimal::ZERO);
    assert_eq!(opp["lost_reason"], price_reason);
    refused(anna.k.update(&anna.ctx, "Opportunity", o2, obj(json!({"status": "Open"})), None).await, "open stage");
    refused(anna.k.update(&anna.ctx, "Lead", l2_id, obj(json!({"status": "Lost"})), None).await, "lost_reason");
    anna.update("Lead", l2_id, json!({"status": "Lost", "lost_reason": reason("No Budget").await})).await;
    refused(anna.k.run_action(&anna.ctx, "Lead", l2_id, "make_opportunity", json!({})).await, "not available");
    refused(anna.k.insert(&anna.ctx, "Opportunity", obj(json!({"lead": l2_id.to_string()}))).await, "reopen it first");

    // A deal lost with its only quotation.
    let o3 = anna.insert("Opportunity", json!({"opportunity_from": "Customer", "customer": acme, "title": "Acme spares", "amount": "500"})).await;
    let o3 = uid(&o3);
    let q6 = vid(&anna.action("Opportunity", o3, "make_quotation", json!({})).await);
    refused(anna.k.submit(&anna.ctx, "Quotation", q6, None).await, "has_items");
    anna.update("Quotation", q6, json!({"items": [{"item": service, "qty": "1"}]})).await;
    anna.submit("Quotation", q6).await;
    refused(anna.k.run_action(&anna.ctx, "Quotation", q6, "mark_lost", json!({})).await, "lost_reason");
    anna.action("Quotation", q6, "mark_lost", json!({"lost_reason": price_reason})).await;
    assert_eq!(anna.get("Quotation", q6).await["status"], json!("Lost"));
    let opp = anna.get("Opportunity", o3).await;
    assert_eq!(opp["status"], json!("Lost"), "the opportunity is lost with its last quotation");
    assert_eq!(opp["lost_reason"], price_reason);

    // ───────────── validity and expiry ─────────────
    let line = json!([{"item": widget, "qty": "1"}]);
    refused(
        anna.k
            .insert(&anna.ctx, "Quotation", obj(json!({"customer": acme, "items": line, "valid_till": (today() - chrono::Duration::days(1)).to_string()})))
            .await,
        "validity_after_date",
    );
    let q4 = uid(&anna.insert("Quotation", json!({"customer": acme, "items": line, "valid_till": today().to_string()})).await);
    anna.submit("Quotation", q4).await;
    ferrum_pkg_crm::maintenance(&k).await.expect("maintenance");
    assert_eq!(anna.get("Quotation", q4).await["status"], json!("Open"), "valid through its last day");
    // A day passes.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update e_quotation set valid_till = current_date - 1 where tenant_id = $1 and id = $2").bind(tenant).bind(q4).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    refused(anna.k.run_action(&anna.ctx, "Quotation", q4, "make_sales_order", json!({})).await, "expired");
    ferrum_pkg_crm::maintenance(&k).await.expect("maintenance");
    ferrum_pkg_crm::maintenance(&k).await.expect("maintenance runs again");
    assert_eq!(anna.get("Quotation", q4).await["status"], json!("Expired"));
    assert_eq!(anna.get("Quotation", q1).await["status"], json!("Ordered"), "ordered quotations never expire");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let expiries: i64 = sqlx::query_scalar("select count(*) from audit_event where tenant_id = $1 and record_id = $2 and action = 'expire'")
        .bind(tenant)
        .bind(q4)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    drop(tx);
    assert_eq!(expiries, 1, "expiry is idempotent");
    refused(anna.k.run_action(&anna.ctx, "Quotation", q4, "make_sales_order", json!({})).await, "expired");
    let backdated = anna
        .insert(
            "Quotation",
            json!({"customer": acme, "items": line, "posting_date": (today() - chrono::Duration::days(40)).to_string(),
                   "valid_till": (today() - chrono::Duration::days(10)).to_string()}),
        )
        .await;
    assert_eq!(anna.submit("Quotation", uid(&backdated)).await["status"], json!("Expired"), "entered after its validity");

    // ───────────── reassignment ─────────────
    maria.update("Lead", l2_id, json!({"assigned_to": "ben@crm.test"})).await;
    assert_eq!(ben.get("Lead", l2_id).await["assigned_to"], json!("ben@crm.test"), "the new assignee sees it");
    assert_eq!(anna.get("Lead", l2_id).await["created_by"], json!(anna.ctx.user.to_string()), "its creator still does, and stays its creator");

    // ───────────── contacts and addresses ─────────────
    let sam = anna.insert("Contact", json!({"first_name": "Sam", "customer": acme, "emails": [{"email": "sam@acme.test"}, {"email": "s.k@acme.test"}]})).await;
    assert_eq!(sam["email"], json!("sam@acme.test"), "the first email is primary");
    assert_eq!(sam["emails"][0]["is_primary"], json!(true));
    let two = anna
        .k
        .insert(
            &anna.ctx,
            "Contact",
            obj(json!({"first_name": "X", "emails": [{"email": "a@x.test", "is_primary": true}, {"email": "b@x.test", "is_primary": true}]})),
        )
        .await;
    assert!(matches!(&two, Err(Error::Validation(e)) if e.iter().any(|f| f.row == Some(("emails".into(), 1)))), "{two:?}");
    refused(
        anna.k.insert(&anna.ctx, "Contact", obj(json!({"first_name": "Y", "emails": [{"email": "a@x.test"}, {"email": "A@x.test"}]}))).await,
        "listed twice",
    );
    refused(anna.k.insert(&anna.ctx, "Address", obj(json!({"address_line1": "1 Nowhere", "city": "Pune"}))).await, "linked");
    let hq = anna.insert("Address", json!({"address_line1": "99 New Street", "city": "Pune", "is_primary": true, "customer": acme})).await;
    assert_eq!(hq["address_title"], json!("Acme Corp"));
    assert_eq!(anna.get("Address", vid(&json!({"id": converted["address"]}))).await["is_primary"], json!(false), "one primary billing address");
    assert_eq!(anna.get("Customer", vid(&json!({"id": acme}))).await["billing_address"], json!("99 New Street\nPune"));

    // ───────────── pipeline and reports ─────────────
    ben.insert(
        "Opportunity",
        json!({"opportunity_from": "Customer", "customer": acme, "title": "Acme service plan", "amount": "1000", "sales_stage": stage("Qualification")}),
    )
    .await;
    let o5 = anna.insert("Opportunity", json!({"opportunity_from": "Customer", "customer": acme, "amount": "3000", "sales_stage": stage("Proposal")})).await;
    assert_eq!(o5["title"], json!("Acme Corp"));
    assert_eq!(dec(&o5["weighted_amount"]), n("1800"));

    let company_param = ("company", cid.clone());
    let pipe = maria.report("sales_pipeline", std::slice::from_ref(&company_param)).await;
    assert_eq!(pipe["totals"]["count"], json!(2));
    assert_eq!(dec(&pipe["totals"]["amount"]), n("4000"));
    assert_eq!(dec(&pipe["totals"]["weighted_amount"]), n("2000"));
    let rows = pipe["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 5, "every open stage");
    let proposal = rows.iter().find(|r| r["sales_stage"] == json!("Proposal")).unwrap();
    assert_eq!((proposal["count"].clone(), dec(&proposal["weighted_amount"])), (json!(1), n("1800")));
    let mine = anna.report("sales_pipeline", std::slice::from_ref(&company_param)).await;
    assert_eq!((mine["totals"]["count"].clone(), dec(&mine["totals"]["amount"])), (json!(1), n("3000")), "a rep's pipeline is their own");
    let bens = maria.report("sales_pipeline", &[company_param.clone(), ("assigned_to", "Ben@crm.test".into())]).await;
    assert_eq!(dec(&bens["totals"]["amount"]), n("1000"));

    let funnel = maria.report("lead_funnel", std::slice::from_ref(&company_param)).await;
    let t = &funnel["totals"];
    assert_eq!(
        (t["leads"].clone(), t["contacted"].clone(), t["qualified"].clone(), t["converted"].clone(), t["lost"].clone()),
        (json!(3), json!(2), json!(2), json!(1), json!(1))
    );
    assert_eq!(dec(&t["conversion_pct"]), n("33.33"));
    let groups: Vec<&str> = funnel["rows"].as_array().unwrap().iter().map(|r| r["group"].as_str().unwrap()).collect();
    assert_eq!(groups, ["Referral", "Trade Show", "Website"]);
    let by_campaign = maria.report("lead_funnel", &[company_param.clone(), ("group_by", "Campaign".into())]).await;
    let expo_row = by_campaign["rows"].as_array().unwrap().iter().find(|r| r["group"] == json!("Expo 2026")).cloned().unwrap();
    assert_eq!((expo_row["leads"].clone(), dec(&expo_row["conversion_pct"])), (json!(1), n("100")));

    let camp = maria.report("campaign_performance", std::slice::from_ref(&company_param)).await;
    let row = &camp["rows"][0];
    assert_eq!(row["campaign"], json!("Expo 2026"));
    assert_eq!(
        (row["leads"].clone(), row["converted_leads"].clone(), row["opportunities"].clone(), row["won"].clone()),
        (json!(1), json!(1), json!(1), json!(1))
    );
    assert_eq!(dec(&row["won_amount"]), n("2000"));
    assert_eq!(dec(&row["cost_per_lead"]), n("800"));
    assert_eq!(dec(&row["roi_pct"]), n("150"), "(2000 won - 800 spent) / 800");

    let reg = maria.report("quotation_register", std::slice::from_ref(&company_param)).await;
    let t = &reg["totals"];
    assert_eq!(t["count"], json!(5), "Q1, Q3, Q6, the expired and the backdated one");
    assert_eq!(dec(&t["quoted"]), n("3410"));
    assert_eq!(dec(&t["ordered"]), n("2530"), "1540 + 660 from Q1, 330 from Q3; the cancelled order does not count");
    assert_eq!(dec(&t["lost"]), n("550"));
    assert_eq!(dec(&t["expired"]), n("330"));
    assert_eq!(dec(&t["conversion_pct"]), n("74.19"));
    assert_eq!(dec(&t["win_rate_pct"]), n("40"));
    let q1_row = reg["rows"].as_array().unwrap().iter().find(|r| r["quotation_id"] == json!(q1.to_string())).cloned().unwrap();
    assert_eq!((q1_row["status"].clone(), dec(&q1_row["ordered_amount"])), (json!("Ordered"), n("2200")));
    let lost_only = maria.report("quotation_register", &[company_param.clone(), ("status", "Lost".into())]).await;
    assert_eq!(lost_only["totals"]["count"], json!(1));

    // ───────────── own records: created or assigned ─────────────
    let dana = maria.insert("Lead", json!({"first_name": "Dana", "organization_name": "Initech", "assigned_to": "anna@crm.test"})).await;
    let dana_id = uid(&dana);
    assert_eq!(anna.get("Lead", dana_id).await["created_by"], json!(maria.ctx.user.to_string()), "the rep sees it; the manager stays its creator");
    let edited = anna.update("Lead", dana_id, json!({"next_follow_up": today().to_string(), "status": "Contacted"})).await;
    assert_eq!(edited["status"], json!("Contacted"), "and the rep can work it");
    assert!(anna.list("Lead", json!({})).await.iter().any(|l| l["id"] == dana["id"]), "it is on the rep's list");
    refused(ben.k.get(&ben.ctx, "Lead", dana_id, false).await, "not found");
    assert!(!ben.list("Lead", json!({})).await.iter().any(|l| l["id"] == dana["id"]), "other reps still cannot see it");

    // Reports need read access to their entity.
    let clerk = user("clerk@crm.test", "Accounts Clerk", &["Accounts User"]).await;
    let denied = ferrum_domain::reports::run(&clerk.k, &clerk.ctx, "sales_pipeline", &HashMap::new()).await;
    assert!(matches!(denied, Err(Error::Forbidden(_))), "{denied:?}");

    // ───────────── several leads of one prospect become one customer ─────────────
    let globex = maria.insert("Prospect", json!({"prospect_name": "Globex Inc", "industry": "Manufacturing"})).await;
    let pat = maria.insert("Lead", json!({"first_name": "Pat", "prospect": globex["id"], "email": "pat@globex.test"})).await;
    assert_eq!(pat["organization_name"], json!("Globex Inc"), "the organization comes from the prospect");
    let lou = maria.insert("Lead", json!({"first_name": "Lou", "last_name": "Grant", "prospect": globex["id"]})).await;
    let pats_call = maria.insert("Activity", json!({"activity_type": "Call", "subject": "Discovery call", "status": "Done", "lead": pat["id"]})).await;
    let lous_card =
        maria.insert("Contact", json!({"first_name": "Lou", "last_name": "Grant", "lead": lou["id"], "emails": [{"email": "lou@globex.test"}]})).await;
    let first = maria.action("Lead", uid(&pat), "convert_to_customer", json!({})).await;
    assert_eq!(maria.get("Activity", uid(&pats_call)).await["customer"], first["id"], "the lead's history moves to the customer");
    assert_eq!(maria.get("Prospect", uid(&globex)).await["customer"], first["id"], "the prospect became that customer");
    let second = maria.action("Lead", uid(&lou), "convert_to_customer", json!({})).await;
    assert_eq!(second["id"], first["id"], "the second lead joins the same customer");
    assert!(second["address"].is_null(), "no address was captured");
    assert_eq!(second["contact"], lous_card["id"], "the contact kept for the lead is reused, not duplicated");
    assert_eq!(maria.get("Contact", uid(&lous_card)).await["customer"], first["id"]);
    assert_eq!(maria.list("Customer", json!({"customer_name": "Globex Inc"})).await.len(), 1);
    assert_eq!(maria.list("Contact", json!({"customer": first["id"]})).await.len(), 2);

    // ───────────── foreign-currency opportunities count in company currency ─────────────
    let usd = admin.one("Currency", json!({"code": "USD"})).await;
    let yesterday = (today() - chrono::Duration::days(1)).to_string();
    admin.insert("ExchangeRate", json!({"from_currency": usd["id"], "to_currency": inr["id"], "rate": "83", "valid_from": yesterday})).await;
    let export = maria
        .insert(
            "Opportunity",
            json!({"opportunity_from": "Customer", "customer": first["id"], "currency": usd["id"], "amount": "100", "sales_stage": stage("Proposal")}),
        )
        .await;
    assert_eq!(dec(&export["conversion_rate"]), n("83"));
    assert_eq!((dec(&export["weighted_amount"]), dec(&export["base_amount"]), dec(&export["base_weighted_amount"])), (n("60"), n("8300"), n("4980")));
    let export = maria.update("Opportunity", uid(&export), json!({"conversion_rate": "84"})).await;
    assert_eq!(dec(&export["base_amount"]), n("8400"), "a rate the user gives stands");
    let export = maria.update("Opportunity", uid(&export), json!({"currency": inr["id"]})).await;
    assert_eq!((dec(&export["conversion_rate"]), dec(&export["base_amount"])), (n("1"), n("100")));
}
