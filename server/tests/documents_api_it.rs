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
use ferrum_server::controllers::documents::{RenderQuery, preview_document, render_one};
use ferrum_server::error::ApiError;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

const PACKAGE: &str = "package: notes\nversion: 0.1.0\ndepends: [core]\nroles:\n  - name: Writer\n    permissions:\n      - {entity: Note, read: true, write: true, create: true}\n\
documents:\n  - key: notes.card\n    entity: Note\n    languages:\n      en: {body: '<p>{{ doc.title }}</p>'}\n  - key: notes.rows\n    entity: Note\n    output: csv\n    crlf: true\n    languages:\n      en: {body: \"{{ doc.title }}\\n\"}\n";
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
