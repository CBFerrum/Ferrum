//! Consent by purpose (Ferrum's closure review, FEO-015), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first): a person withdraws their consent to
//! marketing while marketing to them waits in the outbox, from an automation, from code and from a
//! process's capability; none of it goes, and the transactional notices waiting beside it do. Asked
//! again when mail goes out: a restriction placed after a notice was queued stops that notice too.
//! Consent given again lets marketing through.

use ferrum_kernel::capability::Invocation;
use ferrum_kernel::retention::Purpose;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings, automation};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

const PACKAGE: &str = "package: leads\nversion: 0.1.0\ndepends: [core]\nroles:\n  - name: Seller\n    permissions:\n      - {entity: Lead, read: true, write: true, create: true}\n";
const ENTITIES: &str =
    "name: Lead\nlabel: Lead\nkind: master\ntitle_field: title\nfields:\n  - {name: title, type: data, required: true}\n  - {name: email, type: email}\n";

/// What the server's mailer was asked to send: (to, subject).
static SENT: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

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

fn sent(to: &str, subject: &str) -> usize {
    SENT.lock().unwrap().iter().filter(|(t, s)| t == to && s == subject).count()
}

#[test]
fn marketing_stops_where_consent_ends() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/lead.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let leads = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, leads]).unwrap(), HookRegistry::default(), KernelSettings::default(), Some(Arc::new(Captured)))
        .await
        .expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "shop".into(),
            name: "Shop".into(),
            admin_email: "admin@shop.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["leads".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("shop", "admin@shop.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let (pat, bo) = ("pat@example.com", "bo@example.com");

    // An automation sends each new lead an offer (marketing) and an acknowledgement (transactional).
    let email =
        |subject: &str, purpose: &str| json!({"type": "email", "to": "doc.email", "subject": format!("'{subject}'"), "body": "doc.title", "purpose": purpose});
    automation::save(&k, &admin, &obj(json!({"name": "welcome", "entity": "Lead", "event": "created", "actions": [email("Spring offer", "marketing"), email("We have your enquiry", "transactional")]})))
        .await
        .unwrap();
    k.insert(&admin, "Lead", obj(json!({"title": "Pat", "email": pat}))).await.unwrap();
    k.insert(&admin, "Lead", obj(json!({"title": "Bo", "email": bo}))).await.unwrap();
    // Marketing and a notice to Pat wait in the outbox, queued from code, before anything has gone.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    k.queue_mail_for(&mut tx, tenant, pat, "Autumn offer", "<p>Sale</p>", &[], Purpose::Marketing).await.unwrap();
    k.queue_mail_for(&mut tx, tenant, pat, "Your invoice", "<p>Invoice</p>", &[], Purpose::Transactional).await.unwrap();
    tx.commit().await.unwrap();
    // Pat withdraws their consent to marketing while all of it waits.
    k.record_consent(&admin, pat, Purpose::Marketing, false, "Unsubscribed from the spring campaign").await.expect("withdrawn");
    drain(&k).await;
    assert_eq!((sent(pat, "Autumn offer"), sent(pat, "Your invoice")), (0, 1), "queued before the withdrawal, asked as it went");
    assert_eq!((sent(pat, "Spring offer"), sent(pat, "We have your enquiry")), (0, 1), "the automation's offer stops; its acknowledgement goes");
    assert_eq!(sent(bo, "Spring offer") + sent(bo, "We have your enquiry"), 2, "Bo gets both");

    // Queued after the withdrawal: marketing is not even queued; a process's capability alike.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    k.queue_mail_for(&mut tx, tenant, pat, "Winter offer", "<p>Sale</p>", &[], Purpose::Marketing).await.unwrap();
    let queued: i64 = sqlx::query_scalar("select count(*) from outbox where topic = 'ferrum.mail' and payload->>'subject' = 'Winter offer'")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(queued, 0);
    let marketing = |subject: &str, key: &str| Invocation::new("ferrum.mail.marketing", json!({"to": pat, "subject": subject, "body": "Sale"})).key(key);
    let notice = |subject: &str, key: &str| Invocation::new("ferrum.mail", json!({"to": pat, "subject": subject, "body": "Hello"})).key(key);
    k.invoke(&admin, marketing("Process offer", "mk-1")).await.expect("the capability answers");
    k.invoke(&admin, notice("Process notice", "tx-1")).await.expect("the capability answers");
    drain(&k).await;
    assert_eq!((sent(pat, "Process offer"), sent(pat, "Process notice")), (0, 1), "a process's marketing respects it too");
    assert!(k.catalog(&admin).iter().any(|c| c.id == "ferrum.mail.marketing"), "listed beside ferrum.mail");

    // Their own word again: marketing goes.
    k.record_consent(&admin, pat, Purpose::Marketing, true, "Subscribed again from the newsletter form").await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    k.queue_mail_for(&mut tx, tenant, pat, "Welcome back", "<p>Hi</p>", &[], Purpose::Marketing).await.unwrap();
    tx.commit().await.unwrap();
    drain(&k).await;
    assert_eq!(sent(pat, "Welcome back"), 1);

    // A restriction placed after a notice was queued stops the notice as it goes.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    k.queue_mail_for(&mut tx, tenant, bo, "Your receipt", "<p>Receipt</p>", &[], Purpose::Transactional).await.unwrap();
    tx.commit().await.unwrap();
    k.restrict_processing(&admin, bo, "Disputes how their data is used").await.expect("restricted");
    drain(&k).await;
    assert_eq!(sent(bo, "Your receipt"), 0, "asked as it went");

    // Only marketing takes consent; the record says where it was given.
    assert!(k.record_consent(&admin, pat, Purpose::Transactional, false, "x").await.is_err());
    assert!(k.record_consent(&admin, pat, Purpose::Marketing, false, " ").await.is_err());
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let (granted, source): (bool, String) = sqlx::query_as("select granted, source from contact_consent where tenant_id = $1 and email = $2")
        .bind(tenant)
        .bind(pat)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!((granted, source.as_str()), (true, "Subscribed again from the newsletter form"));
}
