//! A button or an automation calling another app's own step through the capability gate, once (docs/api.md,
//! "Calling another app's steps"; docs/builder/decisions.md BD-13), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first). The app is a fixture: a submittable invoice with
//! lines whose submit hook posts to a ledger table of its own, as ERP's documents post, and refuses an invoice
//! of nothing.

use async_trait::async_trait;
use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, DocHook, Error, HookCtx, HookRegistry, Kernel, KernelSettings, Result, automation};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

const FEES: &str = "package: fees\nversion: 0.2.0\ndepends: [core]\n";
const FEES_ENTITIES: &str = r#"
- name: Invoice
  label: Invoice
  kind: document
  submittable: true
  title_field: customer
  fields:
    - {name: customer, type: data, required: true}
    - {name: lines, type: table, of: InvoiceLine}
- name: InvoiceLine
  label: Invoice Line
  kind: child
  fields:
    - {name: item, type: data}
    - {name: amount, type: int}
- name: Posting
  label: Posting
  kind: ledger
  table: fees_posting
  fields:
    - {name: amount, type: int}
"#;
const LEDGER_SQL: &str = "create table fees_posting (tenant_id uuid not null references tenant (id), id uuid not null, invoice uuid not null, amount int not null, primary key (tenant_id, id));";
const TAGS: &str = "package: tags\nversion: 0.1.0\ndepends: [core]\n";
const TAG_ENTITIES: &str = "- name: Tag\n  label: Tag\n  kind: master\n  fields:\n    - {name: label, type: data}\n";

fn total(doc: &Doc) -> i64 {
    doc.get("lines").and_then(Value::as_array).into_iter().flatten().filter_map(|l| l["amount"].as_i64()).sum()
}

/// The owning app: an invoice of nothing is refused; a submitted one posts its total.
struct Books;

#[async_trait]
impl DocHook for Books {
    async fn before_submit(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if total(doc) <= 0 {
            return Err(Error::invalid("an invoice of nothing is not raised"));
        }
        Ok(())
    }
    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let id: Uuid = doc["id"].as_str().unwrap().parse().unwrap();
        sqlx::query("insert into fees_posting (tenant_id, id, invoice, amount) values ($1, $2, $3, $4)")
            .bind(hc.ctx.tenant)
            .bind(Uuid::now_v7())
            .bind(id)
            .bind(total(doc) as i32)
            .execute(&mut **hc.tx)
            .await?;
        Ok(())
    }
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Value) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn custom(entity: &str, op: Value) -> NewCustomization {
    serde_json::from_value(json!({"entity": entity, "op": op})).unwrap()
}

fn action(name: &str, effects: Value) -> NewCustomization {
    custom("XApplication", json!({"op": "add_action", "action": {"name": name, "label": name, "effects": effects}}))
}

fn message(e: &Error) -> String {
    match e {
        Error::Validation(v) => v.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

/// Raises the application's fee as an invoice with one line, submitted.
fn raise(key: Option<&str>) -> Value {
    let mut e = json!({"type": "invoke", "capability": "fees.invoice.create", "input": {
        "values": "{'customer': doc.applicant, 'lines': [{'item': 'Permit fee', 'amount': doc.fee}]}", "submit": "true"}});
    if let Some(k) = key {
        e["key"] = json!(k);
    }
    e
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
    async fn batch(&self, items: Vec<NewCustomization>) -> std::result::Result<usize, String> {
        self.k.batch_customizations(&self.admin().await, items, false).await.map_err(|(at, e)| format!("{at:?}: {}", message(&e)))
    }
    async fn count(&self, sql: &str) -> i64 {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(self.tenant).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        n
    }
    async fn postings(&self) -> i64 {
        self.count("select count(*) from fees_posting where tenant_id = $1").await
    }
    async fn invoices(&self) -> i64 {
        self.count("select count(*) from e_invoice where tenant_id = $1").await
    }
    async fn get(&self, id: Uuid) -> Doc {
        self.k.get(&self.admin().await, "XApplication", id, false).await.unwrap()
    }
    async fn apply(&self, fee: i64) -> Uuid {
        id_of(&Value::Object(self.k.insert(&self.admin().await, "XApplication", obj(json!({"applicant": "Asha", "fee": fee}))).await.unwrap()))
    }
    async fn press(&self, id: Uuid, name: &str) -> Result<Value> {
        self.k.run_action(&self.admin().await, "XApplication", id, name, json!({})).await
    }
}

#[test]
fn configuration_calls_another_apps_steps_once() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files = |list: &[(&str, &str)]| -> BTreeMap<String, String> { list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() };
    let fees =
        ferrum_meta::catalog::load_package_files(&files(&[("package.yaml", FEES), ("entities/fees.yaml", FEES_ENTITIES), ("sql/001_ledger.sql", LEDGER_SQL)]))
            .expect("fees");
    let tags = ferrum_meta::catalog::load_package_files(&files(&[("package.yaml", TAGS), ("entities/tags.yaml", TAG_ENTITIES)])).expect("tags");
    let mut hooks = HookRegistry::default();
    hooks.register("Invoice", Arc::new(Books));
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, fees, tags]).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "city".into(),
            name: "city".into(),
            admin_email: "admin@city.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["fees".into(), "tags".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login("city", "admin@city.test", "Correct-Horse-9").await.unwrap();
    let w = W { k: k.clone(), tenant, user: session.user };

    let application = json!({"name": "XApplication", "label": "Application", "kind": "master", "fields": [
        {"name": "applicant", "type": "data"}, {"name": "fee", "type": "int"}, {"name": "status", "type": "data", "read_only": true},
        {"name": "invoice", "type": "link", "to": "Invoice"}, {"name": "customer_seen", "type": "data"}]});
    w.batch(vec![
        custom("XApplication", json!({"op": "define_entity", "entity": application})),
        custom(
            "XApplication",
            json!({"op": "add_rule", "rule": {"name": "never_blocked", "check": "doc.status != 'Blocked'", "message": "an application is never blocked"}}),
        ),
    ])
    .await
    .unwrap();

    // ───────────── saved: the capability and its input names are the catalog's ─────────────
    let refusal = |r: std::result::Result<usize, String>, says: &str| {
        let e = r.expect_err("refused");
        assert!(e.contains(says), "expected {says:?}, got {e}");
    };
    let invoke = |cap: &str, input: Value| json!([{"type": "invoke", "capability": cap, "input": input}]);
    refusal(
        w.batch(vec![action("x_a", invoke("fees.nothing.create", json!({})))]).await,
        "fees.nothing.create is not a capability here: is its app installed?",
    );
    refusal(w.batch(vec![action("x_a", invoke("fees.invoice.create", json!({"vals": "{}"})))]).await, "fees.invoice.create has no input vals");
    refusal(w.batch(vec![action("x_a", invoke("fees.invoice.create", json!({"submit": "true"})))]).await, "fees.invoice.create needs its input values");
    refusal(w.batch(vec![action("x_a", invoke("fees.invoice.create", json!({"values": "{'customer': "})))]).await, "effect 1");

    w.batch(vec![
        // Once for good: the fee is invoiced once, its id kept on the application.
        action("invoice_fee", json!([raise(Some("'fee:' + doc.id")), {"type": "set_fields", "values": {"invoice": "results[0].id", "status": "'Invoiced'"}}])),
        // Once per press of the record as it is.
        action("raise", json!([raise(None)])),
        action("raise_and_block", json!([raise(Some("'block:' + doc.id")), {"type": "set_fields", "values": {"status": "'Blocked'"}}])),
        // A read gives its result to the next effect.
        action(
            "look",
            json!([{"type": "invoke", "capability": "fees.invoice.read", "input": {"id": "doc.invoice"}},
            {"type": "set_fields", "values": {"customer_seen": "results[0].customer"}}]),
        ),
        action("tag_it", json!([{"type": "invoke", "capability": "tags.tag.create", "input": {"values": "{'label': doc.applicant}"}}])),
    ])
    .await
    .expect("configured");
    // An effect runs no configured action, through the gate either.
    let own = w.k.catalog(&w.admin().await).into_iter().map(|c| c.id).find(|id| id.ends_with(".invoice_fee")).expect("listed");
    refusal(w.batch(vec![action("x_again", invoke(&own, json!({"id": "doc.id"})))]).await, "is a configured action: an effect does not run another one");

    // ───────────── `key`: once for good ─────────────
    let a = w.apply(100).await;
    w.press(a, "invoice_fee").await.expect("invoiced");
    let kept = w.get(a).await;
    assert_eq!(kept["status"], "Invoiced");
    let invoice: Uuid = kept["invoice"].as_str().expect("results[0].id kept on the application").parse().unwrap();
    let raised = w.k.get(&w.admin().await, "Invoice", invoice, false).await.unwrap();
    assert_eq!(raised["docstatus"], 1, "created and submitted in one step");
    assert_eq!(w.postings().await, 1, "the submit hook posted");
    w.press(a, "invoice_fee").await.expect("a second press is answered");
    assert_eq!((w.postings().await, w.invoices().await), (1, 1), "raised once");
    assert_eq!(w.get(a).await["invoice"], json!(invoice.to_string()), "the second press sees the first invoice");
    // The same key with other input: refused, nothing changes.
    w.k.update(&w.admin().await, "XApplication", a, obj(json!({"fee": 250})), None).await.unwrap();
    let e = w.press(a, "invoice_fee").await.unwrap_err();
    assert_eq!(e.code(), "input_changed", "{e:?}");
    assert!(message(&e).contains(&format!("the key fee:{a} was used for fees.invoice.create with other input")), "{e:?}");
    assert_eq!((w.postings().await, w.invoices().await), (1, 1));

    // ───────────── refused by the owning app: nothing kept ─────────────
    let nothing = w.apply(0).await;
    let e = w.press(nothing, "invoice_fee").await.unwrap_err();
    assert!(message(&e).contains("an invoice of nothing is not raised"), "{e:?}");
    assert_eq!((w.postings().await, w.invoices().await), (1, 1));
    assert_eq!(w.count(&format!("select count(*) from logic_effect where tenant_id = $1 and effect_key = 'fee:{nothing}'")).await, 0, "no receipt either");
    assert_eq!(w.get(nothing).await["status"], Value::Null);

    // ───────────── refused by a later effect of the same press: the posting goes too ─────────────
    let blocked = w.apply(30).await;
    let e = w.press(blocked, "raise_and_block").await.unwrap_err();
    assert!(message(&e).contains("an application is never blocked"), "{e:?}");
    assert_eq!((w.postings().await, w.invoices().await), (1, 1), "the invoice and its posting are undone with the press");

    // ───────────── no key: once per press of the record as it is ─────────────
    let d = w.apply(40).await;
    w.press(d, "raise").await.unwrap();
    assert_eq!(w.postings().await, 2);
    w.press(d, "raise").await.expect("the same press again");
    assert_eq!(w.postings().await, 2, "the record has not changed: the step is not done again");
    w.k.update(&w.admin().await, "XApplication", d, obj(json!({"fee": 41})), None).await.unwrap();
    w.press(d, "raise").await.unwrap();
    assert_eq!(w.postings().await, 3, "a later press on the changed record runs again");

    // ───────────── a read's result used by the next effect ─────────────
    w.press(a, "look").await.unwrap();
    assert_eq!(w.get(a).await["customer_seen"], "Asha");

    // ───────────── a capability gone after save ─────────────
    let t = w.apply(1).await;
    w.press(t, "tag_it").await.expect("tagged");
    w.k.disable_packages(&w.admin().await, &["tags".to_string()]).await.unwrap();
    let e = w.press(t, "tag_it").await.unwrap_err();
    assert!(message(&e).contains("tags.tag.create is not a capability here: is its app installed?"), "{e:?}");
    w.batch(vec![custom("XApplication", json!({"op": "add_field", "field": {"name": "x_note", "type": "data"}}))]).await.expect("the schema still saves");

    // ───────────── the same in an automation on a record's event ─────────────
    let admin = w.admin().await;
    let auto = |actions: Value| {
        obj(json!({"name": "invoice large fees", "entity": "XApplication", "event": "created", "condition": "doc.fee > 1000", "actions": actions}))
    };
    let e = automation::save(&w.k, &admin, &auto(json!([{"type": "invoke", "capability": "fees.nothing.create", "input": {}}]))).await.unwrap_err();
    assert!(message(&e).contains("fees.nothing.create is not a capability here"), "{e:?}");
    automation::save(&w.k, &admin, &auto(json!([raise(Some("'auto:' + doc.id")), {"type": "set_fields", "values": {"invoice": "results[0].id"}}])))
        .await
        .unwrap();
    let big = w.apply(2000).await;
    while w.k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
    let invoice: Uuid = w.get(big).await["invoice"].as_str().expect("the automation kept the invoice").parse().unwrap();
    let posted = w.count(&format!("select coalesce(sum(amount), 0)::bigint from fees_posting where tenant_id = $1 and invoice = '{invoice}'")).await;
    assert_eq!(posted, 2000);
}
