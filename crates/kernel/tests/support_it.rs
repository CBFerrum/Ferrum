//! Support access and account states, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the
//! database is reset first): a grant reaches what it names and no more, ends when it should, and is
//! seen by the customer; a restricted account reads and exports, and changes nothing.

use ferrum_kernel::files::FileStore;
use ferrum_kernel::support::GrantInput;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

const ENTITIES: &str = r#"
- name: Ticket
  label: Ticket
  kind: master
  title_field: subject
  fields:
    - {name: subject, type: data, required: true}
- name: Invoice
  label: Invoice
  kind: master
  title_field: reference
  fields:
    - {name: reference, type: data, required: true}
"#;

#[derive(Default)]
struct MemFiles(Mutex<HashMap<String, Vec<u8>>>);

#[async_trait::async_trait]
impl FileStore for MemFiles {
    async fn put(&self, key: &str, data: &[u8]) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().insert(key.to_string(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> ferrum_kernel::Result<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned().ok_or_else(|| Error::NotFound("object".into()))
    }
    async fn delete(&self, key: &str) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-support-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("cases/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("cases/package.yaml"), "package: cases\nversion: 0.1.0\nlabel: Cases\ndepends: [core]\n").unwrap();
    std::fs::write(dir.join("cases/entities/cases.yaml"), ENTITIES).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

fn grant(level: &str, entities: &[&str], can_write: bool, hours: i64) -> GrantInput {
    GrantInput {
        level: level.into(),
        grantee: "Partner Ltd".into(),
        entities: entities.iter().map(|e| e.to_string()).collect(),
        can_write,
        reason: "Ticket 123".into(),
        hours,
    }
}

async fn signed_in(k: &Kernel, key: &str) -> ferrum_kernel::Result<Ctx> {
    let (s, _) = k.verify_api_key(key).await?;
    k.load_ctx(s.tenant, s.user, Some(s.token_version), "it".into()).await
}

#[test]
fn support_reaches_what_it_was_given() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    k.set_file_store(Arc::new(MemFiles::default()));
    let workspace = |slug: &str| NewTenant {
        slug: slug.into(),
        name: slug.into(),
        admin_email: format!("admin@{slug}.test"),
        admin_password: "Correct-Horse-9".into(),
        admin_name: None,
        packages: vec!["cases".into()],
        settings: None,
    };
    let tenant = k.create_tenant(workspace("acme")).await.expect("workspace");
    let other = k.create_tenant(workspace("other")).await.expect("workspace");
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let session = k.login("other", "admin@other.test", "Correct-Horse-9").await.unwrap();
    let outsider = k.load_ctx(other, session.user, None, "it".into()).await.unwrap();
    let clerk = k.create_user(&admin, "clerk@acme.test", "Clerk", None, &[], false).await.unwrap();
    let clerk = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();

    let ticket = k.insert(&admin, "Ticket", obj(json!({"subject": "Printer"}))).await.unwrap();
    let ticket_id = ticket["id"].as_str().unwrap().parse().unwrap();
    k.insert(&admin, "Invoice", obj(json!({"reference": "INV-1"}))).await.unwrap();

    // ───────────── what can be given ─────────────
    assert!(matches!(k.grant_support(&clerk, grant("health", &[], false, 1)).await, Err(Error::Forbidden(_))));
    for (bad, why) in [
        (grant("everything", &[], false, 1), "no such level"),
        (grant("health", &[], false, 24 * 31), "longer than a health grant lasts"),
        (grant("assistance", &["Ticket"], false, 73), "assistance is three days at most"),
        (grant("incident", &[], false, 25), "an incident is a day at most"),
        (grant("health", &["Ticket"], false, 1), "health reaches no records"),
        (grant("diagnostics", &[], true, 1), "nor writes"),
        (grant("assistance", &[], false, 1), "it must name what it reaches"),
        (grant("assistance", &["Nothing"], false, 1), "no such entity"),
        (GrantInput { reason: " ".into(), ..grant("health", &[], false, 1) }, "no reason"),
        (GrantInput { grantee: " ".into(), ..grant("health", &[], false, 1) }, "nobody"),
    ] {
        assert!(k.grant_support(&admin, bad).await.is_err(), "{why}");
    }

    // ───────────── health: how it is doing, and no record ─────────────
    let (id, key) = k.grant_support(&admin, grant("health", &[], false, 1)).await.expect("granted");
    let ctx = signed_in(&k, &key).await.expect("a key that works");
    assert_eq!(k.support_level(&ctx).await.unwrap().map(|l| l.1), Some("health".to_string()));
    assert!(matches!(k.list(&ctx, "Ticket", &ListQuery::default()).await, Err(Error::Forbidden(_))), "no records");
    assert!(matches!(k.get(&ctx, "Ticket", ticket_id, false).await, Err(Error::Forbidden(_) | Error::NotFound(_))));
    let report = k.support_report(&ctx, "health").await.unwrap();
    assert!(report["packages"].as_array().unwrap().contains(&json!("cases")));
    assert_eq!(report["failed_jobs"], json!([]), "health shows no failures' text");

    // ───────────── seen by the customer ─────────────
    k.support_used(&ctx, id, "GET", "/api/support/report").await.unwrap();
    k.support_used(&ctx, id, "GET", "/api/support/report").await.unwrap();
    let listed = k.support_grants(&admin).await.unwrap();
    assert_eq!((listed[0]["uses"].as_i64(), listed[0]["active"].as_bool(), listed[0]["grantee"].as_str()), (Some(2), Some(true), Some("Partner Ltd")));
    k.support_tried(&ctx, id, "GET", "/api/v1/Invoice", true).await.unwrap();
    let seen = k.support_activity(&admin, id).await.unwrap();
    assert_eq!(seen.len(), 3, "request by request");
    assert_eq!((&seen[0]["refused"], &seen[1]["refused"]), (&json!(true), &json!(false)), "and what was turned away says so");
    assert!(!json!(listed).to_string().contains("fk_"), "the key is shown once, and never again");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let told: Vec<String> = sqlx::query_scalar("select title from notification where tenant_id = $1 and user_id = $2 order by created_at")
        .bind(tenant)
        .bind(admin.user)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(told.iter().any(|t| t.contains("given to Partner Ltd")), "{told:?}");
    assert_eq!(told.iter().filter(|t| t.as_str() == "Support access was used").count(), 1, "the first use, not every one");
    assert!(k.support_grants(&outsider).await.unwrap().is_empty(), "another workspace");
    assert!(matches!(k.support_grants(&clerk).await, Err(Error::Forbidden(_))));

    k.revoke_support(&admin, id).await.expect("revoked");
    assert!(matches!(signed_in(&k, &key).await, Err(Error::Unauthorized(_))), "ended at once");
    assert!(matches!(k.revoke_support(&admin, id).await, Err(Error::NotFound(_))));

    // ───────────── assistance: what was named, as it was named ─────────────
    let (id, key) = k.grant_support(&admin, grant("assistance", &["Ticket"], false, 2)).await.unwrap();
    let ctx = signed_in(&k, &key).await.unwrap();
    assert!(k.get(&ctx, "Ticket", ticket_id, false).await.is_ok(), "what was named");
    assert!(matches!(k.list(&ctx, "Invoice", &ListQuery::default()).await, Err(Error::Forbidden(_))), "and nothing else");
    assert!(
        matches!(k.update(&ctx, "Ticket", ticket_id, obj(json!({"subject": "Changed"})), None).await, Err(Error::Forbidden(_))),
        "read-only unless allowed"
    );
    assert!(!ctx.is_admin, "never an administrator");
    assert!(matches!(k.create_user(&ctx, "new@acme.test", "New", None, &[], true).await, Err(Error::Forbidden(_))));
    assert!(matches!(k.grant_support(&ctx, grant("incident", &[], false, 1)).await, Err(Error::Forbidden(_))), "support cannot extend itself");

    // It ends by itself.
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update support_grant set expires_at = now() - interval '1 minute' where id = $1").bind(id).execute(&mut *tx).await.unwrap();
    sqlx::query("update api_key set expires_at = now() - interval '1 minute' where tenant_id = $1 and scope = 'support' and revoked_at is null")
        .bind(tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(signed_in(&k, &key).await, Err(Error::Unauthorized(_))));
    assert!(k.support_level(&ctx).await.unwrap().is_none());
    k.maintenance().await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let live: i64 = sqlx::query_scalar("select count(*) from app_user where tenant_id = $1 and kind = 'support' and is_active")
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(live, 0, "its account is closed");

    // ───────────── an incident: everything, for a day, with a reason ─────────────
    let (_, key) = k.grant_support(&admin, grant("incident", &[], true, 8)).await.unwrap();
    let ctx = signed_in(&k, &key).await.unwrap();
    k.update(
        &ctx,
        "Invoice",
        k.list(&admin, "Invoice", &ListQuery::default()).await.unwrap().rows[0]["id"].as_str().unwrap().parse().unwrap(),
        obj(json!({"reference": "Fixed"})),
        None,
    )
    .await
    .expect("it can act");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let actor: bool = sqlx::query_scalar("select exists (select 1 from audit_event where tenant_id = $1 and entity = 'Invoice' and actor = $2)")
        .bind(tenant)
        .bind(ctx.user)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(actor, "under its own name on the audit trail");
    assert!(matches!(k.create_user(&ctx, "new@acme.test", "New", None, &[], true).await, Err(Error::Forbidden(_))), "and it is still not an administrator");

    // ───────────── usage and quotas ─────────────
    k.store_file(&admin, None, "a.txt", "text/plain", &[b'x'; 1000]).await.unwrap();
    let used = k.usage(&admin).await.unwrap();
    assert_eq!(used["now"]["storage_bytes"], json!(1000), "{used}");
    assert!(used["now"]["people"].as_i64().unwrap() >= 2);
    assert!(matches!(k.usage(&clerk).await, Err(Error::Forbidden(_))));
    assert!(!k.usage(&admin).await.unwrap()["days"].as_array().unwrap().is_empty(), "kept by the day, to explain a bill");
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update tenant set quotas = '{\"storage_bytes\": 1500}' where id = $1").bind(tenant).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    k.store_file(&admin, None, "b.txt", "text/plain", &[b'x'; 400]).await.expect("within it");
    assert!(matches!(k.store_file(&admin, None, "c.txt", "text/plain", &[b'x'; 200]).await, Err(Error::Conflict(_))), "past it");

    // ───────────── an account in trouble keeps its data, and its way out ─────────────
    assert!(k.set_account_state(tenant, "nonsense", None).await.is_err());
    k.set_account_state(tenant, "grace", Some("Invoice overdue")).await.unwrap();
    let admin = k.load_ctx(tenant, admin.user, None, "it".into()).await.unwrap();
    assert!(k.insert(&admin, "Ticket", obj(json!({"subject": "still fine"}))).await.is_ok(), "grace changes nothing");
    assert_eq!(k.usage(&admin).await.unwrap()["note"], json!("Invoice overdue"), "and says why");

    k.set_account_state(tenant, "restricted", None).await.unwrap();
    let admin = k.load_ctx(tenant, admin.user, None, "it".into()).await.unwrap();
    assert!(matches!(k.insert(&admin, "Ticket", obj(json!({"subject": "no"}))).await, Err(Error::Forbidden(_))), "nothing changes");
    assert!(matches!(k.update(&admin, "Ticket", ticket_id, obj(json!({"subject": "no"})), None).await, Err(Error::Forbidden(_))));
    assert!(k.get(&admin, "Ticket", ticket_id, false).await.is_ok(), "everything can be read");
    let job = k.export_workspace(&admin).await.expect("and taken away");
    assert!(k.run_jobs_once(1).await.is_ok());
    assert_eq!(k.job(&admin, job).await.unwrap()["status"], json!("done"), "export is a right, not a feature");
    k.set_account_state(tenant, "active", None).await.unwrap();
    let admin = k.load_ctx(tenant, admin.user, None, "it".into()).await.unwrap();
    assert!(k.insert(&admin, "Ticket", obj(json!({"subject": "back"}))).await.is_ok(), "and it all comes back");
}
