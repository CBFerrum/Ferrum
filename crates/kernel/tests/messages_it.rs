//! Message templates and channels (crates/kernel/src/messages.rs, BD-20), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first): a package's defaults and a workspace's edits, the
//! `message` effect, channels tried in order and recorded send by send, languages, opt-outs, quiet hours, limits,
//! one-time codes, a provider described as configuration and its delivery reports and replies, and erasure.

use ferrum_kernel::messages::{self, ChannelInput, Message, Recipient};
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, MessageDef};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
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

#[test]
fn messages_from_templates_on_every_channel() {
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

    // A package's message must carry its name, and have a body.
    let mut wrong = files.clone();
    wrong.insert("package.yaml".into(), PACKAGE.replace("key: notes.welcome", "key: other.welcome"));
    let err = Catalog::from_packages(vec![core.clone(), ferrum_meta::catalog::load_package_files(&wrong).unwrap()]).unwrap_err().to_string();
    assert!(err.contains("must be unique and start with 'notes.'"), "{err}");

    let settings = KernelSettings {
        secret_key: Some(SecretKey::new([7u8; 32])),
        webhooks_allow_private: true,
        public_url: Some("https://ferrum.test".into()),
        ..Default::default()
    };
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
    assert_eq!(list.iter().filter(|t| t["source"] == "package").count(), 3);
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
    // A reply with a stop word opts the number out of promotional messages on that channel only.
    let reply = br#"{"from": "+4915199999", "text": " stop "}"#;
    let signed = BTreeMap::from([("X-Signature".to_string(), hmac_hex("h00k", reply))]);
    k.message_inbound(chat_id, &signed, &none, reply).await.unwrap();
    assert_eq!(send(&k, &admin, promo("chat")).await[0].reason.as_deref(), Some("opted out"));
    assert_eq!(send(&k, &admin, promo("text")).await[0].state, "queued");
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
}
