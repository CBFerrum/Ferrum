//! Approval before a document is submitted or cancelled (`set_approval`, docs/builder/decisions.md BD-19), against a
//! real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first). A package's hold, with hooks that check
//! and post on submit, and a workspace's own bill: Submit asks the bosses; when they agree the submit completes once,
//! as the clerk who pressed it, with the app's hooks; a change of quantity starts again; a locked bill is not edited
//! meanwhile; a clerk who lost the right gets a failed request, not a submit by the system.

use async_trait::async_trait;
use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, DocHook, Error, HookCtx, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::{Catalog, PermissionDef};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const PACKAGE: &str = "package: held\nversion: 0.1.0\ndepends: [core]\nroles:\n  - name: Clerk\n    permissions:\n      - {entity: Hold, read: true, write: true, create: true, submit: true, cancel: true}\n  - name: Boss\n    permissions:\n      - {entity: Hold, read: true}\n";
const ENTITIES: &str = "name: Hold\nlabel: Hold\nkind: document\nsubmittable: true\ntitle_field: reference\nfields:\n  - {name: reference, type: data, required: true}\n  - {name: qty, type: int}\n  - {name: note, type: data}\n";

/// The app: no hold of more than there is, and what ran on submit and cancel, as whom.
#[derive(Default)]
struct Stock {
    ran: Mutex<Vec<(String, String, Uuid, bool)>>,
}

impl Stock {
    fn ran(&self, what: &str, reference: &str) -> Vec<(Uuid, bool)> {
        self.ran.lock().unwrap().iter().filter(|r| r.0 == what && r.1 == reference).map(|r| (r.2, r.3)).collect()
    }

    fn note(&self, what: &str, hc: &HookCtx<'_>, doc: &Doc) {
        self.ran.lock().unwrap().push((what.into(), doc["reference"].as_str().unwrap_or_default().into(), hc.ctx.user, hc.ctx.is_system));
    }
}

#[async_trait]
impl DocHook for Stock {
    async fn validate(&self, _: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if doc.get("qty").and_then(Value::as_i64).unwrap_or(0) > 10 {
            return Err(Error::field("qty", "is more than there is"));
        }
        Ok(())
    }

    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        self.note("before_submit", hc, doc);
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        self.note("on_submit", hc, doc);
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        self.note("on_cancel", hc, doc);
        Ok(())
    }
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn op(entity: &str, op: Value) -> NewCustomization {
    serde_json::from_value(json!({"entity": entity, "op": op})).unwrap()
}

fn message(e: &Error) -> String {
    match e {
        Error::Validation(v) => v.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

#[track_caller]
fn refused<T: std::fmt::Debug>(r: Result<T, Error>, says: &str) {
    let e = r.expect_err(says);
    assert!(message(&e).contains(says), "{says}: {}", message(&e));
}

#[test]
fn approval_before_submit_and_cancel() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
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
    let root = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap().user;
    let as_ = |u: Uuid| {
        let k = k.clone();
        async move { k.load_ctx(tenant, u, None, "it".into()).await.unwrap() }
    };
    let batch = |items: Vec<NewCustomization>| {
        let k = k.clone();
        async move {
            let admin = k.load_ctx(tenant, root, None, "it".into()).await.unwrap();
            k.batch_customizations(&admin, items, false).await.map_err(|(at, e)| format!("{at:?}: {}", message(&e)))
        }
    };
    let bosses = json!({"steps": [{"label": "Bosses", "approvers": {"role": "Boss"}, "need": 2}], "reset_on": ["qty"]});

    // ───────────── checked when saved ─────────────
    for (entity, policy, says) in [
        ("Hold", json!({"steps": [{"label": "B", "approvers": {"role": "Nope"}}]}), "submit, approval step 1: there is no role Nope"),
        ("Hold", json!({"steps": [{"label": "B", "approvers": {"role": "Boss"}}], "on_reject": "Draft"}), "a rejected submit or cancel leaves the document"),
        ("Hold", json!({"steps": [{"label": "B", "approvers": {"role": "Boss"}, "not_who_took": ["Go"]}]}), "not_who_took: Go is not a transition"),
        ("Uom", json!({"steps": [{"label": "B", "approvers": {"role": "Boss"}}]}), "is not submittable"),
    ] {
        let e = batch(vec![op(entity, json!({"op": "set_approval", "on": "submit", "approval": policy}))]).await.unwrap_err();
        assert!(e.contains(says), "{says}: {e}");
    }
    batch(vec![
        op("Hold", json!({"op": "set_approval", "on": "submit", "approval": bosses})),
        op("Hold", json!({"op": "set_approval", "on": "cancel", "approval": {"steps": [{"label": "A boss", "approvers": {"role": "Boss"}}]}})),
        op(
            "XBill",
            json!({"op": "define_entity", "entity": {"name": "XBill", "label": "Bill", "kind": "document", "submittable": true, "title_field": "title",
                "fields": [{"name": "title", "type": "data"}, {"name": "amount", "type": "int"}, {"name": "note", "type": "data"}]}}),
        ),
        op(
            "XBill",
            json!({"op": "set_approval", "on": "submit", "approval": {"steps": [{"label": "A boss", "approvers": {"role": "Boss"}}],
                "reset_on": ["amount"], "lock": true, "system_bypass": true}}),
        ),
    ])
    .await
    .expect("the policies");
    let admin = as_(root).await;
    let bill_rights = PermissionDef { entity: "XBill".into(), read: true, write: true, create: true, submit: true, cancel: true, ..Default::default() };
    k.set_role_permission(&admin, "Clerk", &bill_rights).await.unwrap();
    k.set_role_permission(&admin, "Boss", &PermissionDef { entity: "XBill".into(), read: true, ..Default::default() }).await.unwrap();
    let def = k.custom_definition(&admin, "XBill").unwrap();
    assert_eq!(def["approvals"]["submit"]["lock"], json!(true), "the definition carries it: {def}");
    let person = |email: &'static str, roles: &'static [&'static str]| {
        let k = k.clone();
        let admin = admin.clone();
        async move { k.create_user(&admin, email, email, Some("Correct-Horse-9"), &roles.iter().map(|r| r.to_string()).collect::<Vec<_>>(), false).await.unwrap() }
    };
    let clerk = person("clerk@acme.test", &["Clerk"]).await;
    let boss1 = person("boss1@acme.test", &["Boss"]).await;
    let boss2 = person("boss2@acme.test", &["Boss"]).await;
    let (clerkc, b1, b2) = (as_(clerk).await, as_(boss1).await, as_(boss2).await);
    let hold = |reference: &'static str, qty: i64| {
        let (k, clerkc) = (k.clone(), clerkc.clone());
        async move { id_of(&k.insert(&clerkc, "Hold", obj(json!({"reference": reference, "qty": qty}))).await.unwrap()) }
    };
    let status = |entity: &'static str, id: Uuid| {
        let (k, admin) = (k.clone(), admin.clone());
        async move { k.get(&admin, entity, id, false).await.unwrap()["docstatus"].as_i64().unwrap() }
    };
    let decide = |c: Ctx, entity: &'static str, id: Uuid, yes: bool| {
        let k = k.clone();
        async move { k.decide_approval(&c, entity, id, yes, Some("seen")).await }
    };

    // ───────────── asked, approved by two, submitted once as the clerk with the app's hooks ─────────────
    let h1 = hold("H1", 5).await;
    let asked = k.submit(&clerkc, "Hold", h1, None).await.unwrap();
    assert_eq!((asked["docstatus"].as_i64(), asked["_approval"]["status"].as_str()), (Some(0), Some("pending")), "{asked:?}");
    assert_eq!((asked["_approval"]["document"].as_bool(), asked["_approval"]["action"].as_str()), (Some(true), Some("submit")));
    assert!(stock.ran("before_submit", "H1").is_empty(), "nothing of the submit runs while it is asked");
    refused(k.submit(&clerkc, "Hold", h1, None).await, "awaiting approval of 'submit'");
    refused(decide(clerkc.clone(), "Hold", h1, true).await, "not yours to decide");
    let s = decide(b1.clone(), "Hold", h1, true).await.unwrap();
    assert_eq!((s["status"].as_str(), status("Hold", h1).await), (Some("pending"), 0), "one of two");
    let s = decide(b2.clone(), "Hold", h1, true).await.unwrap();
    assert_eq!((s["status"].as_str(), status("Hold", h1).await), (Some("approved"), 1));
    assert_eq!(stock.ran("before_submit", "H1"), [(clerk, false)], "the app's hooks, as the clerk, not the system");
    assert_eq!(stock.ran("on_submit", "H1"), [(clerk, false)], "once");
    assert_eq!(k.get(&admin, "Hold", h1, false).await.unwrap()["updated_by"], json!(clerk.to_string()));
    refused(decide(b1.clone(), "Hold", h1, true).await, "an approval awaiting a decision");

    // ───────────── two bosses at the same moment complete it once ─────────────
    let h2 = hold("H2", 3).await;
    k.submit(&clerkc, "Hold", h2, None).await.unwrap();
    let (one, two) = tokio::join!(decide(b1.clone(), "Hold", h2, true), decide(b2.clone(), "Hold", h2, true));
    let (one, two) = (one.expect("first"), two.expect("second"));
    assert!([&one, &two].iter().any(|s| s["status"] == json!("approved")), "{one} {two}");
    assert_eq!((status("Hold", h2).await, stock.ran("on_submit", "H2").len()), (1, 1), "submitted once");

    // ───────────── rejected, recalled ─────────────
    let h3 = hold("H3", 1).await;
    k.submit(&clerkc, "Hold", h3, None).await.unwrap();
    let s = decide(b1.clone(), "Hold", h3, false).await.unwrap();
    assert_eq!((s["status"].as_str(), status("Hold", h3).await), (Some("rejected"), 0));
    k.submit(&clerkc, "Hold", h3, None).await.expect("asked again");
    assert_eq!(k.recall_approval(&clerkc, "Hold", h3).await.unwrap()["status"], json!("recalled"));
    refused(decide(b1.clone(), "Hold", h3, true).await, "an approval awaiting a decision");
    assert!(stock.ran("before_submit", "H3").is_empty());

    // ───────────── a change of quantity voids the decisions; another edit does not ─────────────
    k.submit(&clerkc, "Hold", h3, None).await.unwrap();
    decide(b1.clone(), "Hold", h3, true).await.unwrap();
    k.update(&clerkc, "Hold", h3, obj(json!({"note": "by van"})), None).await.expect("not locked");
    assert_eq!(k.approval_of(&clerkc, "Hold", h3).await.unwrap()["decisions"][0]["void"], json!(false));
    k.update(&clerkc, "Hold", h3, obj(json!({"qty": 2})), None).await.unwrap();
    let s = k.approval_of(&clerkc, "Hold", h3).await.unwrap();
    assert_eq!((s["decisions"][0]["void"].as_bool(), s["status"].as_str()), (Some(true), Some("pending")), "{s}");
    decide(b1.clone(), "Hold", h3, true).await.expect("asked again of the same boss");
    assert_eq!(status("Hold", h3).await, 0, "the voided decision does not count");

    // ───────────── the clerk lost the right: the request fails, nothing is submitted ─────────────
    let h4 = hold("H4", 2).await;
    k.submit(&clerkc, "Hold", h4, None).await.unwrap();
    k.set_user_roles(&admin, clerk, &[]).await.unwrap();
    decide(b1.clone(), "Hold", h4, true).await.unwrap();
    let s = decide(b2.clone(), "Hold", h4, true).await.expect("the decision is kept");
    assert_eq!(s["status"], json!("failed"), "{s}");
    assert!(s["failure"].as_str().is_some_and(|f| f.contains("not permitted to submit")), "{s}");
    assert_eq!((status("Hold", h4).await, stock.ran("before_submit", "H4").len()), (0, 0), "not submitted, by anyone");
    k.set_user_roles(&admin, clerk, &["Clerk".into()]).await.unwrap();
    let clerkc = as_(clerk).await;

    // ───────────── creating and submitting at once, and the system, go through the same gate ─────────────
    let made = k.insert_submit(&clerkc, "Hold", obj(json!({"reference": "H5", "qty": 1}))).await.unwrap();
    assert_eq!((made["docstatus"].as_i64(), made["_approval"]["status"].as_str()), (Some(0), Some("pending")));
    let system = Ctx::system(tenant, admin.schema.clone());
    let h6 = hold("H6", 1).await;
    let s = k.submit(&system, "Hold", h6, None).await.unwrap();
    assert_eq!(s["_approval"]["requester"], Value::Null, "no bypass: the system asks too");

    // ───────────── cancel has its own policy ─────────────
    let c = k.cancel(&clerkc, "Hold", h1, None).await.unwrap();
    assert_eq!((c["docstatus"].as_i64(), c["_approval"]["action"].as_str()), (Some(1), Some("cancel")));
    decide(b1.clone(), "Hold", h1, true).await.unwrap();
    assert_eq!(status("Hold", h1).await, 2);
    assert_eq!(stock.ran("on_cancel", "H1"), [(clerk, false)]);

    // ───────────── a workspace's own document, locked while asked; the system goes through where it may ─────────────
    let bill = id_of(&k.insert(&clerkc, "XBill", obj(json!({"title": "Rent", "amount": 900}))).await.unwrap());
    k.submit(&clerkc, "XBill", bill, None).await.unwrap();
    refused(k.update(&clerkc, "XBill", bill, obj(json!({"note": "late"})), None).await, "awaiting approval of 'submit' and stays as it is");
    k.update(&clerkc, "XBill", bill, obj(json!({"amount": 950})), None).await.expect("a reset_on field may change");
    let s = decide(b1.clone(), "XBill", bill, true).await.unwrap();
    assert_eq!((s["status"].as_str(), status("XBill", bill).await), (Some("approved"), 1));
    let auto = id_of(&k.insert(&clerkc, "XBill", obj(json!({"title": "Auto", "amount": 5}))).await.unwrap());
    let s = k.submit(&system, "XBill", auto, None).await.unwrap();
    assert_eq!((s["docstatus"].as_i64(), s.get("_approval").is_none()), (Some(1), true), "system_bypass");
}
