//! Message templates and channels (crates/kernel/src/messages.rs, BD-20), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first): a package's defaults and a workspace's edits, the
//! `message` effect, channels tried in order and recorded send by send, languages, opt-outs, quiet hours, limits,
//! one-time codes, a provider described as configuration and its delivery reports and replies, and erasure.

use ferrum_kernel::messages::{self, ChannelInput, Message, Recipient};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::vault::{self, Kdf, Opener};
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, MessageDef};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PACKAGE: &str = r#"package: notes
version: 0.1.0
depends: [core]
roles:
  - name: Writer
    permissions:
      - {entity: Note, read: true, write: true, create: true}
messages:
  - key: notes.welcome
    entity: Note
    languages:
      en:
        email: {subject: "Welcome {{ doc.title }}", body: "<p>Hello {{ doc.title }}</p>"}
        text: {text: "Hello {{ doc.title }}"}
        inapp: {title: "Note {{ doc.title }}"}
      de:
        email: {subject: "Willkommen {{ doc.title }}", body: "<p>Hallo {{ doc.title }}</p>"}
        text: {text: "Hallo {{ doc.title }}"}
  - key: notes.promo
    category: promotional
    languages:
      en:
        email: {subject: "Sale", body: "<p>Sale! {{ opt_out }}</p>"}
        text: {text: "Sale! {{ opt_out }}"}
        chat: {template: "sale_2026", values: ["{{ values.percent }}"]}
    values: [percent]
  - key: notes.code
    values: [code]
    languages:
      en:
        text: {text: "Your code is {{ values.code }}"}
"#;
const ENTITIES: &str = "name: Note\nlabel: Note\nkind: master\ntitle_field: title\nfields:\n  - {name: title, type: data, required: true}\n  - {name: email, type: email}\n  - {name: phone, type: phone}\n  - {name: pin, type: secret}\n";

static SENT: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

struct Captured;

#[async_trait::async_trait]
impl ferrum_kernel::Mailer for Captured {
    async fn send(&self, to: &str, subject: &str, _html: &str) -> ferrum_kernel::Result<()> {
        SENT.lock().unwrap().push((to.to_string(), subject.to_string()));
        Ok(())
    }
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

async fn drain(k: &Kernel) {
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
}

/// Runs every job due, and those waiting for a retry too.
async fn run_jobs(k: &Kernel) {
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update job set run_at = now() where status = 'queued'").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    while k.run_jobs_once(4).await.unwrap() > 0 {}
}

async fn send(k: &Kernel, ctx: &Ctx, m: Message) -> Vec<messages::Sent> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await.unwrap();
    let sent = messages::send(k, &mut tx, ctx, &m).await.unwrap();
    tx.commit().await.unwrap();
    sent
}

async fn row(k: &Kernel, ctx: &Ctx, id: uuid::Uuid) -> Value {
    let mut tx = k.db.tenant_tx(ctx.tenant).await.unwrap();
    let v: Value = sqlx::query_scalar("select to_jsonb(s) from message_send s where tenant_id = $1 and id = $2")
        .bind(ctx.tenant)
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    v
}

fn to(phone: &str) -> Vec<Recipient> {
    vec![Recipient { phone: Some(phone.into()), ..Default::default() }]
}

/// A provider: answers each request with the next of `answers` (status, body) and keeps what it was sent.
async fn provider(answers: Arc<Mutex<Vec<(u16, String)>>>, seen: Arc<Mutex<Vec<String>>>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            let (answers, seen) = (answers.clone(), seen.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                seen.lock().unwrap().push(String::from_utf8_lossy(&buf).to_string());
                let (status, body) = answers.lock().unwrap().remove(0);
                let reply =
                    format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(reply.as_bytes()).await;
            });
        }
    });
    port
}

fn hmac_hex(secret: &str, body: &[u8]) -> String {
    use hmac::Mac;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// The tests one at a time: each starts from an empty database.
static SERIAL: Mutex<()> = Mutex::new(());

fn run<F: std::future::Future<Output = ()> + Send + 'static>(f: F) {
    let _one = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(f).await.unwrap() });
}

#[test]
fn messages_from_templates_on_every_channel() {
    run(scenario());
}

#[test]
fn a_temporary_refusal_keeps_no_secret_value() {
    run(temporary_refusal());
}

#[test]
fn a_mail_servers_refusal_names_no_recipient() {
    run(mail_refusal());
}

/// An empty database, the vault open, a workspace with the notes package and its administrator.
async fn boot(mailer: Arc<dyn ferrum_kernel::Mailer>) -> (Arc<Kernel>, Ctx) {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let opener = Opener { database_url: url, file: PathBuf::from("/nonexistent/root.key"), transit: None, command: None, kdf: Kdf { m_kib: 64, t: 1, p: 1 } };
    vault::open(&db, opener, false).await.expect("the vault opens");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/note.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let notes = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let settings = KernelSettings { webhooks_allow_private: true, public_url: Some("https://ferrum.test".into()), ..Default::default() };
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, notes]).unwrap(), HookRegistry::default(), settings, Some(mailer)).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "msg".into(),
            name: "msg".into(),
            admin_email: "admin@msg.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["notes".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("msg", "admin@msg.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    (k, admin)
}

/// A provider's answer that echoes what it was sent (TP-S1): the message's secret value, the channel's credential and the
/// recipient's number are dots in the job's error, whether the refusal is temporary (the job's `last_error`, the server's
/// log) or final (the send's `error`).
async fn temporary_refusal() {
    let (k, admin) = boot(Arc::new(Captured)).await;
    k.update_tenant_settings(&admin, json!({"messages": {"calling_code": "+49", "trunk_prefix": "0"}}).as_object().unwrap()).await.unwrap();
    let (code, token, number) = ("482913", "s3cr3t-t0ken", "+491701234567");
    let echo = format!(r#"{{"error": {{"message": "you sent 'Your code is {code}' to {number} as Bearer {token} (digits 491701234567)"}}}}"#);
    let answers = Arc::new(Mutex::new(vec![(503, echo.clone()), (400, echo)]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let port = provider(answers, seen.clone()).await;
    let channel = ChannelInput {
        kind: "text".into(),
        name: "Gateway".into(),
        description: serde_json::from_value(json!({
            "url": format!("http://127.0.0.1:{port}/send"),
            "headers": {"Authorization": "Bearer {{ secret.token }}"},
            "body": r#"{"to": {{ to|json }}, "text": {{ text|json }}}"#,
            "error_at": "/error/message"
        }))
        .unwrap(),
        secrets: Some(BTreeMap::from([("token".to_string(), token.to_string())])),
        enabled: true,
    };
    k.save_message_channel(&admin, None, &channel).await.unwrap();
    let m = Message {
        template: "notes.code".into(),
        to: to("0170 1234567"),
        channels: vec!["text".into()],
        values: serde_json::Map::from_iter([("code".to_string(), json!(code))]),
        secret: vec!["code".into()],
        ..Default::default()
    };
    let s = send(&k, &admin, m).await;
    let job_error = || {
        let k = k.clone();
        async move {
            let mut tx = k.db.system_tx().await.unwrap();
            let e: Option<String> =
                sqlx::query_scalar("select last_error from job where kind = $1").bind(messages::DELIVER_KIND).fetch_one(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            e.unwrap_or_default()
        }
    };
    let clear = |said: &str, what: &str| {
        for secret in [code, token, number, "491701234567"] {
            assert!(!said.contains(secret), "{what} holds {secret}: {said}");
        }
        assert!(said.contains("••••••"), "{what}: {said}");
    };
    run_jobs(&k).await;
    let request = seen.lock().unwrap()[0].clone();
    assert!(request.contains(token) && request.contains(code) && request.contains(number), "what went out carried them: {request}");
    let r = row(&k, &admin, s[0].id).await;
    assert_eq!(r["state"], "queued", "a temporary refusal is tried again: {r}");
    let temporary = job_error().await;
    assert!(temporary.starts_with("the provider answered 503"), "{temporary}");
    clear(&temporary, "the job's error after a temporary refusal");
    run_jobs(&k).await;
    let r = row(&k, &admin, s[0].id).await;
    assert_eq!(r["state"], "failed", "{r}");
    clear(r["error"].as_str().unwrap_or_default(), "the send's error after a final refusal");
}

/// A mailer that refuses as a mail server does, quoting the address.
struct Refusing;

#[async_trait::async_trait]
impl ferrum_kernel::Mailer for Refusing {
    async fn send(&self, to: &str, _subject: &str, _html: &str) -> ferrum_kernel::Result<()> {
        Err(ferrum_kernel::Error::Unavailable(format!("mail delivery failed: permanent error (550): 5.1.1 <{to}>: Recipient address rejected")))
    }
}

/// A mail server's refusal quotes the recipient; the outbox's error (shown to administrators, and the server's log)
/// keeps none of it.
async fn mail_refusal() {
    let (k, admin) = boot(Arc::new(Refusing)).await;
    let note = k.insert(&admin, "Note", obj(json!({"title": "Ann", "email": "Ann.Lee@Example.com"}))).await.unwrap();
    let m = Message {
        template: "notes.welcome".into(),
        entity: Some("Note".into()),
        record: Some(note["id"].as_str().unwrap().parse().unwrap()),
        to: vec![Recipient { email: Some("Ann.Lee@Example.com".into()), ..Default::default() }],
        channels: vec!["email".into()],
        ..Default::default()
    };
    assert_eq!(send(&k, &admin, m).await[0].state, "queued");
    drain(&k).await;
    let mut tx = k.db.system_tx().await.unwrap();
    let said: Option<String> = sqlx::query_scalar("select last_error from outbox where topic = 'ferrum.mail'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let said = said.unwrap_or_default();
    assert!(said.contains("Recipient address rejected"), "the refusal is kept: {said}");
    assert!(!said.to_lowercase().contains("ann.lee@example.com"), "the address is not: {said}");
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    // Opened as a server opens it (erasing someone who opted out needs it), with the database's password.
    let opener = Opener { database_url: url, file: PathBuf::from("/nonexistent/root.key"), transit: None, command: None, kdf: Kdf { m_kib: 64, t: 1, p: 1 } };
    vault::open(&db, opener, false).await.expect("the vault opens");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/note.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let notes = ferrum_meta::catalog::load_package_files(&files).expect("package");

    // A package's message must carry its name, and have a body.
    let mut wrong = files.clone();
    wrong.insert("package.yaml".into(), PACKAGE.replace("key: notes.welcome", "key: other.welcome"));
    let err = Catalog::from_packages(vec![core.clone(), ferrum_meta::catalog::load_package_files(&wrong).unwrap()]).unwrap_err().to_string();
    assert!(err.contains("must be unique and start with 'notes.'"), "{err}");

    let settings = KernelSettings { webhooks_allow_private: true, public_url: Some("https://ferrum.test".into()), ..Default::default() };
    let k =
        Kernel::boot(db, Catalog::from_packages(vec![core, notes]).unwrap(), HookRegistry::default(), settings, Some(Arc::new(Captured))).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "msg".into(),
            name: "msg".into(),
            admin_email: "admin@msg.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["notes".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("msg", "admin@msg.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let note = k.insert(&admin, "Note", obj(json!({"title": "Ann", "email": "ann@example.com", "phone": "0151 2345678", "pin": "4321"}))).await.unwrap();
    let note_id: uuid::Uuid = note["id"].as_str().unwrap().parse().unwrap();

    // ── templates: the package's defaults, an edited copy, what is refused, reset ──
    let list = k.message_templates(&admin).await.unwrap();
    // The package's three, beside the platform's own wording for outside people (core.*, BD-22, BD-23).
    let own = |t: &&Value| t["source"] == "package" && !t["key"].as_str().unwrap_or_default().starts_with("core.");
    assert_eq!(list.iter().filter(own).count(), 3);
    let read = k.message_template(&admin, "notes.welcome").await.unwrap();
    let mut def: MessageDef = serde_json::from_value(read["template"].clone()).unwrap();
    def.languages.get_mut("en").unwrap().email.as_mut().unwrap().subject = "Hi {{ doc.title }}".into();
    k.save_message_template(&admin, &def).await.unwrap();
    let read = k.message_template(&admin, "notes.welcome").await.unwrap();
    assert_eq!(read["edited"], json!(true));
    assert_eq!(read["default"]["languages"]["en"]["email"]["subject"], "Welcome {{ doc.title }}", "the default is shown beside the edit");
    assert_eq!(read["default_changed"], json!(false));
    // A release that changes the default does not touch the edit; the workspace is told.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update message_template set base = 'an older default' where tenant_id = $1").bind(tenant).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let listed = k.message_templates(&admin).await.unwrap();
    let welcome = listed.iter().find(|t| t["key"] == "notes.welcome").unwrap();
    assert_eq!((welcome["source"].clone(), welcome["default_changed"].clone()), (json!("edited"), json!(true)));
    for (bad, why, says) in [
        ("{{ doc.titel }}", "unknown field", "en, text: doc.titel is not a field of Note (line 2)"),
        ("{{ doc.pin }}", "a secret field is never rendered", "en, text: doc.pin is a secret field of Note: no template prints one"),
        (
            "{{ values.nope }}",
            "a value the sender does not pass",
            "en, text: the body reads values.nope, which is not among the values this template says the sender passes",
        ),
        ("{% if %}", "does not compile", "en, text: "),
    ] {
        let mut d = def.clone();
        d.languages.get_mut("en").unwrap().text.as_mut().unwrap().text = format!("Hi\n{bad}");
        let e = k.save_message_template(&admin, &d).await.unwrap_err();
        let ferrum_kernel::Error::Validation(f) = &e else { panic!("{why}: {e}") };
        assert!(f[0].field == "template" && f[0].message.starts_with(says), "{why}: {}", f[0].message);
    }
    let mut stray = def.clone();
    stray.key = "notes.nothing".into();
    assert!(k.save_message_template(&admin, &stray).await.unwrap_err().to_string().contains("sends no message notes.nothing"));
    let mut own = def.clone();
    own.key = "shop.hello".into();
    k.save_message_template(&admin, &own).await.unwrap();
    assert!(k.message_templates(&admin).await.unwrap().iter().any(|t| t["key"] == "shop.hello" && t["source"] == "workspace"));
    // Preview: a key or an unsaved template, against the latest record.
    let p = k.preview_message(&admin, &json!("notes.welcome"), "email", None, None).await.unwrap();
    assert_eq!(p["message"]["subject"], "Hi Ann");
    assert_eq!(p["record_id"], json!(note_id));
    let p = k.preview_message(&admin, &json!("notes.welcome"), "text", Some("de-AT"), Some(note_id)).await.unwrap();
    assert_eq!((p["language"].clone(), p["message"]["text"].clone()), (json!("de"), json!("Hallo Ann")));
    k.reset_message_template(&admin, "notes.welcome").await.unwrap();
    assert_eq!(k.message_template(&admin, "notes.welcome").await.unwrap()["edited"], json!(false));

    // ── the effect: e-mail first in the template's order, recorded, sent ──
    let auto = obj(json!({"name": "welcome", "entity": "Note", "event": "created",
        "actions": [{"type": "message", "template": "notes.welcome", "to": "field:email", "when": "doc.email != ''"}]}));
    ferrum_kernel::automation::save(&k, &admin, &auto).await.unwrap();
    let bob = k.insert(&admin, "Note", obj(json!({"title": "Bob", "email": "bob@example.com"}))).await.unwrap();
    let bob_id: uuid::Uuid = bob["id"].as_str().unwrap().parse().unwrap();
    drain(&k).await;
    drain(&k).await;
    let log = k.message_log(&admin, Some(("Note", bob_id)), None, 10).await.unwrap();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!((log[0]["channel"].clone(), log[0]["state"].clone(), log[0]["recipient"].clone()), (json!("email"), json!("sent"), json!("bob@example.com")));
    assert!(SENT.lock().unwrap().contains(&("bob@example.com".into(), "Welcome Bob".into())));

    // ── text messages: no channel, no calling code, then delivered to the test channel ──
    let text = |ch: &str| Message {
        template: "notes.welcome".into(),
        entity: Some("Note".into()),
        record: Some(note_id),
        to: to("0151 2345678"),
        channels: vec![ch.into()],
        ..Default::default()
    };
    let s = send(&k, &admin, text("text")).await;
    assert_eq!((s[0].state.as_str(), s[0].reason.as_deref()), ("skipped", Some("0151 2345678 has no country calling code, and the workspace sets none")));
    k.update_tenant_settings(&admin, json!({"messages": {"calling_code": "+49", "trunk_prefix": "0"}}).as_object().unwrap()).await.unwrap();
    let s = send(&k, &admin, text("text")).await;
    assert_eq!((s[0].state.as_str(), s[0].reason.as_deref()), ("skipped", Some("no channel configured")));
    let test_channel = ChannelInput {
        kind: "text".into(),
        name: "Test".into(),
        description: serde_json::from_value(json!({"test": true})).unwrap(),
        secrets: None,
        enabled: true,
    };
    k.save_message_channel(&admin, None, &test_channel).await.unwrap();
    let s = send(&k, &admin, text("text")).await;
    assert_eq!(s[0].state, "queued");
    run_jobs(&k).await;
    let r = row(&k, &admin, s[0].id).await;
    assert_eq!(
        (r["state"].clone(), r["recipient"].clone(), r["content"]["message"]["text"].clone()),
        (json!("delivered"), json!("+491512345678"), json!("Hello Ann"))
    );
    assert_eq!(r["pending"], Value::Null, "what went out is not kept beyond the test channel's log");
    // The recipient's language, else the workspace's.
    let mut de = text("text");
    de.to[0].language = Some("de".into());
    let s = send(&k, &admin, de).await;
    run_jobs(&k).await;
    assert_eq!(row(&k, &admin, s[0].id).await["content"]["message"]["text"], "Hallo Ann");
    // Channels in order: a user with no number gets the e-mail; with `all`, the app too.
    let mut both = text("text");
    both.to = vec![Recipient::user(admin.user)];
    both.channels = vec!["text".into(), "email".into(), "inapp".into()];
    let s = send(&k, &admin, both.clone()).await;
    assert_eq!(s.iter().map(|x| (x.channel.as_str(), x.state.as_str())).collect::<Vec<_>>(), vec![("email", "queued")]);
    both.all = true;
    let s = send(&k, &admin, both).await;
    let got: Vec<(&str, &str)> = s.iter().map(|x| (x.channel.as_str(), x.state.as_str())).collect();
    assert_eq!(got, vec![("email", "queued"), ("inapp", "delivered"), ("text", "skipped")]);

    // ── promotional: opt-out per number and channel, quiet hours, limits; operational always goes ──
    let promo = |ch: &str| Message {
        template: "notes.promo".into(),
        to: to("+4915199999"),
        channels: vec![ch.into()],
        values: serde_json::Map::from_iter([("percent".to_string(), json!(20))]),
        ..Default::default()
    };
    let s = send(&k, &admin, promo("text")).await;
    run_jobs(&k).await;
    let r = row(&k, &admin, s[0].id).await;
    assert!(r["content"]["message"]["text"].as_str().unwrap().starts_with("Sale! https://"), "a promotional message carries its opt-out link: {r}");
    k.set_message_opt_out(&admin, "015199999", "text", true).await.unwrap();
    assert_eq!(send(&k, &admin, promo("text")).await[0].reason.as_deref(), Some("opted out"));
    let mut op = text("text");
    op.to = to("+4915199999");
    assert_eq!(send(&k, &admin, op).await[0].state, "queued", "an operational message is not stopped by an opt-out");
    k.set_message_opt_out(&admin, "+4915199999", "text", false).await.unwrap();
    let now = chrono::Utc::now();
    let quiet =
        json!({"from": (now - chrono::Duration::hours(1)).format("%H:%M").to_string(), "to": (now + chrono::Duration::hours(1)).format("%H:%M").to_string()});
    k.update_tenant_settings(&admin, json!({"messages": {"calling_code": "+49", "quiet_hours": quiet}}).as_object().unwrap()).await.unwrap();
    let s = send(&k, &admin, promo("text")).await;
    assert_eq!(s[0].state, "held");
    run_jobs(&k).await;
    assert_eq!(row(&k, &admin, s[0].id).await["state"], "held", "held, not dropped, while quiet hours last");
    k.update_tenant_settings(&admin, json!({"messages": {"calling_code": "+49", "per_recipient_hour": 1}}).as_object().unwrap()).await.unwrap();
    run_jobs(&k).await;
    assert_eq!(row(&k, &admin, s[0].id).await["state"], "delivered", "sent once quiet hours end");
    let mut fresh = text("text");
    fresh.to = to("+4917000000");
    assert_eq!(send(&k, &admin, fresh.clone()).await[0].state, "queued");
    assert_eq!(send(&k, &admin, fresh).await[0].reason.as_deref(), Some("this recipient's hourly limit"));
    k.update_tenant_settings(&admin, json!({"messages": {"calling_code": "+49", "trunk_prefix": "0"}}).as_object().unwrap()).await.unwrap();

    // ── one-time codes ──
    let guest = Recipient { phone: Some("0170 1234567".into()), ..Default::default() };
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let sent = k.send_code(&mut tx, &admin, "notes.guest", "notes.code", &guest, "text").await.unwrap();
    tx.commit().await.unwrap();
    run_jobs(&k).await;
    let delivered = messages::test_deliveries(tenant).into_iter().rev().find(|d| d.0 == "+491701234567");
    let delivered = delivered.unwrap();
    let code = delivered.1["text"].as_str().unwrap().rsplit(' ').next().unwrap().to_string();
    assert_eq!(code.len(), 6);
    let logged = row(&k, &admin, sent.id).await;
    assert_eq!(logged["content"]["message"]["text"], "Your code is ••••••", "the log never holds the code");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let stored: Vec<String> = sqlx::query_scalar("select digest from message_code where tenant_id = $1").bind(tenant).fetch_all(&mut *tx).await.unwrap();
    assert!(stored.iter().all(|d| !d.contains(&code)));
    assert!(!k.verify_code(&mut tx, &admin, "notes.guest", &guest, "000000x").await.unwrap());
    assert!(!k.verify_code(&mut tx, &admin, "notes.other", &guest, &code).await.unwrap(), "a code is for its purpose");
    assert!(k.verify_code(&mut tx, &admin, "notes.guest", &guest, &code).await.unwrap());
    assert!(!k.verify_code(&mut tx, &admin, "notes.guest", &guest, &code).await.unwrap(), "once");
    tx.commit().await.unwrap();
    // Tries are limited.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    k.send_code(&mut tx, &admin, "notes.tries", "notes.code", &guest, "text").await.unwrap();
    tx.commit().await.unwrap();
    run_jobs(&k).await;
    let code = messages::test_deliveries(tenant).last().unwrap().1["text"].as_str().unwrap().rsplit(' ').next().unwrap().to_string();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    for _ in 0..5 {
        assert!(!k.verify_code(&mut tx, &admin, "notes.tries", &guest, "999999x").await.unwrap());
    }
    assert!(!k.verify_code(&mut tx, &admin, "notes.tries", &guest, &code).await.unwrap(), "five wrong tries spend it");
    tx.commit().await.unwrap();
    // Sends a recipient an hour are limited (two sent so far; three more pass, the sixth is refused).
    for i in 0..4 {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        let r = k.send_code(&mut tx, &admin, "notes.many", "notes.code", &guest, "text").await;
        tx.commit().await.unwrap();
        assert_eq!(r.is_ok(), i < 3, "{i}: {r:?}");
    }

    // ── a provider as configuration: a chat app over HTTP, its reports and replies ──
    let answers = Arc::new(Mutex::new(vec![
        (200, r#"{"messages": [{"id": "wamid.1"}]}"#.to_string()),
        (503, r#"{"error": {"message": "busy"}}"#.to_string()),
        (200, r#"{"messages": [{"id": "wamid.2"}]}"#.to_string()),
        (400, r#"{"error": {"message": "template not approved"}}"#.to_string()),
    ]));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let port = provider(answers.clone(), seen.clone()).await;
    let chat = ChannelInput {
        kind: "chat".into(),
        name: "Chat gateway".into(),
        description: serde_json::from_value(json!({
            "url": format!("http://127.0.0.1:{port}/v1/messages"),
            "headers": {"Authorization": "Bearer {{ secret.token }}"},
            "body": r#"{"to": {{ to|json }}, "template": {"name": {{ template_name|json }}, "language": {{ language|json }}, "values": {{ values|json }}}}"#,
            "id_at": "/messages/0/id",
            "error_at": "/error/message",
            "inbound": {"auth": "hmac", "header": "X-Signature", "secret": "hook", "id_at": "/id", "status_at": "/status",
                        "statuses": {"read": "delivered", "failed": "failed"}, "from_at": "/from", "text_at": "/text", "stop": ["STOP"], "start": ["START"]}
        }))
        .unwrap(),
        secrets: Some(BTreeMap::from([("token".to_string(), "t0ken".to_string()), ("hook".to_string(), "h00k".to_string())])),
        enabled: true,
    };
    let chat_id = k.save_message_channel(&admin, None, &chat).await.unwrap();
    let listed = k.message_channels(&admin).await.unwrap();
    assert!(!serde_json::to_string(&listed).unwrap().contains("t0ken"), "a secret is never read back");
    let a = send(&k, &admin, promo("chat")).await;
    run_jobs(&k).await;
    let r = row(&k, &admin, a[0].id).await;
    assert_eq!((r["state"].clone(), r["provider_id"].clone()), (json!("sent"), json!("wamid.1")));
    let request = seen.lock().unwrap()[0].clone();
    assert!(request.contains("Bearer t0ken") && request.contains(r#""name":"sale_2026""#) && request.contains(r#""values":["20"]"#), "{request}");
    // A busy provider is retried; one that refuses is not.
    let b = send(&k, &admin, promo("chat")).await;
    run_jobs(&k).await;
    assert_eq!(row(&k, &admin, b[0].id).await["state"], "queued", "the provider was busy: tried again later");
    run_jobs(&k).await;
    assert_eq!((row(&k, &admin, b[0].id).await["state"].clone(), row(&k, &admin, b[0].id).await["provider_id"].clone()), (json!("sent"), json!("wamid.2")));
    let c = send(&k, &admin, promo("chat")).await;
    run_jobs(&k).await;
    let r = row(&k, &admin, c[0].id).await;
    assert_eq!((r["state"].clone(), r["error"].clone()), (json!("failed"), json!("400: template not approved")));
    assert_eq!(seen.lock().unwrap().len(), 4, "a refusal is not tried again");
    // A delivery report, signed; one unsigned or wrongly signed is refused and counted.
    let report = br#"{"id": "wamid.1", "status": "read"}"#;
    let signed = BTreeMap::from([("x-signature".to_string(), format!("sha256={}", hmac_hex("h00k", report)))]);
    let none = BTreeMap::new();
    assert!(k.message_inbound(chat_id, &none, &none, report).await.is_err());
    assert!(k.message_inbound(chat_id, &BTreeMap::from([("x-signature".to_string(), hmac_hex("wrong", report))]), &none, report).await.is_err());
    assert!(k.message_inbound(uuid::Uuid::now_v7(), &signed, &none, report).await.is_err());
    assert!(messages::refused_unknown() >= 1);
    assert_eq!(k.message_inbound(chat_id, &signed, &none, report).await.unwrap(), json!({"updated": 1}));
    assert_eq!(row(&k, &admin, a[0].id).await["state"], "delivered");
    assert_eq!(k.message_channels(&admin).await.unwrap().iter().find(|c| c["id"] == json!(chat_id)).unwrap()["refused"], json!(2));
    // A reply with a start word opts the number in; one with a stop word opts it out of promotional messages on that
    // channel only.
    let start = br#"{"from": "+4915199999", "text": "START"}"#;
    let start_signed = BTreeMap::from([("X-Signature".to_string(), hmac_hex("h00k", start))]);
    assert_eq!(k.message_inbound(chat_id, &start_signed, &none, start).await.unwrap(), json!({"reply": true, "opted_in": true}));
    let reply = br#"{"from": "+4915199999", "text": " stop "}"#;
    let signed = BTreeMap::from([("X-Signature".to_string(), hmac_hex("h00k", reply))]);
    k.message_inbound(chat_id, &signed, &none, reply).await.unwrap();
    assert_eq!(send(&k, &admin, promo("chat")).await[0].reason.as_deref(), Some("opted out"));
    assert_eq!(send(&k, &admin, promo("text")).await[0].state, "queued");
    // The start reply, captured and sent again, is refused and counted as an unsigned call is: the stop stands.
    let again = k.message_inbound(chat_id, &start_signed, &none, start).await;
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert!(k.opted_out(&mut tx, tenant, "chat", "+4915199999").await.unwrap(), "the replayed start opted the number back in: {again:?}");
    tx.commit().await.unwrap();
    assert!(matches!(again, Err(ferrum_kernel::Error::Unauthorized(_))), "{again:?}");
    assert_eq!(k.message_channels(&admin).await.unwrap().iter().find(|c| c["id"] == json!(chat_id)).unwrap()["refused"], json!(3));
    // The signed link opts out as well.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let link: String = sqlx::query_scalar(
        "select content->'message'->>'text' from message_send where tenant_id = $1 and template = 'notes.promo' and content is not null limit 1",
    )
    .bind(tenant)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let token = link.split("token=").nth(1).unwrap();
    assert!(k.opt_out_by_link(&format!("{token}x")).await.is_err(), "a changed link does nothing");
    assert_eq!(k.opt_out_by_link(token).await.unwrap(), "text");
    assert_eq!(send(&k, &admin, promo("text")).await[0].reason.as_deref(), Some("opted out"));

    // ── the log per recipient, and erasure ──
    assert!(k.message_log(&admin, None, Some("0151 2345678"), 50).await.unwrap().len() >= 3);
    let gone =
        k.erase_personal_data_of(&admin, &ferrum_kernel::privacy::Person { email: "bob@example.com".into(), ..Default::default() }, "request 7").await.unwrap();
    assert!(gone["apps"].as_array().unwrap().iter().any(|a| a["app"] == "messages"), "{gone}");
    assert!(k.message_log(&admin, None, Some("bob@example.com"), 50).await.unwrap().is_empty());

    // ── an erased person's opt-out still stops promotional messages, and nothing shows their address (BD-20) ──
    k.set_message_opt_out(&admin, "zoe@example.com", "email", true).await.unwrap();
    k.set_message_opt_out(&admin, "+491515555555", "text", true).await.unwrap();
    let zoe = |m: Message, ch: &str| Message {
        to: vec![Recipient { email: Some("Zoe@Example.com".into()), phone: Some("0151 5555555".into()), ..Default::default() }],
        channels: vec![ch.into()],
        ..m
    };
    assert_eq!(send(&k, &admin, zoe(promo("text"), "text")).await[0].reason.as_deref(), Some("opted out"));
    k.erase_personal_data_of(
        &admin,
        &ferrum_kernel::privacy::Person { email: "zoe@example.com".into(), phone: Some("0151 5555555".into()), ..Default::default() },
        "request 8",
    )
    .await
    .unwrap();
    // Nowhere: the opt-outs, the delivery log, the audit trail, the erasure's journal, the listing.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let found: i64 = sqlx::query_scalar(
        "select (select count(*) from contact_consent c where tenant_id = $1 and to_jsonb(c)::text ilike any($2)) \
               + (select count(*) from message_send s where tenant_id = $1 and to_jsonb(s)::text ilike any($2)) \
               + (select count(*) from audit_event a where tenant_id = $1 and to_jsonb(a)::text ilike any($2)) \
               + (select count(*) from privacy_erasure e where tenant_id = $1 and to_jsonb(e)::text ilike any($2))",
    )
    .bind(tenant)
    .bind(vec!["%zoe@example%", "%1515555555%"])
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(found, 0, "the address is gone");
    let listed = k.message_opt_outs(&admin, None, None, 200, 0).await.unwrap();
    let shown = listed.to_string();
    assert!(!shown.contains("zoe@example") && !shown.contains("1515555555"), "{listed}");
    let erased: Vec<&Value> = listed["opt_outs"].as_array().unwrap().iter().filter(|r| r["address"].is_null()).collect();
    assert_eq!(erased.iter().map(|r| r["channel"].as_str().unwrap()).collect::<Vec<_>>().len(), 2, "{listed}");
    assert!(erased.iter().all(|r| r["erased"].as_str().unwrap().starts_with("erased:") && r["source"] == "recorded by staff" && r["recorded_at"].is_string()));
    assert_eq!(k.message_opt_outs(&admin, Some("zoe"), None, 200, 0).await.unwrap()["total"], json!(0), "not found by its address");
    assert_eq!(k.message_opt_outs(&admin, Some("1515555555"), None, 200, 0).await.unwrap()["total"], json!(0));
    // Imported again, the same address and the same number in another form: promotional skipped, operational goes.
    assert_eq!(send(&k, &admin, zoe(promo("text"), "text")).await[0].reason.as_deref(), Some("opted out"));
    assert_eq!(send(&k, &admin, zoe(promo("email"), "email")).await[0].reason.as_deref(), Some("opted out"));
    assert_eq!(send(&k, &admin, zoe(text("text"), "text")).await[0].state, "queued", "operational goes");
    assert_eq!(send(&k, &admin, zoe(text("text"), "email")).await[0].state, "queued", "operational goes");
    // An app sending through another transport asks the same (BD-45): an erased opt-out kept as a digest refuses it.
    let render = |m: Message, ch: &'static str| {
        let (k, admin) = (&k, admin.clone());
        async move {
            let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
            let r = messages::render_for(k, &mut tx, &admin, &m, ch).await;
            tx.commit().await.unwrap();
            r
        }
    };
    for ch in ["email", "text"] {
        let e = render(zoe(promo(ch), ch), ch).await.expect_err("opted out");
        assert!(e.to_string().contains("opted out"), "{ch}: {e}");
    }
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert!(k.opted_out(&mut tx, tenant, "email", "zoe@example.com").await.unwrap());
    assert!(k.opted_out(&mut tx, tenant, "text", "+491515555555").await.unwrap());
    assert!(!k.opted_out(&mut tx, tenant, "email", "amy@example.com").await.unwrap());
    tx.commit().await.unwrap();
    // Rendered as a send would render it, with the opt-out link and the headers to send it with; nothing sent.
    let amy = |m: Message| Message { to: vec![Recipient { email: Some("amy@example.com".into()), ..Default::default() }], channels: vec!["email".into()], ..m };
    let r = render(amy(promo("email")), "email").await.unwrap();
    let link = r.opt_out.clone().expect("a promotional message's opt-out link");
    assert!(link.starts_with("https://ferrum.test/api/messages/opt-out?token="), "{link}");
    assert_eq!(r.subject.as_deref(), Some("Sale"));
    let token = link.rsplit("token=").next().unwrap();
    assert!(r.html.as_deref().unwrap().contains(&format!("opt-out?token={token}")) && r.text.as_deref().unwrap().contains(&link), "{r:?}");
    assert_eq!(r.list_unsubscribe, Some((format!("<{link}>"), "List-Unsubscribe=One-Click".into())));
    let r = render(amy(text("email")), "email").await.unwrap();
    assert!(r.opt_out.is_none() && r.list_unsubscribe.is_none(), "an operational message carries none");
    // Staff take an erased one away by its row, not by the address.
    let row_of = erased.iter().find(|r| r["channel"] == "text").unwrap()["erased"].as_str().unwrap().to_string();
    k.set_message_opt_out(&admin, "0151 5555555", "text", false).await.unwrap();
    assert_eq!(send(&k, &admin, zoe(promo("text"), "text")).await[0].reason.as_deref(), Some("opted out"), "not by its address");
    assert!(k.set_message_opt_out(&admin, &row_of, "text", true).await.is_err());
    k.set_message_opt_out(&admin, &row_of, "text", false).await.unwrap();
    assert_eq!(send(&k, &admin, zoe(promo("text"), "text")).await[0].state, "queued", "removed by its row");
}
