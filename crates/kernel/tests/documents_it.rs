//! Document templates (crates/kernel/src/documents.rs, BD-21), against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`;
//! the database is reset first): a package's defaults and a workspace's edits, what a person printing reads and what a
//! document generated for outside reads, amounts and words in the document's language and currency, text, CSV and XML
//! files that hostile values cannot break, labels for many records, and the `generate` effect handing its file to a
//! `message` that mails it.

use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::documents::Generate;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, DocumentDef};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const PACKAGE: &str = r#"package: letters
version: 0.1.0
depends: [core]
roles:
  - name: Clerk
    permissions:
      - {entity: Offer, read: true, write: true, create: true}
      - {entity: Currency, read: true}
  - name: Payroll
    permissions:
      - {entity: Offer, read: true, export: true, read_groups: [pay]}
      - {entity: Currency, read: true}
messages:
  - key: letters.sent
    entity: Offer
    languages:
      en:
        email: {subject: "Your offer", body: "<p>Dear {{ doc.candidate }}</p>"}
documents:
  - key: letters.offer
    label: Offer letter
    entity: Offer
    default: true
    languages:
      en:
        body: "<h1>Offer for {{ doc.candidate }}</h1><p>{{ doc.salary|amount }} ({{ doc.salary|words }})</p><p>Bonus {{ doc.bonus }}</p><p>Pin {{ doc.pin }}</p><p>{{ doc.start|date('long') }}</p>{{ barcode('OFR-1') }}"
        footer: "Page {{ page_number() }} of {{ page_count() }}"
      hi:
        body: "<p>{{ doc.candidate }}: {{ doc.salary|amount }} ({{ doc.salary|words }})</p>"
  - key: letters.bank
    entity: Offer
    output: text
    encoding: iso-8859-1
    crlf: true
    roles: [Payroll]
    languages:
      en:
        body: "{{ doc.candidate|pad(12) }}{{ doc.salary|fixed(2)|pad(12, 'right', '0') }}\n"
  - key: letters.list
    entity: Offer
    output: csv
    languages:
      en: {body: "{{ doc.candidate }},{{ doc.salary }}\n"}
  - key: letters.xml
    entity: Offer
    output: xml
    languages:
      en: {body: "<offer name=\"{{ doc.candidate }}\">{{ doc.candidate }}</offer>"}
"#;
const ENTITIES: &str = "name: Offer\nlabel: Offer\nkind: master\ntitle_field: candidate\nfields:\n  - {name: candidate, type: data, required: true}\n  - {name: email, type: email}\n  - {name: currency, type: link, to: Currency}\n  - {name: salary, type: currency, currency: currency}\n  - {name: start, type: date}\n  - {name: bonus, type: int, perm_group: pay}\n  - {name: pin, type: secret}\n";

/// Request bodies the stand-in PDF renderer received.
static RENDERED: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Mail sent, and the files attached to it.
static ATTACHED: Mutex<Vec<(String, String, Vec<u8>)>> = Mutex::new(Vec::new());

struct Captured;

#[async_trait::async_trait]
impl ferrum_kernel::Mailer for Captured {
    async fn send(&self, _to: &str, _subject: &str, _html: &str) -> ferrum_kernel::Result<()> {
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

#[derive(Default)]
struct MemFiles(Mutex<std::collections::HashMap<String, Vec<u8>>>);

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

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_string()
}

async fn drain(k: &Kernel) {
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
}

async fn files_of(k: &Kernel, ctx: &Ctx, id: Uuid) -> Vec<(String, Option<String>)> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await.unwrap();
    let rows = sqlx::query_as("select file_name, document from file_object where tenant_id = $1 and record_id = $2 order by created_at")
        .bind(ctx.tenant)
        .bind(id)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    rows
}

#[test]
fn documents_from_data() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/offer.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let letters = ferrum_meta::catalog::load_package_files(&files).expect("package");

    // A package's document carries its name and is for an entity it may use; a header is a page's only.
    for (from, to, says) in [
        ("key: letters.offer", "key: other.offer", "must be unique and start with 'letters.'"),
        ("entity: Offer\n    output: csv", "entity: Nope\n    output: csv", "is for Nope"),
        ("en: {body: \"{{ doc.candidate }},", "en: {footer: x, body: \"{{ doc.candidate }},", "only a page has a header and a footer"),
    ] {
        let mut wrong = files.clone();
        wrong.insert("package.yaml".into(), PACKAGE.replace(from, to));
        let err = Catalog::from_packages(vec![core.clone(), ferrum_meta::catalog::load_package_files(&wrong).unwrap()]).unwrap_err().to_string();
        assert!(err.contains(says), "{says}: {err}");
    }

    let settings =
        KernelSettings { pdf_url: Some(fake_pdf_renderer().await), secret_key: Some(ferrum_kernel::secret::SecretKey::new([7u8; 32])), ..Default::default() };
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, letters]).unwrap(), HookRegistry::default(), settings, Some(Arc::new(Captured)))
        .await
        .expect("boot");
    k.set_file_store(Arc::new(MemFiles::default()));
    let tenant = k
        .create_tenant(NewTenant {
            slug: "docs".into(),
            name: "docs".into(),
            admin_email: "admin@docs.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["letters".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("docs", "admin@docs.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let clerk = k.create_user(&admin, "clerk@docs.test", "Cleo Clerk", Some("Correct-Horse-9"), &["Clerk".into()], false).await.unwrap();
    let clerk = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    let payroll = k.create_user(&admin, "pay@docs.test", "Pat Payroll", Some("Correct-Horse-9"), &["Payroll".into()], false).await.unwrap();
    let payroll = k.load_ctx(tenant, payroll, None, "it".into()).await.unwrap();

    // The workspace's own currency record: its symbol and decimals are what an amount prints with.
    let q = ferrum_kernel::ListQuery { limit: Some(500), ..Default::default() };
    let inr = k.list(&admin, "Currency", &q).await.unwrap().rows.into_iter().find(|c| c["code"] == "INR").expect("INR is there");
    let inr = k.update(&admin, "Currency", id_of(&inr), obj(json!({"symbol": "₹", "precision": 2})), None).await.unwrap();
    let offer = k
        .insert(
            &admin,
            "Offer",
            obj(json!({"candidate": "Ann \"A\", <b>&</b>", "email": "ann@example.com", "currency": id_of(&inr), "salary": "1234567.50",
                       "start": "2026-12-01", "bonus": 777, "pin": "4321"})),
        )
        .await
        .unwrap();
    let oid = id_of(&offer);

    // ── what a person printing reads: their own view; amounts and words in the language asked for ──
    let r = k.render_document(&clerk, "letters.offer", oid, Some("en-IN"), false).await.unwrap();
    let page = text(&r.bytes);
    assert_eq!((r.content_type.as_str(), r.language.as_str()), ("text/html; charset=utf-8", "en-IN"));
    assert!(page.contains("Ann &quot;A&quot;, &lt;b&gt;&amp;&lt;&#x2f;b&gt;"), "a value is escaped on a page: {page}");
    assert!(page.contains("₹12,34,567.50 (twelve lakh thirty-four thousand five hundred sixty-seven rupees and fifty paise)"), "{page}");
    assert!(page.contains("1 December 2026") && page.contains("<svg"), "{page}");
    assert!(!page.contains("777"), "a field the person may not read is not on their print");
    assert!(!page.contains("4321"), "a secret is never printed");
    let en = text(&k.render_document(&clerk, "letters.offer", oid, Some("en"), false).await.unwrap().bytes);
    assert!(en.contains("₹1,234,567.50 (one million two hundred thirty-four thousand"), "{en}");
    let hi = text(&k.render_document(&clerk, "letters.offer", oid, Some("hi"), false).await.unwrap().bytes);
    assert!(hi.contains("₹12,34,567.50 (बारह लाख चौंतीस हज़ार पाँच सौ सड़सठ रुपये और पचास पैसे)"), "{hi}");
    let pay_page = text(&k.render_document(&payroll, "letters.offer", oid, None, false).await.unwrap().bytes);
    assert!(pay_page.contains("Bonus 777"), "who may read the field reads it: {pay_page}");
    // A PDF has the footer the renderer repeats with page numbers.
    let pdf = k.render_document(&clerk, "letters.offer", oid, None, true).await.unwrap();
    assert!(pdf.bytes.starts_with(b"%PDF") && pdf.file_name == "Offer letter Ann _A__ _b____b_.pdf", "{}", pdf.file_name);
    let form = RENDERED.lock().unwrap().last().cloned().unwrap();
    assert!(form.contains("filename=\"footer.html\"") && form.contains("<span class=\"pageNumber\"></span> of <span class=\"totalPages\"></span>"), "{form}");

    // ── choices: what each person may pick ──
    let c = k.document_choices(&clerk, "Offer").await.unwrap();
    let keys: Vec<&str> = c["templates"].as_array().unwrap().iter().map(|t| t["key"].as_str().unwrap()).collect();
    assert_eq!((keys, c["default"].clone()), (vec!["letters.list", "letters.offer", "letters.xml"], json!("letters.offer")));
    assert_eq!(k.document_choices(&payroll, "Offer").await.unwrap()["templates"].as_array().unwrap().len(), 4);

    // ── files: the template's roles, export permission, encoding, line ends, escaping, on the audit trail ──
    assert!(matches!(k.render_document(&clerk, "letters.bank", oid, None, false).await, Err(Error::Forbidden(_))), "not the clerk's template");
    assert!(matches!(k.render_document(&clerk, "letters.list", oid, None, false).await, Err(Error::Forbidden(_))), "a file of data needs export");
    k.update(&admin, "Offer", oid, obj(json!({"candidate": "Müller"})), None).await.unwrap();
    let bank = k.render_document(&payroll, "letters.bank", oid, None, false).await.unwrap();
    assert_eq!(bank.bytes, b"M\xfcller      001234567.50\r\n", "ISO-8859-1, fixed width, CR LF");
    assert_eq!(bank.content_type, "text/plain; charset=iso-8859-1");
    k.update(&admin, "Offer", oid, obj(json!({"candidate": "=SUM(A1), \"x\";<y a='1'>&"})), None).await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let trail: i64 = sqlx::query_scalar("select count(*) from audit_event where tenant_id = $1 and record_id = $2 and action = 'document:export'")
        .bind(tenant)
        .bind(oid)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(trail, 1, "the bank file's download is on the record's trail");
    let csv = text(&k.render_document(&payroll, "letters.list", oid, None, false).await.unwrap().bytes);
    assert_eq!(csv, "\"'=SUM(A1), \"\"x\"\";<y a='1'>&\",1234567.5\n", "one quoted field, kept as text");
    let xml = text(&k.render_document(&payroll, "letters.xml", oid, None, false).await.unwrap().bytes);
    assert_eq!(xml, "<offer name=\"=SUM(A1), &quot;x&quot;;&lt;y a=&apos;1&apos;&gt;&amp;\">=SUM(A1), &quot;x&quot;;&lt;y a=&apos;1&apos;&gt;&amp;</offer>");

    // ── labels: many records, copies, bounded ──
    let second = k.insert(&admin, "Offer", obj(json!({"candidate": "Bo", "currency": id_of(&inr), "salary": "5"}))).await.unwrap();
    let many = k.render_documents(&payroll, "letters.list", &[oid, id_of(&second)], 3, None, false).await.unwrap();
    assert_eq!(text(&many.bytes).lines().filter(|l| l.starts_with("Bo,")).count(), 3);
    let labels = k.render_documents(&clerk, "letters.offer", &[oid, id_of(&second)], 2, None, false).await.unwrap();
    assert_eq!(text(&labels.bytes).matches("break-after:page").count(), 3, "each label starts a page");
    let too_many = vec![oid; 201];
    assert!(k.render_documents(&payroll, "letters.list", &too_many, 1, None, false).await.unwrap_err().to_string().contains("1 to 200 records"));
    assert!(k.render_documents(&payroll, "letters.list", &[oid; 20], 51, None, false).await.is_err(), "1000 labels at most");

    // ── templates: an edited copy, what is refused, a workspace's own, reset ──
    let read = k.document_template(&admin, "letters.list").await.unwrap();
    let mut def: DocumentDef = serde_json::from_value(read["template"].clone()).unwrap();
    def.languages.get_mut("en").unwrap().body = "{{ doc.candidate }};{{ doc.salary|number(2, 'de') }}\n".into();
    k.save_document_template(&admin, &def).await.unwrap();
    let read = k.document_template(&admin, "letters.list").await.unwrap();
    assert_eq!((read["edited"].clone(), read["default"]["languages"]["en"]["body"].clone()), (json!(true), json!("{{ doc.candidate }},{{ doc.salary }}\n")));
    let csv = text(&k.render_document(&payroll, "letters.list", id_of(&second), None, false).await.unwrap().bytes);
    assert_eq!(csv, "Bo;\"5,00\"\n", "the edit is used; a decimal comma is quoted");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update document_template set base = 'an older default' where tenant_id = $1").bind(tenant).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let listed = k.document_templates(&admin).await.unwrap();
    let list = listed.iter().find(|t| t["key"] == "letters.list").unwrap();
    assert_eq!((list["source"].clone(), list["default_changed"].clone()), (json!("edited"), json!(true)));
    for (body, why) in [
        ("{{ doc.candidat }}", "unknown field"),
        ("{{ doc.pin }}", "a secret field is never rendered"),
        ("{% for l in doc.nope %}{% endfor %}", "an unknown table"),
        ("{{ links.currency.nope }}", "an unknown field of a linked record"),
        ("{% if %}", "does not compile"),
        ("{{ values.nope }}", "a value the caller does not pass"),
    ] {
        let mut d = def.clone();
        d.languages.get_mut("en").unwrap().body = body.into();
        assert!(matches!(k.save_document_template(&admin, &d).await, Err(Error::Validation(_))), "{why}");
    }
    // Said in plain words, where the body names what is not there.
    for (body, says) in [
        ("{{ doc.candidat }}", "en: doc.candidat is not a field of Offer"),
        ("{{ values.nope }}", "en: the body reads values.nope, which is not among the values this template says the sender passes"),
    ] {
        let mut d = def.clone();
        d.languages.get_mut("en").unwrap().body = body.into();
        let Err(Error::Validation(f)) = k.save_document_template(&admin, &d).await else { panic!("{body}") };
        assert!(f[0].message.starts_with(says), "{body}: {}", f[0].message);
    }
    let mut odd = def.clone();
    odd.encoding = Some("ebcdic".into());
    assert!(k.save_document_template(&admin, &odd).await.is_err(), "an encoding there is not");
    let mut stray = def.clone();
    stray.key = "letters.nothing".into();
    assert!(k.save_document_template(&admin, &stray).await.unwrap_err().to_string().contains("has no document letters.nothing"));
    assert!(matches!(k.save_document_template(&clerk, &def).await, Err(Error::Forbidden(_))), "templates are an administrator's");
    let mut own = def.clone();
    own.key = "shop.ean".into();
    own.output = ferrum_meta::DocumentOutput::Page;
    own.default = true;
    own.languages.get_mut("en").unwrap().body = "{{ barcode('400638133393', 'ean13') }}".into();
    k.save_document_template(&admin, &own).await.unwrap();
    assert_eq!(k.document_choices(&clerk, "Offer").await.unwrap()["default"], json!("shop.ean"), "a workspace's default wins");
    assert!(text(&k.render_document(&clerk, "shop.ean", oid, None, false).await.unwrap().bytes).contains(">4006381333931</text>"));
    own.languages.get_mut("en").unwrap().body = "{{ barcode('4006381333932', 'ean13') }}".into();
    k.save_document_template(&admin, &own).await.unwrap();
    let wrong = k.render_document(&clerk, "shop.ean", oid, None, false).await.unwrap_err().to_string();
    assert!(wrong.contains("check digit is 2, not 1"), "a wrong check digit is refused: {wrong}");
    let p = k.preview_document(&admin, &json!("letters.offer"), Some("hi"), Some(oid)).await.unwrap();
    assert_eq!((p["language"].clone(), p["record_id"].clone()), (json!("hi"), json!(oid)));
    k.reset_document_template(&admin, "letters.list").await.unwrap();
    k.reset_document_template(&admin, "shop.ean").await.unwrap();
    assert!(k.reset_document_template(&admin, "letters.list").await.is_err(), "nothing left to reset");

    // ── generated for outside: the workspace's view minus secrets, kept on the record, replaced, mailed ──
    k.update(&admin, "Offer", oid, obj(json!({"candidate": "Ann"})), None).await.unwrap();
    let made = k.generate_document(&clerk, &Generate { template: "letters.offer".into(), record: oid, ..Default::default() }).await.unwrap();
    assert_eq!((made.content_type.as_str(), made.name.as_str(), made.size), ("application/pdf", "Offer letter Ann.pdf", 17));
    let sent = RENDERED.lock().unwrap().last().cloned().unwrap();
    assert!(sent.contains("Bonus 777"), "the workspace's view, whoever caused it");
    assert!(!sent.contains("4321") && sent.contains("<p>Pin </p>"), "never a secret, not even sealed");
    let effects = json!([
        {"type": "generate", "template": "letters.offer", "name": "'Offer ' + doc.candidate", "replace": true},
        {"type": "message", "template": "letters.sent", "to": "field:email", "attachments": ["results[0]"]}
    ]);
    let action = |name: &str, effects: Value| -> NewCustomization {
        serde_json::from_value(json!({"entity": "Offer", "op": {"op": "add_action", "action": {"name": name, "label": name, "effects": effects}}})).unwrap()
    };
    k.batch_customizations(&admin, vec![action("x_send", effects)], false).await.map_err(|e| e.1).unwrap();
    let admin = k.load_ctx(tenant, admin.user, None, "it".into()).await.unwrap();
    let out = k.run_action(&admin, "Offer", oid, "x_send", Value::Null).await.unwrap();
    k.run_action(&admin, "Offer", oid, "x_send", Value::Null).await.unwrap();
    let kept = files_of(&k, &admin, oid).await;
    assert_eq!(kept.iter().filter(|f| f.1.as_deref() == Some("letters.offer")).count(), 1, "each generation replaced the one before: {kept:?}");
    assert_eq!(kept.iter().filter(|f| f.0 == "Offer Ann.pdf").count(), 2, "but not one a mail still waits to attach: {kept:?}");
    assert!(!kept.iter().any(|f| f.0 == "Offer letter Ann.pdf"), "the one no mail waits for is gone: {kept:?}");
    assert!(kept.iter().any(|f| f.0 == "Offer Ann.pdf"), "{kept:?} {out}");
    drain(&k).await;
    drain(&k).await;
    let mailed = ATTACHED.lock().unwrap().clone();
    assert_eq!(mailed.iter().filter(|a| a.0 == "Offer Ann.pdf" && a.2.starts_with(b"%PDF")).count(), 2, "the letter went with each mail");

    // Another record's file is not this record's to mail.
    let other = k.generate_document(&admin, &Generate { template: "letters.list".into(), record: id_of(&second), ..Default::default() }).await.unwrap();
    let leak = json!([{"type": "message", "template": "letters.sent", "to": "field:email", "attachments": [format!("'{}'", other.file)]}]);
    k.batch_customizations(&admin, vec![action("x_leak", leak)], false).await.map_err(|e| e.1).unwrap();
    let admin = k.load_ctx(tenant, admin.user, None, "it".into()).await.unwrap();
    let e = k.run_action(&admin, "Offer", oid, "x_leak", Value::Null).await.unwrap_err();
    assert!(e.to_string().contains("not this record's"), "{e}");
    assert_eq!(other.sha256.len(), 64, "a digest a signing step can check the file by");
}
