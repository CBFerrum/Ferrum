//! Print formats (docs/builder/decisions.md BD-28; `tools/PRINT-FORMATS-SPEC.md` §8.2 T1-T14), against a real
//! PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first) and a stand-in for the PDF service that keeps
//! each form it is sent. With `FERRUM_PRINT_GOTENBERG` set (a real Gotenberg's address) the stand-in also forwards
//! each form there and answers with the real PDF; with `FERRUM_PRINT_EVIDENCE` set (a directory) the worked example's
//! pages are written there as HTML and PDF, A4 and Letter, for a person to look at.

use ferrum_kernel::documents::{Generate, Preview, PreviewAsk, PrintAsk};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, DocumentDef};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const PACKAGE: &str = r#"package: shop
version: 0.1.0
depends: [core]
field_prefix: acc_
roles:
  - name: Clerk
    permissions:
      - {entity: SalesInvoice, read: true, write: true, create: true, submit: true, cancel: true, export: true}
      - {entity: Customer, read: true, write: true, create: true}
      - {entity: Address, read: true, write: true, create: true}
      - {entity: Currency, read: true}
      - {entity: Company, read: true}
  - name: Manager
    permissions:
      - {entity: SalesInvoice, read: true, read_groups: [cost]}
overlays:
  - entity: Company
    ops:
      - {op: add_field, field: {name: acc_logo, type: attach}}
      - {op: add_field, field: {name: acc_print_terms, type: text}}
      - {op: add_field, field: {name: acc_bank_name, type: data}}
      - {op: add_field, field: {name: acc_bank_account_no, type: data}}
      - {op: add_field, field: {name: acc_bank_branch_code, type: data}}
      - {op: add_field, field: {name: acc_registrations, type: table, of: AccRegistration}}
documents:
  - key: shop.invoice
    label: Invoice
    entity: SalesInvoice
    default: true
    layout_file: print/invoice.layout.json
    copies: [Original, Duplicate]
  - key: shop.simple
    label: Simple
    entity: SalesInvoice
    languages:
      en: {body: "<html><body><h1>{{ doc.customer_name }}</h1></body></html>"}
"#;

const ENTITIES: &str = r#"
- name: Address
  label: Address
  kind: master
  title_field: title
  fields:
    - {name: title, type: data, required: true}
    - {name: display, type: text}
- name: Customer
  label: Customer
  kind: master
  title_field: customer_name
  fields:
    - {name: customer_name, type: data, required: true}
    - {name: billing_address, type: text}
    - {name: in_gstin, type: data}
- name: AccRegistration
  label: Registration
  kind: child
  fields:
    - {name: region, type: data}
    - {name: reg_no, type: data}
- name: Vault
  label: Vault
  kind: master
  title_field: code
  fields:
    - {name: code, type: data, required: true}
- name: SalesInvoiceItem
  label: Sales Invoice Item
  kind: child
  fields:
    - {name: item_name, type: data}
    - {name: item, type: data}
    - {name: description, type: text}
    - {name: qty, type: decimal}
    - {name: uom, type: data}
    - {name: rate, type: currency}
    - {name: discount_percent, type: percent}
    - {name: amount, type: currency}
    - {name: net_amount, type: currency}
    - {name: cost, type: currency, perm_group: cost}
- name: SalesTaxLine
  label: Sales Tax Line
  kind: child
  fields:
    - {name: description, type: data}
    - {name: account, type: data}
    - {name: rate, type: percent}
    - {name: tax_amount, type: currency}
- name: SalesInvoice
  label: Sales Invoice
  kind: document
  submittable: true
  title_field: customer_name
  fields:
    - {name: company, type: link, to: Company}
    - {name: customer, type: link, to: Customer}
    - {name: customer_name, type: data}
    - {name: customer_address, type: link, to: Address}
    - {name: ship_to, type: link, to: Address}
    - {name: vault, type: link, to: Vault}
    - {name: currency, type: link, to: Currency}
    - {name: posting_date, type: date}
    - {name: due_date, type: date}
    - {name: return_against, type: data}
    - {name: po_no, type: data}
    - {name: project, type: data}
    - {name: items, type: table, of: SalesInvoiceItem}
    - {name: taxes, type: table, of: SalesTaxLine}
    - {name: total, type: currency}
    - {name: discount_amount, type: currency}
    - {name: net_total, type: currency}
    - {name: total_taxes, type: currency}
    - {name: rounding_adjustment, type: currency}
    - {name: rounded_total, type: currency}
    - {name: grand_total, type: currency}
    - {name: terms, type: text}
    - {name: is_return, type: check}
    - {name: margin, type: currency, perm_group: cost}
    - {name: pin, type: secret}
    - {name: photo, type: attach}
    - {name: code, type: data}
    - {name: signed_qr, type: text, hidden: true}
"#;

/// Request bodies the stand-in PDF renderer received.
static RENDERED: Mutex<Vec<String>> = Mutex::new(Vec::new());
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

/// A stand-in for Gotenberg: records each form it is sent and answers with a tiny PDF, or the real PDF of the
/// Gotenberg at `FERRUM_PRINT_GOTENBERG` when that is set.
async fn fake_pdf_renderer() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let real = std::env::var("FERRUM_PRINT_GOTENBERG").ok();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 65536];
            let got = loop {
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&chunk[..n]);
                let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                let len: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                let ctype = head.lines().find_map(|l| l.strip_prefix("content-type:")).map(|v| v.trim().to_string()).unwrap_or_default();
                if buf.len() >= end + 4 + len {
                    break Some((buf[end + 4..end + 4 + len].to_vec(), ctype));
                }
            };
            let mut pdf = b"%PDF-1.7 stand-in".to_vec();
            if let Some((body, ctype)) = got {
                RENDERED.lock().unwrap().push(String::from_utf8_lossy(&body).to_string());
                if let Some(real) = &real {
                    let client = reqwest::Client::new();
                    if let Ok(r) = client.post(format!("{real}/forms/chromium/convert/html")).header("content-type", ctype).body(body).send().await
                        && let Ok(b) = r.bytes().await
                    {
                        pdf = b.to_vec();
                    }
                }
            }
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", pdf.len());
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(&pdf).await;
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

/// A 1×1 PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06,
    0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x60, 0x60, 0xf8, 0x0f, 0x00, 0x01, 0x04, 0x01,
    0x00, 0x5f, 0xe5, 0xc3, 0x4b, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn layout_file() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sales_invoice.layout.json")).unwrap()
}

fn html_of(p: Preview) -> (String, Vec<String>) {
    match p {
        Preview::Json(v) => {
            (v["content"].as_str().unwrap().to_string(), v["warnings"].as_array().unwrap().iter().map(|w| w.as_str().unwrap().to_string()).collect())
        }
        Preview::Pdf(_) => panic!("asked for HTML"),
    }
}

fn layout_def(key: &str, layout: Value) -> DocumentDef {
    serde_json::from_value(json!({"key": key, "label": "Test", "entity": "SalesInvoice", "layout": layout})).unwrap()
}

fn one_block(block: Value) -> Value {
    json!({"version": 1, "sections": [{"id": "s", "columns": [{"id": "c", "blocks": [block]}]}]})
}

fn fields_of(e: &Error) -> Vec<(String, String)> {
    match e {
        Error::Validation(fs) => fs.iter().map(|f| (f.field.clone(), f.message.clone())).collect(),
        other => vec![("?".into(), other.to_string())],
    }
}

/// Whether a page could run anything: a script, an event handler on a tag, a `javascript:` link, a CSS `url(`.
fn inert(html: &str) {
    let lower = html.to_lowercase();
    assert!(!lower.contains("<script"), "a script: {html}");
    assert!(!lower.contains("javascript:"), "a javascript: URL");
    assert!(!lower.contains("url("), "a CSS url(");
    let tag_with_handler = regex_lite(&lower);
    assert!(!tag_with_handler, "a tag with an on…= handler");
}

/// `<tag … on<x>=` anywhere: a tag's attribute, not escaped text (escaped text has no `<`).
fn regex_lite(s: &str) -> bool {
    let mut in_tag = false;
    let b = s.as_bytes();
    for i in 0..b.len() {
        match b[i] {
            b'<' => in_tag = true,
            b'>' => in_tag = false,
            b' ' | b'\t' | b'\n' | b'/' if in_tag => {
                let rest = &s[i + 1..];
                if rest.starts_with("on") && rest[2..].chars().take_while(|c| c.is_ascii_alphabetic()).count() > 0 {
                    let after: String = rest[2..].chars().skip_while(|c| c.is_ascii_alphabetic()).take(1).collect();
                    if after == "=" {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

#[test]
fn print_formats() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE.to_string()), ("entities/shop.yaml", ENTITIES.to_string()), ("print/invoice.layout.json", layout_file())]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
    let shop = ferrum_meta::catalog::load_package_files(&files).expect("package");
    assert!(shop.manifest.documents[0].layout.is_some() && shop.manifest.documents[0].layout_file.is_none(), "the layout file is read into the document");

    // ── a package's layout file: there, and checked as it loads ──
    let mut missing = files.clone();
    missing.remove("print/invoice.layout.json");
    assert!(ferrum_meta::catalog::load_package_files(&missing).unwrap_err().to_string().contains("is not in the package"));
    let mut broken = files.clone();
    broken.insert("print/invoice.layout.json".into(), layout_file().replace("\"version\": 1", "\"version\": 1, \"css\": \"x\""));
    assert!(ferrum_meta::catalog::load_package_files(&broken).is_err(), "an unknown key in a layout file is refused");

    let settings =
        KernelSettings { pdf_url: Some(fake_pdf_renderer().await), secret_key: Some(ferrum_kernel::secret::SecretKey::new([7u8; 32])), ..Default::default() };
    let k = Kernel::boot(db.clone(), Catalog::from_packages(vec![core, shop]).unwrap(), HookRegistry::default(), settings, Some(Arc::new(Captured)))
        .await
        .expect("boot");
    k.set_file_store(Arc::new(MemFiles::default()));
    let tenant = k
        .create_tenant(NewTenant {
            slug: "print".into(),
            name: "print".into(),
            admin_email: "admin@print.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["shop".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("print", "admin@print.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let clerk = k.create_user(&admin, "clerk@print.test", "Cleo Clerk", Some("Correct-Horse-9"), &["Clerk".into()], false).await.unwrap();
    let clerk = k.load_ctx(tenant, clerk, None, "it".into()).await.unwrap();
    let manager = k.create_user(&admin, "boss@print.test", "Mo Manager", Some("Correct-Horse-9"), &["Clerk".into(), "Manager".into()], false).await.unwrap();
    let manager = k.load_ctx(tenant, manager, None, "it".into()).await.unwrap();

    // ── the data: a company with an uploaded logo, its letter head, a customer, a 3-line invoice ──
    let q = ferrum_kernel::ListQuery { limit: Some(500), ..Default::default() };
    let inr = k.list(&admin, "Currency", &q).await.unwrap().rows.into_iter().find(|c| c["code"] == "INR").expect("INR");
    let logo = k.store_file(&admin, None, "logo.png", "image/png", PNG).await.unwrap();
    let logo_url = logo["url"].as_str().unwrap().to_string();
    let company = k
        .insert(
            &admin,
            "Company",
            obj(json!({"company_name": "Acme <India> Pvt Ltd", "abbr": "ACME", "default_currency": id_of(&inr), "acc_logo": logo_url,
                       "acc_print_terms": "Payment within 30 days.", "acc_bank_name": "State Bank", "acc_bank_account_no": "0011223344",
                       "acc_bank_branch_code": "SBIN0001",
                       "acc_registrations": [{"region": "Karnataka", "reg_no": "29ABCDE1234F1Z5"}, {"region": "Maharashtra", "reg_no": "27ABCDE1234F1Z9"}]})),
        )
        .await
        .unwrap();
    let cid = id_of(&company);
    let customer =
        k.insert(&clerk, "Customer", obj(json!({"customer_name": "Bharat Traders", "billing_address": "12 MG Road\nBengaluru 560001"}))).await.unwrap();
    let addr = k.insert(&clerk, "Address", obj(json!({"title": "Head office", "display": "12 MG Road\nBengaluru 560001\nKarnataka"}))).await.unwrap();
    let vault = k.insert(&admin, "Vault", obj(json!({"code": "V-SECRET"}))).await.unwrap();
    let lines = |n: usize| -> Vec<Value> {
        (0..n)
            .map(|i| {
                json!({"item_name": format!("Widget {}", i + 1), "item": format!("W-{i}"), "description": "Blue, steel", "qty": "2", "uom": "Nos",
                       "rate": "500", "discount_percent": if i == 0 { "10" } else { "0" }, "amount": "1000", "net_amount": "900", "cost": "123.45"})
            })
            .collect()
    };
    let invoice = |n: usize, name: &str| {
        obj(json!({"company": cid, "customer": id_of(&customer), "customer_name": name, "customer_address": id_of(&addr), "vault": id_of(&vault),
                   "currency": id_of(&inr), "posting_date": "2026-11-12", "due_date": "2026-12-12", "po_no": "PO-77", "items": lines(n),
                   "taxes": [{"description": "IGST 18%", "account": "IGST", "rate": "18", "tax_amount": "486"}, {"description": "Cess", "rate": "0", "tax_amount": "0"}],
                   "total": "2700", "discount_amount": "0", "net_total": "2700", "total_taxes": "486", "rounding_adjustment": "0", "rounded_total": "3186",
                   "grand_total": "3186", "margin": "999.99", "pin": "4321", "code": "INV-0001", "photo": logo_url}))
    };
    let inv = k.insert(&admin, "SalesInvoice", invoice(3, "Bharat Traders")).await.unwrap();
    let iid = id_of(&inv);

    // ── the package's layout is the entity's default; the clerk prints it (a draft: watermarked) ──
    let printed = k.print_record(&clerk, "SalesInvoice", iid, &PrintAsk::default()).await.unwrap();
    let page = text(&printed.bytes);
    assert_eq!(printed.content_type, "text/html; charset=utf-8");
    assert!(page.contains("<body class=\"pf pf-font-sans pf-size-md\"") && page.contains("<div class=\"pf-doc\">"), "{page}");
    assert!(page.contains("<h1>Invoice</h1>") && !page.contains("Credit Note"), "a heading by its condition");
    assert!(page.contains("Bharat Traders") && page.contains("12 MG Road<br>Bengaluru 560001<br>Karnataka"), "the link's record, lines kept");
    assert!(page.contains("<thead><tr><th class=\"pf-rn\">#</th><th class=\"pf-a-start\">Description</th>"), "{page}");
    assert!(page.contains("Widget 3") && page.contains("<div class=\"pf-secondary\"><bdi dir=\"ltr\">Blue, steel</bdi></div>"));
    assert!(page.contains("Disc %"), "the discount column shows when a line has one");
    assert!(page.contains("₹3,186.00") && page.contains("three thousand one hundred eighty-six rupees"), "amounts and words: {page}");
    assert!(page.contains("IGST 18%") && !page.contains("Cess"), "a zero tax row is left out");
    assert!(page.contains("<div class=\"pf-watermark\" aria-hidden=\"true\">Draft</div>"), "a draft is watermarked");
    assert!(page.contains("For <bdi dir=\"ltr\">Acme &lt;India&gt; Pvt Ltd</bdi>"), "a token, escaped");
    assert!(page.contains("Payment within 30 days.") && page.contains("0011223344"), "company's terms and bank as fallbacks");
    assert!(!page.contains("999.99") && !page.contains("123.45") && !page.contains("4321"), "no field group the clerk may not read, no secret");
    assert!(page.contains("@page{size:210mm 297mm;margin:15mm 12mm 15mm 12mm}"), "Print Settings' paper for the browser");
    inert(&page);

    // ── T4: who reads what; BD-21's rule for someone outside ──
    let cost = layout_def(
        "cost_sheet",
        json!({"version": 1, "sections": [{"id": "s", "columns": [{"id": "c", "blocks": [
            {"id": "m", "type": "field", "value": "doc.margin", "label": "Margin"},
            {"id": "v", "type": "field", "value": "links.vault.code", "label": "Vault"},
            {"id": "t", "type": "table", "value": "doc.items", "columns": [{"id": "k", "value": "row.cost", "label": "Cost"}]}
        ]}]}]}),
    );
    k.save_document_template(&admin, &cost).await.unwrap();
    let as_clerk =
        text(&k.print_record(&clerk, "SalesInvoice", iid, &PrintAsk { template: Some("cost_sheet".into()), ..Default::default() }).await.unwrap().bytes);
    assert!(!as_clerk.contains("999.99") && !as_clerk.contains("123.45") && !as_clerk.contains("V-SECRET"), "masked fields and an unreadable link: {as_clerk}");
    let as_manager =
        text(&k.print_record(&manager, "SalesInvoice", iid, &PrintAsk { template: Some("cost_sheet".into()), ..Default::default() }).await.unwrap().bytes);
    assert!(as_manager.contains("999.99"), "the field group's readers read it: {as_manager}");
    let tmpl: DocumentDef =
        serde_json::from_value(json!({"key": "cost_text", "entity": "SalesInvoice", "languages": {"en": {"body": "<p>M{{ doc.margin }}</p>"}}})).unwrap();
    k.save_document_template(&admin, &tmpl).await.unwrap();
    let t = text(&k.print_record(&clerk, "SalesInvoice", iid, &PrintAsk { template: Some("cost_text".into()), ..Default::default() }).await.unwrap().bytes);
    assert!(t.contains("<p>M</p>"), "a template prints no field the person may not read: {t}");
    let generated = k.generate_document(&clerk, &Generate { template: "cost_sheet".into(), record: iid, ..Default::default() }).await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let key: String = sqlx::query_scalar("select storage_key from file_object where tenant_id = $1 and id = $2")
        .bind(tenant)
        .bind(generated.file)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let _ = key;
    let forms = RENDERED.lock().unwrap().clone();
    let outside = forms.last().unwrap();
    assert!(outside.contains("999.99") && outside.contains("V-SECRET") && !outside.contains("4321"), "for outside: as the workspace holds it, no secret");

    // ── T5 and T1: what a save refuses, at the block it names ──
    for (block, field, says) in [
        (json!({"id": "f", "type": "field", "value": "doc.nope"}), "layout.f", "has no field nope"),
        (json!({"id": "f", "type": "field", "value": "doc.pin"}), "layout.f", "secret field"),
        (json!({"id": "f", "type": "field", "value": "row.qty"}), "layout.f", "table's columns only"),
        (json!({"id": "f", "type": "field", "value": "print.page"}), "layout.f", "letter head's"),
        (json!({"id": "f", "type": "field", "value": "links.customer.nope"}), "layout.f", "has no field nope"),
        (json!({"id": "f", "type": "field", "value": "doc.number", "when": "doc.(("}), "layout.f", "does not compile"),
        (json!({"id": "f", "type": "field", "value": "doc.number", "color": "red;background:url(x)"}), "layout.f", "#rrggbb"),
        (json!({"id": "t", "type": "table", "value": "doc.items", "columns": [{"id": "c1", "value": "doc.total"}]}), "layout.c1", "row.*, not doc.*"),
        (json!({"id": "t", "type": "table", "value": "doc.nope", "columns": [{"id": "c1", "value": "row.qty"}]}), "layout.t", "is not a table of the record"),
        (json!({"id": "i", "type": "image", "src": {"file": "https://evil.test/x.png"}}), "layout.i", "this workspace's files"),
        (json!({"id": "s2", "type": "spacer", "height_mm": 500}), "layout.s2", "1 to 100"),
    ] {
        let e = k.save_document_template(&admin, &layout_def("bad", one_block(block.clone()))).await.unwrap_err();
        let f = fields_of(&e);
        assert!(f.iter().any(|(n, m)| n == field && m.contains(says)), "{block}: {f:?}");
    }
    let many: Vec<Value> = (0..51).map(|i| json!({"id": format!("b{i}"), "type": "spacer", "height_mm": 1})).collect();
    let e = k
        .save_document_template(&admin, &layout_def("bad", json!({"version": 1, "sections": [{"id": "s", "columns": [{"id": "c", "blocks": many}]}]})))
        .await
        .unwrap_err();
    assert_eq!(fields_of(&e)[0], ("layout.c".into(), "a column holds at most 50 blocks".into()));
    assert!(
        serde_json::from_value::<DocumentDef>(json!({"key": "x", "entity": "SalesInvoice", "layout": {"version": 1, "sections": [], "html": "<b>"}})).is_err()
    );
    // A stored layout edited to break a limit is refused when it renders.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update document_template set def = jsonb_set(def, '{layout,sections,0,columns,0,blocks,0,color}', '\"red\"') where tenant_id = $1 and key = 'cost_sheet'")
        .bind(tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let e = k.print_record(&manager, "SalesInvoice", iid, &PrintAsk { template: Some("cost_sheet".into()), ..Default::default() }).await.unwrap_err();
    assert!(fields_of(&e).iter().any(|(n, m)| n == "layout.m" && m.contains("#rrggbb")), "{e:?}");

    // ── T2: hostile values print escaped wherever they print ──
    let hostile = "<script>alert(1)</script>\"><img src=x onerror=alert(1)>";
    let mut h = invoice(1, hostile);
    h.insert("items".into(), json!([{"item_name": hostile, "description": hostile, "qty": "1", "rate": "1", "amount": "1", "net_amount": "1"}]));
    h.insert("terms".into(), json!(hostile));
    let hid = id_of(&k.insert(&admin, "SalesInvoice", h).await.unwrap());
    let spans = json!({"version": 1, "watermark": {"text": "{doc.customer_name}"}, "sections": [{"id": "s", "title": "<b>Title</b>", "columns": [{"id": "c", "blocks": [
        {"id": "h", "type": "heading", "text": "Bill {doc.customer_name} <i>x</i>"},
        {"id": "p", "type": "text", "content": [{"spans": [{"t": "<u>raw</u> "}, {"v": "doc.terms", "b": true}]}]},
        {"id": "f", "type": "field", "value": "doc.customer_name", "label": "<em>L</em>"},
        {"id": "t", "type": "table", "value": "doc.items", "columns": [{"id": "n", "value": "row.item_name", "label": "<th>", "secondary": "row.description"}]},
        {"id": "g", "type": "signature", "label": "{doc.customer_name}"},
        {"id": "b", "type": "barcode", "kind": "code128", "value": "doc.code"}
    ]}]}]});
    k.save_document_template(&admin, &layout_def("hostile", spans)).await.unwrap();
    let page = text(&k.print_record(&clerk, "SalesInvoice", hid, &PrintAsk { template: Some("hostile".into()), ..Default::default() }).await.unwrap().bytes);
    inert(&page);
    assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;&quot;&gt;&lt;img src=x onerror=alert(1)&gt;"), "{page}");
    assert!(page.contains("&lt;b&gt;Title&lt;/b&gt;") && page.contains("&lt;u&gt;raw&lt;/u&gt;") && page.contains("&lt;th&gt;"), "a layout's own text too");
    let t: DocumentDef = serde_json::from_value(json!({"key": "hostile_t", "entity": "SalesInvoice", "languages": {"en": {"body": "<p>{{ doc.customer_name }}</p>{% for l in doc.items %}<td>{{ l.item_name }}</td>{% endfor %}"}}})).unwrap();
    k.save_document_template(&admin, &t).await.unwrap();
    inert(&text(&k.print_record(&clerk, "SalesInvoice", hid, &PrintAsk { template: Some("hostile_t".into()), ..Default::default() }).await.unwrap().bytes));

    // ── T3 and the uploaded logo: images are this workspace's own files, read as their reader may, capped ──
    let other = k
        .create_tenant(NewTenant {
            slug: "other".into(),
            name: "other".into(),
            admin_email: "admin@other.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["shop".into()],
            settings: None,
        })
        .await
        .unwrap();
    let os = k.login("other", "admin@other.test", "Correct-Horse-9").await.unwrap();
    let oadmin = k.load_ctx(other, os.user, None, "it".into()).await.unwrap();
    let theirs = k.store_file(&oadmin, None, "theirs.png", "image/png", PNG).await.unwrap()["url"].as_str().unwrap().to_string();
    let big = k.store_file(&admin, None, "big.png", "image/png", &[PNG, &vec![0u8; 2 << 20]].concat()).await.unwrap()["url"].as_str().unwrap().to_string();
    let not_image = k.store_file(&admin, None, "x.html", "text/html", b"<html><script>alert(1)</script>").await.unwrap()["url"].as_str().unwrap().to_string();
    let imgs = json!({"version": 1, "sections": [{"id": "s", "columns": [{"id": "c", "blocks": [
        {"id": "logo", "type": "image", "src": {"value": "company.acc_logo"}},
        {"id": "own", "type": "image", "src": {"file": logo_url}},
        {"id": "foreign", "type": "image", "src": {"file": theirs}},
        {"id": "huge", "type": "image", "src": {"file": big}},
        {"id": "html", "type": "image", "src": {"file": not_image}},
        {"id": "att", "type": "image", "src": {"value": "doc.photo"}}
    ]}]}]});
    let ask = |t: Value| PreviewAsk { template: t, record: Some(iid), ..Default::default() };
    let def = layout_def("images", imgs);
    let (page, warnings) = html_of(k.preview_print(&admin, &ask(serde_json::to_value(&def).unwrap())).await.unwrap());
    assert_eq!(page.matches("<img src=\"data:image/png;base64,").count(), 3, "the company's logo, the format's own image, the record's: {page}");
    for (url, why) in [(&theirs, "no such file here"), (&big, "1 MB"), (&not_image, "not a png")] {
        assert!(warnings.iter().any(|w| w.contains(url.as_str()) && w.contains(why)), "{why}: {warnings:?}");
    }
    inert(&page);
    // The ERP's old way: an attach field printed by a template, now through image().
    let logo_t: DocumentDef =
        serde_json::from_value(json!({"key": "logo_t", "entity": "SalesInvoice", "languages": {"en": {"body": "<img src=\"{{ image(company.acc_logo) }}\">{{ image('https://x.test/a.png') }}"}}}))
            .unwrap();
    k.save_document_template(&admin, &logo_t).await.unwrap();
    let t = text(&k.print_record(&clerk, "SalesInvoice", iid, &PrintAsk { template: Some("logo_t".into()), ..Default::default() }).await.unwrap().bytes);
    assert!(t.contains("<img src=\"data:image/png;base64,iVBORw0KGgo"), "T14: a template's image() of the company's uploaded logo: {t}");
    // A template that does not compile says where.
    let bad: DocumentDef = serde_json::from_value(json!({"key": "bad_t", "entity": "SalesInvoice", "languages": {"en": {"body": "a\nb\n{% if %}"}}})).unwrap();
    let e = k.save_document_template(&admin, &bad).await.unwrap_err();
    assert!(fields_of(&e)[0].1.starts_with("en body, line 3: "), "{e:?}");
    let bad: DocumentDef =
        serde_json::from_value(json!({"key": "bad_t", "entity": "SalesInvoice", "languages": {"hi": {"body": "x", "footer": "\n{{ doc.nope }}"}}})).unwrap();
    assert!(fields_of(&k.save_document_template(&admin, &bad).await.unwrap_err())[0].1.starts_with("hi footer, line 2: "));

    // ── letter heads: checked as they are saved, worn by every page, repeated by the PDF service ──
    let lh_bad = k
        .insert(&admin, "LetterHead", obj(json!({"letter_head_name": "Bad", "header": {"sections": [{"id": "s", "columns": [{"id": "c", "blocks": [{"id": "x", "type": "field", "value": "doc.number"}]}]}]}})))
        .await
        .unwrap_err();
    assert!(fields_of(&lh_bad).iter().any(|(f, m)| f == "header" && m.starts_with("x: doc.number")), "{lh_bad:?}");
    let lh = k
        .insert(
            &admin,
            "LetterHead",
            obj(json!({"letter_head_name": "Acme India", "company": cid, "header_height_mm": 25,
                "header": {"sections": [{"id": "s", "widths": [3, 9], "columns": [
                    {"id": "l", "blocks": [{"id": "logo", "type": "image", "src": {"value": "company.acc_logo"}, "height_mm": 18}]},
                    {"id": "r", "blocks": [{"id": "name", "type": "heading", "text": "{company.company_name}", "level": 2, "align": "end"},
                                            {"id": "bank", "type": "field", "value": "company.acc_bank_name", "label": null, "align": "end"}]}]}]},
                "footer": {"sections": [{"id": "f", "columns": [{"id": "fc", "blocks": [{"id": "pg", "type": "text", "content": [{"align": "center", "spans": [
                    {"t": "Page "}, {"v": "print.page"}, {"t": " of "}, {"v": "print.pages"}]}]}]}]}]}})),
        )
        .await
        .unwrap();
    let lhid = id_of(&lh);
    k.update(&admin, "Company", cid, obj(json!({"letter_head": lhid})), None).await.unwrap();
    let page = text(&k.print_record(&clerk, "SalesInvoice", iid, &PrintAsk::default()).await.unwrap().bytes);
    assert!(
        page.contains("<header class=\"pf-letterhead\">") && page.contains("Acme &lt;India&gt; Pvt Ltd</bdi></h2>"),
        "the company's letter head, in the body: {page}"
    );
    assert!(
        page.contains("<footer class=\"pf-letterhead-foot\">") && !page.contains("Page  of") && !page.contains(">Page "),
        "no half page numbers outside the PDF: {page}"
    );
    let none = text(&k.print_record(&clerk, "SalesInvoice", iid, &PrintAsk { letterhead: Some("none".into()), ..Default::default() }).await.unwrap().bytes);
    assert!(!none.contains("<header class=\"pf-letterhead\""), "letterhead=none");
    let unknown = PrintAsk { letterhead: Some(Uuid::nil().to_string()), ..Default::default() };
    match k.print_record(&clerk, "SalesInvoice", iid, &unknown).await {
        Err(ferrum_kernel::Error::Validation(f)) => assert_eq!(f[0].field, "letterhead", "{f:?}"),
        other => panic!("an unknown letter head is refused, not printed without one: {:?}", other.map(|r| r.content_type)),
    }
    RENDERED.lock().unwrap().clear();
    let pdf = k
        .print_record(&clerk, "SalesInvoice", iid, &PrintAsk { pdf: true, copies: vec!["Original".into(), "Duplicate".into()], ..Default::default() })
        .await
        .unwrap();
    assert!(pdf.bytes.starts_with(b"%PDF") && pdf.file_name.starts_with("Invoice ") && pdf.file_name.ends_with(".pdf"), "{}", pdf.file_name);
    let form = RENDERED.lock().unwrap().last().cloned().unwrap();
    let field = |name: &str| form.split(&format!("name=\"{name}\"\r\n\r\n")).nth(1).and_then(|r| r.split("\r\n").next()).unwrap_or_default().to_string();
    assert!(form.contains("filename=\"header.html\"") && form.contains("filename=\"footer.html\""), "the letter head repeated");
    assert!(form.contains("<span class=\"pageNumber\"></span>") && form.contains("<span class=\"totalPages\"></span>"), "page numbers in the footer");
    assert_eq!((field("paperWidth"), field("paperHeight"), field("preferCssPageSize")), ("8.27".into(), "11.69".into(), "false".into()));
    assert_eq!(field("marginTop"), format!("{:.3}", 40.0 / 25.4), "15 mm and the 25 mm header");
    let index = form.split("filename=\"index.html\"").nth(1).unwrap();
    assert!(!index.split("filename=\"header.html\"").next().unwrap().contains("pf-letterhead\">"), "not in the body as well");
    // T7: copies, each a document of its own, labelled where the layout says.
    assert_eq!(index.matches("<div class=\"pf-doc\" data-copy=").count(), 2);
    assert!(index.contains("data-copy=\"Original\"") && index.contains(">Duplicate</div>"));

    // ── T7 in HTML: three labels ──
    let three = text(
        &k.print_record(
            &clerk,
            "SalesInvoice",
            iid,
            &PrintAsk {
                copies: vec!["Original for Recipient".into(), "Duplicate for Transporter".into(), "Triplicate for Supplier".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .bytes,
    );
    assert_eq!((three.matches("<div class=\"pf-doc\"").count(), three.matches("<div class=\"pf-break\"></div>").count()), (3, 2));
    assert!(three.contains("pf-copy-label pf-a-end pf-s-sm pf-w-bold\">Triplicate for Supplier</div>"), "{three}");
    assert!(k.print_record(&clerk, "SalesInvoice", iid, &PrintAsk { copies: vec!["x".repeat(61)], ..Default::default() }).await.is_err());

    // ── T6: drafts and cancelled records ──
    let ps = k
        .insert(&admin, "PrintSettings", obj(json!({"allow_print_draft": false, "copies": "Original for Recipient\nDuplicate for Transporter"})))
        .await
        .unwrap();
    let refused = k.print_record(&clerk, "SalesInvoice", iid, &PrintAsk::default()).await.unwrap_err();
    assert!(matches!(&refused, Error::Forbidden(m) if m.contains("printing a draft is turned off")), "{refused:?}");
    let choices = k.print_choices(&clerk, "SalesInvoice", Some(iid)).await.unwrap();
    assert_eq!((choices["may_print"].clone(), choices["copies"].clone()), (json!(false), json!(["Original for Recipient", "Duplicate for Transporter"])));
    let (preview, _) = html_of(k.preview_print(&admin, &ask(json!("shop.invoice"))).await.unwrap());
    assert!(preview.contains(">Draft</div>"), "an administrator's preview still works");
    k.submit(&clerk, "SalesInvoice", iid, None).await.unwrap();
    let submitted = text(&k.print_record(&clerk, "SalesInvoice", iid, &PrintAsk::default()).await.unwrap().bytes);
    assert!(!submitted.contains("<div class=\"pf-watermark\""), "a submitted record is not watermarked");
    k.cancel(&clerk, "SalesInvoice", iid, None).await.unwrap();
    let cancelled = text(&k.print_record(&clerk, "SalesInvoice", iid, &PrintAsk::default()).await.unwrap().bytes);
    assert!(cancelled.contains("aria-hidden=\"true\">Cancelled</div>"));
    k.update(&admin, "PrintSettings", id_of(&ps), obj(json!({"allow_print_draft": true})), None).await.unwrap();

    // ── T8: the default's order (D9), roles ──
    let fresh = id_of(&k.insert(&admin, "SalesInvoice", invoice(1, "Default check")).await.unwrap());
    let default_of = |c: &Ctx| {
        let (k, c) = (k.clone(), c.clone());
        async move { k.print_choices(&c, "SalesInvoice", None).await.unwrap()["default"].clone() }
    };
    assert_eq!(default_of(&clerk).await, json!("shop.invoice"), "the package's default");
    let mut flagged = layout_def("flagged", one_block(json!({"id": "f", "type": "heading", "text": "FLAGGED"})));
    flagged.default = true;
    k.save_document_template(&admin, &flagged).await.unwrap();
    assert_eq!(default_of(&clerk).await, json!("flagged"), "saving default: true is the workspace's choice");
    k.set_document_default(&admin, "SalesInvoice", None).await.unwrap();
    assert_eq!(default_of(&clerk).await, json!("flagged"), "a workspace template flagged default, with no choice made");
    let mut managers = layout_def("managers", one_block(json!({"id": "f", "type": "heading", "text": "MANAGERS"})));
    managers.roles = vec!["Manager".into()];
    k.save_document_template(&admin, &managers).await.unwrap();
    k.set_document_default(&admin, "SalesInvoice", Some("managers")).await.unwrap();
    assert_eq!(default_of(&manager).await, json!("managers"), "the workspace's choice");
    assert_eq!(default_of(&clerk).await, json!("flagged"), "one the clerk may not use falls back");
    assert!(text(&k.print_record(&manager, "SalesInvoice", fresh, &PrintAsk::default()).await.unwrap().bytes).contains("MANAGERS"));
    assert!(matches!(
        k.print_record(&clerk, "SalesInvoice", fresh, &PrintAsk { template: Some("managers".into()), ..Default::default() }).await,
        Err(Error::Forbidden(_))
    ));
    let mut off = flagged.clone();
    off.enabled = false;
    k.save_document_template(&admin, &off).await.unwrap();
    assert_eq!(default_of(&clerk).await, json!("shop.invoice"), "a disabled one is not offered");
    assert!(k.print_record(&clerk, "SalesInvoice", fresh, &PrintAsk { template: Some("flagged".into()), ..Default::default() }).await.is_err());
    k.reset_document_template(&admin, "managers").await.unwrap();
    k.reset_document_template(&admin, "flagged").await.unwrap();
    assert_eq!(default_of(&manager).await, json!("shop.invoice"), "deleting the chosen one deletes the choice");
    assert!(k.set_document_default(&admin, "SalesInvoice", Some("nope")).await.is_err());
    let standard =
        text(&k.print_record(&clerk, "SalesInvoice", fresh, &PrintAsk { template: Some("standard".into()), ..Default::default() }).await.unwrap().bytes);
    assert!(
        standard.contains("Sales Invoice") && standard.contains("Default check") && standard.contains("aria-hidden=\"true\">Draft</div>"),
        "the print format, now with the letter head and watermark"
    );

    // ── T9: what an e-mail attaches is what /print?format=pdf makes with the same choices ──
    RENDERED.lock().unwrap().clear();
    let mail = k.print_record(&clerk, "SalesInvoice", fresh, &PrintAsk { pdf: true, ..Default::default() }).await.unwrap();
    let form = RENDERED.lock().unwrap().last().cloned().unwrap();
    assert!(form.contains("pf-doc") && !form.contains("<span class=\"status\">"), "the default format (the layout), not the print format");
    assert!(mail.file_name.starts_with("Invoice "), "{}", mail.file_name);

    // ── T10: If-Match, duplicating a package's layout, editing it in place refused ──
    let read = k.document_template(&admin, "shop.invoice").await.unwrap();
    let tag = read["etag"].as_str().unwrap().to_string();
    assert_eq!(read["template"]["kind"], json!("layout"));
    let mut package: DocumentDef = serde_json::from_value({
        let mut t = read["template"].clone();
        t.as_object_mut().unwrap().remove("kind");
        t
    })
    .unwrap();
    package.page = Some(serde_json::from_value(json!({"size": "A5", "orientation": "landscape"})).unwrap());
    let tag2 = k.save_document_template_if(&admin, &package, Some(&tag)).await.unwrap();
    assert_ne!(tag, tag2, "a package's paper changes in an edited copy");
    assert!(matches!(k.save_document_template_if(&admin, &package, Some(&tag)).await, Err(Error::Conflict(_))), "a stale tag is refused");
    let mut moved = package.clone();
    moved.layout.as_mut().unwrap().sections.remove(0);
    assert!(fields_of(&k.save_document_template(&admin, &moved).await.unwrap_err())[0].1.contains("duplicate this format"), "Q3: not in place");
    let copy = k.duplicate_document_template(&admin, "shop.invoice", "my_invoice", "My invoice").await.unwrap();
    assert_eq!((copy.based_on.as_deref(), copy.default, copy.enabled), (Some("shop.invoice"), false, true));
    assert!(matches!(k.duplicate_document_template(&admin, "shop.invoice", "my_invoice", "Again").await, Err(Error::Conflict(_))));
    assert!(k.duplicate_document_template(&admin, "shop.invoice", "shop.mine", "Mine").await.is_err(), "a package's name");
    let listed = k.document_templates_of(&admin, Some("SalesInvoice"), Some("page")).await.unwrap();
    let mine = listed.iter().find(|t| t["key"] == "my_invoice").unwrap();
    assert_eq!((mine["kind"].clone(), mine["source"].clone(), mine["based_on"].clone()), (json!("layout"), json!("workspace"), json!("shop.invoice")));
    // Versions: each save kept; a restore is a save.
    let mut v2 = copy.clone();
    v2.label = Some("My invoice v2".into());
    k.save_document_template(&admin, &v2).await.unwrap();
    let versions = k.document_template_versions(&admin, "my_invoice").await.unwrap();
    assert_eq!(versions.iter().map(|v| v["n"].as_i64().unwrap()).collect::<Vec<_>>(), vec![2, 1]);
    let old: DocumentDef = serde_json::from_value(k.document_template_version(&admin, "my_invoice", 1).await.unwrap()).unwrap();
    assert_eq!(old.label.as_deref(), Some("My invoice"));
    // A format read back with `default: true` saves without choosing the default again: PUT …/defaults chooses.
    k.set_document_default(&admin, "SalesInvoice", Some("my_invoice")).await.unwrap();
    let mut back: DocumentDef = serde_json::from_value(k.document_template(&admin, "shop.invoice").await.unwrap()["template"].clone()).unwrap();
    assert!(back.default);
    back.label = Some("Invoice (edited)".into());
    k.save_document_template(&admin, &back).await.unwrap();
    assert_eq!(k.print_choices(&clerk, "SalesInvoice", None).await.unwrap()["default"], json!("my_invoice"), "the choice stays");
    k.set_document_default(&admin, "SalesInvoice", None).await.unwrap();
    let rows = k.document_templates_of(&admin, Some("SalesInvoice"), None).await.unwrap();
    assert!(rows.iter().find(|t| t["key"] == "my_invoice").unwrap()["updated_by_name"].is_string(), "who saved it, by name");
    // The paper of the edited package format reaches the PDF service.
    RENDERED.lock().unwrap().clear();
    k.print_record(&clerk, "SalesInvoice", fresh, &PrintAsk { pdf: true, template: Some("shop.invoice".into()), ..Default::default() }).await.unwrap();
    let form = RENDERED.lock().unwrap().last().cloned().unwrap();
    let field = |name: &str| form.split(&format!("name=\"{name}\"\r\n\r\n")).nth(1).and_then(|r| r.split("\r\n").next()).unwrap_or_default().to_string();
    assert_eq!((field("paperWidth"), field("paperHeight")), ("8.27".into(), "5.83".into()), "T11 through the whole path");

    // ── T13: barcodes ──
    let mut bc = invoice(1, "Codes");
    bc.insert("code".into(), json!("4006381333931"));
    bc.insert("po_no".into(), json!("4006381333932"));
    bc.insert("terms".into(), json!("x".repeat(1001)));
    let bcid = id_of(&k.insert(&admin, "SalesInvoice", bc).await.unwrap());
    let codes = json!({"version": 1, "sections": [{"id": "s", "columns": [{"id": "c", "blocks": [
        {"id": "c128", "type": "barcode", "kind": "code128", "value": "doc.customer_name"},
        {"id": "ean", "type": "barcode", "kind": "ean13", "value": "doc.code"},
        {"id": "badean", "type": "barcode", "kind": "ean13", "value": "doc.po_no"},
        {"id": "qr", "type": "barcode", "kind": "qr", "value": "doc.customer_name"},
        {"id": "bigqr", "type": "barcode", "kind": "qr", "value": "doc.terms"}
    ]}]}]});
    let (page, warnings) = html_of(
        k.preview_print(&admin, &PreviewAsk { template: serde_json::to_value(layout_def("codes", codes)).unwrap(), record: Some(bcid), ..Default::default() })
            .await
            .unwrap(),
    );
    assert_eq!(page.matches("<div class=\"pf-block pf-barcode").count(), 3, "code128, ean13, qr: {page}");
    assert!(warnings.iter().any(|w| w.starts_with("badean:")) && warnings.iter().any(|w| w.starts_with("bigqr:") && w.contains("1000 bytes")), "{warnings:?}");

    // ── what the ERP's formats needed: a sign left off, a zero cell blank, a company's table, one line said once, the
    // time of printing ──
    let mut credit = invoice(1, "Credit");
    credit.insert("grand_total".into(), json!("-3186"));
    credit.insert(
        "items".into(),
        json!([{"item_name": "", "description": "Returned widget", "qty": "-1", "rate": "500", "discount_percent": "0", "amount": "-500"}]),
    );
    let crid = id_of(&k.insert(&admin, "SalesInvoice", credit).await.unwrap());
    let extras = json!({"version": 1, "sections": [{"id": "s", "columns": [{"id": "c", "blocks": [
        {"id": "gt", "type": "field", "value": "doc.grand_total", "label": "Total", "format": {"as": "amount", "sign": "abs"}},
        {"id": "gw", "type": "field", "value": "doc.grand_total", "label": "Words", "format": {"as": "words", "sign": "abs"}},
        {"id": "lines", "type": "table", "value": "doc.items", "columns": [
            {"id": "d", "value": ["row.item_name", "row.description"], "secondary": "row.description"},
            {"id": "q", "value": "row.qty", "format": {"as": "number", "sign": "abs"}},
            {"id": "z", "value": "row.discount_percent", "label": "Disc", "hide_zero": true}]},
        {"id": "regs", "type": "table", "value": "company.acc_registrations", "columns": [{"id": "r", "value": "row.region"}, {"id": "n", "value": "row.reg_no"}]},
        {"id": "at", "type": "field", "value": "print.time", "label": "Printed"}
    ]}]}]});
    let (page, _) = html_of(
        k.preview_print(
            &admin,
            &PreviewAsk { template: serde_json::to_value(layout_def("extras", extras)).unwrap(), record: Some(crid), ..Default::default() },
        )
        .await
        .unwrap(),
    );
    assert!(
        page.contains("<span class=\"pf-value\"><bdi dir=\"ltr\">₹3,186.00</bdi></span>")
            && page.contains("three thousand one hundred eighty-six rupees")
            && !page.contains("minus"),
        "{page}"
    );
    assert_eq!(page.matches("Returned widget").count(), 1, "said once: {page}");
    assert!(
        page.contains("<td class=\"pf-a-end\"><bdi dir=\"ltr\">1</bdi></td>") && page.contains("<td class=\"pf-a-end\"></td>"),
        "qty without its sign, a zero discount blank: {page}"
    );
    assert!(page.contains("29ABCDE1234F1Z5") && page.contains("Maharashtra"), "the company's own table: {page}");
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    assert!(
        page.contains(&format!("<span class=\"pf-label\">Printed</span><span class=\"pf-value\"><bdi dir=\"ltr\">{}", &today[..4])),
        "the time of printing"
    );
    let bad = one_block(json!({"id": "t", "type": "table", "value": "links.customer.nope", "columns": [{"id": "x", "value": "row.x"}]}));
    assert!(fields_of(&k.save_document_template(&admin, &layout_def("bad_t2", bad)).await.unwrap_err()).iter().any(|(f, _)| f == "layout.t"));
    let cat = k.document_fields(&admin, "SalesInvoice").unwrap();
    assert!(cat["company_tables"][0]["path"] == json!("company.acc_registrations") && cat["options"]["format"]["sign"][0] == json!("abs"));
    assert!(cat["print"].as_array().unwrap().iter().any(|p| p["path"] == "print.time"));

    // ── right to left: every value isolated, a phone number and an amount keep their order in an Arabic line ──
    let mut phone = invoice(1, "شركة التجارة");
    phone.insert("code".into(), json!("+91 22 4000 1234"));
    let phid = id_of(&k.insert(&admin, "SalesInvoice", phone).await.unwrap());
    let rtl = json!({"version": 1, "sections": [{"id": "s", "columns": [{"id": "c", "blocks": [
        {"id": "p", "type": "field", "value": "doc.code", "label": "هاتف"},
        {"id": "n", "type": "field", "value": "doc.customer_name", "label": null},
        {"id": "t", "type": "field", "value": "doc.grand_total", "label": "المجموع"}]}]}]});
    let ask =
        PreviewAsk { template: serde_json::to_value(layout_def("rtl", rtl)).unwrap(), record: Some(phid), language: Some("ar".into()), ..Default::default() };
    let (ar, _) = html_of(k.preview_print(&admin, &ask).await.unwrap());
    assert!(ar.contains("dir=\"rtl\"") && ar.contains("<bdi dir=\"ltr\">+91 22 4000 1234</bdi>"), "{ar}");
    assert!(ar.contains("<bdi dir=\"auto\">شركة التجارة</bdi>") && ar.contains("<bdi dir=\"ltr\">٣٬١٨٦٫٠٠"), "{ar}");

    // ── a format that prints its own company block only without a letter head; dates by name ──
    let own = json!({"version": 1, "sections": [{"id": "s", "columns": [{"id": "c", "blocks": [
        {"id": "co", "type": "heading", "text": "OWN COMPANY BLOCK", "when": "!print.letterhead"},
        {"id": "lhn", "type": "field", "value": "print.letterhead_name", "label": "LH"},
        {"id": "d1", "type": "field", "value": "doc.posting_date", "label": "On"},
        {"id": "d2", "type": "field", "value": "doc.posting_date", "label": "Short", "format": {"as": "date", "date": "short"}},
        {"id": "d3", "type": "field", "value": "doc.posting_date", "label": "Long", "format": {"as": "date", "date": "long"}}
    ]}]}]});
    let own_def = serde_json::to_value(layout_def("own", own)).unwrap();
    let with = |lh: Option<&str>| PreviewAsk { template: own_def.clone(), record: Some(crid), letterhead: lh.map(str::to_string), ..Default::default() };
    let (worn, _) = html_of(k.preview_print(&admin, &with(None)).await.unwrap());
    assert!(
        !worn.contains("OWN COMPANY BLOCK") && worn.contains("<span class=\"pf-value\"><bdi dir=\"ltr\">Acme India</bdi></span>"),
        "a letter head worn: {worn}"
    );
    let (bare, _) = html_of(k.preview_print(&admin, &with(Some("none"))).await.unwrap());
    assert!(bare.contains("OWN COMPANY BLOCK") && !bare.contains("Acme India</span>"), "none worn: {bare}");
    assert!(
        bare.contains("<bdi dir=\"ltr\">12 Nov 2026<") && bare.contains("<bdi dir=\"ltr\">11/12/2026<") && bare.contains("<bdi dir=\"ltr\">November 12, 2026<"),
        "medium by default, short and long when asked: {bare}"
    );

    // ── a letter head as tall as its blocks (no height given): the PDF's top margin follows it ──
    let margin_top = |form: &str| -> f64 {
        let v: f64 = form.split("name=\"marginTop\"\r\n\r\n").nth(1).and_then(|r| r.split("\r\n").next()).unwrap().parse().unwrap();
        v * 25.4
    };
    let short = json!({"header": {"sections": [{"id": "s", "columns": [{"id": "c", "blocks": [{"id": "h", "type": "heading", "text": "{company.company_name}", "level": 2}]}]}]}});
    let full = json!({"header": {"sections": [{"id": "s", "widths": [3, 9], "columns": [
        {"id": "l", "blocks": [{"id": "logo", "type": "image", "src": {"value": "company.acc_logo"}, "height_mm": 18}]},
        {"id": "r", "blocks": [{"id": "h", "type": "heading", "text": "{company.company_name}", "level": 2},
            {"id": "a", "type": "field", "value": "company.acc_print_terms", "label": null},
            {"id": "b", "type": "field", "value": "company.acc_bank_name", "label": "Bank"},
            {"id": "n", "type": "field", "value": "company.acc_bank_account_no", "label": "Account"},
            {"id": "empty", "type": "field", "value": "company.tax_id", "label": "Tax ID"}]}]},
        {"id": "s2", "columns": [{"id": "c2", "blocks": [{"id": "hr", "type": "divider"}]}]}]}});
    let mut tops = Vec::new();
    for (name, lh) in [("short", short), ("full", full)] {
        RENDERED.lock().unwrap().clear();
        let ask = PreviewAsk { template: json!("shop.invoice"), record: Some(crid), letterhead_draft: Some(lh), pdf: true, ..Default::default() };
        let Preview::Pdf(pdf) = k.preview_print(&admin, &ask).await.unwrap() else { panic!("a PDF") };
        if let Ok(dir) = std::env::var("FERRUM_PRINT_EVIDENCE") {
            std::fs::write(Path::new(&dir).join(format!("letterhead-auto-{name}.pdf")), &pdf).unwrap();
        }
        let form = RENDERED.lock().unwrap().last().cloned().unwrap();
        tops.push((name, margin_top(&form) - 15.0));
    }
    println!("auto letter head heights (mm): {tops:?}");
    assert!(tops[0].1 > 5.0 && tops[0].1 < 13.0, "one heading line: {tops:?}");
    assert!(tops[1].1 > 26.0 && tops[1].1 < 40.0, "an 18 mm logo beside a heading and three lines, a rule below: {tops:?}");

    // ── the designer's canvas, the sample record, the catalogue ──
    let (canvas, _) =
        html_of(k.preview_print(&admin, &PreviewAsk { template: json!("shop.invoice"), sample: true, design: true, ..Default::default() }).await.unwrap());
    assert!(
        canvas.contains("pf-design")
            && canvas.contains("data-b=\"parties\"")
            && canvas.contains("data-slot=\"bill:0\"")
            && canvas.contains("data-slot=\"bill:3\"")
    );
    // What a condition hides on this record is a collapsed ghost (still selectable); `design: all` draws it dimmed.
    assert!(canvas.contains("<div class=\"pf-ghost\" data-b=\"title_cn\" data-ghost=\"heading\"") && !canvas.contains("Credit Note</h1>"), "{canvas}");
    assert!(canvas.contains("<th class=\"pf-ghost-col\" data-b=\"c_disc\" data-ghost=\"column\"") && !canvas.contains("Disc %"), "a hidden column: {canvas}");
    let (all, _) = html_of(
        k.preview_print(&admin, &PreviewAsk { template: json!("shop.invoice"), sample: true, design: true, design_all: true, ..Default::default() })
            .await
            .unwrap(),
    );
    assert!(all.contains("pf-hidden") && all.contains("Credit Note</h1>") && all.contains("Disc %") && !all.contains("pf-ghost\""), "everything at full size");
    let preview = k.preview_print(&admin, &PreviewAsk { template: json!("shop.invoice"), sample: true, ..Default::default() }).await.unwrap();
    let Preview::Json(answer) = preview else { panic!() };
    let printed_sample = answer["content"].as_str().unwrap().to_string();
    assert!(!printed_sample.contains("data-b=") && !printed_sample.contains("data-slot="), "no marks outside the canvas");
    // The sample reads as a document: a number, a customer, three lines that add up, the company's currency.
    for says in ["SAMPLE-0001", "Sample Customer", "Steel bracket", "Assembly service", "3,000.00", "₹", "Tax 18%"] {
        assert!(printed_sample.contains(says), "{says}: {printed_sample}");
    }
    assert!(!printed_sample.contains(">—<"), "no dashes");
    assert!(answer["latest_record"]["id"].is_string(), "the latest record is offered: {}", answer["latest_record"]);
    // On screen the page's margins are its padding (the PDF is told them instead).
    assert!(printed_sample.contains("@media screen{body.pf{padding:"), "{printed_sample}");
    // A template using |amount previews on the sample in the company's currency.
    let amt: DocumentDef =
        serde_json::from_value(json!({"key": "amt_t", "entity": "SalesInvoice", "languages": {"en": {"body": "<p>{{ doc.total|amount }}</p>"}}})).unwrap();
    let (a, _) =
        html_of(k.preview_print(&admin, &PreviewAsk { template: serde_json::to_value(&amt).unwrap(), sample: true, ..Default::default() }).await.unwrap());
    assert!(a.contains("<p>₹"), "{a}");
    // A template that fails as it runs says where, as a save does.
    let late: DocumentDef =
        serde_json::from_value(json!({"key": "late_t", "entity": "SalesInvoice", "languages": {"en": {"body": "a\n{{ doc.posting_date|date('%Q') }}"}}}))
            .unwrap();
    k.save_document_template(&admin, &late).await.unwrap();
    let e = k.print_record(&clerk, "SalesInvoice", fresh, &PrintAsk { template: Some("late_t".into()), ..Default::default() }).await.unwrap_err();
    assert!(fields_of(&e)[0].1.starts_with("en body, line 2: "), "{e:?}");
    // A sign left off on a condition.
    let cond = one_block(
        json!({"id": "g", "type": "field", "value": "doc.grand_total", "label": "T", "format": {"as": "amount", "sign": {"abs_when": "doc.grand_total < 0.0"}}}),
    );
    let (neg, _) = html_of(
        k.preview_print(
            &admin,
            &PreviewAsk { template: serde_json::to_value(layout_def("cond", cond.clone())).unwrap(), record: Some(crid), ..Default::default() },
        )
        .await
        .unwrap(),
    );
    assert!(neg.contains("<span class=\"pf-value\"><bdi dir=\"ltr\">₹3,186.00</bdi></span>"), "{neg}");
    let bad = one_block(json!({"id": "g", "type": "field", "value": "doc.grand_total", "format": {"sign": {"abs_when": "doc.(("}}}));
    assert!(
        fields_of(&k.save_document_template(&admin, &layout_def("bad_sign", bad)).await.unwrap_err())
            .iter()
            .any(|(f, m)| f == "layout.g" && m.contains("does not compile"))
    );
    // A letter head not saved yet, previewed as its form holds it; checked as a save checks it.
    let draft = json!({"header": {"sections": [{"id": "s", "columns": [{"id": "c", "blocks": [{"id": "h", "type": "heading", "text": "Draft head {company.company_name}"}]}]}]}});
    let ask = PreviewAsk { template: json!("shop.invoice"), sample: true, letterhead_draft: Some(draft), ..Default::default() };
    let (with_draft, _) = html_of(k.preview_print(&admin, &ask).await.unwrap());
    assert!(with_draft.contains("Draft head"), "{with_draft}");
    let bad = json!({"header": {"sections": [{"id": "s", "columns": [{"id": "c", "blocks": [{"id": "t", "type": "field", "value": "doc.number"}]}]}]}});
    let ask = PreviewAsk { template: json!("shop.invoice"), sample: true, letterhead_draft: Some(bad), ..Default::default() };
    assert!(matches!(k.preview_print(&admin, &ask).await, Err(Error::Validation(_))));
    let cat = k.document_fields(&admin, "SalesInvoice").unwrap();
    let every: Vec<&str> = ["fields", "system"].iter().flat_map(|k| cat[k].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap())).collect();
    assert_eq!(every.iter().filter(|p| **p == "doc.status").count(), 1, "status once");
    assert!(every.contains(&"doc.signed_qr"), "a field the form hides prints");
    let customer = cat["links"].as_array().unwrap().iter().find(|l| l["field"] == "customer").unwrap();
    assert!(customer["fields"].as_array().unwrap().iter().any(|f| f["path"] == "links.customer.number"), "a linked record's number");
    let paths: Vec<&str> = cat["fields"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert!(paths.contains(&"doc.customer") && !paths.contains(&"doc.pin"), "no secret in the catalogue");
    assert_eq!(cat["tables"][0]["columns"][0]["path"], json!("row.item_name"));
    assert!(cat["company"].as_array().unwrap().iter().any(|f| f["path"] == "company.acc_logo"));
    assert!(matches!(k.document_fields(&clerk, "SalesInvoice"), Err(Error::Forbidden(_))));
    let choices = k.print_choices(&clerk, "SalesInvoice", Some(fresh)).await.unwrap();
    assert_eq!(choices["letterheads"][0]["default_for_record"], json!(true));
    assert_eq!(choices["standard"]["page"], json!({"size": "A4", "orientation": "portrait"}), "the print format's paper: Print Settings'");
    assert_eq!(choices["templates"].as_array().unwrap().iter().find(|t| t["key"] == "shop.invoice").unwrap()["copies"], json!(["Original", "Duplicate"]));

    // ── T12: what a render costs: the worked example over a 50-line invoice ──
    let fifty = id_of(&k.insert(&admin, "SalesInvoice", invoice(50, "Fifty Lines Ltd")).await.unwrap());
    let mut times = Vec::new();
    for _ in 0..50 {
        let started = std::time::Instant::now();
        k.print_record(&clerk, "SalesInvoice", fifty, &PrintAsk { template: Some("my_invoice".into()), ..Default::default() }).await.unwrap();
        times.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    let mut design_times = Vec::new();
    for _ in 0..50 {
        let started = std::time::Instant::now();
        k.preview_print(&admin, &PreviewAsk { template: json!("my_invoice"), record: Some(fifty), design: true, ..Default::default() }).await.unwrap();
        design_times.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    for (what, t) in [("print", &mut times), ("design preview", &mut design_times)] {
        t.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("T12 {what}, 50-line invoice, 50 renders: p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms", t[24], t[47], t[49]);
    }

    // ── what a person looks at: the worked example, A4 and Letter, HTML and PDF ──
    if let Ok(dir) = std::env::var("FERRUM_PRINT_EVIDENCE") {
        let dir = Path::new(&dir);
        std::fs::create_dir_all(dir).unwrap();
        let ten = id_of(&k.insert(&admin, "SalesInvoice", invoice(12, "Bharat Traders")).await.unwrap());
        k.submit(&clerk, "SalesInvoice", ten, None).await.unwrap();
        for (size, name) in [("A4", "a4"), ("Letter", "letter")] {
            let mut d = copy.clone();
            d.page = Some(serde_json::from_value(json!({"size": size})).unwrap());
            k.save_document_template(&admin, &d).await.unwrap();
            let ask = PrintAsk { template: Some("my_invoice".into()), copies: vec!["Original".into()], ..Default::default() };
            std::fs::write(dir.join(format!("invoice-{name}.html")), &k.print_record(&clerk, "SalesInvoice", ten, &ask).await.unwrap().bytes).unwrap();
            let pdf = k.print_record(&clerk, "SalesInvoice", ten, &PrintAsk { pdf: true, ..ask }).await.unwrap();
            std::fs::write(dir.join(format!("invoice-{name}.pdf")), &pdf.bytes).unwrap();
            // The repeated letter head as the PDF service was sent it.
            let form = RENDERED.lock().unwrap().last().cloned().unwrap_or_default();
            for part in ["header", "footer"] {
                if let Some(html) =
                    form.split(&format!("filename=\"{part}.html\"")).nth(1).and_then(|r| r.split("\r\n\r\n").nth(1)).and_then(|r| r.split("\r\n--").next())
                {
                    std::fs::write(dir.join(format!("invoice-{name}-{part}.html")), html).unwrap();
                }
            }
        }
        // Page breaks, read back from the real PDF (pdftotext): only what must stay together moves. A 6-line invoice is
        // one A4 page, its totals and signature with its lines; a 12-line one breaks after its last line, the totals
        // block (about 42 mm) not fitting in what is left, and nothing of the lines moves with it.
        if std::process::Command::new("pdftotext").arg("-v").output().is_ok() {
            let mut a4 = copy.clone();
            a4.page = Some(serde_json::from_value(json!({"size": "A4"})).unwrap());
            k.save_document_template(&admin, &a4).await.unwrap();
            let pages = |pdf: &[u8]| -> Vec<String> {
                let file = dir.join("break-check.pdf");
                std::fs::write(&file, pdf).unwrap();
                let out = std::process::Command::new("pdftotext").arg(&file).arg("-").output().unwrap();
                let _ = std::fs::remove_file(&file);
                String::from_utf8_lossy(&out.stdout).split('\u{c}').map(str::to_string).filter(|p| !p.trim().is_empty()).collect()
            };
            let ask = PrintAsk { template: Some("my_invoice".into()), pdf: true, ..Default::default() };
            let six = id_of(&k.insert(&admin, "SalesInvoice", invoice(6, "Six Lines")).await.unwrap());
            let p = pages(&k.print_record(&clerk, "SalesInvoice", six, &ask).await.unwrap().bytes);
            assert_eq!(p.len(), 1, "six lines, totals, terms and signature on one page: {p:?}");
            let p = pages(&k.print_record(&clerk, "SalesInvoice", ten, &ask).await.unwrap().bytes);
            assert_eq!(p.len(), 2, "{p:?}");
            assert!(p[0].contains("Widget 12") && !p[0].contains("Subtotal"), "the lines fill page 1: {}", p[0]);
            assert!(p[1].contains("Subtotal") && p[1].contains("For Acme"), "totals and signature together on page 2: {}", p[1]);
        }
        // The draft, its watermark, two copies, A4.
        let draft = id_of(&k.insert(&admin, "SalesInvoice", invoice(3, "Draft Customer")).await.unwrap());
        let ask = PrintAsk { template: Some("my_invoice".into()), copies: vec!["Original".into(), "Duplicate".into()], pdf: true, ..Default::default() };
        std::fs::write(dir.join("invoice-draft-copies.pdf"), &k.print_record(&clerk, "SalesInvoice", draft, &ask).await.unwrap().bytes).unwrap();
        // Hindi and Arabic: the fonts the PDF service has.
        for lang in ["hi", "ar"] {
            let ask = PrintAsk { template: Some("my_invoice".into()), language: Some(lang.into()), pdf: true, ..Default::default() };
            std::fs::write(dir.join(format!("invoice-{lang}.pdf")), &k.print_record(&clerk, "SalesInvoice", ten, &ask).await.unwrap().bytes).unwrap();
        }
        let mut words = invoice(2, "भारत ट्रेडर्स / شركة التجارة");
        words.insert("terms".into(), json!("भुगतान 30 दिनों के भीतर। الدفع خلال 30 يومًا."));
        let wid = id_of(&k.insert(&admin, "SalesInvoice", words).await.unwrap());
        let ask = PrintAsk { template: Some("my_invoice".into()), language: Some("hi".into()), pdf: true, ..Default::default() };
        std::fs::write(dir.join("invoice-scripts.pdf"), &k.print_record(&clerk, "SalesInvoice", wid, &ask).await.unwrap().bytes).unwrap();
    }
    let _ = db;
}
