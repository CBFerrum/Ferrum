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
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/note.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
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
    k.insert(&admin, "Note", obj(json!({"title": "Hostile", "body": format!("{HOSTILE}\nsecond line")}))).await.unwrap();
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
    k.insert(&admin, "Note", obj(json!({"title": "Templated", "body": HOSTILE}))).await.unwrap();
    drain(&k).await;
    let html = queued_html(&k, &admin, "Formatted").await;
    assert!(html.starts_with("<p>Hello ") && html.ends_with("</p>"), "the template's markup stays: {html}");
    assert!(!html.contains("<a") && !html.contains("<script"), "no tag from the record: {html}");
    assert!(html.contains("&lt;a href=&quot;") && html.contains("&lt;script&gt;x&lt;"), "the value, escaped: {html}");
}
