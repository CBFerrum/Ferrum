//! The messages API as an authoring screen uses it, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, reset
//! first): previewing a template by key or unsaved (administrators only; a render error a 422 on `template`), saving
//! and reading it back, a channel listed without its secret, the log of a record for whoever reads it, and a pack
//! carrying the templates and the channel's description but never its secret.

mod common;

use axum::response::IntoResponse;
use ferrum_kernel::messages::ChannelInput;
use ferrum_kernel::pack::ExportOptions;
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, MessageDef};
use ferrum_server::controllers::messages::preview_message;
use ferrum_server::error::ApiError;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

const PACKAGE: &str = "package: notes\nversion: 0.1.0\ndepends: [core]\nroles:\n  - name: Writer\n    permissions:\n      - {entity: Note, read: true, write: true, create: true}\n\
messages:\n  - key: notes.hello\n    entity: Note\n    languages:\n      en:\n        email: {subject: 'Hello {{ doc.title }}', body: '<p>{{ doc.title }}</p>'}\n        inapp: {title: 'Hello {{ doc.title }}'}\n";
const ENTITIES: &str = "name: Note\nlabel: Note\nkind: master\ntitle_field: title\nfields:\n  - {name: title, type: data, required: true}\n";

async fn answer(e: ApiError) -> (u16, Value) {
    let r = e.into_response();
    let status = r.status().as_u16();
    (status, serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap())
}

async fn workspace(k: &Kernel, slug: &str) -> Ctx {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["notes".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap()
}

#[test]
fn an_editor_previews_saves_and_packs_message_templates() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    common::reset(&db.pool).await;
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages/core")).expect("core");
    let files: BTreeMap<String, String> =
        [("package.yaml", PACKAGE), ("entities/note.yaml", ENTITIES)].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let notes = ferrum_meta::catalog::load_package_files(&files).expect("package");
    let settings = KernelSettings { secret_key: Some(SecretKey::new([7u8; 32])), ..Default::default() };
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, notes]).unwrap(), HookRegistry::default(), settings, None).await.expect("boot");
    let admin = workspace(&k, "acme").await;
    let writer = k.create_user(&admin, "w@acme.test", "w", None, &["Writer".to_string()], false).await.unwrap();
    let writer = k.load_ctx(admin.tenant, writer, None, "it".into()).await.unwrap();
    let note = k.insert(&admin, "Note", json!({"title": "<b>Ann</b>"}).as_object().cloned().unwrap()).await.unwrap();
    let id = note["id"].as_str().unwrap().to_string();

    // By key: the subject as text, the body's values escaped.
    let said = preview_message(&k, &admin, &json!({"template": "notes.hello", "channel": "email", "record_id": id})).await.unwrap();
    assert_eq!(said["message"], json!({"subject": "Hello <b>Ann</b>", "html": "<p>&lt;b&gt;Ann&lt;&#x2f;b&gt;</p>"}), "{said}");
    // Unsaved, with an unknown field: a 422 on `template`.
    let mut def: MessageDef = serde_json::from_value(k.message_template(&admin, "notes.hello").await.unwrap()["template"].clone()).unwrap();
    def.languages.get_mut("en").unwrap().inapp.as_mut().unwrap().title = "{{ doc.nope }}".into();
    let (status, body) = answer(preview_message(&k, &admin, &json!({"template": def, "channel": "inapp"})).await.unwrap_err()).await;
    assert_eq!((status, body["fields"][0]["field"].as_str()), (422, Some("template")), "{body}");
    // Not an administrator: refused.
    let (status, _) = answer(preview_message(&k, &writer, &json!({"template": "notes.hello", "channel": "email"})).await.unwrap_err()).await;
    assert_eq!(status, 403);

    // Saved and read back; a channel listed without its secret.
    def.languages.get_mut("en").unwrap().inapp.as_mut().unwrap().title = "Hi {{ doc.title }}".into();
    k.save_message_template(&admin, &def).await.unwrap();
    assert_eq!(k.message_template(&admin, "notes.hello").await.unwrap()["template"]["languages"]["en"]["inapp"]["title"], "Hi {{ doc.title }}");
    let input: ChannelInput = serde_json::from_value(json!({"kind": "text", "name": "Gateway", "secrets": {"key": "s3cret"},
        "description": {"url": "https://sms.example.test/send", "form": {"to": "{{ to }}", "text": "{{ text }}", "key": "{{ secret.key }}"}, "id_at": "/id"}}))
    .unwrap();
    k.save_message_channel(&admin, None, &input).await.unwrap();
    assert!(!serde_json::to_string(&k.message_channels(&admin).await.unwrap()).unwrap().contains("s3cret"));

    // The log of a record: for whoever reads it.
    let sent = k.send_test_message(&admin, "notes.hello", "inapp", None, Some(id.parse().unwrap())).await.unwrap();
    assert_eq!(sent[0].state, "delivered");
    let log = k.message_log(&writer, Some(("Note", id.parse().unwrap())), None, 10).await.unwrap();
    assert_eq!(log.len(), 1);
    assert!(k.message_log(&writer, None, Some("admin@acme.test"), 10).await.is_err(), "by recipient: administrators only");

    // A pack carries the edited template and the channel's description, never its secret; installed elsewhere, the
    // channel arrives turned off.
    let pack = k.export_pack(&admin, &ExportOptions { name: "msgs".into(), label: "Messages".into(), version: 1, records: vec![] }).await.unwrap();
    assert_eq!(pack["messages"][0]["key"], "notes.hello");
    assert_eq!(pack["channels"][0]["name"], "Gateway");
    assert!(!pack.to_string().contains("s3cret"));
    let other = workspace(&k, "globex").await;
    let plan = k.plan_pack(&other, &pack).await.unwrap();
    k.install_pack(&other, &pack, plan["digest"].as_str().unwrap()).await.unwrap();
    assert_eq!(k.message_template(&other, "notes.hello").await.unwrap()["edited"], json!(true));
    assert_eq!(k.message_channels(&other).await.unwrap()[0]["enabled"], json!(false));
}
