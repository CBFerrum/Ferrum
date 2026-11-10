//! Published lists (BD-15) as a workspace keeps them, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`;
//! the database is reset first): refused when saved with the reason, a filter whose value does not fit
//! refused too; the same name again supersedes the earlier one in the customization list; administrators
//! see a list on its entity's description and members do not; a change set and a pack carry one, and
//! a pack's plan says in plain words what it publishes.

use ferrum_kernel::pack::ExportOptions;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, OverlayOp};
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

fn op(v: Value) -> OverlayOp {
    serde_json::from_value(v).unwrap()
}

fn entities() -> Vec<(&'static str, OverlayOp)> {
    vec![
        (
            "XCategory",
            op(json!({"op": "define_entity", "entity": {"name": "XCategory", "label": "Category", "label_plural": "Categories", "kind": "master",
                "title_field": "category_name", "fields": [{"name": "category_name", "type": "data"}]}})),
        ),
        (
            "XPermit",
            op(
                json!({"op": "define_entity", "entity": {"name": "XPermit", "label": "Permit", "kind": "document", "submittable": true, "title_field": "holder",
                "fields": [{"name": "holder", "type": "data"}, {"name": "kind", "type": "data", "label": "Type"}, {"name": "issued_on", "type": "date"},
                           {"name": "fee", "type": "currency"}, {"name": "contact_email", "type": "email"}, {"name": "category", "type": "link", "to": "XCategory"}]}}),
            ),
        ),
    ]
}

fn permits(extra: Value) -> OverlayOp {
    let mut l = json!({"name": "permits", "label": "Permits", "fields": ["holder", "kind", "issued_on"], "csv": true});
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        l[k] = v;
    }
    op(json!({"op": "publish_list", "list": l}))
}

async fn workspace(k: &Kernel, slug: &str) -> (Ctx, Ctx) {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();
    let member = k.create_user(&admin, &format!("member@{slug}.test"), "Member", None, &[], false).await.unwrap();
    let member = k.load_ctx(tenant, member, None, "it".into()).await.unwrap();
    (admin, member)
}

async fn fresh(k: &Kernel, ctx: &Ctx) -> Ctx {
    k.load_ctx(ctx.tenant, ctx.user, None, "it".into()).await.unwrap()
}

#[test]
fn published_lists_are_kept_like_any_customization() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 8, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let k = Kernel::boot(db, Catalog::from_packages(vec![core]).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let (admin, member) = workspace(&k, "acme").await;
    for (e, o) in entities() {
        k.add_customization(&admin, e, o, None).await.unwrap();
    }

    // Refused when saved, with why.
    for (list, says) in [
        (permits(json!({"fields": ["holder", "contact_email"]})), "contact_email holds an email address"),
        (permits(json!({"fields": ["holder", "id"]})), "id is not a field of XPermit"),
        (permits(json!({"name": "Permits!"})), "lowercase letters, digits and hyphens"),
        (permits(json!({"filter": [["fee", ">", "a lot"]]})), "published list permits"),
        (permits(json!({"filter": [["holder", "~", "x"]]})), "unknown filter operator"),
    ] {
        let e = k.add_customization(&fresh(&k, &admin).await, "XPermit", list, None).await.unwrap_err().to_string();
        assert!(e.contains(says), "{says}: {e}");
    }
    assert!(k.add_customization(&fresh(&k, &member).await, "XPermit", permits(json!({})), None).await.is_err(), "administrators only");

    // Saved, then replaced: the earlier one is superseded by the later.
    let first = k.add_customization(&fresh(&k, &admin).await, "XPermit", permits(json!({})), None).await.unwrap();
    let admin = fresh(&k, &admin).await;
    let second = k.add_customization(&admin, "XPermit", permits(json!({"label": "Issued permits", "fields": ["holder", "category"]})), None).await.unwrap();
    let rows = k.list_customizations(&admin).await.unwrap();
    let row = |id: Uuid| rows.iter().find(|r| r.id == id).unwrap();
    assert_eq!((row(first).superseded_by, row(second).superseded_by), (Some(second), None));
    // A name is one of a kind in the workspace.
    let admin = fresh(&k, &admin).await;
    let e = k.add_customization(&admin, "XCategory", permits(json!({"fields": ["category_name"]})), None).await.unwrap_err().to_string();
    assert!(e.contains("the name is taken by a list of XPermit"), "{e}");

    // Its administrators see what it publishes on the entity's description; members see nothing of it.
    let admin = fresh(&k, &admin).await;
    let ent = admin.schema.entity("XPermit").unwrap();
    assert_eq!(
        ferrum_kernel::describe::describe(&admin, ent)["published_lists"],
        json!([{"name": "permits", "label": "Issued permits", "fields": ["holder", "category"], "csv": true}])
    );
    let member = fresh(&k, &member).await;
    assert!(ferrum_kernel::describe::describe(&member, member.schema.entity("XPermit").unwrap()).get("published_lists").is_none());

    // A change set carries it.
    let bundle = k.export_customizations(&admin).await.unwrap();
    let (other, _) = workspace(&k, "other").await;
    k.import_customizations(&other, &bundle).await.unwrap();
    let other = fresh(&k, &other).await;
    assert_eq!(other.schema.published_list("permits").map(|l| l.list.label.as_str()), Some("Issued permits"));

    // So does a pack, and its plan says what it publishes, to whom, in plain words.
    let pack = k.export_pack(&admin, &ExportOptions { name: "permits".into(), label: "Permits".into(), version: 1, records: vec![] }).await.unwrap();
    assert!(pack["customizations"].as_array().unwrap().iter().any(|c| c["op"]["op"] == json!("publish_list")), "{pack}");
    let (bare, _) = workspace(&k, "bare").await;
    let plan = k.plan_pack(&bare, &pack).await.unwrap();
    let warnings: Vec<&str> = plan["warnings"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(warnings.contains(&"This pack publishes a list of Permits to anyone, without signing in (permits): Holder, Category."), "{plan}");
    let digest = plan["digest"].as_str().unwrap().to_string();
    k.install_pack(&bare, &pack, &digest).await.unwrap();
    assert!(fresh(&k, &bare).await.schema.published_list("permits").is_some());

    // Taken down.
    let admin = fresh(&k, &admin).await;
    k.add_customization(&admin, "XPermit", op(json!({"op": "unpublish_list", "name": "permits"})), None).await.unwrap();
    assert!(fresh(&k, &admin).await.schema.published_lists.is_empty());
}
