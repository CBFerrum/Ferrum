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
    let settings = KernelSettings { pdf_url: Some(fake_pdf_renderer().await), ..Default::default() };
    Kernel::boot(db, catalog, HookRegistry::default(), settings, Some(Arc::new(Captured))).await.expect("boot")
}

/// Request bodies the stand-in PDF renderer received.
static RENDERED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// A stand-in for Gotenberg: records each form it is sent and answers with a tiny PDF.
async fn fake_pdf_renderer() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let body = loop {
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&chunk[..n]);
                let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                let len: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                if buf.len() >= end + 4 + len {
                    break Some(String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).to_string());
                }
            };
            if let Some(b) = body {
                RENDERED.lock().unwrap().push(b);
            }
            let pdf = b"%PDF-1.7 stand-in";
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", pdf.len());
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(pdf).await;
        }
    });
    format!("http://{addr}")
}

/// Mail the kernel sent, in order.
static SENT: std::sync::Mutex<Vec<(String, String, String)>> = std::sync::Mutex::new(Vec::new());

struct Captured;

#[async_trait::async_trait]
impl ferrum_kernel::Mailer for Captured {
    async fn send(&self, to: &str, subject: &str, html: &str) -> ferrum_kernel::Result<()> {
        SENT.lock().unwrap().push((to.into(), subject.into(), html.into()));
        Ok(())
    }

    fn can_attach(&self) -> bool {
        true
    }

    async fn send_with(&self, to: &str, subject: &str, html: &str, attachments: &[ferrum_kernel::MailAttachment]) -> ferrum_kernel::Result<()> {
        ATTACHED.lock().unwrap().extend(attachments.iter().map(|a| (a.file_name.clone(), a.content_type.clone(), a.data.clone())));
        self.send(to, subject, html).await
    }
}

/// Files the kernel attached to mail, in order.
static ATTACHED: std::sync::Mutex<Vec<(String, String, Vec<u8>)>> = std::sync::Mutex::new(Vec::new());

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

#[test]
fn collaboration_follows_record_access() {
    // One long scenario: its async state machine needs more than the default 2 MiB test stack.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let k = fresh_kernel().await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "collab".into(),
            name: "Collab Co".into(),
            admin_email: "admin@collab.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: Some("Ada Admin".into()),
            packages: vec!["selling".into()],
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

    // ── PDFs: the print format, laid out by the renderer, for those who may read the record ──
    let pdf = k.print_pdf(&admin, "Customer", cid).await.expect("pdf");
    assert!(pdf.starts_with(b"%PDF"));
    let sent = RENDERED.lock().unwrap().last().cloned().unwrap_or_default();
    assert!(sent.contains("filename=\"index.html\"") && sent.contains("Globex") && sent.contains("name=\"preferCssPageSize\""), "{sent}");
    assert!(k.print_pdf(&otto_ctx, "Customer", cid).await.is_err(), "no PDF of a record you cannot read");

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

    // ── queued mail goes out once after commit, never after a rollback ──
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    k.queue_mail(&mut tx, tenant, "guest@example.test", "Your booking", "<p>See you</p>").await.unwrap();
    tx.rollback().await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert!(k.queue_mail(&mut tx, tenant, "a@x.test, b@x.test", "Hi", "x").await.is_err(), "one address");
    assert!(k.queue_mail(&mut tx, tenant, "guest@example.test", "Hi\r\nBcc: spy@x.test", "x").await.is_err(), "no header injection");
    k.queue_mail(&mut tx, tenant, "guest@example.test", "Your booking", "<p>See you</p>").await.unwrap();
    tx.commit().await.unwrap();
    drain(&k).await;
    drain(&k).await;
    assert_eq!(SENT.lock().unwrap().clone(), vec![("guest@example.test".to_string(), "Your booking".to_string(), "<p>See you</p>".to_string())]);

    // ── queued mail can carry stored files, read when it goes out ──
    let pdf = k.store_file(&admin, Some(("Customer", cid)), "quote.pdf", "application/pdf", b"%PDF-1.7 quote").await.unwrap();
    let pdf_id: Uuid = pdf["id"].as_str().unwrap().parse().unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert!(k.queue_mail_with(&mut tx, tenant, "guest@example.test", "Quote", "x", &[Uuid::now_v7()]).await.is_err(), "only stored files");
    k.queue_mail_with(&mut tx, tenant, "guest@example.test", "Your quote", "<p>Attached</p>", &[pdf_id]).await.unwrap();
    tx.commit().await.unwrap();
    drain(&k).await;
    assert_eq!(SENT.lock().unwrap().last().map(|m| m.1.clone()), Some("Your quote".to_string()));
    assert_eq!(ATTACHED.lock().unwrap().clone(), vec![("quote.pdf".to_string(), "application/pdf".to_string(), b"%PDF-1.7 quote".to_vec())]);

    // ── objects an app released go from storage once its transaction commits, only its own ──
    let key = |id: Uuid| format!("tenants/{tenant}/{id}");
    let gone = k.store_file(&admin, Some(("Customer", cid)), "old.txt", "text/plain", b"old").await.unwrap();
    let gone: Uuid = gone["id"].as_str().unwrap().parse().unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    ferrum_kernel::outbox::release_objects(&mut tx, tenant, &[key(gone)]).await.unwrap();
    tx.rollback().await.unwrap();
    drain(&k).await;
    assert!(objects.0.lock().unwrap().contains_key(&key(gone)), "a rolled-back release deletes nothing");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert!(ferrum_kernel::outbox::release_objects(&mut tx, tenant, &[format!("tenants/{}/x", Uuid::now_v7())]).await.is_err(), "another workspace's objects");
    assert!(ferrum_kernel::outbox::release_objects(&mut tx, tenant, &[format!("tenants/{tenant}/../x")]).await.is_err());
    sqlx::query("delete from file_object where tenant_id = $1 and id = $2").bind(tenant).bind(gone).execute(&mut *tx).await.unwrap();
    ferrum_kernel::outbox::release_objects(&mut tx, tenant, &[key(gone)]).await.unwrap();
    tx.commit().await.unwrap();
    drain(&k).await;
    assert!(!objects.0.lock().unwrap().contains_key(&key(gone)), "released after commit");

    // ── rules with conditions reach only records whose fields hold the values; rules add up ──
    let rule = |v: Value| serde_json::from_value::<PermissionDef>(v).unwrap();
    let bad = rule(json!({"entity": "Customer", "read": true, "where": {"no_such_field": "x"}}));
    assert!(k.set_role_permission(&admin, "Retail Desk", &bad).await.is_err(), "conditions name real fields");
    let bad = rule(json!({"entity": "Customer", "read": true, "where": {"customer_group": "Retail"}}));
    assert!(k.set_role_permission(&admin, "Retail Desk", &bad).await.is_err(), "links are tested by record id");
    let bad = rule(json!({"entity": "Customer", "read": true, "where": {"customer_name": "x"}}));
    assert!(k.set_role_permission(&admin, "Retail Desk", &bad).await.is_ok(), "data fields may be tested");
    k.set_role_permission(&admin, "Retail Desk", &rule(json!({"entity": "Customer", "where": {"customer_name": "x"}}))).await.unwrap();
    let people = rule(json!({"entity": "Customer", "read": true, "where": {"customer_type": ["Individual"], "disabled": false}}));
    k.set_role_permission(&admin, "Retail Desk", &people).await.unwrap();
    let rita = k.create_user(&admin, "rhea@collab.test", "Rhea Retail", Some("Correct-Horse-9"), &["Retail Desk".into()], false).await.unwrap();
    let rita_ctx = k.load_ctx(tenant, rita, None, "it".into()).await.unwrap();
    let id_of = |d: &ferrum_kernel::Doc| d["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let person = k.insert(&admin, "Customer", obj(json!({"customer_name": "Pat Person", "customer_type": "Individual"}))).await.unwrap();
    let firm = k.insert(&admin, "Customer", obj(json!({"customer_name": "Firm Ltd", "customer_type": "Company"}))).await.unwrap();
    k.insert(&admin, "Customer", obj(json!({"customer_name": "Old Person", "customer_type": "Individual", "disabled": true}))).await.unwrap();
    let names = |ctx: Ctx| {
        let k = k.clone();
        async move {
            let page = k.list(&ctx, "Customer", &ListQuery { limit: Some(100), ..Default::default() }).await.unwrap();
            let mut n: Vec<String> = page.rows.iter().map(|r| r["customer_name"].as_str().unwrap().to_string()).collect();
            n.sort();
            n
        }
    };
    assert_eq!(names(rita_ctx.clone()).await, ["Pat Person"]);
    assert!(k.get(&rita_ctx, "Customer", id_of(&person), false).await.is_ok());
    assert!(k.get(&rita_ctx, "Customer", id_of(&firm), false).await.is_err(), "a firm is outside the rule");
    let filtered = k
        .list(
            &rita_ctx,
            "Customer",
            &ListQuery { filters: ferrum_kernel::query::parse_filters(&json!({"customer_type": "Company"})).unwrap(), ..Default::default() },
        )
        .await
        .unwrap();
    assert!(filtered.rows.is_empty(), "filters cannot reach past the rule");
    // A second rule: the firms she created herself. Creating needs no condition.
    k.set_role_permission(&admin, "Retail Desk", &rule(json!({"entity": "Customer", "read": true, "if_owner": true, "where": {"customer_type": "Company"}})))
        .await
        .unwrap();
    k.set_role_permission(&admin, "Retail Desk", &rule(json!({"entity": "Customer", "create": true}))).await.unwrap();
    let rita_ctx = k.load_ctx(tenant, rita, None, "it".into()).await.unwrap();
    k.insert(&rita_ctx, "Customer", obj(json!({"customer_name": "Rita Firm", "customer_type": "Company"}))).await.unwrap();
    assert_eq!(names(rita_ctx.clone()).await, ["Pat Person", "Rita Firm"], "her own firm, not other firms");

    // ── engine-kept child rows: replaced whole, ids kept, other parents' rows untouched ──
    let find = |entity: &'static str, name: &'static str| {
        let k = k.clone();
        let admin = admin.clone();
        async move {
            let field = if entity == "Uom" { "uom_name" } else { "group_name" };
            let page = k
                .list(&admin, entity, &ListQuery { filters: ferrum_kernel::query::parse_filters(&json!({field: name})).unwrap(), ..Default::default() })
                .await
                .unwrap();
            page.rows[0]["id"].as_str().unwrap().to_string()
        }
    };
    let (nos, boxes, sets, group) = (find("Uom", "Nos").await, find("Uom", "Box").await, find("Uom", "Set").await, find("ItemGroup", "Products").await);
    let item = |code: &str| json!({"item_code": code, "item_name": code, "item_group": group, "stock_uom": nos});
    let a = id_of(&k.insert(&admin, "Item", obj(item("KEPT-A"))).await.unwrap());
    let b = id_of(&k.insert(&admin, "Item", obj(item("KEPT-B"))).await.unwrap());
    let sys = Ctx::system(tenant, k.registry.schema(tenant).await.unwrap());
    let set = |parent: Uuid, rows: Vec<Value>| {
        let (k, sys) = (k.clone(), sys.clone());
        async move {
            let mut tx = k.db.tenant_tx(tenant).await.unwrap();
            k.set_child_rows_tx(&sys, &mut tx, "Item", "uoms", parent, rows.into_iter().map(obj).collect()).await?;
            tx.commit().await.unwrap();
            Ok::<_, Error>(())
        }
    };
    set(a, vec![json!({"uom": boxes, "conversion_factor": "12"}), json!({"uom": sets, "conversion_factor": 4})]).await.unwrap();
    let units = k.get(&admin, "Item", a, false).await.unwrap()["uoms"].as_array().unwrap().clone();
    assert_eq!(units.len(), 2);
    assert_eq!(units[1]["conversion_factor"].as_str().map(|v| v.parse::<rust_decimal::Decimal>().unwrap()), Some(4.into()), "values are normalized");
    let kept = units[0]["id"].clone();
    set(a, vec![json!({"id": kept, "uom": boxes, "conversion_factor": "10"})]).await.unwrap();
    let units = k.get(&admin, "Item", a, false).await.unwrap()["uoms"].as_array().unwrap().clone();
    assert_eq!((units.len(), units[0]["id"].clone()), (1, kept.clone()), "replaced whole, the id kept");
    assert!(set(a, vec![json!({"uom": boxes, "factor": "1"})]).await.is_err(), "unknown fields are refused");
    set(b, vec![json!({"id": kept, "uom": sets, "conversion_factor": "2"})]).await.unwrap();
    let a_units = k.get(&admin, "Item", a, false).await.unwrap()["uoms"].as_array().unwrap().clone();
    assert_eq!((a_units.len(), a_units[0]["uom"].as_str()), (1, Some(boxes.as_str())), "another parent cannot take a row by its id");

    // ── personal-data requests: export, erasure, audit redaction, the login account ──
    let pat = id_of(
        &k.insert(
            &admin,
            "Customer",
            obj(json!({"customer_name": "Pat Private", "customer_type": "Individual", "email": "Pat@Private.test", "phone": "+44 20 7946 0000"})),
        )
        .await
        .unwrap(),
    );
    k.update(&admin, "Customer", pat, obj(json!({"billing_address": "1 Quiet Lane"})), None).await.unwrap();
    let firm = id_of(
        &k.insert(&admin, "Customer", obj(json!({"customer_name": "Pat's Firm Ltd", "customer_type": "Company", "email": "pat@private.test"}))).await.unwrap(),
    );
    k.create_user(&admin, "pat@private.test", "Pat Private", Some("Correct-Horse-9"), &["Retail Desk".into()], false).await.unwrap();
    let refused = k.personal_data(&rita_ctx, "pat@private.test").await;
    assert!(matches!(refused, Err(Error::Forbidden(_))), "administrators only");
    let found = k.personal_data(&admin, "PAT@private.test").await.unwrap();
    let ids: Vec<&str> = found["records"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), 2, "{found}");
    assert_eq!(found["account"]["full_name"], json!("Pat Private"));
    assert!(k.erase_personal_data(&admin, "pat@private.test", " ").await.is_err(), "a reason is required");
    let done = k.erase_personal_data(&admin, "pat@private.test", "Request #42").await.unwrap();
    assert_eq!((done["erased"].as_array().unwrap().len(), done["account_erased"].clone()), (2, json!(true)), "{done}");
    let person = k.get(&admin, "Customer", pat, false).await.unwrap();
    assert!(person["customer_name"].as_str().unwrap().starts_with("Erased "), "{person:?}");
    assert!(person["email"].is_null() && person["phone"].is_null() && person["billing_address"].is_null());
    let company = k.get(&admin, "Customer", firm, false).await.unwrap();
    assert_eq!((company["customer_name"].clone(), company["email"].clone()), (json!("Pat's Firm Ltd"), Value::Null), "a company keeps its name");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let history: Vec<String> = sqlx::query_scalar("select diff::text from audit_event where tenant_id = $1 and record_id = $2")
        .bind(tenant)
        .bind(pat)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert!(!history.iter().any(|d| d.contains("Pat Private") || d.contains("Quiet Lane") || d.contains("7946")), "{history:?}");
    assert_eq!(ferrum_kernel::audit::verify_record(&mut tx, tenant, "Customer", pat).await.unwrap(), None, "the chain still holds");
    let tamper = sqlx::query("update audit_event set diff = '{}' where tenant_id = $1 and record_id = $2").bind(tenant).bind(pat).execute(&mut *tx).await;
    assert!(tamper.is_err(), "only a recorded redaction may change history");
    drop(tx);
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let account: (String, bool) = sqlx::query_as("select full_name, is_active from app_user where tenant_id = $1 and email::text like 'erased-%'")
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    drop(tx);
    assert!(account.0.starts_with("Erased user") && !account.1);
    assert!(k.login("collab", "pat@private.test", "Correct-Horse-9").await.is_err(), "the account no longer signs in");
    let again = k.personal_data(&admin, "pat@private.test").await.unwrap();
    assert!(again["records"].as_array().unwrap().is_empty() && again["account"].is_null(), "nothing is left to find");

    // ── notifications: each kind reaches a person the way they chose ──
    let choose = |kind: &'static str, channel: &'static str| {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(tenant).await.unwrap();
            sqlx::query("insert into notification_pref (tenant_id, user_id, kind, channel) values ($1, $2, $3, $4) on conflict (tenant_id, user_id, kind) do update set channel = excluded.channel")
                .bind(tenant)
                .bind(bea)
                .bind(kind)
                .bind(channel)
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
    };
    let delivered = |title: &'static str| {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(tenant).await.unwrap();
            let rows: Vec<(String, String, bool)> = sqlx::query_as(
                "select kind, delivery, emailed_at is not null from notification where tenant_id = $1 and user_id = $2 and title = $3 order by created_at",
            )
            .bind(tenant)
            .bind(bea)
            .bind(title)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
            tx.commit().await.unwrap();
            rows
        }
    };
    choose("mention", "email").await;
    choose("comment", "off").await;
    k.set_following(&bea_ctx, "Customer", cid, true).await.unwrap();
    let mails = SENT.lock().unwrap().len();
    k.add_comment(&admin, "Customer", cid, "A word for Bea", &[bea]).await.unwrap();
    k.add_comment(&admin, "Customer", cid, "And one for everyone", &[]).await.unwrap();
    drain(&k).await;
    assert_eq!(
        delivered("Ada Admin mentioned you on Customer Globex").await.last(),
        Some(&("mention".to_string(), "email".to_string(), true)),
        "in the bar and emailed"
    );
    let sent = SENT.lock().unwrap()[mails..].to_vec();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!((sent[0].0.as_str(), sent[0].1.as_str()), ("bea@collab.test", "Ada Admin mentioned you on Customer Globex"));
    assert!(sent[0].2.contains("Collab Co") && sent[0].2.contains("A word for Bea"), "{}", sent[0].2);
    assert!(delivered("Ada Admin commented on Customer Globex").await.iter().all(|r| r.0 != "comment" || r.1 != "inapp"), "comments are off");
    assert!(!unread(&k, &bea_ctx).await.iter().any(|t| t == "Ada Admin commented on Customer Globex"), "nothing in the bar either");
    // Sent once: a retry of the event finds it emailed.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let emailed: Uuid = sqlx::query_scalar("select id from notification where tenant_id = $1 and user_id = $2 and delivery = 'email'")
        .bind(tenant)
        .bind(bea)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    ferrum_kernel::outbox::enqueue(&mut tx, tenant, ferrum_kernel::notify::MAIL_TOPIC, None, None, json!({"notification": emailed})).await.unwrap();
    tx.commit().await.unwrap();
    drain(&k).await;
    assert_eq!(SENT.lock().unwrap().len(), mails + 1);

    // Changes to what Bea follows, in a daily digest: once the oldest has waited a day, then not again.
    choose("watch", "digest").await;
    k.update(&admin, "Customer", cid, obj(json!({"customer_name": "Globex Corp"})), None).await.unwrap();
    drain(&k).await;
    assert_eq!(delivered("Customer Globex Corp was updated").await, vec![("watch".to_string(), "digest".to_string(), false)]);
    assert_eq!(k.send_digests().await.unwrap(), 0, "not a day old yet");
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update notification set created_at = created_at - interval '25 hours' where tenant_id = $1 and user_id = $2 and delivery = 'digest'")
        .bind(tenant)
        .bind(bea)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(k.send_digests().await.unwrap(), 1);
    assert_eq!(k.send_digests().await.unwrap(), 0, "taken once");
    drain(&k).await;
    let digest = SENT.lock().unwrap().last().cloned().unwrap();
    assert_eq!((digest.0.as_str(), digest.1.as_str()), ("bea@collab.test", "1 notification in Collab Co"));
    assert!(digest.2.contains("Customer Globex Corp was updated") && digest.2.contains("daily digest"), "{}", digest.2);
    assert_eq!(delivered("Customer Globex Corp was updated").await, vec![("watch".to_string(), "digest".to_string(), true)]);
    // Read in the bar before the digest goes: not emailed.
    k.update(&admin, "Customer", cid, obj(json!({"customer_name": "Globex Inc"})), None).await.unwrap();
    drain(&k).await;
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update notification set created_at = created_at - interval '25 hours', read_at = now() where tenant_id = $1 and title = 'Customer Globex Inc was updated'")
        .bind(tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(k.send_digests().await.unwrap(), 0, "read already");
    // Off: nothing at all; the default again: in the bar.
    choose("watch", "off").await;
    k.update(&admin, "Customer", cid, obj(json!({"customer_name": "Globex Two"})), None).await.unwrap();
    drain(&k).await;
    assert!(delivered("Customer Globex Two was updated").await.is_empty());
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("delete from notification_pref where tenant_id = $1 and user_id = $2 and kind = 'watch'")
        .bind(tenant)
        .bind(bea)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    k.update(&admin, "Customer", cid, obj(json!({"customer_name": "Globex Three"})), None).await.unwrap();
    drain(&k).await;
    assert_eq!(delivered("Customer Globex Three was updated").await, vec![("watch".to_string(), "inapp".to_string(), false)]);
    // ── approvals: records waiting for a decision only some roles may make ──
    let manager = PermissionDef { entity: "Customer".into(), read: true, write: true, create: true, ..Default::default() };
    k.set_role_permission(&admin, "Sales Manager", &manager).await.unwrap();
    let maya = k.create_user(&admin, "maya@collab.test", "Maya Manager", Some("Correct-Horse-9"), &["Sales Manager".into()], false).await.unwrap();
    let maya_ctx = k.load_ctx(tenant, maya, None, "it".into()).await.unwrap();
    let workflow: ferrum_meta::WorkflowDef = serde_json::from_value(json!({
        "name": "Customer vetting", "entity": "Customer", "initial_state": "Draft",
        "states": [{"name": "Draft"}, {"name": "Pending"}, {"name": "Approved"}],
        "transitions": [
            {"action": "Send for approval", "from": "Draft", "to": "Pending"},
            {"action": "Approve", "from": "Pending", "to": "Approved", "roles": ["Sales Manager"], "not_creator": true,
             "condition": "doc.customer_name != 'Blocked Ltd'"},
        ],
    }))
    .unwrap();
    k.add_customization(&admin, "Customer", ferrum_meta::OverlayOp::SetWorkflow { workflow }, None).await.unwrap();
    let (bea_ctx, maya_ctx) = (k.load_ctx(tenant, bea, None, "it".into()).await.unwrap(), k.load_ctx(tenant, maya_ctx.user, None, "it".into()).await.unwrap());
    let pending = |ctx: &ferrum_kernel::Ctx, name: &str| {
        let (k, ctx, name) = (k.clone(), ctx.clone(), name.to_string());
        async move {
            let c = id_of(&k.insert(&ctx, "Customer", obj(json!({"customer_name": name}))).await.unwrap());
            k.transition(&ctx, "Customer", c, "Send for approval", None).await.unwrap();
            c
        }
    };
    let vetted = pending(&bea_ctx, "Vetting Co").await;
    pending(&bea_ctx, "Blocked Ltd").await;
    pending(&maya_ctx, "Maya's Own").await;
    let waiting = k.my_approvals(&maya_ctx, 50).await.unwrap();
    let titles: Vec<&str> = waiting.iter().map(|a| a["title"].as_str().unwrap()).collect();
    assert_eq!(titles, vec!["Vetting Co"], "not her own (a second person approves), not one whose condition fails: {waiting:?}");
    assert_eq!((waiting[0]["state"].clone(), waiting[0]["actions"].clone()), (json!("Pending"), json!(["Approve"])));
    assert!(k.my_approvals(&bea_ctx, 50).await.unwrap().is_empty(), "Bea cannot approve");
    assert!(k.my_approvals(&admin, 50).await.unwrap().is_empty(), "an administrator may approve, but it is not their duty");
    k.transition(&maya_ctx, "Customer", vetted, "Approve", None).await.unwrap();
    assert!(k.my_approvals(&maya_ctx, 50).await.unwrap().is_empty(), "decided");

    // ── cards: what a preview of a record shows, for those who may read it ──
    let group = id_of(&k.insert(&admin, "CustomerGroup", obj(json!({"group_name": "Wholesale"}))).await.unwrap());
    let carded = id_of(
        &k.insert(&admin, "Customer", obj(json!({"customer_name": "Card Co", "customer_group": group.to_string(), "email": "hello@card.test"}))).await.unwrap(),
    );
    let card = k.card(&maya_ctx, "Customer", carded).await.unwrap();
    assert_eq!((card["title"].clone(), card["kind"].clone(), card["state"].clone()), (json!("Card Co"), json!("Customer"), json!("Draft")));
    let fields: Vec<(String, Value)> =
        card["fields"].as_array().unwrap().iter().map(|f| (f["name"].as_str().unwrap().to_string(), f["value"].clone())).collect();
    assert_eq!(
        fields,
        vec![("customer_group".to_string(), json!("Wholesale")), ("email".to_string(), json!("hello@card.test"))],
        "the list's fields, a link by its title, not the title again"
    );
    assert_eq!(card["url"], json!(format!("/open/Customer/{carded}")));
    assert!(matches!(k.card(&otto_ctx, "Customer", carded).await, Err(Error::Forbidden(_) | Error::NotFound(_))), "only for those who may read it");

    assert_eq!(ferrum_kernel::notify::declared_default(&["inapp", "email"]), "email");
    assert_eq!(ferrum_kernel::notify::declared_default(&["push"]), "inapp");
    assert_eq!(ferrum_kernel::notify::declared_default(&[]), "off");
}
