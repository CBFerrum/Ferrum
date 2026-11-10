//! An automation's e-mail goes out only if its run commits, and once per event however often the event
//! is delivered (defect found 8 November 2026, docs/logic/evidence/w0-baseline.md; FLC-HOOK-002, 006),
//! against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first).

use ferrum_kernel::automation;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

const PACKAGE: &str = "package: notes\nversion: 0.1.0\ndepends: [core]\nroles:\n  - name: Writer\n    permissions:\n      - {entity: Note, read: true, write: true, create: true}\n";
const ENTITIES: &str =
    "name: Note\nlabel: Note\nkind: master\ntitle_field: title\nfields:\n  - {name: title, type: data, required: true}\n  - {name: body, type: text}\n";

/// An entity that never has a record.
const MEMO: &str = "name: Memo\nlabel: Memo\nkind: master\ntitle_field: title\nfields:\n  - {name: title, type: data}\n";

/// Mail the server's mailer was asked to send.
static SENT: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

struct Captured;

#[async_trait::async_trait]
impl ferrum_kernel::Mailer for Captured {
    async fn send(&self, _to: &str, subject: &str, _html: &str) -> ferrum_kernel::Result<()> {
        SENT.lock().unwrap().push(subject.to_string());
        Ok(())
    }
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

async fn drain(k: &Kernel) {
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
}

fn sent(subject: &str) -> usize {
    SENT.lock().unwrap().iter().filter(|s| s.as_str() == subject).count()
}

async fn queued(k: &Kernel, ctx: &Ctx, subject: &str) -> i64 {
    let mut tx = k.db.tenant_tx(ctx.tenant).await.unwrap();
    let n: i64 = sqlx::query_scalar("select count(*) from outbox where topic = 'ferrum.mail' and payload->>'subject' = $1")
        .bind(subject)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    n
}

/// The HTML of every mail queued with this subject.
async fn all_queued_html(k: &Kernel, ctx: &Ctx, subject: &str) -> Vec<String> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await.unwrap();
    let html = sqlx::query_scalar("select payload->>'html' from outbox where topic = 'ferrum.mail' and payload->>'subject' = $1")
        .bind(subject)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    html
}

/// The HTML of the one mail queued with this subject, read from its payload.
async fn queued_html(k: &Kernel, ctx: &Ctx, subject: &str) -> String {
    let mut tx = k.db.tenant_tx(ctx.tenant).await.unwrap();
    let payload: Value = sqlx::query_scalar("select payload from outbox where topic = 'ferrum.mail' and payload->>'subject' = $1")
        .bind(subject)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    payload["html"].as_str().unwrap().to_string()
}

const HOSTILE: &str = "<a href=\"https://evil.test\">click</a><script>x</script>";

#[test]
fn automation_mail_follows_its_run() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> = [("package.yaml", PACKAGE), ("entities/note.yaml", ENTITIES), ("entities/memo.yaml", MEMO)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let notes = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, notes]).unwrap(), HookRegistry::default(), KernelSettings::default(), Some(Arc::new(Captured)))
        .await
        .expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "mail".into(),
            name: "mail".into(),
            admin_email: "admin@mail.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["notes".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("mail", "admin@mail.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let auto = |name: &str, actions: Value| obj(json!({"name": name, "entity": "Note", "event": "created", "actions": actions}));
    let mail = |subject: &str| json!({"type": "email", "to": "'reader@example.com'", "subject": format!("'{subject}'"), "body": "doc.title"});

    // E-mail, then an action that fails: the run rolls back, and so must its mail.
    let failing = automation::save(
        &k,
        &admin,
        &auto("mail then fail", json!([mail("Rolled back"), {"type": "create_document", "entity": "Note", "values": {"body": "'no title'"}}])),
    )
    .await
    .unwrap();
    k.insert(&admin, "Note", obj(json!({"title": "First"}))).await.unwrap();
    drain(&k).await;
    drain(&k).await;
    assert_eq!(queued(&k, &admin, "Rolled back").await, 0, "nothing queued for a run that did not commit");
    assert_eq!(sent("Rolled back"), 0, "and nothing sent, on the first delivery or its retries");

    // A run that commits mails once, however often its event comes round.
    let mut off = auto("mail then fail", json!([mail("Rolled back")]));
    off.insert("id".into(), json!(failing.to_string()));
    off.insert("is_active".into(), json!(false));
    automation::save(&k, &admin, &off).await.unwrap();
    automation::save(&k, &admin, &auto("welcome", json!([mail("Welcome")]))).await.unwrap();
    let second = k.insert(&admin, "Note", obj(json!({"title": "Second"}))).await.unwrap();
    let second: uuid::Uuid = second["id"].as_str().unwrap().parse().unwrap();
    drain(&k).await;
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    // The event delivered again, as a crash after handling and before marking would.
    sqlx::query("update outbox set processed_at = null, attempts = 0, available_at = now() where topic = 'Note.created' and record_id = $1")
        .bind(second)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drain(&k).await;
    assert_eq!(queued(&k, &admin, "Welcome").await, 1, "queued with its run, once");
    assert_eq!(sent("Welcome"), 1, "and sent once");

    // A body is text: what a record holds (typed into a public form, say) never becomes markup in the mail.
    automation::save(&k, &admin, &auto("plain", json!([{"type": "email", "to": "'reader@example.com'", "subject": "'Plain'", "body": "doc.body"}])))
        .await
        .unwrap();
    let hostile = k.insert(&admin, "Note", obj(json!({"title": "Hostile", "body": format!("{HOSTILE}\nsecond line")}))).await.unwrap();
    drain(&k).await;
    let html = queued_html(&k, &admin, "Plain").await;
    assert!(!html.contains("<a") && !html.contains("<script"), "no tag from the record: {html}");
    assert!(html.contains("&lt;a href=&quot;https://evil.test&quot;&gt;click&lt;/a&gt;&lt;script&gt;"), "the text, escaped: {html}");
    assert!(html.contains("<br>second line"), "a line break kept: {html}");

    // A template makes formatted mail: its own markup stays, and what it prints from the record is escaped.
    let templated = |template: &str| {
        auto("formatted", json!([{"type": "email", "to": "'reader@example.com'", "subject": "'Formatted'", "body": "''", "template": template}]))
    };
    let refused = automation::save(&k, &admin, &templated("<p>Hello {{ doc.title </p>")).await.unwrap_err();
    assert!(matches!(refused, ferrum_kernel::Error::Validation(_)), "a template that does not compile is refused: {refused}");
    let too_large = "x".repeat(automation::MAX_TEMPLATE + 1);
    assert!(automation::save(&k, &admin, &templated(&too_large)).await.is_err(), "and one too large");
    automation::save(&k, &admin, &templated("<p>Hello {{ doc.body }}</p>")).await.unwrap();
    let templated_note = k.insert(&admin, "Note", obj(json!({"title": "Templated", "body": HOSTILE}))).await.unwrap();
    drain(&k).await;
    let html = queued_html(&k, &admin, "Formatted").await;
    assert!(html.starts_with("<p>Hello ") && html.ends_with("</p>"), "the template's markup stays: {html}");
    assert!(!html.contains("<a") && !html.contains("<script"), "no tag from the record: {html}");
    assert!(html.contains("&lt;a href=&quot;") && html.contains("&lt;script&gt;x&lt;"), "the value, escaped: {html}");

    // A preview is what the send made, byte for byte, for the same record.
    let id = |d: &Doc| d["id"].as_str().unwrap().parse::<uuid::Uuid>().unwrap();
    let plain = automation::preview_mail(&k, &admin, "Note", Some("doc.body"), None, Some(id(&hostile))).await.unwrap();
    assert!(all_queued_html(&k, &admin, "Plain").await.contains(&plain.0), "body: the same HTML as a mail queued");
    assert_eq!(plain.1, Some(id(&hostile)));
    let src = "<p>Hello {{ doc.body }}</p>";
    let fmt = automation::preview_mail(&k, &admin, "Note", Some("''"), Some(src), Some(id(&templated_note))).await.unwrap();
    assert_eq!(fmt.0, queued_html(&k, &admin, "Formatted").await, "template: the same HTML as the mail queued");

    // Escaped text with a line break, or the template's own markup around an escaped value.
    assert!(plain.0.contains("&lt;script&gt;") && plain.0.contains("<br>second line") && !plain.0.contains("<script"), "{}", plain.0);
    assert!(fmt.0.starts_with("<p>Hello &lt;a href") && !fmt.0.contains("<script"), "{}", fmt.0);

    // No record named: the most recently changed one the caller may read.
    let latest = automation::preview_mail(&k, &admin, "Note", Some("doc.title"), None, None).await.unwrap();
    assert_eq!(latest, ("Templated".into(), Some(id(&templated_note))));

    // Refusals are field errors on the right field, with the engine's message.
    let field_of = |r: ferrum_kernel::Result<(String, Option<uuid::Uuid>)>| match r.unwrap_err() {
        ferrum_kernel::Error::Validation(f) => (f[0].field.clone(), f[0].message.clone()),
        e => panic!("not a field error: {e}"),
    };
    let (f, m) = field_of(automation::preview_mail(&k, &admin, "Note", None, Some("<p>{{ doc.title </p>"), None).await);
    assert!(f == "template" && m.contains("e-mail template"), "{f}: {m}");
    let (f, m) = field_of(automation::preview_mail(&k, &admin, "Note", None, Some("{{ doc.title.nope.deeper() }}"), None).await);
    assert!(f == "template" && m.contains("failed"), "{f}: {m}");
    assert_eq!(field_of(automation::preview_mail(&k, &admin, "Note", Some("doc.title +"), None, None).await).0, "body");
    assert_eq!(field_of(automation::preview_mail(&k, &admin, "Note", Some("doc.title.nope.x"), None, None).await).0, "body");
    assert_eq!(field_of(automation::preview_mail(&k, &admin, "Note", None, Some(&too_large), None).await).0, "template");
    assert!(automation::preview_mail(&k, &admin, "Note", Some("''"), None, Some(uuid::Uuid::now_v7())).await.is_err(), "unknown record");

    // Nothing sent and nothing written by previewing.
    assert_eq!(queued(&k, &admin, "Plain").await, 2, "one for each record made, none for a preview");

    // A refused action names its place: the second action is the invalid one.
    let bad = auto("bad second", json!([mail("Fine"), {"type": "email", "to": "'a@b.c'", "subject": "'s'", "body": "doc.title +"}]));
    match automation::save(&k, &admin, &bad).await.unwrap_err() {
        ferrum_kernel::Error::Validation(f) => assert_eq!((f[0].field.as_str(), f[0].row.clone()), ("actions", Some(("actions".to_string(), 1)))),
        e => panic!("{e}"),
    }

    // An entity with no records renders against an empty record.
    let empty = automation::preview_mail(&k, &admin, "Memo", Some("doc.title"), None, None).await.unwrap();
    assert_eq!(empty, (String::new(), None));
}
