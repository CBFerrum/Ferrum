//! A workspace's customizations across a package upgrade, against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first): what an upgrade will change underneath
//! one, and which records wait in workflow states it changes, is said before it lands (FEO-056); what
//! it changed is reported after; it neither undoes the customization nor is silently undone by it, and
//! a field a package has come to require is required until an administrator decides otherwise.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, OverlayOp};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::Path;
use uuid::Uuid;

const V1: &str = r#"
- name: Case
  label: Case
  kind: master
  title_field: title
  fields:
    - {name: title, type: data, required: true}
    - {name: reference, type: data, label: Reference}
    - {name: summary, type: data}
    - {name: legacy, type: data}
    - {name: priority, type: data}
    - {name: channel, type: data, label: Channel}
- name: Claim
  label: Claim
  label_plural: Claims
  kind: master
  title_field: title
  fields:
    - {name: title, type: data, required: true}
- name: Ticket
  label: Ticket
  kind: master
  title_field: title
  fields:
    - {name: title, type: data, required: true}
"#;

// Summary is required now, Reference and Channel have new labels, Legacy is gone.
const V2: &str = r#"
- name: Case
  label: Case
  kind: master
  title_field: title
  fields:
    - {name: title, type: data, required: true}
    - {name: reference, type: data, label: Your reference}
    - {name: summary, type: data, required: true}
    - {name: priority, type: data}
    - {name: channel, type: data, label: Source}
- name: Claim
  label: Claim
  label_plural: Claims
  kind: master
  title_field: title
  fields:
    - {name: title, type: data, required: true}
- name: Ticket
  label: Ticket
  kind: master
  title_field: title
  fields:
    - {name: title, type: data, required: true}
"#;

const W1: &str = r#"
workflows:
  - name: Claims
    entity: Claim
    initial_state: Draft
    states: [{name: Draft}, {name: Submitted}, {name: Parked}, {name: Approved}]
    transitions:
      - {action: Submit, from: Draft, to: Submitted}
      - {action: Park, from: Draft, to: Parked}
      - {action: Approve, from: Submitted, to: Approved}
  - name: Tickets
    entity: Ticket
    initial_state: Open
    states: [{name: Open}, {name: Closed}]
    transitions:
      - {action: Close, from: Open, to: Closed}
"#;

// Parked is gone, and whoever raised a claim no longer approves it; tickets can wait.
const W2: &str = r#"
workflows:
  - name: Claims
    entity: Claim
    initial_state: Draft
    states: [{name: Draft}, {name: Submitted}, {name: Approved}]
    transitions:
      - {action: Submit, from: Draft, to: Submitted}
      - {action: Approve, from: Submitted, to: Approved, not_creator: true}
  - name: Tickets
    entity: Ticket
    initial_state: Open
    states: [{name: Open}, {name: Waiting}, {name: Closed}]
    transitions:
      - {action: Wait, from: Open, to: Waiting}
      - {action: Close, from: Open, to: Closed}
      - {action: Done, from: Waiting, to: Closed}
"#;

fn catalog(entities: &str, workflows: &str) -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-customize-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("cases/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("cases/package.yaml"), format!("package: cases\nversion: 0.1.0\nlabel: Cases\ndepends: [core]\n{workflows}")).unwrap();
    std::fs::write(dir.join("cases/entities/cases.yaml"), entities).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

fn set(field: &str, property: &str, value: Value) -> OverlayOp {
    serde_json::from_value(json!({"op": "set_property", "field": field, "property": property, "value": value})).unwrap()
}

fn doc(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

#[test]
fn customizations_survive_a_package_upgrade() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let connect = || async { Db::connect(DbConfig { url: url.clone(), max_connections: 10, ..Default::default() }).await.expect("connect") };
    let db = connect().await;
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(V1, W1), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["cases".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let clerk = k.create_user(&admin, "clerk@acme.test", "Clerk", None, &[], false).await.unwrap();

    let optional = k.add_customization(&admin, "Case", set("summary", "required", json!(false)), Some("We leave it blank".into())).await.unwrap();
    let renamed = k.add_customization(&admin, "Case", set("reference", "label", json!("Our ref")), None).await.unwrap();
    let hidden = k.add_customization(&admin, "Case", set("legacy", "hidden", json!(true)), None).await.unwrap();
    k.add_customization(&admin, "Case", set("priority", "label", json!("Urgency")), None).await.unwrap();
    // A whole workflow of the workspace's own, in place of its package's.
    let ours: ferrum_meta::WorkflowDef = serde_json::from_value(json!({
        "name": "Our tickets", "entity": "Ticket", "initial_state": "Open", "states": [{"name": "Open"}, {"name": "Resolved"}],
        "transitions": [{"action": "Resolve", "from": "Open", "to": "Resolved"}],
    }))
    .unwrap();
    let tickets = k.add_customization(&admin, "Ticket", OverlayOp::SetWorkflow { workflow: ours }, None).await.unwrap();
    // One made before customizations kept what they replaced: it is compared from what its package
    // says when this version first starts.
    let legacy = Uuid::now_v7();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("insert into meta_overlay (tenant_id, id, entity, op, note) values ($1, $2, 'Case', $3, 'made long ago')")
        .bind(tenant)
        .bind(legacy)
        .bind(serde_json::to_value(set("channel", "label", json!("Way in"))).unwrap())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("update tenant set meta_version = meta_version + 1 where id = $1").bind(tenant).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    k.registry.invalidate(tenant);
    assert!(k.customization_conflicts(&admin).await.unwrap().is_empty(), "nothing has moved yet");
    assert_eq!(k.adopt_customization_baselines().await.unwrap(), 1, "the one that had not said what it replaced");
    assert_eq!(k.adopt_customization_baselines().await.unwrap(), 0, "once");
    assert!(k.insert(&admin, "Case", doc(json!({"title": "before"}))).await.is_ok());

    // Claims waiting where the upgrade changes things, and one where it does not.
    let claim = |title: &'static str, steps: &'static [&'static str]| {
        let (k, admin) = (k.clone(), admin.clone());
        async move {
            let c = id(&k.insert(&admin, "Claim", doc(json!({"title": title}))).await.unwrap());
            for step in steps {
                k.transition(&admin, "Claim", c, step, None).await.unwrap();
            }
            c
        }
    };
    let waiting = claim("Taxi", &["Submit"]).await;
    let parked = claim("Hotel", &["Park"]).await;
    claim("Lunch", &["Submit", "Approve"]).await;

    // Before it lands: what the upgrade's plan says.
    let v2 = catalog(V2, W2);
    let clerk_ctx = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    assert!(matches!(k.upgrade_impact(&clerk_ctx, &v2, &[]).await, Err(Error::Forbidden(_))), "administrators only");
    let impact = k.upgrade_impact(&admin, &v2, &[]).await.unwrap();
    let predicted: BTreeSet<Uuid> = impact.customizations.iter().map(|c| c.id).collect();
    assert_eq!(predicted, BTreeSet::from([optional, renamed, hidden, tickets, legacy]), "{:?}", impact.customizations);
    assert!(impact.customizations.iter().find(|c| c.id == optional).unwrap().suspended, "and that this one will be suspended");
    let mut work: Vec<(&str, &str, i64, bool)> = impact.open_work.iter().map(|w| (w.entity.as_str(), w.state.as_str(), w.records, w.removed)).collect();
    work.sort();
    assert_eq!(work, [("Claim", "Parked", 1, true), ("Claim", "Submitted", 1, false)], "not Approved, which it leaves as it was");
    assert!(k.upgrade_impact(&admin, &catalog(V1, W1), &[]).await.unwrap().customizations.is_empty(), "the same packages move nothing");

    // The upgrade.
    let db = connect().await;
    let k = Kernel::boot(db, catalog(V2, W2), HookRegistry::default(), KernelSettings::default(), None).await.expect("upgraded");
    k.install_packages(tenant, &["cases".to_string()]).await.expect("upgrades");
    let admin = k.load_ctx(tenant, admin.user, None, "it".into()).await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let told: i64 = sqlx::query_scalar("select count(*) from notification where tenant_id = $1 and user_id = $2 and title like 'An update changed%'")
        .bind(tenant)
        .bind(admin.user)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(told, 1, "administrators hear of it");

    let conflicts = k.customization_conflicts(&admin).await.unwrap();
    assert_eq!(conflicts.iter().map(|c| c.id).collect::<BTreeSet<_>>(), predicted, "what the plan said: {conflicts:?}");
    let of = |id| conflicts.iter().find(|c| c.id == id).unwrap();
    assert!(of(optional).suspended, "the package's new requirement stands for now");
    assert_eq!((&of(renamed).was, &of(renamed).now), (&json!("Reference"), &json!("Your reference")));
    assert!(!of(renamed).suspended, "a label harms nobody: yours stands");
    assert!(of(hidden).gone);
    assert_eq!((&of(legacy).was, &of(legacy).now), (&json!("Channel"), &json!("Source")), "compared from when it was adopted");
    assert_eq!(
        (of(tickets).was["states"].as_array().unwrap().len(), of(tickets).now["states"].as_array().unwrap().len()),
        (2, 3),
        "the package's workflow, then and now"
    );

    let case = admin.schema.entity("Case").unwrap();
    assert_eq!(case.field("reference").unwrap().label.as_deref(), Some("Our ref"), "not undone by the upgrade");
    assert_eq!(case.field("priority").unwrap().label.as_deref(), Some("Urgency"), "not a conflict: its package changed nothing there");
    assert_eq!(case.field("channel").unwrap().label.as_deref(), Some("Way in"));
    assert_eq!(admin.schema.entity("Ticket").unwrap().workflow.as_ref().unwrap().def.name, "Our tickets", "still the workspace's own");
    assert!(case.field("summary").unwrap().required, "not silently undoing the upgrade");
    assert!(matches!(k.insert(&admin, "Case", doc(json!({"title": "after"}))).await, Err(Error::Validation(_))), "required");
    assert!(admin.schema.warnings.iter().any(|w| w.message.contains("suspended")), "and it says why");
    // The claim the plan said would go on under the new rules does: its raiser no longer approves it.
    assert!(matches!(k.transition(&admin, "Claim", waiting, "Approve", None).await, Err(Error::Forbidden(_))), "four eyes now");
    assert_eq!(k.get(&admin, "Claim", parked, false).await.unwrap()["workflow_state"], "Parked", "kept as it was, in a state the workflow no longer has");

    assert!(
        k.upgrade_impact(&admin, &catalog(V2, W2), &[]).await.unwrap().customizations.is_empty(),
        "what parted earlier and nobody has dealt with is not the doing of a plan that moves nothing"
    );

    // Someone decides.
    assert!(matches!(k.keep_customization(&admin, hidden).await, Err(Error::Invalid(_))), "nothing left for it to replace");
    for kept in [optional, renamed, tickets] {
        k.keep_customization(&admin, kept).await.expect("kept");
    }
    k.set_customization_active(&admin, legacy, false).await.unwrap();
    let admin = k.load_ctx(tenant, admin.user, None, "it".into()).await.unwrap();
    assert!(!admin.schema.entity("Case").unwrap().field("summary").unwrap().required, "their decision");
    assert_eq!(admin.schema.entity("Case").unwrap().field("channel").unwrap().label.as_deref(), Some("Source"), "the package's, switched off");
    assert!(k.insert(&admin, "Case", doc(json!({"title": "decided"}))).await.is_ok());
    let left = k.customization_conflicts(&admin).await.unwrap();
    assert_eq!(left.iter().map(|c| c.id).collect::<Vec<_>>(), [hidden], "only what nobody has dealt with");
    k.set_customization_active(&admin, hidden, false).await.unwrap();
    assert!(k.customization_conflicts(&admin).await.unwrap().is_empty());

    let clerk = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    assert!(matches!(k.customization_conflicts(&clerk).await, Err(Error::Forbidden(_))));
    assert!(matches!(k.keep_customization(&clerk, optional).await, Err(Error::Forbidden(_))));
}
