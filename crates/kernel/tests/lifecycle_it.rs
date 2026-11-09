//! People arriving and leaving through an identity provider, against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first). When someone is turned off, their
//! access ends everywhere at once, on every replica, and their work moves on.

use ferrum_kernel::files::FileStore;
use ferrum_kernel::lifecycle::{ACCESS_JOURNAL_PREFIX, DEPROVISIONED_TOPIC};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const PASSWORD: &str = "Correct-Horse-9";

/// Object storage, in memory.
#[derive(Default)]
struct MemFiles(std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>);

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

async fn connect() -> Db {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect")
}

async fn replica() -> Arc<Kernel> {
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    let settings = KernelSettings { ctx_ttl: Duration::from_secs(3600), public_url: Some("https://erp.test".into()), ..Default::default() };
    Kernel::boot(connect().await, catalog, HookRegistry::default(), settings, None).await.expect("boot")
}

async fn workspace(k: &Kernel, slug: &str) -> Uuid {
    k.create_tenant(NewTenant {
        slug: slug.into(),
        name: slug.into(),
        admin_email: format!("admin@{slug}.test"),
        admin_password: PASSWORD.into(),
        admin_name: None,
        packages: vec!["parties".into()],
        settings: None,
    })
    .await
    .expect("workspace")
}

async fn role_names(k: &Kernel, tenant: Uuid, user: Uuid) -> Vec<String> {
    k.load_ctx(tenant, user, None, "it".into()).await.unwrap().roles
}

fn id_of(v: &Value) -> Uuid {
    v["id"].as_str().unwrap().parse().unwrap()
}

#[test]
fn people_come_and_go_through_the_provider() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let db = connect().await;
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = replica().await;
    let files = Arc::new(MemFiles::default());
    k.set_file_store(files.clone());
    let tenant = workspace(&k, "acme").await;
    let other = workspace(&k, "other").await;
    let session = k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let other_session = k.login("other", "admin@other.test", PASSWORD).await.unwrap();
    let outsider = k.load_ctx(other, other_session.user, Some(other_session.token_version), "it".into()).await.unwrap();

    // A key for the provider is an administrator's, and has no other use.
    let plain = k.create_user(&admin, "plain@acme.test", "Plain", Some(PASSWORD), &[], false).await.unwrap();
    let plain = k.load_ctx(tenant, plain, None, "it".into()).await.unwrap();
    assert!(matches!(k.create_api_key(&plain, plain.user, "idp", None, Some("scim")).await, Err(Error::Forbidden(_))));
    let (_, key) = k.create_api_key(&admin, admin.user, "idp", None, Some("scim")).await.unwrap();
    assert_eq!(k.verify_api_key(&key).await.unwrap().1.as_deref(), Some("scim"));
    assert!(k.scim_users(&plain, None, None, None).await.is_err(), "SCIM is for administrators");

    // ───────────── the provider creates a person ─────────────
    let pat = k
        .scim_create_user(
            &admin,
            &json!({"schemas": [ferrum_kernel::scim::USER_URN], "userName": "Pat@Acme.test", "externalId": "okta-1",
            "name": {"givenName": "Pat", "familyName": "Lee"}, "emails": [{"value": "pat@acme.test", "primary": true}], "title": "Ignored"}),
        )
        .await
        .expect("created");
    assert_eq!(pat["userName"], json!("pat@acme.test"));
    assert_eq!(pat["displayName"], json!("Pat Lee"));
    assert_eq!(pat["active"], json!(true));
    assert_eq!(pat["meta"]["location"], json!(format!("https://erp.test/api/scim/v2/Users/{}", pat["id"].as_str().unwrap())));
    let pat_id = id_of(&pat);
    assert!(k.login("acme", "pat@acme.test", "anything-At-All-1").await.is_err(), "no password works: they sign in through the provider");
    let again = k.scim_create_user(&admin, &json!({"userName": "pat@acme.test"})).await;
    assert!(matches!(again, Err(Error::Conflict(_))), "{again:?}");

    let found = k.scim_users(&admin, Some(r#"userName eq "PAT@acme.test""#), None, None).await.unwrap();
    assert_eq!(found["totalResults"], json!(1), "emails compare without regard to case");
    let found = k.scim_users(&admin, Some(r#"externalId eq "okta-1""#), None, None).await.unwrap();
    assert_eq!(found["Resources"][0]["id"], pat["id"]);
    assert_eq!(k.scim_users(&admin, Some(r#"userName eq "nobody@acme.test""#), None, None).await.unwrap()["totalResults"], json!(0));
    assert!(matches!(k.scim_users(&admin, Some("userName sw \"p\""), None, None).await, Err(Error::Validation(_))), "never a filter ignored");
    let page = k.scim_users(&admin, None, Some(2), Some(1)).await.unwrap();
    assert_eq!(page["itemsPerPage"], json!(1));
    assert_eq!(page["startIndex"], json!(2));
    assert_eq!(page["totalResults"], json!(3), "the administrator, plain and Pat");

    // Another workspace's provider reaches none of it.
    assert!(matches!(k.scim_user(&outsider, pat_id).await, Err(Error::NotFound(_))));
    assert!(matches!(k.scim_delete_user(&outsider, pat_id).await, Err(Error::NotFound(_))));
    assert_eq!(k.scim_users(&outsider, Some(r#"userName eq "pat@acme.test""#), None, None).await.unwrap()["totalResults"], json!(0));

    // ───────────── groups grant the roles mapped to them ─────────────
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    for role in ["Sales User", "Finance User", "plugin:evil"] {
        sqlx::query("insert into role (tenant_id, name) values ($1, $2) on conflict do nothing").bind(tenant).bind(role).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    let sales = k.scim_create_group(&admin, &json!({"displayName": "Sales User", "members": [{"value": pat_id}]})).await.unwrap();
    assert_eq!(role_names(&k, tenant, pat_id).await, vec!["Sales User"], "the role of the group's name");
    assert_eq!(k.scim_user(&admin, pat_id).await.unwrap()["groups"][0]["display"], json!("Sales User"));
    let emea = k.scim_create_group(&admin, &json!({"displayName": "EMEA Finance"})).await.unwrap();
    let emea_id = id_of(&emea);
    k.scim_patch_group(&admin, emea_id, &json!({"Operations": [{"op": "Add", "path": "members", "value": [{"value": pat_id}]}]})).await.unwrap();
    assert_eq!(role_names(&k, tenant, pat_id).await, vec!["Sales User"], "a group mapped to nothing grants nothing");
    k.scim_map_group(&admin, emea_id, &["Finance User".to_string()]).await.unwrap();
    assert_eq!(role_names(&k, tenant, pat_id).await, vec!["Finance User", "Sales User"], "mapping reaches its members at once");
    assert!(matches!(k.scim_map_group(&admin, emea_id, &["plugin:evil".to_string()]).await, Err(Error::Validation(_))), "plugins' roles are not for groups");
    assert!(k.scim_create_group(&admin, &json!({"displayName": "plugin:evil", "members": [{"value": pat_id}]})).await.is_ok());
    assert!(!role_names(&k, tenant, pat_id).await.contains(&"plugin:evil".to_string()), "nor by a group's name");
    let bad = k.scim_patch_group(&admin, emea_id, &json!({"Operations": [{"op": "add", "path": "members", "value": [{"value": outsider.user}]}]})).await;
    assert!(matches!(bad, Err(Error::Validation(_))), "members are of this workspace: {bad:?}");

    // A role given by hand survives leaving a group that also grants it.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update user_role set via_scim = false where tenant_id = $1 and user_id = $2 and role = 'Sales User'")
        .bind(tenant)
        .bind(pat_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    k.invalidate_user(tenant, pat_id);
    let out = json!({"Operations": [{"op": "remove", "path": format!("members[value eq \"{pat_id}\"]")}]});
    k.scim_patch_group(&admin, id_of(&sales), &out).await.unwrap();
    k.scim_patch_group(&admin, emea_id, &out).await.unwrap();
    assert_eq!(role_names(&k, tenant, pat_id).await, vec!["Sales User"], "what came through the group left with it; what was given by hand stayed");
    k.scim_patch_group(&admin, emea_id, &json!({"Operations": [{"op": "add", "path": "members", "value": [{"value": pat_id}]}]})).await.unwrap();

    // ───────────── someone leaves: their access ends everywhere at once ─────────────
    k.set_password(&admin, pat_id, None, PASSWORD).await.unwrap();
    let pat_session = k.login("acme", "pat@acme.test", PASSWORD).await.unwrap();
    let pat_ctx = k.load_ctx(tenant, pat_id, Some(pat_session.token_version), "it".into()).await.unwrap();
    let (_, pat_key) = k.create_api_key(&pat_ctx, pat_id, "laptop", None, None).await.unwrap();
    let successor = plain.user;
    let (doc_a, doc_b, doc_c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    for (record, who) in [(doc_a, pat_id), (doc_c, pat_id), (doc_b, pat_id), (doc_b, successor)] {
        sqlx::query("insert into record_assignment (tenant_id, id, entity, record_id, user_id, assigned_by) values ($1, $2, 'Customer', $3, $4, $5)")
            .bind(tenant)
            .bind(Uuid::now_v7())
            .bind(record)
            .bind(who)
            .bind(admin.user)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();

    // A second replica has them cached for an hour, and hears of it through the database.
    let b = replica().await;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let listener = b.spawn_user_listener(stopped);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(b.load_ctx(tenant, pat_id, Some(pat_session.token_version), "it".into()).await.is_ok(), "cached on B");

    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update tenant set settings = settings || jsonb_build_object('deprovision_reassign_to', $2::text) where id = $1")
        .bind(tenant)
        .bind(successor.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Entra's way of saying it, with a string for the boolean.
    let off = json!({"schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], "Operations": [{"op": "Replace", "path": "active", "value": "False"}]});
    let turned_off = k.scim_patch_user(&admin, pat_id, &off).await.expect("turned off");
    assert_eq!(turned_off["active"], json!(false));

    assert!(matches!(k.load_ctx(tenant, pat_id, Some(pat_session.token_version), "it".into()).await, Err(Error::Unauthorized(_))), "the session is over");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(matches!(b.load_ctx(tenant, pat_id, Some(pat_session.token_version), "it".into()).await, Err(Error::Unauthorized(_))), "on the other replica too");
    assert!(k.verify_api_key(&pat_key).await.is_err(), "API keys stop");
    assert!(k.login("acme", "pat@acme.test", PASSWORD).await.is_err(), "and so does signing in");

    // Deprovisioning tells the replicas itself: SCIM's own update above also announced a change.
    let quinn = k.create_user(&admin, "quinn@acme.test", "Quinn", Some(PASSWORD), &[], false).await.unwrap();
    let quinn_session = k.login("acme", "quinn@acme.test", PASSWORD).await.unwrap();
    assert!(b.load_ctx(tenant, quinn, Some(quinn_session.token_version), "it".into()).await.is_ok(), "cached on B");
    k.deprovision_user(&admin, quinn, None, "left").await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(matches!(b.load_ctx(tenant, quinn, Some(quinn_session.token_version), "it".into()).await, Err(Error::Unauthorized(_))), "B dropped its copy");

    let mut tx = k.db.system_tx().await.unwrap();
    let open: Vec<(Uuid, Uuid, String)> =
        sqlx::query_as("select record_id, user_id, status from record_assignment where tenant_id = $1 and record_id = any($2) order by created_at, user_id")
            .bind(tenant)
            .bind(vec![doc_a, doc_b, doc_c])
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    assert!(open.iter().filter(|r| r.2 == "Open").all(|r| r.1 == successor), "open work moved to the successor: {open:?}");
    assert_eq!(open.iter().filter(|r| r.2 == "Open").count(), 3, "all three records still have someone on them");
    assert_eq!(open.iter().filter(|r| r.1 == pat_id).count(), 1, "the one they already held closes rather than doubles");
    let (keys_live,): (i64,) = sqlx::query_as("select count(*) from api_key where tenant_id = $1 and user_id = $2 and revoked_at is null")
        .bind(tenant)
        .bind(pat_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(keys_live, 0, "revoked for good, not just while inactive");
    let (event,): (Value,) = sqlx::query_as("select payload from outbox where topic = $1 and record_id = $2")
        .bind(DEPROVISIONED_TOPIC)
        .bind(pat_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(event["reassign_to"], json!(successor), "apps hear of it, and hand over what they keep themselves");
    let (audit_diff,): (Value,) = sqlx::query_as("select diff from audit_event where entity = 'User' and record_id = $1 and action = 'deprovision'")
        .bind(pat_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert!(!audit_diff.to_string().contains("pat@acme.test"), "the audit trail names them by id: {audit_diff}");
    tx.commit().await.unwrap();
    let _ = stop.send(true);
    let _ = listener.await;

    // Repeating it is harmless; turning them on again does not bring old keys back.
    k.deprovision_user(&admin, pat_id, None, "again").await.expect("idempotent");
    k.scim_patch_user(&admin, pat_id, &json!({"Operations": [{"op": "replace", "value": {"active": true}}]})).await.unwrap();
    assert_eq!(k.scim_user(&admin, pat_id).await.unwrap()["active"], json!(true));
    assert!(k.load_ctx(tenant, pat_id, None, "it".into()).await.is_ok());
    assert!(k.verify_api_key(&pat_key).await.is_err());

    // ───────────── a provider's delete hides them, and they can come back ─────────────
    k.scim_delete_user(&admin, pat_id).await.unwrap();
    assert!(matches!(k.scim_user(&admin, pat_id).await, Err(Error::NotFound(_))), "gone, as the RFC asks");
    assert_eq!(k.scim_users(&admin, Some(r#"userName eq "pat@acme.test""#), None, None).await.unwrap()["totalResults"], json!(0));
    assert!(matches!(k.scim_delete_user(&admin, pat_id).await, Err(Error::NotFound(_))));
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let granted: Vec<String> = sqlx::query_scalar("select role from user_role where tenant_id = $1 and user_id = $2 and via_scim")
        .bind(tenant)
        .bind(pat_id)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(granted.is_empty(), "group grants left with them: {granted:?}");
    let back = k.scim_create_user(&admin, &json!({"userName": "pat@acme.test", "displayName": "Pat Lee-Khan", "externalId": "okta-9"})).await.unwrap();
    assert_eq!(id_of(&back), pat_id, "the same person, with their history");
    assert_eq!(back["displayName"], json!("Pat Lee-Khan"));

    // ───────────── nobody locks a workspace out ─────────────
    assert!(matches!(k.deprovision_user(&admin, admin.user, None, "oops").await, Err(Error::Invalid(_))), "not yourself");
    let second = k.scim_create_user(&admin, &json!({"userName": "second@acme.test"})).await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update app_user set is_admin = true where tenant_id = $1 and id = $2").bind(tenant).bind(id_of(&second)).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    k.invalidate_user(tenant, id_of(&second));
    let second_admin = k.load_ctx(tenant, id_of(&second), None, "it".into()).await.unwrap();
    let last = k.deprovision_user(&second_admin, admin.user, None, "leaving").await;
    assert!(last.is_ok(), "one of two may go: {last:?}");
    let last = k.deprovision_user(&admin, id_of(&second), None, "and the other").await;
    assert!(last.is_err(), "the one administrator left stays");
    let fresh = k.load_ctx(tenant, id_of(&second), None, "it".into()).await.unwrap();
    assert!(matches!(k.deprovision_user(&fresh, id_of(&second), None, "last").await, Err(Error::Invalid(_))));
    let nobody = k.deprovision_user(&fresh, plain.user, Some(pat_id), "needs a successor who is active").await;
    assert!(nobody.is_ok(), "{nobody:?}");
    assert!(matches!(k.deprovision_user(&fresh, pat_id, Some(plain.user), "successor is gone").await, Err(Error::Validation(_))));
    assert!(matches!(k.deprovision_user(&fresh, pat_id, None, "  ").await, Err(Error::Validation(_))), "a reason is required");
    // Plugins' service accounts are not people.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let service = Uuid::now_v7();
    sqlx::query("insert into app_user (tenant_id, id, email, kind) values ($1, $2, 'p@plugins.ferrum.invalid', 'service')")
        .bind(tenant)
        .bind(service)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(k.deprovision_user(&fresh, service, None, "no").await, Err(Error::Invalid(_))));
    assert!(matches!(k.scim_user(&fresh, service).await, Err(Error::NotFound(_))), "SCIM sees members only");

    // ───────────── who left since a backup stays out after a restore (FEO-014) ─────────────
    // Each change of someone's access is journalled outside the database, by their id only.
    let rae = k.create_user(&fresh, "rae@acme.test", "Rae", Some(PASSWORD), &[], false).await.unwrap();
    let sam = k.create_user(&fresh, "sam@acme.test", "Sam", Some(PASSWORD), &[], false).await.unwrap();
    k.deprovision_user(&fresh, rae, Some(fresh.user), "left").await.unwrap();
    k.set_user_active(&fresh, sam, false).await.unwrap();
    k.set_user_active(&fresh, sam, true).await.unwrap();
    let sam_ctx = k.load_ctx(tenant, sam, None, "it".into()).await.unwrap();
    let (_, sam_key) = k.create_api_key(&sam_ctx, sam, "laptop", None, None).await.unwrap();
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    let keys: Vec<String> = files.0.lock().unwrap().keys().filter(|k| k.starts_with(ACCESS_JOURNAL_PREFIX)).cloned().collect();
    assert!(keys.len() >= 3, "{keys:?}");
    for key in &keys {
        let entry = String::from_utf8(files.0.lock().unwrap()[key].clone()).unwrap();
        assert!(!entry.contains('@'), "no address in the journal: {entry}");
    }
    // A backup from before: Rae still in, Sam out.
    let mut tx = k.db.system_tenant_tx(tenant).await.unwrap();
    sqlx::query("update app_user set is_active = (id = $2) where tenant_id = $1 and id in ($2, $3)")
        .bind(tenant)
        .bind(rae)
        .bind(sam)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    k.invalidate_user(tenant, rae);
    k.invalidate_user(tenant, sam);
    k.login("acme", "rae@acme.test", PASSWORD).await.expect("as a backup from before would have it");
    let n = k.reapply_access(&keys).await.unwrap();
    assert_eq!(n, 2, "Rae out again, Sam in again, everyone else as they stand");
    assert!(k.verify_api_key(&sam_key).await.is_ok(), "only the last word counts: a key made after coming back is not ended on the way");
    assert!(k.login("acme", "rae@acme.test", PASSWORD).await.is_err(), "who left is out again");
    k.login("acme", "sam@acme.test", PASSWORD).await.expect("who came back is in, the last word standing");
    assert_eq!(k.reapply_access(&keys).await.unwrap(), 0, "doing it twice changes nothing");
}
