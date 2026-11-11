//! A workspace's settings (docs/builder/decisions.md BD-17), against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`;
//! the database is reset first): a workspace's own settings entity holds one record, and every expression reads it
//! and a package's settings as `settings.<Entity>.<field>`, as the workspace, whoever saves.

use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, automation};
use ferrum_meta::{Catalog, PermissionDef};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

/// A package's settings, with a secret no expression may read.
const PACKAGE: &str = "- name: GovSettings\n  label: Office Settings\n  kind: settings\n  fields:\n    - {name: fee, type: int, default: 50}\n    - {name: api_key, type: secret}\n";

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-settings-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("gov/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("gov/package.yaml"), "package: gov\nversion: 0.1.0\nlabel: Government\ndepends: [core]\n").unwrap();
    std::fs::write(dir.join("gov/entities/gov.yaml"), PACKAGE).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn op(entity: &str, op: Value) -> NewCustomization {
    serde_json::from_value(json!({"entity": entity, "op": op})).unwrap()
}

fn define(entity: Value) -> NewCustomization {
    op(entity["name"].as_str().unwrap(), json!({"op": "define_entity", "entity": entity}))
}

fn message(e: &Error) -> String {
    match e {
        Error::Validation(v) => v.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

#[track_caller]
fn refused<T: std::fmt::Debug>(r: Result<T, Error>, says: &str) {
    let e = r.expect_err(says);
    assert!(message(&e).contains(says), "{says}: {}", message(&e));
}

async fn boot(db: Db) -> Arc<Kernel> {
    Kernel::boot(
        db,
        catalog(),
        HookRegistry::default(),
        KernelSettings { secret_key: Some(ferrum_kernel::secret::SecretKey::new([42u8; 32])), ..Default::default() },
        None,
    )
    .await
    .expect("boot")
}

#[test]
fn workspace_settings() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = boot(db).await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "city".into(),
            name: "City".into(),
            admin_email: "admin@city.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["gov".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let user = k.login("city", "admin@city.test", "Correct-Horse-9").await.unwrap().user;
    let admin = k.load_ctx(tenant, user, None, "it".into()).await.unwrap();
    let batch = |items: Vec<NewCustomization>| {
        let (k, admin) = (k.clone(), admin.clone());
        async move { k.batch_customizations(&admin, items, false).await.map_err(|(at, e)| format!("{at:?}: {}", message(&e))) }
    };

    // ───────────── the workspace's own settings entity ─────────────
    let e = batch(vec![define(json!({"name": "XPolicy", "label": "Claim Policy", "kind": "settings", "company_scoped": true,
        "fields": [{"name": "company", "type": "link", "to": "Company"}]}))])
    .await
    .unwrap_err();
    assert!(e.contains("one record for the workspace"), "{e}");
    batch(vec![define(json!({"name": "XPolicy", "label": "Claim Policy", "kind": "settings", "fields": [
        {"name": "limit", "type": "int", "default": 100}, {"name": "note", "type": "data", "default": "checked"}]}))])
    .await
    .expect("a settings entity of the workspace's own");

    // ───────────── refused when saved, naming what is not there ─────────────
    let claim = |rules: Value| {
        define(json!({"name": "XClaim", "label": "Claim", "kind": "master", "fields": [
            {"name": "amount", "type": "int"}, {"name": "note", "type": "data"},
            {"name": "fee", "type": "int", "computed": "settings.GovSettings.fee * 2"}], "rules": rules}))
    };
    let rule = |check: &str| json!([{"name": "within", "check": check, "message": "over the limit"}]);
    for (check, says) in [
        ("doc.amount <= settings.XNope.limit", "settings.XNope.limit: there is no entity XNope"),
        ("doc.amount <= settings.XPolicy.colour", "settings.XPolicy.colour: Claim Policy has no field colour"),
        ("doc.amount <= settings.GovSettings.api_key", "api_key is a secret"),
        ("doc.amount <= settings.XClaim.amount", "XClaim is not the workspace's settings"),
        ("settings != null", "settings.<Entity>.<field>"),
    ] {
        let e = batch(vec![claim(rule(check))]).await.unwrap_err();
        assert!(e.contains(says), "{says}: {e}");
    }
    batch(vec![
        claim(rule("doc.amount <= settings.XPolicy.limit")),
        op(
            "XClaim",
            json!({"op": "add_action", "action": {"name": "x_stamp", "label": "Stamp",
                "refuse_if": [{"when": "doc.amount * 2 > settings.XPolicy.limit", "message": "too large to stamp"}],
                "effects": [{"type": "set_fields", "values": {"note": "settings.XPolicy.note"}}]}}),
        ),
    ])
    .await
    .expect("a rule, a computed field, a refusal and an effect reading settings");
    let e = batch(vec![op(
        "XClaim",
        json!({"op": "add_action", "action": {"name": "x_bad", "label": "Bad",
        "effects": [{"type": "set_fields", "values": {"note": "settings.XPolicy.colour"}}]}}),
    )])
    .await
    .unwrap_err();
    assert!(e.contains("Claim Policy has no field colour"), "{e}");
    let admin = k.load_ctx(tenant, user, None, "it".into()).await.unwrap();
    refused(
        automation::save(
            &k,
            &admin,
            &obj(json!({"name": "Bad", "entity": "XClaim", "event": "created", "condition": "settings.XPolicy.colour == ''",
                "actions": [{"type": "notify", "to": "owner", "title": "'t'"}]})),
        )
        .await,
        "has no field colour",
    );

    // ───────────── defaults until the record is saved ─────────────
    refused(k.insert(&admin, "XClaim", obj(json!({"amount": 150}))).await, "over the limit");
    let before = ferrum_kernel::settings::loads();
    let small = k.insert(&admin, "XClaim", obj(json!({"amount": 50}))).await.expect("within the default limit of 100");
    assert_eq!(ferrum_kernel::settings::loads() - before, 1, "one save loads its settings once, both entities together");
    assert_eq!(small["fee"], json!(100), "a package's settings, by its default: 50 * 2");

    // ───────────── one record ─────────────
    let before = ferrum_kernel::settings::loads();
    let policy = k.insert(&admin, "XPolicy", obj(json!({"limit": 200, "note": "approved"}))).await.expect("the settings record");
    assert_eq!(ferrum_kernel::settings::loads(), before, "a save whose expressions name no settings loads none");
    refused(k.insert(&admin, "XPolicy", obj(json!({"limit": 1}))).await, "holds one record for the workspace");
    k.insert(&admin, "GovSettings", obj(json!({"fee": 70, "api_key": "s3cret"}))).await.expect("the package's settings record");

    // ───────────── read as the workspace, by someone who may not open the settings ─────────────
    let rule = PermissionDef { entity: "XClaim".into(), read: true, write: true, create: true, ..Default::default() };
    k.set_role_permission(&admin, "Clerk", &rule).await.unwrap();
    let id = k.create_user(&admin, "clerk@city.test", "Clerk", None, &["Clerk".to_string()], false).await.unwrap();
    let clerk: Ctx = k.load_ctx(tenant, id, None, "it".into()).await.unwrap();
    refused(k.list(&clerk, "XPolicy", &Default::default()).await, "not permitted");
    let c = k.insert(&clerk, "XClaim", obj(json!({"amount": 150}))).await.expect("the limit is 200 now, seen at once");
    assert_eq!(c["fee"], json!(140), "the package's settings as saved: 70 * 2");
    refused(k.insert(&clerk, "XClaim", obj(json!({"amount": 250}))).await, "over the limit");

    // ───────────── refusals and effects read them too ─────────────
    refused(k.run_action(&clerk, "XClaim", id_of(&c), "x_stamp", Value::Null).await, "too large to stamp");
    k.run_action(&clerk, "XClaim", id_of(&small), "x_stamp", Value::Null).await.expect("50 * 2 is within 200");
    assert_eq!(k.get(&admin, "XClaim", id_of(&small), false).await.unwrap()["note"], json!("approved"));

    // ───────────── a change seen by the next evaluation, through another replica ─────────────
    let other = boot(Db::connect(DbConfig { url, max_connections: 4, ..Default::default() }).await.expect("connect")).await;
    let admin2 = other.load_ctx(tenant, user, None, "it".into()).await.unwrap();
    other.update(&admin2, "XPolicy", id_of(&policy), obj(json!({"limit": 120})), None).await.expect("changed elsewhere");
    refused(k.insert(&clerk, "XClaim", obj(json!({"amount": 150}))).await, "over the limit");

    // ───────────── an automation reads them ─────────────
    automation::save(
        &k,
        &admin,
        &obj(json!({"name": "Note", "entity": "XClaim", "event": "created", "condition": "doc.amount <= settings.XPolicy.limit",
            "actions": [{"type": "set_fields", "values": {"note": "'per policy: ' + settings.XPolicy.note"}}]})),
    )
    .await
    .expect("an automation reading settings");
    let d = k.insert(&clerk, "XClaim", obj(json!({"amount": 10}))).await.unwrap();
    while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
    assert_eq!(k.get(&admin, "XClaim", id_of(&d), false).await.unwrap()["note"], json!("per policy: approved"));

    // ───────────── a pack carries the settings entity and, asked, its values; never over a workspace's own ─────────────
    let options = ferrum_kernel::pack::ExportOptions { name: "claims".into(), label: "Claims".into(), version: 1, records: vec!["XPolicy".into()] };
    let pack = k.export_pack(&admin, &options).await.expect("export");
    let rows = &pack["records"][0];
    assert_eq!((&rows["entity"], &rows["key"], rows["rows"][0]["limit"].clone()), (&json!("XPolicy"), &json!([]), json!(120)));
    let install = |slug: &'static str| {
        let (k, pack) = (k.clone(), pack.clone());
        async move {
            let t = k
                .create_tenant(NewTenant {
                    slug: slug.into(),
                    name: slug.into(),
                    admin_email: format!("admin@{slug}.test"),
                    admin_password: "Correct-Horse-9".into(),
                    admin_name: None,
                    packages: vec!["gov".into()],
                    settings: None,
                })
                .await
                .expect("workspace");
            let u = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap().user;
            let a = k.load_ctx(t, u, None, "it".into()).await.unwrap();
            let plan = k.plan_pack(&a, &pack).await.expect("plan");
            k.install_pack(&a, &pack, plan["digest"].as_str().unwrap()).await.expect("install");
            (t, u)
        }
    };
    let (t, u) = install("town").await;
    let a = k.load_ctx(t, u, None, "it".into()).await.unwrap();
    let theirs = k.list(&a, "XPolicy", &Default::default()).await.unwrap().rows.remove(0);
    assert_eq!(theirs["limit"], json!(120), "a workspace with no settings record takes the pack's");
    k.update(&a, "XPolicy", id_of(&theirs), obj(json!({"limit": 999})), None).await.unwrap();
    let plan = k.plan_pack(&a, &pack).await.expect("plan again");
    k.install_pack(&a, &pack, plan["digest"].as_str().unwrap()).await.expect("install again");
    let kept = k.list(&a, "XPolicy", &Default::default()).await.unwrap().rows;
    assert_eq!((kept.len(), &kept[0]["limit"]), (1, &json!(999)), "installing again keeps the workspace's own value");
    refused(k.insert(&a, "XClaim", obj(json!({"amount": 1000}))).await, "over the limit");
}
