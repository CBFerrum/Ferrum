//! What someone is named on moves to their successor after they leave, against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first).

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

const ENTITIES: &str = r#"
- name: CaseHelper
  label: Case Helper
  kind: child
  fields:
    - {name: helper, type: email, required: true}
- name: Case
  label: Case
  kind: master
  title_field: title
  fields:
    - {name: title, type: data, required: true}
    - {name: owner, type: email}
    - {name: status, type: select, options: [Open, Closed], default: Open}
    - {name: helpers, type: table, of: CaseHelper}
"#;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn write(dir: &Path, handover: &str) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir.join("cases/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("cases/package.yaml"), format!("package: cases\nversion: 0.1.0\nlabel: Cases\ndepends: [core]\nhandover:\n{handover}")).unwrap();
    std::fs::write(dir.join("cases/entities/cases.yaml"), ENTITIES).unwrap();
}

fn temp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ferrum-handover-it-{name}-{}", std::process::id()))
}

fn catalog() -> Catalog {
    let dir = temp("ok");
    write(&dir, "  - {entity: Case, field: owner, where: {status: Open}}\n  - {entity: Case, field: helpers.helper, where: {status: Open}}\n");
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

#[test]
fn work_moves_to_whoever_takes_over() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    for (bad, why) in [
        ("  - {entity: Nothing, field: owner}\n", "unknown entity"),
        ("  - {entity: Case, field: title2}\n", "unknown field"),
        ("  - {entity: Case, field: status}\n", "not an address"),
        ("  - {entity: Case, field: helpers.nothing}\n", "unknown child field"),
        ("  - {entity: Case, field: owner, where: {nothing: 1}}\n", "unknown condition"),
    ] {
        let dir = temp("bad");
        write(&dir, bad);
        assert!(Catalog::load_dir(&dir).is_err(), "{why}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
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
    let pat = k.create_user(&admin, "pat@acme.test", "Pat", None, &[], false).await.unwrap();
    let sam = k.create_user(&admin, "sam@acme.test", "Sam", None, &[], false).await.unwrap();
    let kim = k.create_user(&admin, "kim@acme.test", "Kim", None, &[], false).await.unwrap();

    let new = |title: &str, owner: &str, status: &str, helpers: &[&str]| {
        let rows: Vec<Value> = helpers.iter().map(|h| json!({"helper": h})).collect();
        obj(json!({"title": title, "owner": owner, "status": status, "helpers": rows}))
    };
    let open = k.insert(&admin, "Case", new("open", "Pat@acme.test", "Open", &[])).await.unwrap();
    let closed = k.insert(&admin, "Case", new("closed", "pat@acme.test", "Closed", &["pat@acme.test"])).await.unwrap();
    let elsewhere = k.insert(&admin, "Case", new("elsewhere", "kim@acme.test", "Open", &["pat@acme.test", "kim@acme.test"])).await.unwrap();
    let both = k.insert(&admin, "Case", new("both", "kim@acme.test", "Open", &["pat@acme.test", "sam@acme.test"])).await.unwrap();
    // More than one batch, so the second round has to find its own work.
    for i in 0..230 {
        k.insert(&admin, "Case", new(&format!("many-{i}"), "pat@acme.test", "Open", &[])).await.unwrap();
    }
    let id = |d: &Doc| d["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let reread = |d: Uuid| {
        let (k, admin) = (k.clone(), admin.clone());
        async move { k.get(&admin, "Case", d, false).await.unwrap() }
    };
    let helpers = |d: &Doc| -> Vec<String> {
        let mut h: Vec<String> = d["helpers"].as_array().unwrap().iter().map(|r| r["helper"].as_str().unwrap().to_string()).collect();
        h.sort();
        h
    };
    let drain = || async { while k.dispatch_outbox_once().await.unwrap() > 0 {} };

    // ───────────── no successor: their access ends, their work waits ─────────────
    k.deprovision_user(&admin, pat, None, "left").await.expect("deprovisioned");
    drain().await;
    assert_eq!(reread(id(&open)).await["owner"], json!("pat@acme.test"), "nobody was named");

    // ───────────── someone is named later ─────────────
    let clerk = k.create_user(&admin, "clerk@acme.test", "Clerk", None, &[], false).await.unwrap();
    let clerk = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    assert!(matches!(k.hand_over_work(&clerk, pat, sam, "taking over").await, Err(Error::Forbidden(_))), "administrators only");
    assert!(matches!(k.hand_over_work(&admin, pat, sam, " ").await, Err(Error::Validation(_))), "a reason is required");
    assert!(matches!(k.hand_over_work(&admin, pat, pat, "self").await, Err(Error::Validation(_))), "to someone else");
    assert!(matches!(k.hand_over_work(&admin, sam, pat, "to the departed").await, Err(Error::Validation(_))), "who is active");
    assert!(matches!(k.hand_over_work(&admin, Uuid::now_v7(), sam, "nobody").await, Err(Error::NotFound(_))));
    k.hand_over_work(&admin, pat, sam, "Sam takes over").await.expect("handed over");
    drain().await;

    assert_eq!(reread(id(&open)).await["owner"], json!("sam@acme.test"), "open work moves, whatever case the address was typed in");
    let done = reread(id(&closed)).await;
    assert_eq!(done["owner"], json!("pat@acme.test"), "what is finished keeps who did it");
    assert_eq!(helpers(&done), vec!["pat@acme.test"], "child rows too");
    assert_eq!(helpers(&reread(id(&elsewhere)).await), vec!["kim@acme.test", "sam@acme.test"], "a child row moves");
    assert_eq!(reread(id(&elsewhere)).await["owner"], json!("kim@acme.test"), "and only what named them");
    assert_eq!(helpers(&reread(id(&both)).await), vec!["sam@acme.test"], "and is not doubled when the successor is on it already");
    let rows = k.list(&admin, "Case", &ListQuery { limit: Some(500), ..Default::default() }).await.unwrap().rows;
    assert_eq!(
        rows.iter().filter(|r| r["title"].as_str().unwrap().starts_with("many-") && r["owner"] == json!("sam@acme.test")).count(),
        230,
        "past the first batch"
    );

    let trail = k.audit_trail(&admin, "Case", id(&open)).await.unwrap().to_string();
    assert!(trail.contains("hand_over"), "{trail}");
    let sam_ctx = k.load_ctx(tenant, sam, None, "it".into()).await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let told: Vec<String> = sqlx::query_scalar("select title from notification where tenant_id = $1 and user_id = $2")
        .bind(tenant)
        .bind(sam_ctx.user)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(told.iter().any(|t| t.contains("records moved to you from pat@acme.test")), "{told:?}");

    // Repeating it finds nothing left, and a failed run can start again.
    k.hand_over_work(&admin, pat, sam, "again").await.expect("idempotent");
    drain().await;
    assert_eq!(reread(id(&open)).await["owner"], json!("sam@acme.test"));

    // A successor named when they leave moves work straight away.
    let lee = k.create_user(&admin, "lee@acme.test", "Lee", None, &[], false).await.unwrap();
    let hers = k.insert(&admin, "Case", new("lee's", "lee@acme.test", "Open", &[])).await.unwrap();
    k.deprovision_user(&admin, lee, Some(kim), "left").await.unwrap();
    drain().await;
    assert_eq!(reread(id(&hers)).await["owner"], json!("kim@acme.test"));
}
