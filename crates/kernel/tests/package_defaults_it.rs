//! What a package ships that a workspace used to set up by hand: approval policies on submitting its documents and on
//! its workflow's transitions, and automations (one on a schedule), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first). Each is a default the workspace switches off, replaces or
//! edits; a release changes what the workspace left alone and flags what it edited; a policy or automation naming what
//! the package neither ships nor requires is refused when the package is loaded.

use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings, automation};
use ferrum_meta::{Catalog, Package};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use uuid::Uuid;

const MANIFEST: &str = r#"package: work
version: VERSION
depends: [core]
roles:
  - name: Clerk
    permissions:
      - {entity: Timesheet, read: true, write: true, create: true, submit: true}
      - {entity: Change, read: true, write: true, create: true}
  - name: Boss
    permissions:
      - {entity: Timesheet, read: true}
      - {entity: Change, read: true, write: true}
messages:
  - key: work.nudge
    languages:
      en:
        email: {subject: "Your timesheet", body: "<p>Please fill it in</p>"}
workflows:
  - name: Changes
    entity: Change
    initial_state: Draft
    states: [{name: Draft}, {name: Done}]
    transitions: [{action: Close, from: Draft, to: Done, approval: {steps: [{label: A boss, approvers: {role: Boss}}]}}]
approvals:
  - {entity: Timesheet, on: submit, policy: {steps: [{label: LABEL, approvers: {role: Boss}}]}}
automations:
  - {name: stamp, entity: Timesheet, event: created, actions: [{type: set_fields, values: {note: "STAMP"}}]}
  - {name: nudge, entity: Timesheet, event: scheduled, schedule: {every: {period: day, at: "09:00"}}, CONDITION
     actions: [{type: message, template: work.nudge, to: owner}]}
"#;
const ENTITIES: &str = r#"
- name: Timesheet
  label: Timesheet
  kind: document
  submittable: true
  fields:
    - {name: hours, type: int}
    - {name: note, type: data}
- name: Change
  label: Change
  kind: master
  fields:
    - {name: title, type: data}
"#;

fn work(version: &str, label: &str, stamp: &str, condition: &str, swap: (&str, &str)) -> Result<Package, String> {
    let manifest = MANIFEST.replace("VERSION", version).replace("LABEL", label).replace("STAMP", stamp).replace("CONDITION", condition).replace(swap.0, swap.1);
    let files: BTreeMap<String, String> =
        [("package.yaml", manifest.as_str()), ("entities/work.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    ferrum_meta::catalog::load_package_files(&files).map_err(|e| e.to_string())
}

fn core() -> Package {
    ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core")
}

fn catalog(p: Package) -> Result<Catalog, String> {
    Catalog::from_packages(vec![core(), p]).map_err(|e| e.to_string())
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn op(entity: &str, op: Value) -> NewCustomization {
    serde_json::from_value(json!({"entity": entity, "op": op})).unwrap()
}

async fn rows(k: &Kernel, tenant: Uuid) -> Vec<Value> {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let mut out: Vec<Value> = sqlx::query_scalar("select to_jsonb(a) from automation a where tenant_id = $1 and package = 'work' order by package_key")
        .bind(tenant)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    automation::package_marks(&k.registry.catalog(), &mut out);
    out
}

#[test]
fn packages_ship_approvals_and_automations() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let v1 = || work("0.1.0", "The boss", "'stamped'", "", ("", ""));
    // ───────────── refused when loaded, naming the package and the entity ─────────────
    for (swap, says) in [
        (("approvers: {role: Boss}}]}}\nautomations", "approvers: {role: Ghost}}]}}\nautomations"), "work: Timesheet: approval on submit names role Ghost"),
        (("template: work.nudge", "template: work.gone"), "work: Timesheet: automation nudge names template work.gone"),
        (("{label: A boss, approvers: {role: Boss}}", "{label: A boss, approvers: {role: Boss}, calendar: Office}"), "names calendar Office"),
        (("entity: Timesheet, event: created", "entity: Company, event: created"), "Company is not this package's entity"),
        (("{note: \"'stamped'\"}", "{note: \"settings.XNothing.note\"}"), "names settings entity XNothing"),
    ] {
        let e = catalog(work("0.1.0", "The boss", "'stamped'", "", swap).unwrap()).expect_err(says);
        assert!(e.contains(says), "{says}: {e}");
    }

    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(v1().unwrap()).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "work".into(),
            name: "work".into(),
            admin_email: "admin@work.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["work".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("work", "admin@work.test", "Correct-Horse-9").await.unwrap();
    let admin = || {
        let k = k.clone();
        async move { k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap() }
    };
    let a = admin().await;
    let clerk = k.create_user(&a, "clerk@work.test", "Clerk", Some("Correct-Horse-9"), &["Clerk".into()], false).await.unwrap();
    k.create_user(&a, "boss@work.test", "Boss", Some("Correct-Horse-9"), &["Boss".into()], false).await.unwrap();
    let clerk: Ctx = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    let drain = || {
        let k = k.clone();
        async move { while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {} }
    };

    // ───────────── installed: the automations run, submit and the transition ask the bosses ─────────────
    let installed = rows(&k, tenant).await;
    assert_eq!(
        installed.iter().map(|r| (r["package_key"].as_str().unwrap(), r["is_active"].as_bool().unwrap())).collect::<Vec<_>>(),
        [("nudge", true), ("stamp", true)]
    );
    assert!(installed.iter().all(|r| r["created_by"].is_null() && r["edited"] == json!(false)), "the system's, as shipped: {installed:?}");
    let t1 = k.insert(&clerk, "Timesheet", obj(json!({"hours": 8}))).await.unwrap();
    let t1: Uuid = t1["id"].as_str().unwrap().parse().unwrap();
    drain().await;
    assert_eq!(k.get(&a, "Timesheet", t1, false).await.unwrap()["note"], "stamped", "the package's automation ran");
    let asked = k.submit(&clerk, "Timesheet", t1, None).await.unwrap();
    assert_eq!((asked["docstatus"].as_i64(), asked["_approval"]["status"].as_str()), (Some(0), Some("pending")), "{asked:?}");
    let c1 = k.insert(&clerk, "Change", obj(json!({"title": "Swap the router"}))).await.unwrap();
    let c1: Uuid = c1["id"].as_str().unwrap().parse().unwrap();
    let moved = k.transition(&clerk, "Change", c1, "Close", None).await.unwrap();
    assert_eq!((moved["workflow_state"].as_str(), moved["_approval"]["status"].as_str()), (Some("Draft"), Some("pending")), "{moved:?}");
    let shown = k.package_approvals_of(&a, "Timesheet").unwrap();
    assert_eq!((shown["approvals"][0]["source"].clone(), shown["approvals"][0]["changed"].clone()), (json!("package"), json!(false)), "{shown}");

    // ───────────── the workspace switches them off, replaces one, edits an automation ─────────────
    k.batch_customizations(
        &a,
        vec![
            op("Timesheet", json!({"op": "set_approval", "on": "submit", "approval": null})),
            op("Change", json!({"op": "set_transition_approval", "action": "Close", "from": "Draft", "approval": null})),
        ],
        false,
    )
    .await
    .map_err(|e| e.1)
    .expect("switched off");
    let (a, clerk) = (admin().await, k.load_ctx(tenant, clerk.user, None, "it".into()).await.unwrap());
    let t2 = k.insert(&clerk, "Timesheet", obj(json!({"hours": 4}))).await.unwrap();
    let t2: Uuid = t2["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(k.submit(&clerk, "Timesheet", t2, None).await.unwrap()["docstatus"], json!(1), "no approval asked");
    let c2 = k.insert(&clerk, "Change", obj(json!({"title": "Patch"}))).await.unwrap();
    let c2: Uuid = c2["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(k.transition(&clerk, "Change", c2, "Close", None).await.unwrap()["workflow_state"], json!("Done"));
    let shown = k.package_approvals_of(&a, "Timesheet").unwrap();
    assert_eq!((shown["approvals"][0]["changed"].clone(), shown["approvals"][0]["workspace"].clone()), (json!(true), Value::Null), "{shown}");
    let mut stamp = installed[1].as_object().cloned().unwrap();
    stamp.insert("actions".into(), json!([{"type": "set_fields", "values": {"note": "'mine'"}}]));
    automation::save(&k, &a, &stamp).await.expect("edited");
    let mut nudge = installed[0].as_object().cloned().unwrap();
    nudge.insert("is_active".into(), json!(false));
    automation::save(&k, &a, &nudge).await.expect("switched off");

    // ───────────── a release changes the defaults: what the workspace set stays, the rest follows ─────────────
    let v2 = work("0.2.0", "The bosses", "'stamped v2'", "condition: \"doc.hours > 0\",", ("", "")).unwrap();
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    let k2 = Kernel::boot(db, catalog(v2).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("the release");
    k2.install_packages(tenant, &["work".into()]).await.expect("upgraded");
    let after = rows(&k2, tenant).await;
    assert_eq!(after[1]["actions"][0]["values"]["note"], "'mine'", "the edited one is the workspace's");
    assert_eq!((after[1]["edited"].clone(), after[1]["default_changed"].clone()), (json!(true), json!(true)), "and flagged");
    assert_eq!((after[0]["condition"].as_str(), after[0]["is_active"].as_bool()), (Some("doc.hours > 0"), Some(false)), "changed, still off");
    assert_eq!((after[0]["edited"].clone(), after[0]["default_changed"].clone()), (json!(false), json!(false)));
    let a2 = k2.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let parted = k2.customization_conflicts(&a2).await.unwrap();
    assert!(parted.iter().any(|c| c.entity == "Timesheet" && c.now["steps"][0]["label"] == json!("The bosses")), "the switched-off approval is flagged");
    let clerk2 = k2.load_ctx(tenant, clerk.user, None, "it".into()).await.unwrap();
    let t3 = k2.insert(&clerk2, "Timesheet", obj(json!({"hours": 2}))).await.unwrap();
    let t3: Uuid = t3["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(k2.submit(&clerk2, "Timesheet", t3, None).await.unwrap()["docstatus"], json!(1), "still switched off after the release");
}
