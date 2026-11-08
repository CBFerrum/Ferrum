//! FLC-HOOK-001 to 007 and FLC-COMP-004/005/008/009: customer rules in a workspace, from a solution's source
//! to every save of their records, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; reset first).
//! Two workspaces; in the first an administrator who writes a solution, another who reviews it, a clerk, and
//! a stranger who may create orders but not read parties.

use async_trait::async_trait;
use ferrum_kernel::capability::Invocation;
use ferrum_kernel::jobs::{Job, JobHandler, NewJob};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, DocHook, Error, HookCtx, HookRegistry, Kernel, KernelSettings, Result, automation};
use ferrum_meta::{Catalog, OverlayOp};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const PACKAGE: &str = "package: orders\nversion: 1.0.0\ndepends: [core]\nroles:\n  - name: Clerk\n    permissions:\n      - {entity: Order, read: true, write: true, create: true}\n      - {entity: Party, read: true}\n  - name: Stranger\n    permissions:\n      - {entity: Order, read: true, write: true, create: true}\n";
const ENTITIES: &str = r#"
- name: Order
  label: Order
  kind: document
  title_field: reference
  fields:
    - {name: reference, type: data, required: true}
    - {name: party, type: link, to: Party}
    - {name: total, type: decimal}
    - {name: cost, type: decimal, perm_group: costing}
- name: Party
  label: Party
  kind: master
  title_field: party_name
  fields:
    - {name: party_name, type: data, required: true}
    - {name: credit_limit, type: decimal}
    - {name: bank_account, type: data, perm_group: banking}
"#;

/// The owning app: its guard sees the record as the customer's rules left it.
#[derive(Default)]
struct OrdersApp {
    seen: Mutex<Vec<Value>>,
}

#[async_trait]
impl DocHook for OrdersApp {
    fn name(&self) -> &'static str {
        "orders.guard"
    }
    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        self.seen.lock().unwrap().push(doc.get("x_band").cloned().unwrap_or(Value::Null));
        Ok(())
    }
}

/// A job that saves an order it is handed: the jobs path.
struct SaveOrder;

#[async_trait]
impl JobHandler for SaveOrder {
    fn kind(&self) -> &'static str {
        "orders.save"
    }
    fn title(&self) -> &'static str {
        "Save an order"
    }
    async fn run(&self, k: &Kernel, job: &Job) -> Result<Value> {
        let schema = k.registry.schema(job.tenant_id).await?;
        let ctx = Ctx::system(job.tenant_id, schema);
        k.insert(&ctx, "Order", job.payload.as_object().cloned().unwrap_or_default()).await?;
        Ok(json!({}))
    }
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn solution(version: &str, extra_rules: Value) -> String {
    let mut rules = json!([
        {"id": "band", "entity": "Order", "phase": "derive", "when": "has(doc.total)", "writes": {"x_band": "doc.total > parameters.large ? \"large\" : \"small\""}},
        {"id": "flag", "entity": "Order", "phase": "derive", "writes": {"x_flag": "doc.x_band == \"large\""}},
        {"id": "positive", "entity": "Order", "phase": "validate", "check": "doc.total > dec(\"0\")", "message": "An order has a total above zero", "field": "total", "audit_refusals": true},
        {"id": "within_credit", "entity": "Order", "phase": "validate", "when": "has(doc.party)", "message": "This order is over the party's credit limit",
         "lookups": [{"name": "party", "link": "party", "fields": ["credit_limit"]}], "check": "doc.total <= lookups.party.credit_limit"}
    ]);
    rules.as_array_mut().unwrap().extend(extra_rules.as_array().cloned().unwrap_or_default());
    json!({
        "schema_version": "ferrum.solution/1", "kind": "Solution", "id": "acme.orders", "version": version, "label": "Order rules",
        "parameters": {"large": {"type": "decimal", "value": "1000"}},
        "rules": rules
    })
    .to_string()
}

fn code(e: &Error) -> &'static str {
    e.code()
}

fn message(e: &Error) -> String {
    match e {
        Error::Validation(v) => v.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

async fn scalar(k: &Kernel, tenant: Uuid, sql: &'static str) -> i64 {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let n: i64 = sqlx::query_scalar(sql).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

async fn drain(k: &Kernel) {
    for _ in 0..20 {
        if k.dispatch_outbox_once().await.unwrap() == 0 {
            break;
        }
    }
}

#[test]
fn customer_rules_run_on_every_save() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn workspace(k: &Kernel, slug: &str) -> Ctx {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["orders".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap()
}

async fn user(k: &Kernel, admin: &Ctx, email: &str, role: &str, is_admin: bool) -> Ctx {
    let id = k.create_user(admin, email, role, Some("Correct-Horse-9"), &[role.to_string()], is_admin).await.unwrap();
    k.load_ctx(admin.tenant, id, None, "it".into()).await.unwrap()
}

/// The same person, with the workspace as it is now.
async fn reload(k: &Kernel, c: &Ctx) -> Ctx {
    k.load_ctx(c.tenant, c.user, None, "it".into()).await.unwrap()
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/orders.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let pkg = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let app = Arc::new(OrdersApp::default());
    let mut hooks = HookRegistry::default();
    hooks.register("Order", app.clone());
    hooks.add_job_handler(Arc::new(SaveOrder));
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, pkg]).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    let admin = workspace(&k, "acme").await;
    let reviewer = user(&k, &admin, "reviewer@acme.test", "Clerk", true).await;
    let clerk = user(&k, &admin, "clerk@acme.test", "Clerk", false).await;
    let stranger = user(&k, &admin, "stranger@acme.test", "Stranger", false).await;
    for (name, ty) in [("x_band", "data"), ("x_flag", "check")] {
        let op: OverlayOp = serde_json::from_value(json!({"op": "add_field", "field": {"name": name, "type": ty}})).unwrap();
        k.add_customization(&admin, "Order", op, None).await.unwrap();
    }
    let (admin, reviewer, clerk, stranger) = (reload(&k, &admin).await, reload(&k, &reviewer).await, reload(&k, &clerk).await, reload(&k, &stranger).await);
    let party = k.insert(&admin, "Party", obj(json!({"party_name": "Kumar Traders", "credit_limit": "10000", "bank_account": "secret"}))).await.unwrap();
    let party = party["id"].as_str().unwrap().to_string();

    // Checking keeps nothing; the builder sees every fault with a pointer.
    let bad = solution("0.9.0", json!([{"id": "peek", "entity": "Order", "phase": "validate", "check": "doc.cost > dec(\"0\")", "message": "m"}]));
    let faults = k.compile_solution(&admin, &bad).await.unwrap().unwrap_err();
    assert!(faults.iter().any(|d| d.message.contains("restricted") && d.pointer == "/rules/4/check"), "a rule may not read a restricted field: {faults:?}");
    let mailing = serde_json::from_str::<Value>(&solution("0.9.0", json!([]))).map(|mut v| {
        v["rules"][2]["capability"] = json!({"id": "ferrum.mail", "version": "1.0.0"});
        v.to_string()
    });
    assert!(k.compile_solution(&admin, &mailing.unwrap()).await.unwrap().is_err(), "a rule cannot name an effect (FLC-COMP-008)");
    if let Err(d) = k.compile_solution(&clerk, &solution("1.0.0", json!([]))).await.unwrap() {
        panic!("checking is for anyone; keeping is not: {d:?}");
    }
    assert_eq!(code(&k.save_release(&clerk, &solution("1.0.0", json!([]))).await.unwrap_err()), "forbidden");

    // Kept, then reviewed by someone else, of the digest they were shown.
    let saved = k.save_release(&admin, &solution("1.0.0", json!([]))).await.unwrap();
    let release: Uuid = saved["id"].as_str().unwrap().parse().unwrap();
    let digest = saved["artifact_digest"].as_str().unwrap().to_string();
    assert_eq!(code(&k.save_release(&admin, &solution("1.0.0", json!([]))).await.unwrap_err()), "conflict", "a version is kept once");
    assert_eq!(code(&k.activate_release(&admin, release, None).await.unwrap_err()), "conflict", "not approved yet");
    assert_eq!(code(&k.review_release(&admin, release, &digest, true, "").await.unwrap_err()), "forbidden", "not by its author");
    assert_eq!(code(&k.review_release(&reviewer, release, "sha256:something-else", true, "").await.unwrap_err()), "stale_approval");
    assert_eq!(code(&k.review_release(&reviewer, release, &digest, false, " ").await.unwrap_err()), "validation_failed", "a rejection says why");
    k.review_release(&reviewer, release, &digest, true, "Looks right").await.unwrap();
    assert_eq!(code(&k.review_release(&reviewer, release, &digest, true, "").await.unwrap_err()), "conflict", "once");
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    let changed = sqlx::query("update logic_release set version = '9.9.9' where id = $1").bind(release).execute(&mut *tx).await;
    assert!(changed.is_err(), "a release does not change");
    drop(tx);
    let generation = k.activate_release(&admin, release, None).await.unwrap();
    assert_eq!(generation, 1);
    assert_eq!(code(&k.activate_release(&admin, release, None).await.unwrap_err()), "conflict", "the generation the caller saw is compared");

    // Derivations fill the customer's fields in the order they need, before the app's guard looks.
    let small = k.insert(&clerk, "Order", obj(json!({"reference": "SO-1", "party": party, "total": "500"}))).await.unwrap();
    assert_eq!((small["x_band"].clone(), small["x_flag"].clone()), (json!("small"), json!(false)));
    let large = k.insert(&clerk, "Order", obj(json!({"reference": "SO-2", "party": party, "total": "5000"}))).await.unwrap();
    assert_eq!((large["x_band"].clone(), large["x_flag"].clone()), (json!("large"), json!(true)));
    assert_eq!(app.seen.lock().unwrap().last(), Some(&json!("large")), "the app's guard saw the derived value (FLC-HOOK-001)");

    // A refused save writes nothing; the evidence it asked for is kept apart (FLC-HOOK-007).
    let orders = scalar(&k, admin.tenant, "select count(*) from e_order").await;
    let e = k.insert(&clerk, "Order", obj(json!({"reference": "SO-3", "party": party, "total": "0"}))).await.unwrap_err();
    assert_eq!(code(&e), "validation_failed");
    let said = message(&e);
    assert!(said.starts_with("total: An order has a total above zero [acme.orders 1.0.0, rule positive]"), "{said}");
    assert_eq!(scalar(&k, admin.tenant, "select count(*) from e_order").await, orders, "nothing written");
    assert_eq!(scalar(&k, admin.tenant, "select count(*) from logic_refusal where rule = 'positive' and outcome = 'false'").await, 1, "evidence kept");
    let refusals = k.logic_refusals(&admin, 10).await.unwrap();
    assert_eq!((refusals[0]["solution"].as_str(), refusals[0]["actor"].as_str()), (Some("acme.orders"), Some(clerk.user.to_string().as_str())));
    assert!(!refusals[0].to_string().contains("SO-3"), "the evidence holds no copy of what was refused");
    let missing = k.insert(&clerk, "Order", obj(json!({"reference": "SO-4"}))).await.unwrap_err();
    assert!(message(&missing).contains("could not be checked"), "a missing total is not a pass: {}", message(&missing));

    // A linked record, read with the saver's rights before any rule runs (FLC-HOOK-004).
    let over = k.insert(&clerk, "Order", obj(json!({"reference": "SO-5", "party": party, "total": "20000"}))).await.unwrap_err();
    assert!(message(&over).contains("over the party's credit limit"), "{}", message(&over));
    let e = k.insert(&stranger, "Order", obj(json!({"reference": "SO-6", "party": party, "total": "10"}))).await.unwrap_err();
    assert_eq!(code(&e), "forbidden", "a rule reads nothing its saver could not: {e}");

    // One invalid record through every path that saves: all refused alike (FLC-HOOK-005).
    let invalid = json!({"reference": "SO-X", "party": party, "total": "-1"});
    let refused = |e: &Error| message(e).contains("An order has a total above zero");
    assert!(refused(&k.insert(&clerk, "Order", obj(invalid.clone())).await.unwrap_err()), "insert (REST, /api/v1)");
    let id: Uuid = small["id"].as_str().unwrap().parse().unwrap();
    assert!(refused(&k.update(&clerk, "Order", id, obj(json!({"total": "-1"})), None).await.unwrap_err()), "update");
    let (done, failed) = k.import_rows(&admin, "Order", std::slice::from_ref(&invalid)).await.unwrap();
    assert!(done.is_empty() && failed[0].to_string().contains("An order has a total above zero"), "import: {failed:?}");
    let bulk = k
        .bulk(&clerk, "Order", ferrum_kernel::bulk::BulkTarget::Ids(vec![id]), &ferrum_kernel::bulk::BulkOp::Update { values: obj(json!({"total": "-1"})) })
        .await
        .unwrap();
    assert!(bulk.to_string().contains("An order has a total above zero"), "bulk: {bulk}");
    let created = Invocation { effect_key: Some("k-1".into()), ..Invocation::new("orders.order.create", json!({"values": invalid})) };
    assert!(refused(&k.invoke(&clerk, created).await.unwrap_err()), "a capability's invocation (agents, processes)");
    automation::save(
        &k,
        &admin,
        &obj(json!({"name": "order on party", "entity": "Party", "event": "updated",
        "actions": [{"type": "create_document", "entity": "Order", "values": {"reference": "'AUTO'", "total": "'-1'"}}]})),
    )
    .await
    .unwrap();
    k.update(&admin, "Party", party.parse().unwrap(), obj(json!({"party_name": "Kumar Traders Ltd"})), None).await.unwrap();
    drain(&k).await;
    let why: String = {
        let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
        // A failed automation leaves its event to be tried again, saying why.
        let why: String =
            sqlx::query_scalar("select last_error from outbox where topic = 'Party.updated' and last_error is not null").fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        why
    };
    assert!(why.contains("An order has a total above zero"), "automation: {why}");
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    let job = ferrum_kernel::jobs::enqueue(
        &mut tx,
        admin.tenant,
        Some(admin.user),
        NewJob { kind: "orders.save".into(), payload: invalid.clone(), priority: 1, max_attempts: 1, dedupe_key: None, undo_on_cancel: false },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    k.run_jobs_once(1).await.unwrap();
    let ran = k.job(&admin, job).await.unwrap();
    assert!(ran.to_string().contains("An order has a total above zero"), "job: {ran}");
    assert_eq!(scalar(&k, admin.tenant, "select count(*) from e_order where total < 0").await, 0, "no path wrote it");

    // Two solutions filling one field cannot both run (FLC-HOOK-003).
    let rival = json!({"schema_version": "ferrum.solution/1", "kind": "Solution", "id": "acme.rival", "version": "1.0.0", "label": "Rival",
        "rules": [{"id": "band_too", "entity": "Order", "phase": "derive", "writes": {"x_band": "\"medium\""}}]});
    let rival = k.save_release(&admin, &rival.to_string()).await.unwrap();
    let rival_id: Uuid = rival["id"].as_str().unwrap().parse().unwrap();
    k.review_release(&reviewer, rival_id, rival["artifact_digest"].as_str().unwrap(), true, "").await.unwrap();
    let e = k.activate_release(&admin, rival_id, None).await.unwrap_err();
    assert!(e.to_string().contains("both fill Order.x_band"), "{e}");

    // A new version approved, then the workspace changes under it: what was approved is what runs, or
    // nothing does (FLC-COMP-005).
    let next = k.save_release(&admin, &solution("1.1.0", json!([]))).await.unwrap();
    let next_id: Uuid = next["id"].as_str().unwrap().parse().unwrap();
    k.review_release(&reviewer, next_id, next["artifact_digest"].as_str().unwrap(), true, "").await.unwrap();
    let flag = k.list_customizations(&admin).await.unwrap().into_iter().find(|c| c.op.to_string().contains("x_flag")).unwrap();
    k.set_customization_active(&admin, flag.id, false).await.unwrap();
    let admin = reload(&k, &admin).await;
    let e = k.activate_release(&admin, next_id, Some(generation)).await.unwrap_err();
    assert_eq!(code(&e), "stale_approval", "{e}");
    let clerk = reload(&k, &clerk).await;
    let e = k.insert(&clerk, "Order", obj(json!({"reference": "SO-7", "total": "5"}))).await.unwrap_err();
    assert_eq!(code(&e), "stale_approval", "the active release cannot run as approved, so its records wait: {e}");
    assert_eq!(code(&k.deactivate_solution(&admin, "acme.orders", generation + 1).await.unwrap_err()), "conflict");
    k.deactivate_solution(&admin, "acme.orders", generation).await.unwrap();
    let clerk = reload(&k, &clerk).await;
    k.insert(&clerk, "Order", obj(json!({"reference": "SO-8", "total": "0"}))).await.expect("no rules run once it is stopped");

    // Another workspace runs none of this, and sees none of it.
    let other = workspace(&k, "globex").await;
    k.insert(&other, "Order", obj(json!({"reference": "G-1", "total": "-5"}))).await.expect("another workspace's rules are its own");
    assert!(k.logic_releases(&other).await.unwrap().is_empty());
    assert_eq!(k.logic_releases(&admin).await.unwrap().len(), 3);
}
