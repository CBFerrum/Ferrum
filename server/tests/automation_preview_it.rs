//! The automations API as an automation editor uses it, against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`, reset first): previewing a mail (administrators only; a record of another
//! workspace is not found) and a refused save naming the action it is about, as the API answers them.

mod common;

use axum::response::IntoResponse;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use ferrum_server::controllers::admin::preview_automation_mail;
use ferrum_server::error::ApiError;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

const PACKAGE: &str = "package: notes\nversion: 0.1.0\ndepends: [core]\nroles:\n  - name: Writer\n    permissions:\n      - {entity: Note, read: true, write: true, create: true}\n";
const ENTITIES: &str =
    "name: Note\nlabel: Note\nkind: master\ntitle_field: title\nfields:\n  - {name: title, type: data, required: true}\n  - {name: body, type: text}\n";

/// What the API answers with an error: its status and JSON.
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
fn an_editor_previews_a_mail_and_sees_which_action_is_wrong() {
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
    let other = workspace(&k, "globex").await;
    let writer = k.create_user(&admin, "w@acme.test", "w", None, &["Writer".to_string()], false).await.unwrap();
    let writer = k.load_ctx(admin.tenant, writer, None, "it".into()).await.unwrap();
    let note = |title: &str| json!({"title": title, "body": "<script>x</script>\nnext"}).as_object().cloned().unwrap();
    let mine = k.insert(&admin, "Note", note("Mine")).await.unwrap()["id"].as_str().unwrap().to_string();
    let theirs = k.insert(&other, "Note", note("Theirs")).await.unwrap()["id"].as_str().unwrap().to_string();

    // An administrator: the HTML and the record it was made from.
    let said = preview_automation_mail(&k, &admin, &json!({"entity": "Note", "body": "doc.body", "record_id": mine})).await.unwrap();
    assert_eq!(said, json!({"html": "&lt;script&gt;x&lt;/script&gt;<br>next", "record_id": mine}));
    let latest = preview_automation_mail(&k, &admin, &json!({"entity": "Note", "template": "<p>{{ doc.title }}</p>"})).await.unwrap();
    assert_eq!(latest, json!({"html": "<p>Mine</p>", "record_id": mine}), "the latest record when none is named");

    // Not an administrator: refused, even with a role that reads notes.
    let (status, _) = answer(preview_automation_mail(&k, &writer, &json!({"entity": "Note", "body": "doc.title"})).await.unwrap_err()).await;
    assert_eq!(status, 403);

    // Another workspace's record, or none at all: not found.
    for id in [theirs, uuid::Uuid::now_v7().to_string()] {
        let (status, _) =
            answer(preview_automation_mail(&k, &admin, &json!({"entity": "Note", "body": "doc.title", "record_id": id})).await.unwrap_err()).await;
        assert_eq!(status, 404, "{id}");
    }

    // A template that does not compile: 422, on `template`.
    let (status, body) = answer(preview_automation_mail(&k, &admin, &json!({"entity": "Note", "template": "{{ doc.title"})).await.unwrap_err()).await;
    assert_eq!((status, body["fields"][0]["field"].as_str()), (422, Some("template")), "{body}");

    // A refused save: the message on `actions`, in the row of the second action.
    let input = json!({"name": "n", "entity": "Note", "event": "created", "actions": [
        {"type": "notify", "to": "owner", "title": "'hi'"},
        {"type": "email", "to": "'a@b.c'", "subject": "'s'", "body": "doc.title +"}]});
    let refused = ferrum_kernel::automation::save(&k, &admin, input.as_object().unwrap()).await.unwrap_err();
    let (status, body) = answer(ApiError(refused)).await;
    assert_eq!(status, 422);
    assert_eq!(body["fields"][0]["field"], "actions");
    assert_eq!(body["fields"][0]["row"], json!(["actions", 1]), "{body}");
    assert!(body["fields"][0]["message"].as_str().unwrap().starts_with("expression 'doc.title +'"), "{body}");
}
