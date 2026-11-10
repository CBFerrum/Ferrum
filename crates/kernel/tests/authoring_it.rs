//! What an authoring screen saves (docs/api.md, "Customization"), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset first): a batch of customizations is saved in
//! one transaction or not at all, its refusals name the operation that caused them, and a dry run
//! finds what only the database can find and keeps nothing. A custom entity with records is
//! redefined: fields, lines, a unique key and a total come; a field goes and comes back with its
//! values; what would damage records is refused; switching a redefinition off is its undo; and a
//! change set holding redefinitions gives another workspace the same schema.

use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-authoring-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("shop/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("shop/package.yaml"), "package: shop\nversion: 0.1.0\nlabel: Shop\ndepends: [core]\n").unwrap();
    std::fs::write(dir.join("shop/entities/shop.yaml"), "- name: Shop\n  label: Shop\n  kind: master\n  fields:\n    - {name: name, type: data}\n").unwrap();
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

fn op(kind: &str, entity: Value) -> NewCustomization {
    serde_json::from_value(json!({"entity": entity["name"], "op": {"op": kind, "entity": entity}})).expect("an operation")
}

fn entity(name: &str, kind: &str, fields: Value) -> Value {
    json!({"name": name, "label": name.trim_start_matches('X'), "kind": kind, "fields": fields})
}

struct W {
    k: Arc<Kernel>,
    tenant: Uuid,
    user: Uuid,
}

impl W {
    async fn admin(&self) -> Ctx {
        self.k.load_ctx(self.tenant, self.user, None, "it".into()).await.unwrap()
    }
    async fn count(&self, sql: &str) -> i64 {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(self.tenant).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        n
    }
    /// Saves `items` as a batch.
    async fn batch(&self, items: Vec<NewCustomization>, dry_run: bool) -> std::result::Result<usize, (Option<usize>, Error)> {
        self.k.batch_customizations(&self.admin().await, items, dry_run).await
    }
    async fn customizations(&self) -> usize {
        self.k.list_customizations(&self.admin().await).await.unwrap().len()
    }
}

async fn workspace(k: &Arc<Kernel>, slug: &str) -> W {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["shop".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    W { k: k.clone(), tenant, user: session.user }
}

fn refused(r: std::result::Result<usize, (Option<usize>, Error)>, index: Option<usize>, says: &str) {
    match r {
        Err((i, e)) => {
            assert_eq!(i, index, "{e}");
            assert!(e.to_string().contains(says), "{e}");
        }
        Ok(n) => panic!("saved {n}, expected a refusal saying {says}"),
    }
}

#[test]
fn authoring_screens_save_and_redefine_entities() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let w = workspace(&k, "acme").await;

    // ───────────── a batch: all or nothing, refusals naming their operation ─────────────
    let step = entity("XStep", "child", json!([{"name": "what", "type": "data"}]));
    let task = entity("XTask", "document", json!([{"name": "title", "type": "data"}, {"name": "steps", "type": "table", "of": "XStep"}]));
    assert_eq!(w.batch(vec![op("define_entity", step.clone()), op("define_entity", task.clone())], false).await.unwrap(), 2, "a child and its parent together");
    assert!(w.admin().await.schema.entity("XTask").is_some_and(|e| e.children.contains_key("steps")));
    let before = w.customizations().await;
    let broken = entity("XBroken", "master", json!([{"name": "rows", "type": "table", "of": "XNothing"}]));
    refused(w.batch(vec![op("define_entity", entity("XFirst", "master", json!([]))), op("define_entity", broken)], false).await, Some(1), "XNothing");
    assert_eq!(w.customizations().await, before, "the first was not kept either");
    assert!(w.admin().await.schema.entity("XFirst").is_none());
    refused(w.batch(vec![op("define_entity", step.clone())], false).await, Some(0), "already active");
    let many: Vec<NewCustomization> = (0..101).map(|i| op("define_entity", entity(&format!("XMany{i}"), "master", json!([])))).collect();
    refused(w.batch(many, false).await, None, "at most 100");
    let mut clerk = w.admin().await;
    clerk.is_admin = false;
    let r = k.batch_customizations(&clerk, vec![op("define_entity", entity("XClerk", "master", json!([])))], false).await;
    assert!(matches!(r, Err((None, Error::Forbidden(_)))), "{r:?}");

    // ───────────── an order with records ─────────────
    let line = entity("XLine", "child", json!([{"name": "item", "type": "data"}, {"name": "amount", "type": "currency"}]));
    let order = |fields: Value| entity("XOrder", "document", fields);
    let v1 = order(json!([{"name": "code", "type": "data"}, {"name": "qty", "type": "int"}, {"name": "note", "type": "data"}]));
    assert_eq!(w.batch(vec![op("define_entity", line), op("define_entity", v1.clone())], false).await.unwrap(), 2);
    let admin = w.admin().await;
    let a = k.insert(&admin, "XOrder", obj(json!({"code": "A", "qty": 2, "note": "same"}))).await.unwrap();
    let b = k.insert(&admin, "XOrder", obj(json!({"code": "B", "qty": 3, "note": "same"}))).await.unwrap();

    // A dry run does everything, the unique key's fill included, and keeps nothing.
    let (version, before) = (w.admin().await.schema.version, w.customizations().await);
    let mut keyed = v1.clone();
    keyed["fields"][2]["unique"] = json!(true);
    let extra = entity("XExtra", "master", json!([]));
    refused(w.batch(vec![op("define_entity", extra.clone()), op("redefine_entity", keyed)], true).await, Some(1), "records already share same");
    assert_eq!(w.batch(vec![op("define_entity", extra)], true).await.unwrap(), 1, "a dry run that passes");
    assert_eq!((w.admin().await.schema.version, w.customizations().await), (version, before), "nothing saved, no new version");
    assert!(w.admin().await.schema.entity("XExtra").is_none());

    // ───────────── redefining it: fields, lines, a key and a total come; a field goes ─────────────
    let v2 = order(json!([
        {"name": "code", "type": "data", "unique": true},
        {"name": "qty", "type": "decimal"},
        {"name": "region", "type": "data", "required": true},
        {"name": "spare", "type": "data"},
        {"name": "total", "type": "currency", "rollup": {"table": "lines", "op": "sum", "field": "amount"}},
        {"name": "lines", "type": "table", "of": "XLine"}
    ]));
    assert_eq!(w.batch(vec![op("redefine_entity", v2.clone())], false).await.unwrap(), 1, "int to decimal is safe, note goes");
    let admin = w.admin().await;
    let loaded = k.get(&admin, "XOrder", id_of(&a), false).await.expect("an old record loads");
    assert_eq!((&loaded["code"], loaded.get("note"), &loaded["lines"]), (&json!("A"), None, &json!([])), "{loaded:?}");
    assert_eq!(w.count("select count(*) from custom_key where tenant_id = $1 and entity = 'XOrder'").await, 2, "the key filled from the records");
    let e = k.insert(&admin, "XOrder", obj(json!({"code": "A", "region": "north"}))).await.expect_err("and enforced");
    assert!(e.to_string().contains("already has this Code"), "{e}");
    let saved = k
        .update(&admin, "XOrder", id_of(&a), obj(json!({"region": "north", "lines": [{"item": "x", "amount": "0.1"}, {"item": "y", "amount": "0.2"}]})), None)
        .await
        .expect("saved with what it now requires");
    assert_eq!(saved["total"], json!("0.3"), "{saved:?}");
    assert!(k.update(&admin, "XOrder", id_of(&b), obj(json!({"qty": 4})), None).await.is_err(), "a newly required field is asked for on the next save");

    // The field comes back with its values.
    let mut v3 = v2.clone();
    v3["fields"].as_array_mut().unwrap().push(json!({"name": "note", "type": "text"}));
    w.batch(vec![op("redefine_entity", v3.clone())], false).await.unwrap();
    let admin = w.admin().await;
    assert_eq!(k.get(&admin, "XOrder", id_of(&a), false).await.unwrap()["note"], json!("same"), "kept through a save while it was gone");

    // What would damage records is refused; the same change where no record holds a value is not.
    let mut unsafe_qty = v3.clone();
    unsafe_qty["fields"][1]["type"] = json!("int");
    refused(
        w.batch(vec![op("redefine_entity", unsafe_qty)], false).await,
        Some(0),
        "field qty: records hold values in it, and a decimal value does not read as int",
    );
    let mut spare_int = v3.clone();
    spare_int["fields"][3]["type"] = json!("int");
    w.batch(vec![op("redefine_entity", spare_int)], false).await.expect("no record holds spare");
    let mut master = v3.clone();
    master["kind"] = json!("master");
    refused(w.batch(vec![op("redefine_entity", master)], false).await, Some(0), "kind cannot change from document to master");
    refused(
        w.batch(vec![op("redefine_entity", entity("Shop", "master", json!([{"name": "name", "type": "data"}])))], false).await,
        Some(0),
        "comes with a package",
    );
    refused(w.batch(vec![op("redefine_entity", entity("XNobody", "master", json!([])))], false).await, Some(0), "no custom entity XNobody");
    let mut twice = v3.clone();
    twice["name"] = json!("XTask");
    twice["fields"] = json!([{"name": "steps", "type": "table", "of": "XStep"}, {"name": "lines", "type": "table", "of": "XLine"}]);
    refused(w.batch(vec![op("redefine_entity", twice)], false).await, Some(0), "XLine is already the lines of XOrder.lines");

    // ───────────── switching a redefinition off is its undo ─────────────
    let mut unkeyed = v3.clone();
    unkeyed["fields"][0]["unique"] = json!(false);
    w.batch(vec![op("redefine_entity", unkeyed)], false).await.unwrap();
    assert_eq!(w.count("select count(*) from custom_key where tenant_id = $1 and entity = 'XOrder'").await, 0, "a key that goes takes its rows");
    let admin = w.admin().await;
    let last = k.list_customizations(&admin).await.unwrap().into_iter().rfind(|r| r.is_active).unwrap();
    assert_eq!(last.op["op"], json!("redefine_entity"));
    k.set_customization_active(&admin, last.id, false).await.unwrap();
    let admin = w.admin().await;
    assert_eq!(admin.schema.entity("XOrder").unwrap().field("spare").unwrap().ftype, ferrum_meta::FieldType::Int, "the definition before it");
    assert_eq!(w.count("select count(*) from custom_key where tenant_id = $1 and entity = 'XOrder'").await, 2, "with its key");
    assert!(k.insert(&admin, "XOrder", obj(json!({"code": "B", "region": "south"}))).await.is_err());

    // ───────────── a change set with redefinitions gives another workspace the same schema ─────────────
    let set = k.export_customizations(&admin).await.unwrap();
    let beta = workspace(&k, "beta").await;
    assert!(k.import_customizations(&beta.admin().await, &set).await.unwrap() > 0);
    let custom = |c: &Ctx| -> Value {
        c.schema.canonical()["entities"].as_object().unwrap().iter().filter(|(n, _)| n.starts_with('X')).map(|(n, v)| (n.clone(), v.clone())).collect()
    };
    assert_eq!(custom(&beta.admin().await), custom(&w.admin().await));
    assert_eq!(k.import_customizations(&beta.admin().await, &set).await.unwrap(), 0, "an import skips what is already active");
}
