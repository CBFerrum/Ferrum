//! A workspace's customizations across a package upgrade, against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first): what an upgrade changes underneath one
//! is reported, it neither undoes the customization nor is silently undone by it, and a field a
//! package has come to require is required until an administrator decides otherwise.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, OverlayOp};
use serde_json::{Value, json};
use std::path::Path;

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
"#;

// Summary is required now, Reference has a new label, Legacy is gone.
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
"#;

fn catalog(entities: &str) -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-customize-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("cases/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("cases/package.yaml"), "package: cases\nversion: 0.1.0\nlabel: Cases\ndepends: [core]\n").unwrap();
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

#[test]
fn an_upgrade_and_a_customization_do_not_silently_undo_each_other() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let connect = || async { Db::connect(DbConfig { url: url.clone(), max_connections: 10, ..Default::default() }).await.expect("connect") };
    let db = connect().await;
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(V1), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
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

    let optional = k.add_customization(&admin, "Case", set("summary", "required", json!(false)), Some("We leave it blank".into())).await.unwrap();
    let renamed = k.add_customization(&admin, "Case", set("reference", "label", json!("Our ref")), None).await.unwrap();
    let hidden = k.add_customization(&admin, "Case", set("legacy", "hidden", json!(true)), None).await.unwrap();
    k.add_customization(&admin, "Case", set("priority", "label", json!("Urgency")), None).await.unwrap();
    assert!(k.customization_conflicts(&admin).await.unwrap().is_empty(), "nothing has moved yet");
    assert!(k.insert(&admin, "Case", doc(json!({"title": "before"}))).await.is_ok());

    // The upgrade.
    let db = connect().await;
    let k = Kernel::boot(db, catalog(V2), HookRegistry::default(), KernelSettings::default(), None).await.expect("upgraded");
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
    assert_eq!(conflicts.len(), 3, "{conflicts:?}");
    let of = |id| conflicts.iter().find(|c| c.id == id).unwrap();
    assert!(of(optional).suspended, "the package's new requirement stands for now");
    assert_eq!((&of(renamed).was, &of(renamed).now), (&json!("Reference"), &json!("Your reference")));
    assert!(!of(renamed).suspended, "a label harms nobody: yours stands");
    assert!(of(hidden).gone);

    let case = admin.schema.entity("Case").unwrap();
    assert_eq!(case.field("reference").unwrap().label.as_deref(), Some("Our ref"), "not undone by the upgrade");
    assert_eq!(case.field("priority").unwrap().label.as_deref(), Some("Urgency"), "not a conflict: its package changed nothing there");
    assert!(case.field("summary").unwrap().required, "not silently undoing the upgrade");
    assert!(matches!(k.insert(&admin, "Case", doc(json!({"title": "after"}))).await, Err(Error::Validation(_))), "required");
    assert!(admin.schema.warnings.iter().any(|w| w.message.contains("suspended")), "and it says why");

    // Someone decides.
    assert!(matches!(k.keep_customization(&admin, hidden).await, Err(Error::Invalid(_))), "nothing left for it to replace");
    k.keep_customization(&admin, optional).await.expect("kept");
    k.keep_customization(&admin, renamed).await.expect("kept");
    let admin = k.load_ctx(tenant, admin.user, None, "it".into()).await.unwrap();
    assert!(!admin.schema.entity("Case").unwrap().field("summary").unwrap().required, "their decision");
    assert!(k.insert(&admin, "Case", doc(json!({"title": "decided"}))).await.is_ok());
    let left = k.customization_conflicts(&admin).await.unwrap();
    assert_eq!(left.iter().map(|c| c.id).collect::<Vec<_>>(), [hidden], "only what nobody has dealt with");
    k.set_customization_active(&admin, hidden, false).await.unwrap();
    assert!(k.customization_conflicts(&admin).await.unwrap().is_empty());

    let session = k.create_user(&admin, "clerk@acme.test", "Clerk", None, &[], false).await.unwrap();
    let clerk = k.load_ctx(tenant, session, None, "it".into()).await.unwrap();
    assert!(matches!(k.customization_conflicts(&clerk).await, Err(Error::Forbidden(_))));
    assert!(matches!(k.keep_customization(&clerk, optional).await, Err(Error::Forbidden(_))));
}
