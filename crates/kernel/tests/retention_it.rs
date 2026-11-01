//! Retention rules, restriction of processing and field masks, against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first).

use ferrum_kernel::retention::RetentionInput;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

const ENTITIES: &str = r#"
- name: Enquiry
  label: Enquiry
  kind: master
  title_field: subject
  fields:
    - {name: subject, type: data, required: true}
    - {name: person, type: email}
    - {name: status, type: select, options: [Open, Closed], default: Open}
    - {name: received, type: date}
    - {name: tax_id, type: data, perm_group: identity, mask: last4}
    - {name: contact, type: email, perm_group: identity, mask: email}
    - {name: pin, type: data, perm_group: identity, mask: hidden}
    - {name: salary, type: data, perm_group: identity}
- name: Receipt
  label: Receipt
  kind: master
  title_field: subject
  fields:
    - {name: subject, type: data, required: true}
    - {name: person, type: email}
"#;

const PACKAGE: &str = r#"package: cases
version: 0.1.0
label: Cases
depends: [core]
personal_data:
  - {entity: Enquiry, match: person, erase: [person, tax_id]}
  - {entity: Receipt, match: person, retain: tax records are kept for seven years}
roles:
  - name: Clerk
    permissions:
      - {entity: Enquiry, read: true, write: true, create: true}
  - name: Officer
    permissions:
      - {entity: Enquiry, read: true, write: true, create: true, read_groups: [identity], write_groups: [identity]}
"#;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-retention-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("cases/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("cases/package.yaml"), PACKAGE).unwrap();
    std::fs::write(dir.join("cases/entities/cases.yaml"), ENTITIES).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

fn rule(entity: &str, action: &str, condition: Value) -> RetentionInput {
    RetentionInput {
        entity: entity.into(),
        after_days: 30,
        date_field: "received".into(),
        action: action.into(),
        condition,
        reason: "Enquiries are of no use after a month".into(),
        is_active: true,
    }
}

#[test]
fn keeping_and_using_data_are_separate_decisions() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
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
    let clerk = k.create_user(&admin, "clerk@acme.test", "Clerk", None, &["Clerk".to_string()], false).await.unwrap();
    let clerk = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    let officer = k.create_user(&admin, "officer@acme.test", "Officer", None, &["Officer".to_string()], false).await.unwrap();
    let officer = k.load_ctx(tenant, officer, None, "it".into()).await.unwrap();

    // ───────────── field masks: what is seen, and what stays closed ─────────────
    let d = k
        .insert(
            &officer,
            "Enquiry",
            obj(json!({"subject": "masked", "tax_id": "AB 12 34 56 C", "contact": "pat@example.com", "pin": "1234", "salary": "90000"})),
        )
        .await
        .unwrap();
    let id: Uuid = d["id"].as_str().unwrap().parse().unwrap();
    let seen = k.get(&clerk, "Enquiry", id, false).await.unwrap();
    assert_eq!(seen["tax_id"], json!("•••• 456C"), "the last four");
    assert_eq!(seen["contact"], json!("p•••@example.com"));
    assert_eq!(seen["pin"], json!("••••••••"), "that there is one, and nothing of it");
    assert!(!seen.contains_key("salary"), "no mask: left out, as before");
    let full = k.get(&officer, "Enquiry", id, false).await.unwrap();
    assert_eq!(full["tax_id"], json!("AB 12 34 56 C"), "those in the group see it");
    let listed = k.list(&clerk, "Enquiry", &ListQuery::default()).await.unwrap();
    assert_eq!(listed.rows[0]["tax_id"], json!("•••• 456C"), "lists too");
    let by_value = ferrum_kernel::query::parse_filters(&json!({"tax_id": "AB 12 34 56 C"})).unwrap();
    assert!(
        matches!(k.list(&clerk, "Enquiry", &ListQuery { filters: by_value, ..Default::default() }).await, Err(Error::Forbidden(_))),
        "no guessing by filter"
    );
    assert!(
        matches!(k.list(&clerk, "Enquiry", &ListQuery { sort: Some("tax_id".into()), ..Default::default() }).await, Err(Error::Forbidden(_))),
        "or by order"
    );
    k.delete(&admin, "Enquiry", id, None).await.unwrap();

    // ───────────── restriction: nothing goes out, nothing is removed ─────────────
    let mail = |to: &'static str| {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(tenant).await.unwrap();
            let before: i64 = sqlx::query_scalar("select count(*) from outbox where topic = 'ferrum.mail'").fetch_one(&mut *tx).await.unwrap();
            k.queue_mail(&mut tx, tenant, to, "Hello", "<p>Hello</p>").await.expect("queued, as far as the caller knows");
            let after: i64 = sqlx::query_scalar("select count(*) from outbox where topic = 'ferrum.mail'").fetch_one(&mut *tx).await.unwrap();
            tx.rollback().await.unwrap();
            after - before
        }
    };
    let pat = k.create_user(&admin, "pat@acme.test", "Pat", None, &[], false).await.unwrap();
    let mut tx = k.db.system_tenant_tx(tenant).await.unwrap();
    sqlx::query("update app_user set email = 'pat.new@acme.test' where tenant_id = $1 and id = $2").bind(tenant).bind(pat).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    k.insert(&admin, "Enquiry", obj(json!({"subject": "theirs", "person": "pat.new@acme.test"}))).await.unwrap();
    assert_eq!(mail("pat.new@acme.test").await, 1);
    assert!(matches!(k.restrict_processing(&clerk, "pat@acme.test", "objected").await, Err(Error::Forbidden(_))));
    assert!(matches!(k.restrict_processing(&admin, "pat@acme.test", " ").await, Err(Error::Validation(_))), "a reason is required");
    let covered = k.restrict_processing(&admin, "pat@acme.test", "Objected on 3 May").await.expect("restricted");
    assert_eq!(covered.len(), 2, "every address they have had: {covered:?}");
    assert_eq!(mail("pat.new@acme.test").await, 0, "not to their address");
    assert_eq!(mail("PAT@acme.test").await, 0, "or their old one, however it is typed");
    assert_eq!(mail("someone.else@acme.test").await, 1, "others are not affected");
    assert_eq!(k.restrictions(&admin).await.unwrap().len(), 2);
    let kept = k.personal_data(&admin, "pat.new@acme.test").await.unwrap();
    assert_eq!(kept["records"].as_array().map_or(0, Vec::len), 1, "nothing is removed: {kept}");
    k.restrict_processing(&admin, "pat@acme.test", "Again").await.expect("repeating it changes nothing");
    assert_eq!(k.restrictions(&admin).await.unwrap().len(), 2);
    k.lift_restriction(&admin, "pat.new@acme.test").await.expect("lifted");
    assert_eq!(mail("pat@acme.test").await, 1, "and it can be undone");
    assert!(matches!(k.lift_restriction(&admin, "pat@acme.test").await, Err(Error::NotFound(_))));

    // ───────────── retention ─────────────
    assert!(matches!(k.save_retention_rule(&clerk, None, rule("Enquiry", "delete", json!({}))).await, Err(Error::Forbidden(_))));
    for (bad, why) in [
        (rule("Nothing", "delete", json!({})), "no such entity"),
        (rule("Receipt", "delete", json!({})), "its package keeps these, and says why"),
        (rule("Enquiry", "shred", json!({})), "no such action"),
        (RetentionInput { after_days: 0, ..rule("Enquiry", "delete", json!({})) }, "no days"),
        (RetentionInput { date_field: "subject".into(), ..rule("Enquiry", "delete", json!({})) }, "not a date"),
        (RetentionInput { reason: " ".into(), ..rule("Enquiry", "delete", json!({})) }, "no reason"),
    ] {
        assert!(k.save_retention_rule(&admin, None, bad).await.is_err(), "{why}");
    }
    let old = "2020-01-01";
    let today = chrono::Utc::now().date_naive().to_string();
    let enquiry = |subject: &'static str, status: &'static str, received: String| {
        let (k, admin) = (k.clone(), admin.clone());
        async move {
            let d = k
                .insert(
                    &admin,
                    "Enquiry",
                    obj(json!({"subject": subject, "person": format!("{subject}@acme.test"), "status": status, "received": received, "tax_id": "99"})),
                )
                .await
                .unwrap();
            d["id"].as_str().unwrap().parse::<Uuid>().unwrap()
        }
    };
    let gone = enquiry("gone", "Closed", old.into()).await;
    let open = enquiry("open", "Open", old.into()).await;
    let recent = enquiry("recent", "Closed", today.clone()).await;
    let held = enquiry("held", "Closed", old.into()).await;
    k.place_legal_hold(&admin, None, Some(("Enquiry", held)), "Litigation").await.unwrap();

    let delete = k.save_retention_rule(&admin, None, rule("Enquiry", "delete", json!({"status": "Closed"}))).await.expect("a rule");
    assert_eq!(k.retention_preview(&admin, delete).await.unwrap()["reaches"], json!(2), "says what it would reach, and touches nothing");
    assert!(k.get(&admin, "Enquiry", gone, false).await.is_ok());
    let runs = |k: std::sync::Arc<Kernel>| async move { while k.run_jobs_once(1).await.unwrap() > 0 {} };
    k.maintenance().await.unwrap();
    runs(k.clone()).await;
    assert!(k.get(&admin, "Enquiry", gone, false).await.is_err(), "past its time, and matching");
    assert!(k.get(&admin, "Enquiry", open, false).await.is_ok(), "does not match");
    assert!(k.get(&admin, "Enquiry", recent, false).await.is_ok(), "not yet");
    assert!(k.get(&admin, "Enquiry", held, false).await.is_ok(), "a legal hold stops it");
    let rules = k.retention_rules(&admin).await.unwrap();
    let last = &rules.iter().find(|r| r["id"] == json!(delete)).unwrap()["last_result"];
    assert_eq!((last["done"].as_u64(), last["held"].as_u64()), (Some(1), Some(1)), "{last}");
    k.maintenance().await.unwrap();
    assert_eq!(k.run_jobs_once(1).await.unwrap(), 0, "once a day");

    k.delete_retention_rule(&admin, delete).await.unwrap();
    let anonymize = k.save_retention_rule(&admin, None, rule("Enquiry", "anonymize", json!({"status": "Open"}))).await.expect("a rule");
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update job set created_at = now() - interval '2 days' where kind = 'platform.retention'").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    k.maintenance().await.unwrap();
    runs(k.clone()).await;
    let stays = k.get(&admin, "Enquiry", open, false).await.expect("it stays");
    assert_eq!(stays["subject"], json!("open"), "what is not personal stays");
    assert_ne!(stays["person"], json!("open@acme.test"), "the person does not");
    assert_ne!(stays["tax_id"], json!("99"));
    assert_eq!(k.retention_preview(&admin, anonymize).await.unwrap()["reaches"], json!(1), "it still matches: anonymizing again changes nothing");
}
