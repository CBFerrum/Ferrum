//! Comments and files shared with outside people (docs/builder/decisions.md BD-22) against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first). Two portal people of different customers and one member
//! of staff: everything staff do not mark stays out of an outside person's sight (listing, reading, downloading,
//! notices, history), another customer's record answers as if it were not there, and what an outside person writes
//! or uploads is shared by nature and reaches staff.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, PermissionDef};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const ENTITIES: &str = r#"
- name: Ticket
  label: Ticket
  kind: master
  title_field: subject
  fields:
    - {name: subject, type: data, required: true}
    - {name: email, type: email}
    - {name: report, type: attach}
"#;

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-outside-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("desk/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("desk/package.yaml"), "package: desk\nversion: 0.1.0\nlabel: Desk\ndepends: [core]\n").unwrap();
    std::fs::write(dir.join("desk/entities/desk.yaml"), ENTITIES).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

#[derive(Default)]
struct MemFiles(Mutex<HashMap<String, Vec<u8>>>);

#[async_trait::async_trait]
impl ferrum_kernel::files::FileStore for MemFiles {
    async fn put(&self, key: &str, data: &[u8]) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().insert(key.into(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> ferrum_kernel::Result<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned().ok_or_else(|| Error::NotFound(key.into()))
    }
    async fn delete(&self, key: &str) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn rule(v: Value) -> PermissionDef {
    serde_json::from_value(v).expect("a rule")
}

fn uuid(v: &Value) -> Uuid {
    v.as_str().unwrap().parse().unwrap()
}

async fn person(k: &Kernel, admin: &Ctx, email: &str, roles: &[&str], portal: bool) -> Ctx {
    let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
    let user = k.create_user(admin, email, email, None, if portal { &[] } else { &roles }, false).await.unwrap();
    if portal {
        k.set_user_kind(admin, user, "portal").await.unwrap();
        k.set_user_roles_until(admin, user, &roles, &Default::default()).await.expect("portal roles given");
    }
    k.load_ctx(admin.tenant, user, None, "it".into()).await.unwrap()
}

/// How many messages from `template` went to `user`, and how many notices they hold.
async fn told(k: &Kernel, tenant: Uuid, user: Uuid, template: &str) -> (i64, i64) {
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let sent: i64 = sqlx::query_scalar("select count(*) from message_send where tenant_id = $1 and user_id = $2 and template = $3")
        .bind(tenant)
        .bind(user)
        .bind(template)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let notices: i64 = sqlx::query_scalar("select count(*) from notification where tenant_id = $1 and user_id = $2")
        .bind(tenant)
        .bind(user)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (sent, notices)
}

fn hidden(r: &ferrum_kernel::Result<impl std::fmt::Debug>) -> bool {
    matches!(r, Err(Error::NotFound(_)))
}

#[test]
fn outside_people_see_only_what_staff_share() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    k.set_file_store(Arc::new(MemFiles::default()));
    let tenant = k
        .create_tenant(NewTenant {
            slug: "desk".into(),
            name: "Desk".into(),
            admin_email: "admin@desk.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["desk".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login("desk", "admin@desk.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();

    // ───────────── the rule says what of the conversation a portal role sees ─────────────
    for (bad, why) in [
        (json!({"entity": "Ticket", "read": true, "via": "email", "shared": "everything"}), "an unknown reach"),
        (json!({"entity": "Ticket", "create": true, "via": "email", "shared": "add"}), "a rule that does not read"),
        (json!({"entity": "*", "read": true, "if_owner": true, "shared": "read"}), "every entity"),
    ] {
        let e = k.set_role_permission(&admin, "Customers", &rule(bad.clone())).await.expect_err(why);
        assert!(matches!(e, Error::Validation(_)), "{why}: {e}");
    }
    k.set_role_permission(&admin, "Customers", &rule(json!({"entity": "Ticket", "read": true, "via": "email", "shared": "add"}))).await.unwrap();
    k.set_role_portal(&admin, "Customers", true).await.expect("own records, with their conversation");
    k.set_role_permission(&admin, "Watchers", &rule(json!({"entity": "Ticket", "read": true, "via": "email", "shared": "read"}))).await.unwrap();
    k.set_role_portal(&admin, "Watchers", true).await.unwrap();
    k.set_role_permission(&admin, "Plain", &rule(json!({"entity": "Ticket", "read": true, "via": "email"}))).await.unwrap();
    k.set_role_portal(&admin, "Plain", true).await.unwrap();
    k.set_role_permission(&admin, "Staff", &rule(json!({"entity": "Ticket", "read": true, "write": true, "create": true}))).await.unwrap();

    let staff = person(&k, &admin, "sam@desk.test", &["Staff"], false).await;
    let ann = person(&k, &admin, "ann@acme.test", &["Customers"], true).await;
    let bob = person(&k, &admin, "bob@globex.test", &["Customers"], true).await;
    let ann_t = id_of(&k.insert(&staff, "Ticket", obj(json!({"subject": "Ann's printer", "email": "ann@acme.test"}))).await.unwrap());
    let bob_t = id_of(&k.insert(&staff, "Ticket", obj(json!({"subject": "Bob's router", "email": "bob@globex.test"}))).await.unwrap());

    // ───────────── comments ─────────────
    // Staff talk among themselves, mention Ann (who must not hear of it), then share one with her.
    k.add_comment(&staff, "Ticket", ann_t, "she is on the old plan", &[ann.user]).await.unwrap();
    assert_eq!(told(&k, tenant, ann.user, "core.comment_shared").await, (0, 0), "an unshared comment, even one naming her, tells her nothing");
    let shared = k.add_comment_shared(&staff, "Ticket", ann_t, "We are sending a technician", &[], true).await.unwrap();
    let (sent, notices) = told(&k, tenant, ann.user, "core.comment_shared").await;
    assert_eq!((sent, notices), (1, 0), "told once, by e-mail first, of the shared comment");
    assert_eq!(told(&k, tenant, bob.user, "core.comment_shared").await.0, 0, "Bob is told nothing of Ann's ticket");

    let seen = k.comments(&ann, "Ticket", ann_t).await.unwrap();
    assert_eq!(seen.len(), 1, "only the shared comment: {seen:?}");
    assert_eq!(
        (seen[0]["body"].clone(), seen[0]["shared"].clone(), seen[0]["author_email"].clone()),
        (json!("We are sending a technician"), json!(true), Value::Null)
    );
    assert_eq!(k.comments(&staff, "Ticket", ann_t).await.unwrap().len(), 2, "staff read every comment");

    // Another customer's ticket is not there for her, the same as one that does not exist.
    assert!(hidden(&k.comments(&ann, "Ticket", bob_t).await), "Bob's ticket");
    assert!(hidden(&k.comments(&ann, "Ticket", Uuid::now_v7()).await), "no ticket at all");
    assert!(hidden(&k.add_comment(&ann, "Ticket", bob_t, "let me in", &[]).await));

    // What she writes is shared, whatever she asks, and reaches staff, who follow the ticket since they wrote on it.
    let before = told(&k, tenant, staff.user, "core.comment_shared").await.1;
    let hers = k.add_comment_shared(&ann, "Ticket", ann_t, "Thanks, Tuesday works", &[staff.user], false).await.unwrap();
    assert_eq!(told(&k, tenant, staff.user, "core.comment_shared").await.1, before + 1, "staff following the ticket are told");
    let all = k.comments(&staff, "Ticket", ann_t).await.unwrap();
    assert!(all.iter().any(|c| c["id"] == json!(hers) && c["shared"] == json!(true)), "hers is shared: {all:?}");
    assert_eq!(k.comments(&ann, "Ticket", ann_t).await.unwrap().len(), 2, "she sees the shared one and her own");
    // Staff stay staff: she shares nothing, and her own comment cannot be unshared.
    assert!(matches!(k.share_comment(&ann, shared, false).await, Err(Error::Forbidden(_))));
    assert!(k.share_comment(&staff, hers, false).await.is_err(), "what an outside person wrote stays shared");
    // Staff share the first comment later: she is told again, and now sees it.
    let first = all.iter().find(|c| c["shared"] == json!(false)).map(|c| uuid(&c["id"])).unwrap();
    k.share_comment(&staff, first, true).await.unwrap();
    assert_eq!(told(&k, tenant, ann.user, "core.comment_shared").await.0, 2);
    assert_eq!(k.comments(&ann, "Ticket", ann_t).await.unwrap().len(), 3);
    k.share_comment(&staff, first, false).await.unwrap();
    assert_eq!(k.comments(&ann, "Ticket", ann_t).await.unwrap().len(), 2, "and stops seeing it when it is unshared");

    // A rule that only reads, and one that says nothing of the conversation.
    let wes = person(&k, &admin, "wes@acme.test", &["Watchers"], true).await;
    let wes_t = id_of(&k.insert(&staff, "Ticket", obj(json!({"subject": "Wes's fax", "email": "wes@acme.test"}))).await.unwrap());
    k.add_comment_shared(&staff, "Ticket", wes_t, "Fax machines are retired", &[], true).await.unwrap();
    assert_eq!(k.comments(&wes, "Ticket", wes_t).await.unwrap().len(), 1);
    assert!(matches!(k.add_comment(&wes, "Ticket", wes_t, "but I like it", &[]).await, Err(Error::Forbidden(_))), "reading is not adding");
    let pat = person(&k, &admin, "pat@acme.test", &["Plain"], true).await;
    let pat_t = id_of(&k.insert(&staff, "Ticket", obj(json!({"subject": "Pat's mouse", "email": "pat@acme.test"}))).await.unwrap());
    k.add_comment_shared(&staff, "Ticket", pat_t, "shared, but not with a role that says nothing", &[], true).await.unwrap();
    assert!(matches!(k.comments(&pat, "Ticket", pat_t).await, Err(Error::Forbidden(_))));
    assert!(k.get(&pat, "Ticket", pat_t, false).await.is_ok(), "the record itself stays hers");

    // ───────────── files ─────────────
    let upload = |ctx: &Ctx, to: Option<Uuid>, name: &str| {
        let (ctx, name) = (ctx.clone(), name.to_string());
        let k = &k;
        async move { k.store_file(&ctx, to.map(|t| ("Ticket", t)), &name, "text/plain", b"x").await }
    };
    let internal = uuid(&upload(&staff, Some(ann_t), "internal.txt").await.unwrap()["id"]);
    let quote = uuid(&upload(&staff, Some(ann_t), "quote.txt").await.unwrap()["id"]);
    k.share_file(&staff, quote, true).await.unwrap();
    let names = |v: Vec<Value>| v.iter().map(|f| f["file_name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(names(k.attachments(&ann, "Ticket", ann_t).await.unwrap()), ["quote.txt"]);
    assert_eq!(k.attachments(&staff, "Ticket", ann_t).await.unwrap().len(), 2);
    k.file_info(&ann, quote).await.expect("a shared file of her ticket");
    assert!(hidden(&k.file_info(&ann, internal).await), "an unshared file answers as if it were not there");
    assert!(hidden(&k.file_info(&ann, Uuid::now_v7()).await));
    assert!(hidden(&k.file_info(&bob, quote).await), "Bob, a shared file of Ann's ticket");
    // Hers is shared by nature; Bob attaches nothing to her ticket, and Wes (reading only) nothing to his.
    let mine = uuid(&upload(&ann, Some(ann_t), "photo.txt").await.unwrap()["id"]);
    assert!(k.attachments(&staff, "Ticket", ann_t).await.unwrap().iter().any(|f| f["id"] == json!(mine) && f["shared"] == json!(true)));
    k.file_info(&ann, mine).await.expect("her own upload");
    // Stored without the flag, as the server's upload route once kept hers: still listed as what it is, shared.
    let mut tx = k.db.tenant_tx(staff.tenant).await.unwrap();
    sqlx::query("update file_object set shared = false where tenant_id = $1 and id = $2").bind(staff.tenant).bind(mine).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    for who in [&staff, &ann] {
        assert!(
            k.attachments(who, "Ticket", ann_t).await.unwrap().iter().any(|f| f["id"] == json!(mine) && f["shared"] == json!(true)),
            "the flag says what is true"
        );
    }
    assert!(upload(&bob, Some(ann_t), "x.txt").await.is_err(), "Bob attaches to Ann's ticket");
    assert!(matches!(upload(&wes, Some(wes_t), "x.txt").await, Err(Error::Forbidden(_))), "reading only");
    assert!(k.share_file(&staff, mine, false).await.is_err(), "what an outside person uploaded stays shared");
    // A file a field of her ticket names stays hers to download, as before: the record is hers.
    let named = uuid(&upload(&staff, None, "report.txt").await.unwrap()["id"]);
    k.update(&staff, "Ticket", ann_t, obj(json!({"report": format!("/files/{named}")})), None).await.unwrap();
    k.file_info(&ann, named).await.expect("named by a field she reads");
    k.share_file(&staff, quote, false).await.unwrap();
    assert!(hidden(&k.file_info(&ann, quote).await), "unshared again");

    // ───────────── the rest stays the members' ─────────────
    assert!(matches!(k.audit_trail(&ann, "Ticket", ann_t).await, Err(Error::Forbidden(_))), "history");
    assert!(k.tags(&ann, "Ticket", ann_t).await.is_err() && k.assignments(&ann, "Ticket", ann_t).await.is_err());
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let follows: i64 = sqlx::query_scalar("select count(*) from record_follower where tenant_id = $1 and user_id = $2")
        .bind(tenant)
        .bind(ann.user)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(follows, 0, "an outside person follows nothing, so staff's notices never reach her");
}
