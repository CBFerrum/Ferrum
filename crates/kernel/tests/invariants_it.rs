//! AT-EXT-010: what no automation can talk its way past, against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first). An automation runs as the
//! administrator who saved it, so each rule here is one that holds for an administrator (and the
//! system) too, or the automation would be a way around it; and it stops when they are not one.

use async_trait::async_trait;
use ferrum_kernel::automation;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, DocHook, Error, HookCtx, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const PACKAGE: &str = "package: held\nversion: 0.1.0\ndepends: [core]\nroles:\n  - name: Clerk\n    permissions:\n      - {entity: Hold, read: true, write: true, create: true, submit: true}\n";
const ENTITIES: &str = "name: Hold\nlabel: Hold\nkind: document\nsubmittable: true\ntitle_field: reference\nfields:\n  - {name: reference, type: data, required: true}\n  - {name: qty, type: int}\n  - {name: note, type: data}\n";

/// What the business depends on: a hold of no more than there is, and a posting for every submission.
#[derive(Default)]
struct Stock {
    posted: Mutex<Vec<String>>,
}

#[async_trait]
impl DocHook for Stock {
    async fn validate(&self, _: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if doc.get("qty").and_then(Value::as_i64).unwrap_or(0) > 10 {
            return Err(Error::field("qty", "is more than there is"));
        }
        Ok(())
    }

    async fn on_submit(&self, _: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        self.posted.lock().unwrap().push(doc["reference"].as_str().unwrap().to_string());
        Ok(())
    }
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

async fn drain(k: &Kernel) {
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
}

#[test]
fn an_automation_goes_through_what_everyone_goes_through() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn outcome(k: &Kernel, ctx: &Ctx, name: &str) -> i64 {
    let mut tx = k.db.tenant_tx(ctx.tenant).await.unwrap();
    let done: i64 =
        sqlx::query_scalar("select count(*) from automation_run r join automation a on a.id = r.automation_id where a.name = $1 and r.status = 'done'")
            .bind(name)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    done
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml".to_string(), PACKAGE.to_string()), ("entities/hold.yaml".to_string(), ENTITIES.to_string())].into_iter().collect();
    let held = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let stock = Arc::new(Stock::default());
    let mut hooks = HookRegistry::default();
    hooks.register("Hold", stock.clone());
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, held]).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["held".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let clerk = k.create_user(&admin, "clerk@acme.test", "Clerk", Some("Correct-Horse-9"), &["Clerk".to_string()], false).await.unwrap();
    let clerk = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();

    // Saying what runs as the system is an administrator's, and it says only what automations can do.
    let auto = |name: &str, event: &str, action: Value| obj(json!({"name": name, "entity": "Hold", "event": event, "actions": [action]}));
    let edit = json!({"type": "set_fields", "values": {"note": "'seen'"}});
    assert!(matches!(automation::save(&k, &clerk, &auto("mine", "created", edit.clone())).await, Err(Error::Forbidden(_))));
    for attempt in [
        json!({"type": "sql", "statement": "update e_hold set qty = 99"}),
        json!({"type": "set_fields", "values": {"qty": "99"}, "as": "admin"}),
        json!({"type": "submit"}),
        json!({"type": "delete"}),
    ] {
        assert!(automation::save(&k, &admin, &auto("raw", "created", attempt.clone())).await.is_err(), "{attempt}");
    }

    // That it works at all, so that what follows means something.
    automation::save(&k, &admin, &auto("notes", "created", edit)).await.expect("saved");
    let first = k.insert(&clerk, "Hold", obj(json!({"reference": "A", "qty": 5}))).await.unwrap();
    drain(&k).await;
    assert_eq!(k.get(&admin, "Hold", id(&first), false).await.unwrap()["note"], json!("seen"));
    assert_eq!(outcome(&k, &admin, "notes").await, 1);
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update automation set is_active = false").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();

    // A hold beyond the stock, on the record itself or a new one.
    automation::save(&k, &admin, &auto("oversell", "created", json!({"type": "set_fields", "values": {"qty": "99"}}))).await.unwrap();
    automation::save(&k, &admin, &auto("spawn", "created", json!({"type": "create_document", "entity": "Hold", "values": {"reference": "'B'", "qty": "99"}})))
        .await
        .unwrap();
    // A posting skipped by writing the state instead of submitting.
    automation::save(&k, &admin, &auto("skip the posting", "updated", json!({"type": "set_fields", "values": {"docstatus": "1"}}))).await.unwrap();
    let second = k.insert(&clerk, "Hold", obj(json!({"reference": "C", "qty": 7}))).await.unwrap();
    k.update(&clerk, "Hold", id(&second), obj(json!({"note": "changed"})), None).await.unwrap();
    drain(&k).await;
    let now = k.get(&admin, "Hold", id(&second), false).await.unwrap();
    assert_eq!(now["qty"], json!(7), "the hold stands");
    assert_eq!(now["docstatus"], json!(0), "and it is not submitted: {now:?}");
    assert!(stock.posted.lock().unwrap().is_empty());
    for name in ["oversell", "spawn"] {
        assert_eq!(outcome(&k, &admin, name).await, 0, "{name} did nothing");
    }
    let refs = k.list(&admin, "Hold", &Default::default()).await.unwrap();
    assert!(!format!("{refs:?}").contains("\"B\""), "{refs:?}");

    // What is submitted stays as submitted, for the system too.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update automation set is_active = false").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    automation::save(&k, &admin, &auto("rewrite", "submitted", json!({"type": "set_fields", "values": {"qty": "1"}}))).await.unwrap();
    k.submit(&clerk, "Hold", id(&second), None).await.expect("submitted");
    drain(&k).await;
    assert_eq!(stock.posted.lock().unwrap().as_slice(), ["C"], "the one real submission, posted");
    assert_eq!(k.get(&admin, "Hold", id(&second), false).await.unwrap()["qty"], json!(7));
    assert_eq!(outcome(&k, &admin, "rewrite").await, 0);

    // It runs as the administrator who saved it, read when it runs: once they are not one, it does
    // not run and says why, until an administrator saves it again (FERRUM-IDENTITY-015).
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update automation set is_active = false").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let boss_id = k.create_user(&admin, "boss@acme.test", "Boss", Some("Correct-Horse-9"), &[], true).await.unwrap();
    let boss = k.load_ctx(tenant, boss_id, None, "it".into()).await.unwrap();
    let signed = obj(json!({"name": "signed", "entity": "Hold", "event": "created", "actions": [{"type": "set_fields", "values": {"note": "user.email"}}]}));
    let signed_id = automation::save(&k, &boss, &signed).await.unwrap();
    let third = k.insert(&clerk, "Hold", obj(json!({"reference": "D", "qty": 1}))).await.unwrap();
    drain(&k).await;
    assert_eq!(k.get(&admin, "Hold", id(&third), false).await.unwrap()["note"], json!("boss@acme.test"), "as its saver");
    k.set_user_active(&admin, boss_id, false).await.unwrap();
    let fourth = k.insert(&clerk, "Hold", obj(json!({"reference": "E", "qty": 1}))).await.unwrap();
    drain(&k).await;
    assert_eq!(k.get(&admin, "Hold", id(&fourth), false).await.unwrap()["note"], Value::Null, "not run for one who has left");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let why: String = sqlx::query_scalar("select error from automation_run where automation_id = $1 and status = 'failed'")
        .bind(signed_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(why.contains("an administrator saves it again"), "{why}");
    let mut again = signed.clone();
    again.insert("id".into(), json!(signed_id.to_string()));
    automation::save(&k, &admin, &again).await.unwrap();
    let fifth = k.insert(&clerk, "Hold", obj(json!({"reference": "F", "qty": 1}))).await.unwrap();
    drain(&k).await;
    assert_eq!(k.get(&admin, "Hold", id(&fifth), false).await.unwrap()["note"], json!("admin@acme.test"), "saved again, as its new saver");
}
