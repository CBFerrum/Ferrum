//! Collaboration on records (comments, assignments, tags, followers, attachments) against a real
//! PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_test`).

use ferrum_kernel::query::parse_filters;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::{Catalog, PermissionDef};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

fn obj(v: Value) -> ferrum_kernel::Doc {
    v.as_object().cloned().unwrap()
}

async fn fresh_kernel() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    Kernel::boot(db, catalog, HookRegistry::default(), KernelSettings::default(), None).await.expect("boot")
}

async fn unread(k: &Kernel, ctx: &Ctx) -> Vec<String> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await.unwrap();
    let titles: Vec<String> = sqlx::query_scalar("select title from notification where tenant_id = $1 and user_id = $2 and read_at is null order by created_at")
        .bind(ctx.tenant)
        .bind(ctx.user)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    titles
}

/// An in-memory object store standing in for S3.
#[derive(Default)]
struct MemFiles(std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>);

#[async_trait::async_trait]
impl ferrum_kernel::files::FileStore for MemFiles {
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

async fn drain(k: &Kernel) {
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
}

#[tokio::test]
async fn collaboration_follows_record_access() {
    let k = fresh_kernel().await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "collab".into(),
            name: "Collab Co".into(),
            admin_email: "admin@collab.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: Some("Ada Admin".into()),
            packages: vec!["core".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let s = k.login("collab", "admin@collab.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, s.user, Some(s.token_version), "it".into()).await.unwrap();

    // A colleague who can read and edit customers, and an outsider who cannot see them.
    let sales = PermissionDef { entity: "Customer".into(), read: true, write: true, create: true, ..Default::default() };
    k.set_role_permission(&admin, "Sales", &sales).await.unwrap();
    let bea = k.create_user(&admin, "bea@collab.test", "Bea Sales", Some("Correct-Horse-9"), &["Sales".into()], false).await.unwrap();
    let otto = k.create_user(&admin, "otto@collab.test", "Otto Outsider", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let bea_ctx = k.load_ctx(tenant, bea, None, "it".into()).await.unwrap();
    let otto_ctx = k.load_ctx(tenant, otto, None, "it".into()).await.unwrap();

    let c = k.insert(&admin, "Customer", obj(json!({"customer_name": "Globex"}))).await.unwrap();
    let cid: Uuid = c["id"].as_str().unwrap().parse().unwrap();

    // ── comments: mentions reach only people who can read the record ──
    let first = k.add_comment(&admin, "Customer", cid, "  Please call **Globex** today  ", &[bea, otto]).await.unwrap();
    assert!(k.add_comment(&admin, "Customer", cid, "   ", &[]).await.is_err(), "empty comments are refused");
    assert!(matches!(k.comments(&otto_ctx, "Customer", cid).await, Err(Error::Forbidden(_) | Error::NotFound(_))));
    assert!(k.add_comment(&otto_ctx, "Customer", cid, "let me in", &[]).await.is_err());
    let list = k.comments(&bea_ctx, "Customer", cid).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["body"], json!("Please call **Globex** today"), "trimmed");
    assert_eq!(list[0]["author"], json!("Ada Admin"));
    assert_eq!(unread(&k, &bea_ctx).await, vec!["Ada Admin mentioned you on Customer Globex".to_string()]);
    assert!(unread(&k, &otto_ctx).await.is_empty(), "no notification leaks about records the user cannot read");

    // Only the author edits; the author or an administrator deletes.
    assert!(matches!(k.edit_comment(&bea_ctx, first, "hijack").await, Err(Error::Forbidden(_))));
    k.edit_comment(&admin, first, "Please call Globex tomorrow").await.unwrap();
    let reply = k.add_comment(&bea_ctx, "Customer", cid, "Done, they want a quote", &[]).await.unwrap();
    // Ada follows the record since her comment: Bea's reply notifies her.
    assert!(unread(&k, &admin).await.iter().any(|t| t == "Bea Sales commented on Customer Globex"));
    k.delete_comment(&admin, reply).await.unwrap();
    assert!(matches!(k.delete_comment(&bea_ctx, first).await, Err(Error::Forbidden(_))));
    let list = k.comments(&admin, "Customer", cid).await.unwrap();
    assert_eq!(list.len(), 1);
    assert!(list[0]["edited_at"].is_string());

    // ── assignments ──
    assert!(k.assign(&admin, "Customer", cid, otto, None, None, None).await.is_err(), "the assignee must be able to open the record");
    assert!(k.assign(&admin, "Customer", cid, bea, None, None, Some("Urgent")).await.is_err(), "unknown priority");
    let due = chrono::NaiveDate::from_ymd_opt(2026, 11, 2);
    let a = k.assign(&admin, "Customer", cid, bea, Some("Send the quote"), due, Some("High")).await.unwrap();
    assert!(matches!(k.assign(&admin, "Customer", cid, bea, None, None, None).await, Err(Error::Conflict(_))), "one open assignment per user");
    assert!(unread(&k, &bea_ctx).await.iter().any(|t| t == "Ada Admin assigned Customer Globex to you"));
    let mine = k.my_assignments(&bea_ctx, None).await.unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0]["title"], json!("Globex"));
    assert_eq!(mine[0]["due_date"], json!("2026-11-02"));
    assert!(matches!(k.close_assignment(&otto_ctx, a, "Done").await, Err(Error::Forbidden(_) | Error::NotFound(_))));
    assert!(k.close_assignment(&bea_ctx, a, "Finished").await.is_err());
    k.close_assignment(&bea_ctx, a, "Done").await.unwrap();
    assert!(k.close_assignment(&bea_ctx, a, "Done").await.is_err(), "already done");
    assert!(k.my_assignments(&bea_ctx, None).await.unwrap().is_empty());
    assert_eq!(k.my_assignments(&bea_ctx, Some("Done")).await.unwrap().len(), 1);
    assert!(unread(&k, &admin).await.iter().any(|t| t == "Bea Sales completed an assignment on Customer"));
    // Done, a new assignment for the same person is possible again.
    k.assign(&admin, "Customer", cid, bea, None, None, None).await.unwrap();

    // ── tags: normalized, de-duplicated, filterable ──
    let tags = k.set_tags(&bea_ctx, "Customer", cid, &["  key   account ".into(), "Key Account".into(), "vip".into()]).await.unwrap();
    assert_eq!(tags, vec!["key account".to_string(), "vip".to_string()]);
    assert!(k.set_tags(&otto_ctx, "Customer", cid, &["x".into()]).await.is_err());
    assert!(k.set_tags(&admin, "Customer", cid, &["x".repeat(61)]).await.is_err());
    let other = k.insert(&admin, "Customer", obj(json!({"customer_name": "Initech"}))).await.unwrap();
    let vip = k.list(&admin, "Customer", &ListQuery { filters: parse_filters(&json!([["_tag", "=", "vip"]])).unwrap(), ..Default::default() }).await.unwrap();
    assert_eq!(vip.rows.len(), 1);
    assert_eq!(vip.rows[0]["id"], c["id"]);
    let any = k.list(&admin, "Customer", &ListQuery { filters: parse_filters(&json!([["_tag", "in", ["vip", "nope"]]])).unwrap(), ..Default::default() }).await.unwrap();
    assert_eq!(any.rows.len(), 1);
    assert_eq!(k.tag_counts(&admin, "Customer").await.unwrap(), vec![json!({"tag": "key account", "count": 1}), json!({"tag": "vip", "count": 1})]);
    assert!(k.tag_counts(&otto_ctx, "Customer").await.is_err());

    // ── followers hear about changes made by others ──
    drain(&k).await;
    let before = unread(&k, &admin).await.len();
    k.update(&bea_ctx, "Customer", cid, obj(json!({"phone": "+91 98"})), None).await.unwrap();
    drain(&k).await;
    let after = unread(&k, &admin).await;
    assert_eq!(after.len(), before + 1);
    assert_eq!(after.last().unwrap(), "Customer Globex was updated");
    let bea_before = unread(&k, &bea_ctx).await.len();
    k.update(&bea_ctx, "Customer", cid, obj(json!({"phone": "+91 99"})), None).await.unwrap();
    drain(&k).await;
    assert_eq!(unread(&k, &bea_ctx).await.len(), bea_before, "nobody is told about their own change");
    k.set_following(&admin, "Customer", cid, false).await.unwrap();
    assert!(!k.following(&admin, "Customer", cid).await.unwrap());
    let quiet = unread(&k, &admin).await.len();
    k.update(&bea_ctx, "Customer", cid, obj(json!({"phone": "+91 97"})), None).await.unwrap();
    drain(&k).await;
    assert_eq!(unread(&k, &admin).await.len(), quiet);

    // ── deleting a record removes its collaboration ──
    let oid: Uuid = other["id"].as_str().unwrap().parse().unwrap();
    k.add_comment(&admin, "Customer", oid, "short-lived", &[]).await.unwrap();
    k.set_tags(&admin, "Customer", oid, &["vip".into()]).await.unwrap();
    k.delete(&admin, "Customer", oid, None).await.unwrap();
    drain(&k).await;
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let left: i64 = sqlx::query_scalar(
        "select (select count(*) from record_comment where record_id = $1) + (select count(*) from record_tag where record_id = $1) \
         + (select count(*) from record_follower where record_id = $1)",
    )
    .bind(oid)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(left, 0);

    // ── automations can notify the user named in a field ──
    let bad = ferrum_kernel::automation::save(
        &k,
        &admin,
        json!({"name": "bad", "entity": "Customer", "event": "updated", "actions": [{"type": "notify", "to": "somebody", "title": "'x'"}]}).as_object().unwrap(),
    )
    .await;
    assert!(bad.is_err(), "unknown notify targets are refused when saving");
    ferrum_kernel::automation::save(
        &k,
        &admin,
        json!({"name": "tell the contact", "entity": "Customer", "event": "updated",
               "actions": [{"type": "notify", "to": "field:email", "title": "'Customer ' + doc.customer_name + ' changed'"}]})
        .as_object()
        .unwrap(),
    )
    .await
    .unwrap();
    drain(&k).await;
    k.update(&admin, "Customer", cid, obj(json!({"email": "BEA@collab.test"})), None).await.unwrap();
    drain(&k).await;
    assert!(unread(&k, &bea_ctx).await.iter().any(|t| t == "Customer Globex changed"));

    // ── attachments listing follows the record too ──
    assert!(k.attachments(&bea_ctx, "Customer", cid).await.unwrap().is_empty());
    assert!(k.attachments(&otto_ctx, "Customer", cid).await.is_err());

    // ── files packages store land with the uploads and follow the record's access ──
    assert!(k.store_file(&admin, Some(("Customer", cid)), "a.txt", "text/plain", b"x").await.is_err(), "no store configured yet");
    k.set_file_store(Arc::new(MemFiles::default()));
    let f = k.store_file(&admin, Some(("Customer", cid)), "quote.html", "text/html", b"<p>hi</p>").await.unwrap();
    let fid: Uuid = f["id"].as_str().unwrap().parse().unwrap();
    let (name, ctype, data) = k.read_file(&bea_ctx, fid).await.unwrap();
    assert_eq!((name.as_str(), ctype.as_str(), data.as_slice()), ("quote.html", "application/octet-stream", b"<p>hi</p>".as_slice()), "active content is never served as such");
    assert!(k.read_file(&otto_ctx, fid).await.is_err());
    assert_eq!(k.attachments(&bea_ctx, "Customer", cid).await.unwrap().len(), 1);
    assert!(k.store_file(&otto_ctx, Some(("Customer", cid)), "x.txt", "text/plain", b"x").await.is_err(), "attaching needs read access");
}
