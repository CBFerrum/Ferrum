//! The documents API as an authoring screen and a person printing use it, against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`, reset first): previewing a template by key or unsaved (administrators only; a render
//! error a 422 on `template`), rendering a page and a file with the headers each is served with, a format there is
//! not refused, and a pack carrying the workspace's templates to another workspace.

mod common;

use axum::response::IntoResponse;
use ferrum_kernel::pack::ExportOptions;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, DocumentDef};
use ferrum_server::controllers::documents::{RenderQuery, preview_document, render_one, run_query};
use ferrum_server::error::ApiError;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Stored files, in memory.
#[derive(Default)]
struct Store(Mutex<BTreeMap<String, Vec<u8>>>);

#[async_trait::async_trait]
impl ferrum_kernel::files::FileStore for Store {
    async fn put(&self, key: &str, data: &[u8]) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().insert(key.into(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> ferrum_kernel::Result<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned().ok_or_else(|| ferrum_kernel::Error::NotFound(key.into()))
    }
    async fn delete(&self, key: &str) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

const PACKAGE: &str = "package: notes\nversion: 0.1.0\ndepends: [core]\nroles:\n  - name: Writer\n    permissions:\n      - {entity: Note, read: true, write: true, create: true}\n\
documents:\n  - key: notes.card\n    entity: Note\n    languages:\n      en: {body: '<p>{{ doc.title }}</p>'}\n  - key: notes.rows\n    entity: Note\n    output: csv\n    crlf: true\n    languages:\n      en: {body: \"{{ doc.title }}\\n\"}\n  - key: notes.note\n    entity: Note\n    output: text\n    languages:\n      en: {body: 'x'}\n  - key: notes.feed\n    entity: Note\n    output: xml\n    languages:\n      en: {body: '<n/>'}\n";
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
fn an_editor_previews_saves_and_packs_document_templates() {
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
    let k = Kernel::boot(db, Catalog::from_packages(vec![core, notes]).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let admin = workspace(&k, "acme").await;
    let writer = k.create_user(&admin, "w@acme.test", "w", None, &["Writer".to_string()], false).await.unwrap();
    let writer = k.load_ctx(admin.tenant, writer, None, "it".into()).await.unwrap();
    let note = k.insert(&admin, "Note", json!({"title": "<b>Ann</b>"}).as_object().cloned().unwrap()).await.unwrap();
    let id = note["id"].as_str().unwrap().to_string();

    // Preview by key: the record's values escaped on a page.
    let said = preview_document(&k, &admin, &json!({"template": "notes.card", "record_id": id})).await.unwrap();
    assert_eq!(said["content"], "<p>&lt;b&gt;Ann&lt;&#x2f;b&gt;</p>", "{said}");
    // Unsaved, with an unknown field: a 422 on `template`.
    let mut def: DocumentDef = serde_json::from_value(k.document_template(&admin, "notes.card").await.unwrap()["template"].clone()).unwrap();
    def.languages.get_mut("en").unwrap().body = "{{ doc.nope }}".into();
    let (status, body) = answer(preview_document(&k, &admin, &json!({"template": def})).await.unwrap_err()).await;
    assert_eq!((status, body["fields"][0]["field"].as_str()), (422, Some("template")), "{body}");
    let (status, _) = answer(preview_document(&k, &writer, &json!({"template": "notes.card"})).await.unwrap_err()).await;
    assert_eq!(status, 403, "templates are an administrator's");

    // A page is served as HTML that runs nothing; a file as an attachment with its charset; a file of data needs export.
    let id = id.parse().unwrap();
    let page = render_one(&k, &writer, "notes.card", id, &RenderQuery::default()).await.unwrap();
    assert_eq!(page.headers()["content-type"], "text/html; charset=utf-8");
    assert!(page.headers()["content-security-policy"].to_str().unwrap().starts_with("default-src 'none'"));
    let (status, _) = answer(render_one(&k, &writer, "notes.rows", id, &RenderQuery::default()).await.unwrap_err()).await;
    assert_eq!(status, 403, "the writer may not export");
    let file = render_one(&k, &admin, "notes.rows", id, &RenderQuery::default()).await.unwrap();
    assert_eq!(file.headers()["content-type"], "text/csv; charset=utf-8");
    assert_eq!(file.headers()["content-disposition"], "attachment; filename=\"rows _b_Ann__b_.csv\"");
    assert_eq!(axum::body::to_bytes(file.into_body(), 1 << 20).await.unwrap().as_ref(), b"<b>Ann</b>\r\n");
    let (status, _) =
        answer(render_one(&k, &admin, "notes.card", id, &RenderQuery { format: Some("doc".into()), ..Default::default() }).await.unwrap_err()).await;
    assert_eq!(status, 422);
    // A record's template is no query's; a query's needs the analytics engine (not on this test's server).
    let (status, body) = answer(run_query(&k, &admin, "notes.card", &Value::Null).await.unwrap_err()).await;
    assert_eq!(status, 422, "{body}");
    assert!(body.to_string().contains("is about one record, not a query"), "{body}");
    let query: DocumentDef = serde_json::from_value(json!({"key": "x.rows", "entity": "Note", "output": "csv", "query": {"report": "Notes"},
        "languages": {"en": {"body": "x"}}}))
    .unwrap();
    let (status, body) = answer(k.save_document_template(&admin, &query).await.map_err(ApiError).unwrap_err()).await;
    assert_eq!(status, 422, "{body}");
    assert!(body.to_string().contains("need the analytics engine"), "{body}");

    // A package's entity has no definition to replace: Customize gets the approvals its package declares.
    let definition = ferrum_server::controllers::admin::definition_of(&k, &admin, "Note").unwrap();
    assert_eq!(definition, json!({"entity": "Note", "package": "notes", "approvals": []}));
    assert_eq!(answer(ferrum_server::controllers::admin::definition_of(&k, &writer, "Note").unwrap_err()).await.0, 403);

    // Generated files keep their type, and are served as what they are: never sniffed, never run on the app's origin.
    k.set_file_store(Arc::new(Store::default()));
    let mut kept = Vec::new();
    for (template, ctype) in [
        ("notes.card", "text/html; charset=utf-8"),
        ("notes.rows", "text/csv; charset=utf-8"),
        ("notes.note", "text/plain; charset=utf-8"),
        ("notes.feed", "application/xml; charset=utf-8"),
    ] {
        let g = ferrum_kernel::documents::Generate { template: template.into(), record: id, ..Default::default() };
        let made = k.generate_document(&admin, &g).await.unwrap();
        let (_, stored, _) = k.read_file(&admin, made.file).await.unwrap();
        assert_eq!((made.content_type.as_str(), stored.as_str()), (ctype, ctype), "{template}");
        kept.push((made.name, stored));
    }
    let served = |ctype: &str, name: &str, inline: bool| {
        let mut h = axum::http::HeaderMap::new();
        ferrum_server::controllers::system::file_headers(&mut h, ctype, name, inline);
        let get = |n: &str| h.get(n).map(|v| v.to_str().unwrap().to_string());
        (get("content-type"), get("content-disposition"), get("x-content-type-options"), get("content-security-policy"))
    };
    let (ctype, how, nosniff, csp) = served(&kept[0].1, &kept[0].0, true);
    assert_eq!(ctype.as_deref(), Some("text/html; charset=utf-8"));
    assert!(how.is_some_and(|h| h.starts_with("attachment;")), "HTML is never inline, even where files are");
    assert_eq!(nosniff.as_deref(), Some("nosniff"));
    assert!(csp.is_some_and(|c| c.contains("sandbox")), "and runs nothing if opened");
    let (_, how, _, csp) = served(&kept[3].1, &kept[3].0, true);
    assert!(how.is_some_and(|h| h.starts_with("attachment;")) && csp.is_some(), "nor is XML");
    let (ctype, how, _, csp) = served("text/csv; charset=utf-8", "a.csv", false);
    assert_eq!((ctype.as_deref(), how.as_deref(), csp), (Some("text/csv; charset=utf-8"), Some("attachment; filename=\"a.csv\""), None));
    let (ctype, how, ..) = served("image/png", "a.png", true);
    assert_eq!((ctype.as_deref(), how.as_deref()), (Some("image/png"), Some("inline; filename=\"a.png\"")), "an image stays inline where it was");
    let (ctype, ..) = served("", "old.html", false);
    assert_eq!(ctype.as_deref(), Some("application/octet-stream"), "a file kept without a type is bytes, as before");

    // A pack carries the edited template; installed elsewhere it is that workspace's edit.
    def.languages.get_mut("en").unwrap().body = "<h1>{{ doc.title }}</h1>".into();
    k.save_document_template(&admin, &def).await.unwrap();
    let pack = k.export_pack(&admin, &ExportOptions { name: "docs".into(), label: "Documents".into(), version: 1, records: vec![] }).await.unwrap();
    assert_eq!(pack["documents"][0]["key"], "notes.card");
    let other = workspace(&k, "globex").await;
    let plan = k.plan_pack(&other, &pack).await.unwrap();
    k.install_pack(&other, &pack, plan["digest"].as_str().unwrap()).await.unwrap();
    let there = k.document_template(&other, "notes.card").await.unwrap();
    assert_eq!((there["edited"].clone(), there["template"]["languages"]["en"]["body"].clone()), (json!(true), json!("<h1>{{ doc.title }}</h1>")));
}
