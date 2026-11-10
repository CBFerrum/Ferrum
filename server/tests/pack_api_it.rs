//! The packs API as an administrator uses it (docs/api.md, "Packs"), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`, reset first): export, plan and install, and the refusals the API answers with
//! their status: not an administrator, a body or pack with a key it does not know, a digest not the plan's.

mod common;

use axum::response::IntoResponse;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, OverlayOp};
use ferrum_server::controllers::admin::{PackQuery, export_pack, install_pack, plan_pack};
use ferrum_server::error::ApiError;
use serde_json::{Value, json};
use std::path::Path;

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
            packages: vec!["core".into()],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap()
}

#[test]
fn an_administrator_exports_plans_and_installs_a_pack() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    common::reset(&db.pool).await;
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages/core")).expect("core");
    let k = Kernel::boot(db, Catalog::from_packages(vec![core]).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let admin = workspace(&k, "acme").await;
    let op: OverlayOp =
        serde_json::from_value(json!({"op": "define_entity", "entity": {"name": "XRoom", "label": "Room", "kind": "master", "title_field": "name",
        "fields": [{"name": "name", "type": "data", "unique": true}, {"name": "seats", "type": "int"}]}}))
        .unwrap();
    k.add_customization(&admin, "XRoom", op, None).await.unwrap();
    let admin = k.load_ctx(admin.tenant, admin.user, None, "it".into()).await.unwrap();
    k.insert(&admin, "XRoom", json!({"name": "Hall", "seats": 40}).as_object().cloned().unwrap()).await.unwrap();
    let member = k.create_user(&admin, "m@acme.test", "m", None, &[], false).await.unwrap();
    let member = k.load_ctx(admin.tenant, member, None, "it".into()).await.unwrap();

    // Export: the query names the records, the pack and its version.
    let q = PackQuery { records: Some("XRoom".into()), name: Some("rooms".into()), label: Some("Rooms".into()), version: Some(3) };
    let pack = export_pack(&k, &admin, &q).await.expect("export");
    assert_eq!((&pack["name"], &pack["version"], &pack["records"][0]["rows"][0]["name"]), (&json!("rooms"), &json!(3), &json!("Hall")));
    assert_eq!(answer(export_pack(&k, &member, &q).await.unwrap_err()).await.0, 403, "administrators only");
    let bad = PackQuery { name: Some("Rooms!".into()), ..PackQuery { records: None, name: None, label: None, version: None } };
    let (status, body) = answer(export_pack(&k, &admin, &bad).await.unwrap_err()).await;
    assert_eq!((status, body["fields"][0]["field"].as_str()), (422, Some("name")), "{body}");

    // Plan in another workspace, then install what it showed.
    let other = workspace(&k, "globex").await;
    let plan = plan_pack(&k, &other, &json!({"pack": pack})).await.expect("plan");
    assert_eq!(plan["sections"]["records"]["created"], json!(["XRoom Hall"]), "{plan}");
    let digest = plan["digest"].as_str().unwrap();
    assert_eq!(answer(plan_pack(&k, &member, &json!({"pack": pack})).await.unwrap_err()).await.0, 403);
    let (status, body) = answer(plan_pack(&k, &other, &json!({"pack": pack, "dry_run": true})).await.unwrap_err()).await;
    assert_eq!((status, body["fields"][0]["field"].as_str()), (422, Some("dry_run")), "{body}");
    let mut odd = pack.clone();
    odd["sql"] = json!("drop table x");
    let (status, body) = answer(plan_pack(&k, &other, &json!({"pack": odd})).await.unwrap_err()).await;
    assert_eq!(status, 422);
    assert!(body["fields"][0]["message"].as_str().unwrap().contains("unknown field `sql`"), "{body}");
    let (status, body) = answer(install_pack(&k, &other, &json!({"pack": pack})).await.unwrap_err()).await;
    assert_eq!((status, body["fields"][0]["field"].as_str()), (422, Some("digest")), "{body}");
    let (status, body) = answer(install_pack(&k, &other, &json!({"pack": pack, "digest": "sha256:0"})).await.unwrap_err()).await;
    assert_eq!((status, body["fields"][0]["field"].as_str()), (422, Some("digest")), "{body}");
    assert_eq!(answer(install_pack(&k, &member, &json!({"pack": pack, "digest": digest})).await.unwrap_err()).await.0, 403);
    let done = install_pack(&k, &other, &json!({"pack": pack, "digest": digest})).await.expect("install");
    assert_eq!(done["summary"]["customizations"]["created"], json!(["XRoom: define_entity"]));
    let other = k.load_ctx(other.tenant, other.user, None, "it".into()).await.unwrap();
    let rooms = k.list(&other, "XRoom", &Default::default()).await.unwrap().rows;
    assert_eq!((rooms.len(), &rooms[0]["seats"]), (1, &json!(40)));
    let again = install_pack(&k, &other, &json!({"pack": pack, "digest": digest})).await.unwrap();
    assert_eq!(again["summary"]["records"]["unchanged"], json!(["XRoom Hall"]));
    let listed = k.installed_packs(&other).await.unwrap();
    assert_eq!(listed.iter().map(|p| (p["name"].clone(), p["version"].clone())).collect::<Vec<_>>(), [(json!("rooms"), json!(3)), (json!("rooms"), json!(3))]);
}
