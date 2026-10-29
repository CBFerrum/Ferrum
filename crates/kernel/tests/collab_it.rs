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
    let titles: Vec<String> =
        sqlx::query_scalar("select title from notification where tenant_id = $1 and user_id = $2 and read_at is null order by created_at")
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
    let any = k
        .list(&admin, "Customer", &ListQuery { filters: parse_filters(&json!([["_tag", "in", ["vip", "nope"]]])).unwrap(), ..Default::default() })
        .await
        .unwrap();
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
        json!({"name": "bad", "entity": "Customer", "event": "updated", "actions": [{"type": "notify", "to": "somebody", "title": "'x'"}]})
            .as_object()
            .unwrap(),
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
    let objects = Arc::new(MemFiles::default());
    k.set_file_store(objects.clone());
    let f = k.store_file(&admin, Some(("Customer", cid)), "quote.html", "text/html", b"<p>hi</p>").await.unwrap();
    let fid: Uuid = f["id"].as_str().unwrap().parse().unwrap();
    let (name, ctype, data) = k.read_file(&bea_ctx, fid).await.unwrap();
    assert_eq!(
        (name.as_str(), ctype.as_str(), data.as_slice()),
        ("quote.html", "application/octet-stream", b"<p>hi</p>".as_slice()),
        "active content is never served as such"
    );
    assert!(k.read_file(&otto_ctx, fid).await.is_err());
    assert_eq!(k.attachments(&bea_ctx, "Customer", cid).await.unwrap().len(), 1);
    assert!(k.store_file(&otto_ctx, Some(("Customer", cid)), "x.txt", "text/plain", b"x").await.is_err(), "attaching needs read access");
    assert!(k.copy_file(&otto_ctx, fid, None).await.is_err(), "copying needs read access to the source");
    let copy = k.copy_file(&bea_ctx, fid, None).await.unwrap();
    assert_ne!(copy["id"], f["id"]);
    assert_eq!((copy["file_name"].as_str(), copy["sha256"].as_str()), (Some("quote.html"), f["sha256"].as_str()));
    // Apps remove files completely (record and bytes) and attach uploads inside a transaction.
    let copy_id: Uuid = copy["id"].as_str().unwrap().parse().unwrap();
    let stored_before = objects.0.lock().unwrap().len();
    assert!(k.remove_file(&otto_ctx, copy_id).await.is_err(), "only its uploader or an editor");
    k.remove_file(&bea_ctx, copy_id).await.unwrap();
    assert_eq!(objects.0.lock().unwrap().len(), stored_before - 1, "the bytes are gone too");
    assert!(k.read_file(&bea_ctx, copy_id).await.is_err());
    let loose = k.store_file(&bea_ctx, None, "photo.png", "image/png", b"\x89PNG").await.unwrap();
    let loose_id: Uuid = loose["id"].as_str().unwrap().parse().unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert!(k.attach_file_tx(&otto_ctx, &mut tx, loose_id, "Customer", cid).await.is_err(), "Otto cannot read the record");
    tx.rollback().await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    k.attach_file_tx(&bea_ctx, &mut tx, loose_id, "Customer", cid).await.unwrap();
    assert!(k.attach_file_tx(&bea_ctx, &mut tx, loose_id, "Customer", cid).await.is_err(), "attached once");
    tx.commit().await.unwrap();
    assert!(k.attachments(&bea_ctx, "Customer", cid).await.unwrap().iter().any(|a| a["id"] == loose["id"]));
    // An attachment belongs to its record: a reader who attached it cannot delete it, an editor can.
    let readers = PermissionDef { entity: "Customer".into(), read: true, ..Default::default() };
    k.set_role_permission(&admin, "Reader", &readers).await.unwrap();
    let rita = k.create_user(&admin, "rita@collab.test", "Rita Reader", Some("Correct-Horse-9"), &["Reader".into()], false).await.unwrap();
    let rita_ctx = k.load_ctx(tenant, rita, None, "it".into()).await.unwrap();
    let note = k.store_file(&rita_ctx, Some(("Customer", cid)), "note.txt", "text/plain", b"seen").await.unwrap();
    let note_id: Uuid = note["id"].as_str().unwrap().parse().unwrap();
    assert!(matches!(k.remove_file(&rita_ctx, note_id).await, Err(Error::Forbidden(_))));
    k.remove_file(&bea_ctx, note_id).await.unwrap();

    // ── preferences: per user, versioned ──
    let cols = json!({"columns": ["customer_name", "email"], "sort": "customer_name asc"});
    assert_eq!(k.set_pref(&bea_ctx, "list:Customer", &cols, None).await.unwrap(), 1);
    assert_eq!(k.set_pref(&bea_ctx, "list:Customer", &cols, Some(1)).await.unwrap(), 2);
    assert!(matches!(k.set_pref(&bea_ctx, "list:Customer", &json!({}), Some(1)).await, Err(Error::Conflict(_))), "a stale tab cannot overwrite");
    assert!(k.set_pref(&bea_ctx, "list Customer", &cols, None).await.is_err());
    assert_eq!(k.prefs(&bea_ctx).await.unwrap()["list:Customer"], json!({"value": cols, "version": 2}));
    assert_eq!(k.prefs(&otto_ctx).await.unwrap(), json!({}), "each user sees only their own");
    k.delete_pref(&bea_ctx, "list:Customer").await.unwrap();
    assert_eq!(k.prefs(&bea_ctx).await.unwrap(), json!({}));

    // ── bulk: record by record, with the caller's permissions ──
    use ferrum_kernel::bulk::{BulkOp, BulkTarget};
    let mut ids = vec![];
    for name in ["Initech", "Umbrella"] {
        let d = k.insert(&admin, "Customer", obj(json!({"customer_name": name}))).await.unwrap();
        ids.push(d["id"].as_str().unwrap().parse::<Uuid>().unwrap());
    }
    let disable = BulkOp::Update { values: obj(json!({"disabled": true})) };
    let r = k.bulk(&bea_ctx, "Customer", BulkTarget::Ids(vec![ids[0], ids[1], Uuid::now_v7()]), &disable).await.unwrap();
    assert_eq!((r["succeeded"].clone(), r["failed"].clone()), (json!(2), json!(1)), "{r}");
    assert_eq!(r["results"].as_array().unwrap().iter().filter(|x| x["error"]["error"] == json!("not_found")).count(), 1);
    let bad = BulkOp::Update { values: obj(json!({"customer_type": "Martian"})) };
    let r = k.bulk(&bea_ctx, "Customer", BulkTarget::Ids(vec![ids[0]]), &bad).await.unwrap();
    assert_eq!(r["results"][0]["error"]["error"], json!("validation_failed"));
    let r = k.bulk(&bea_ctx, "Customer", BulkTarget::Ids(ids.clone()), &BulkOp::Delete).await.unwrap();
    assert_eq!(r["failed"], json!(2), "no delete permission, nothing deleted");
    let disabled = parse_filters(&json!([["disabled", "=", true]])).unwrap();
    let r = k.bulk(&bea_ctx, "Customer", BulkTarget::Filters(disabled.clone()), &BulkOp::AddTags { tags: vec!["vip".into()] }).await.unwrap();
    assert_eq!(r["succeeded"], json!(2));
    assert_eq!(k.tags(&bea_ctx, "Customer", ids[1]).await.unwrap(), vec!["vip".to_string()]);
    k.bulk(&bea_ctx, "Customer", BulkTarget::Filters(disabled), &BulkOp::RemoveTags { tags: vec!["vip".into()] }).await.unwrap();
    assert!(k.tags(&bea_ctx, "Customer", ids[1]).await.unwrap().is_empty());
    let r = k.bulk(&otto_ctx, "Customer", BulkTarget::Ids(ids.clone()), &BulkOp::Update { values: obj(json!({"disabled": false})) }).await.unwrap();
    assert_eq!(r["failed"], json!(2), "outsiders change nothing");
    assert_eq!(k.get(&admin, "Customer", ids[0], false).await.unwrap()["disabled"], json!(true));
    assert!(k.bulk(&bea_ctx, "Customer", BulkTarget::Ids(vec![]), &BulkOp::Submit).await.is_err(), "an empty selection is refused");

    // ── filters, sorts and link paths never reveal what the caller cannot read ──
    let list =
        |filters: Value, sort: Option<&str>| ListQuery { filters: parse_filters(&filters).unwrap(), sort: sort.map(str::to_string), ..Default::default() };
    k.update(&admin, "Customer", ids[0], obj(json!({"credit_limit": "5000"})), None).await.unwrap();
    // The audit trail shows that the hidden field changed, never its values.
    let changed = |trail: Value| trail["events"].as_array().unwrap().iter().rev().find_map(|e| e["diff"].get("credit_limit").cloned()).unwrap();
    assert_eq!(changed(k.audit_trail(&bea_ctx, "Customer", ids[0]).await.unwrap()), json!({"hidden": true}));
    assert_eq!(changed(k.audit_trail(&admin, "Customer", ids[0]).await.unwrap()), json!([null, "5000"]));
    let rich = json!([["credit_limit", ">", "1000"]]);
    assert_eq!(k.list(&admin, "Customer", &list(rich.clone(), None)).await.unwrap().rows.len(), 1);
    assert!(matches!(k.list(&bea_ctx, "Customer", &list(rich, None)).await, Err(Error::Forbidden(_))), "credit_limit is in a group Bea cannot read");
    assert!(matches!(k.list(&bea_ctx, "Customer", &list(json!([]), Some("credit_limit desc"))).await, Err(Error::Forbidden(_))), "nor sort by it");
    let vip = k.insert(&admin, "CustomerGroup", obj(json!({"group_name": "VIP"}))).await.unwrap();
    k.update(&admin, "Customer", ids[0], obj(json!({"customer_group": vip["id"]})), None).await.unwrap();
    let in_vip = json!([["customer_group.group_name", "=", "VIP"]]);
    assert!(matches!(k.list(&bea_ctx, "Customer", &list(in_vip.clone(), None)).await, Err(Error::Forbidden(_))), "no read access to groups");
    // Read access to her own groups only: the admin's VIP group stays invisible through the path.
    let own_groups = PermissionDef { entity: "CustomerGroup".into(), read: true, write: true, create: true, if_owner: true, ..Default::default() };
    k.set_role_permission(&admin, "Sales", &own_groups).await.unwrap();
    let bea_ctx = k.load_ctx(tenant, bea, None, "it".into()).await.unwrap();
    assert!(k.list(&bea_ctx, "Customer", &list(in_vip, None)).await.unwrap().rows.is_empty(), "the VIP group is not hers");
    let retail = k.insert(&bea_ctx, "CustomerGroup", obj(json!({"group_name": "Retail"}))).await.unwrap();
    k.update(&admin, "Customer", ids[1], obj(json!({"customer_group": retail["id"]})), None).await.unwrap();
    let in_retail = json!([["customer_group.group_name", "=", "Retail"]]);
    assert_eq!(k.list(&bea_ctx, "Customer", &list(in_retail, None)).await.unwrap().rows.len(), 1);
    // Tag counts only count records the caller can read.
    k.set_tags(&admin, "CustomerGroup", vip["id"].as_str().unwrap().parse().unwrap(), &["key-account".into()]).await.unwrap();
    k.set_tags(&bea_ctx, "CustomerGroup", retail["id"].as_str().unwrap().parse().unwrap(), &["local".into()]).await.unwrap();
    let tags = |v: Vec<Value>| v.iter().map(|t| t["tag"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(tags(k.tag_counts(&bea_ctx, "CustomerGroup").await.unwrap()), ["local"]);
    assert_eq!(tags(k.tag_counts(&admin, "CustomerGroup").await.unwrap()).len(), 2);

    // A custom link field lives in the JSON column (no foreign key): deletes still respect it.
    let op = serde_json::from_value(json!({"op": "add_field", "field": {"name": "x_referred_by", "type": "link", "to": "CustomerGroup"}})).unwrap();
    k.add_customization(&admin, "Customer", op, None).await.unwrap();
    let admin = k.load_ctx(tenant, s.user, Some(s.token_version), "it".into()).await.unwrap();
    let spare = k.insert(&admin, "CustomerGroup", obj(json!({"group_name": "Referrals"}))).await.unwrap();
    let spare_id: Uuid = spare["id"].as_str().unwrap().parse().unwrap();
    k.update(&admin, "Customer", ids[0], obj(json!({"x_referred_by": spare["id"]})), None).await.unwrap();
    assert!(matches!(k.delete(&admin, "CustomerGroup", spare_id, None).await, Err(Error::Conflict(m)) if m.contains("x_referred_by")));
    k.update(&admin, "Customer", ids[0], obj(json!({"x_referred_by": null})), None).await.unwrap();
    k.delete(&admin, "CustomerGroup", spare_id, None).await.expect("nothing links to it any more");
}
