//! Approval before one of an entity's actions runs (`on: {action: name}`, docs/builder/decisions.md BD-34), against a
//! real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first). A package's change with an app-handled
//! `apply` (a secret PIN among its parameters) whose policy the package ships, and a workspace's own button with
//! effects: pressing either asks; the last approval runs it once, as whoever pressed it, with what they gave; a
//! rejection or a recall runs nothing; a record that no longer meets the action's condition is refused then, in plain
//! words; a secret is never readable from the request or the audit trail; approval lends no rights.

use async_trait::async_trait;
use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, DocHook, Error, HookCtx, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const PIN: &str = "pin-7319-never-shown";

const PACKAGE: &str = "package: board\nversion: 0.1.0\ndepends: [core]\nroles:\n  - name: Changer\n    permissions:\n      - {entity: Change, read: true, write: true, create: true}\n  - name: Board\n    permissions:\n      - {entity: Change, read: true}\n\
approvals:\n  - entity: Change\n    on: {action: apply}\n    policy: {steps: [{label: The board, approvers: {role: Board}}]}\n";
const CHANGE: &str = "name: Change\nlabel: Change\nkind: master\ntitle_field: title\nfields:\n  - {name: title, type: data}\n  - {name: state, type: data}\n  - {name: bumps, type: int}\n\
actions:\n  - name: apply\n    label: Apply\n    condition: \"doc.state != 'Closed'\"\n    params:\n      - {name: window, label: Window}\n      - {name: pin, label: PIN, secret: true}\n  - {name: peek, label: Peek, read_only: true}\n";

/// The app: what `apply` was given, by whom.
#[derive(Default)]
struct Board {
    ran: Mutex<Vec<(String, Uuid, Value)>>,
}

impl Board {
    fn ran(&self, title: &str) -> Vec<(Uuid, Value)> {
        self.ran.lock().unwrap().iter().filter(|r| r.0 == title).map(|r| (r.1, r.2.clone())).collect()
    }
}

#[async_trait]
impl DocHook for Board {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if name != "apply" {
            return Ok(None);
        }
        self.ran.lock().unwrap().push((doc["title"].as_str().unwrap_or_default().into(), hc.ctx.user, params.clone()));
        Ok(Some(json!({"applied": true})))
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
fn approval_before_an_action() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/change.yaml", CHANGE)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let pkg = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let board = Arc::new(Board::default());
    let mut hooks = HookRegistry::default();
    hooks.register("Change", board.clone());
    let settings = KernelSettings { secret_key: Some(ferrum_kernel::secret::SecretKey::new([5u8; 32])), ..Default::default() };
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, pkg]).unwrap(), hooks, settings, None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["board".into()],
            settings: None,
        })
        .await
        .unwrap();
    let root = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap().user;
    let admin = k.load_ctx(tenant, root, None, "it".into()).await.unwrap();
    let batch = |items: Vec<NewCustomization>| {
        let (k, admin) = (k.clone(), admin.clone());
        async move { k.batch_customizations(&admin, items, false).await.map_err(|(at, e)| format!("{at:?}: {}", message(&e))) }
    };

    // ───────────── checked when saved ─────────────
    let any = json!({"steps": [{"label": "B", "approvers": {"role": "Board"}}]});
    for (action, says) in [("nope", "has no action nope"), ("peek", "changes nothing")] {
        let e = batch(vec![op("Change", json!({"op": "set_approval", "on": {"action": action}, "approval": any}))]).await.unwrap_err();
        assert!(e.contains(says), "{says}: {e}");
    }
    batch(vec![
        op("Change", json!({"op": "add_action", "action": {"name": "x_bump", "label": "Bump", "roles": ["Changer"],
            "effects": [{"type": "set_fields", "values": {"bumps": "(has(doc.bumps) && doc.bumps != null ? doc.bumps : 0) + 1"}}]}})),
        op("Change", json!({"op": "set_approval", "on": {"action": "x_bump"}, "approval": {"steps": [{"label": "Two of the board", "approvers": {"role": "Board"}, "need": 2}]}})),
    ])
    .await
    .expect("the workspace's policy");
    let declared = k.package_approvals_of(&admin, "Change").unwrap();
    assert_eq!(declared["approvals"][0]["on"], json!("action:apply"), "the package's, as customize lists it: {declared}");

    let person = |email: &'static str, roles: &'static [&'static str]| {
        let (k, admin) = (k.clone(), admin.clone());
        async move { k.create_user(&admin, email, email, Some("Correct-Horse-9"), &roles.iter().map(|r| r.to_string()).collect::<Vec<_>>(), false).await.unwrap() }
    };
    let as_ = |u: Uuid| {
        let k = k.clone();
        async move { k.load_ctx(tenant, u, None, "it".into()).await.unwrap() }
    };
    let changer = person("changer@acme.test", &["Changer"]).await;
    let boss1 = person("b1@acme.test", &["Board"]).await;
    let boss2 = person("b2@acme.test", &["Board"]).await;
    let (cc, b1, b2) = (as_(changer).await, as_(boss1).await, as_(boss2).await);
    let change = |title: &'static str| {
        let (k, cc) = (k.clone(), cc.clone());
        async move { id_of(&k.insert(&cc, "Change", obj(json!({"title": title, "state": "Open"}))).await.unwrap()) }
    };
    let decide = |c: Ctx, id: Uuid, yes: bool| {
        let k = k.clone();
        async move { k.decide_approval(&c, "Change", id, yes, Some("seen")).await }
    };
    let bumps = |id: Uuid| {
        let (k, admin) = (k.clone(), admin.clone());
        async move { k.get(&admin, "Change", id, false).await.unwrap()["bumps"].as_i64().unwrap_or(0) }
    };

    // ───────────── the package's policy: asked, parameters replayed exactly, the secret sealed ─────────────
    let c1 = change("C1").await;
    let given = json!({"window": "Sat 02:00", "pin": PIN});
    let asked = k.run_action(&cc, "Change", c1, "apply", given.clone()).await.unwrap();
    let a = &asked["_approval"];
    assert_eq!((a["status"].as_str(), a["on_action"].as_bool(), a["action"].as_str()), (Some("pending"), Some(true), Some("apply")), "{asked}");
    assert_eq!(a["params"], json!({"window": "Sat 02:00", "pin": "[redacted]"}), "the secret is not shown");
    assert!(board.ran("C1").is_empty(), "nothing runs while it is asked");
    refused(k.run_action(&cc, "Change", c1, "apply", given.clone()).await, "awaiting approval of 'apply'");
    refused(decide(cc.clone(), c1, true).await, "not yours to decide");
    let s = decide(b1.clone(), c1, true).await.unwrap();
    assert_eq!(s["status"], json!("approved"), "{s}");
    assert_eq!(board.ran("C1"), [(changer, given.clone())], "once, as the changer, with exactly what was given");
    for seen in
        [k.approval_of(&cc, "Change", c1).await.unwrap(), k.approval_of(&b1, "Change", c1).await.unwrap(), k.audit_trail(&admin, "Change", c1).await.unwrap()]
    {
        assert!(!seen.to_string().contains(PIN), "{seen}");
    }
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let rows: Vec<String> =
        sqlx::query_scalar("select t::text from audit_event t union all select r.params::text || coalesce(r.sealed::text, '') from approval_request r")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    assert!(rows.iter().any(|r| r.contains("[redacted]")) && rows.iter().all(|r| !r.contains(PIN)), "no table keeps the secret in the clear");

    // ───────────── a rejection and a recall run nothing ─────────────
    let c2 = change("C2").await;
    k.run_action(&cc, "Change", c2, "apply", json!({"window": "now"})).await.unwrap();
    assert_eq!(decide(b1.clone(), c2, false).await.unwrap()["status"], json!("rejected"));
    k.run_action(&cc, "Change", c2, "apply", json!({"window": "later"})).await.expect("asked again");
    assert_eq!(k.recall_approval(&cc, "Change", c2).await.unwrap()["status"], json!("recalled"));
    refused(decide(b1.clone(), c2, true).await, "an approval awaiting a decision");
    assert!(board.ran("C2").is_empty(), "never ran");

    // ───────────── the record no longer meets the action's condition: refused then, the requester told ─────────────
    let c3 = change("C3").await;
    k.run_action(&cc, "Change", c3, "apply", json!({})).await.unwrap();
    k.update(&cc, "Change", c3, obj(json!({"state": "Closed"})), None).await.unwrap();
    let s = decide(b1.clone(), c3, true).await.expect("the decision is kept");
    assert_eq!(s["status"], json!("failed"), "{s}");
    assert!(s["failure"].as_str().is_some_and(|f| f.contains("'Apply' is not available for this document")), "{s}");
    assert!(board.ran("C3").is_empty());
    let told: i64 = sqlx::query_scalar("select count(*) from notification where user_id = $1 and title like 'Approved, but not done: apply%'")
        .bind(changer)
        .fetch_one(&mut *k.db.tenant_tx(tenant).await.unwrap())
        .await
        .unwrap();
    assert_eq!(told, 1, "the requester is told");

    // ───────────── a workspace's button with effects: two of the board at the same moment run it once ─────────────
    let c4 = change("C4").await;
    k.run_action(&cc, "Change", c4, "x_bump", Value::Null).await.unwrap();
    assert_eq!(bumps(c4).await, 0, "asked, not run");
    decide(b1.clone(), c4, true).await.unwrap();
    assert_eq!(bumps(c4).await, 0, "one of two");
    decide(b2.clone(), c4, true).await.unwrap();
    assert_eq!(bumps(c4).await, 1, "ran once");
    let c5 = change("C5").await;
    k.run_action(&cc, "Change", c5, "x_bump", Value::Null).await.unwrap();
    let (one, two) = tokio::join!(decide(b1.clone(), c5, true), decide(b2.clone(), c5, true));
    let (one, two) = (one.expect("first"), two.expect("second"));
    assert!([&one, &two].iter().any(|s| s["status"] == json!("approved")), "{one} {two}");
    let c6 = change("C6").await;
    k.run_action(&cc, "Change", c6, "apply", json!({})).await.unwrap();
    let (one, two) = tokio::join!(decide(b1.clone(), c6, true), decide(b2.clone(), c6, true));
    assert!([one, two].into_iter().flatten().any(|s| s["status"] == json!("approved")), "one of them completed it");
    assert_eq!((bumps(c5).await, board.ran("C6").len()), (1, 1), "each ran once");

    // ───────────── approval lends no rights ─────────────
    let c7 = change("C7").await;
    refused(k.run_action(&b1, "Change", c7, "x_bump", Value::Null).await, "not permitted to write");
    let e = k.run_action(&b1, "Change", c7, "apply", json!({})).await.expect_err("the board may not write a change");
    assert!(matches!(e, Error::Forbidden(_) | Error::NotFound(_)), "{e}");
    k.run_action(&cc, "Change", c7, "x_bump", Value::Null).await.unwrap();
    k.set_user_roles(&admin, changer, &[]).await.unwrap();
    decide(b1.clone(), c7, true).await.unwrap();
    let s = decide(b2.clone(), c7, true).await.expect("the decision is kept");
    assert_eq!(s["status"], json!("failed"), "the requester lost the right: {s}");
    assert_eq!(bumps(c7).await, 0, "not run, by anyone");

    // ───────────── the server's key rotated while a request waits: its sealed parameters move with the rest ─────────────
    k.set_user_roles(&admin, changer, &["Changer".into()]).await.unwrap();
    let cc = as_(changer).await;
    let c8 = change("C8").await;
    k.run_action(&cc, "Change", c8, "apply", json!({"window": "Sun", "pin": PIN})).await.unwrap();
    let boot = |current: [u8; 32], retired: Option<[u8; 32]>| {
        let board = board.clone();
        async move {
            let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
            let db = Db::connect(DbConfig { url, max_connections: 4, ..Default::default() }).await.expect("connect");
            let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
            let files: BTreeMap<String, String> =
                [("package.yaml", PACKAGE), ("entities/change.yaml", CHANGE)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            let pkg = ferrum_meta::catalog::load_package_files(&files).expect("package");
            let mut hooks = HookRegistry::default();
            hooks.register("Change", board);
            let settings = KernelSettings {
                secret_key: Some(ferrum_kernel::secret::SecretKey::new(current)),
                retired_secret_keys: retired.into_iter().map(ferrum_kernel::secret::SecretKey::new).collect(),
                ..Default::default()
            };
            Kernel::boot(db, Catalog::from_packages(vec![core, pkg]).unwrap(), hooks, settings, None).await.expect("boot")
        }
    };
    let rotating = boot([6u8; 32], Some([5u8; 32])).await;
    assert!(rotating.reseal_secrets().await.unwrap().moved >= 1, "the waiting PIN moved to the new key");
    let only_new = boot([6u8; 32], None).await;
    let b1n = only_new.load_ctx(tenant, boss1, None, "it".into()).await.unwrap();
    let s = only_new.decide_approval(&b1n, "Change", c8, true, Some("ok")).await.unwrap();
    assert_eq!(s["status"], json!("approved"), "{s}");
    assert_eq!(board.ran("C8"), [(changer, json!({"window": "Sun", "pin": PIN}))], "replayed under the new key alone");
}
