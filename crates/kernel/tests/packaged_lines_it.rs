//! A workspace's own lines on an entity that comes with a package (docs/builder/decisions.md BD-06, 10 November
//! 2026): a checklist on a basket, added by a customization, its rows custom records under the basket's id. Against
//! a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first). What could go wrong is storage: the
//! rows share `custom_record` with every other custom record of the workspace, and their parent lives in the
//! package's own table. So the test deletes, uninstalls (keeping and deleting data), upgrades, switches the
//! customization off and on, exports and imports, erases and merges, and checks after each that the workspace's
//! other custom records are untouched and that no line is left without its record.

use async_trait::async_trait;
use ferrum_kernel::files::FileStore;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, Result};
use ferrum_meta::{Catalog, EntityDef, FieldDef, OverlayOp};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const V1: &str = r#"
- name: Basket
  label: Basket
  kind: master
  title_field: name
  mergeable: true
  fields:
    - {name: name, type: data, required: true}
    - {name: items, type: table, of: BasketItem}
- name: BasketItem
  label: Basket Item
  kind: child
  fields:
    - {name: price, type: currency}
"#;

/// The package's next release: a new field on the entity that holds the workspace's lines.
const V2: &str = r#"
- name: Basket
  label: Basket
  kind: master
  title_field: name
  mergeable: true
  fields:
    - {name: name, type: data, required: true}
    - {name: note, type: data}
    - {name: items, type: table, of: BasketItem}
- name: BasketItem
  label: Basket Item
  kind: child
  fields:
    - {name: price, type: currency}
"#;

fn catalog(entities: &str) -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-packaged-lines-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("shop/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("shop/package.yaml"), "package: shop\nversion: 0.1.0\nlabel: Shop\ndepends: [core]\n").unwrap();
    std::fs::write(dir.join("shop/entities/shop.yaml"), entities).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

#[derive(Default)]
struct MemFiles(Mutex<HashMap<String, Vec<u8>>>);

#[async_trait]
impl FileStore for MemFiles {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        self.0.lock().unwrap().insert(key.to_string(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned().ok_or_else(|| Error::NotFound("object".into()))
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn col(d: &Doc, table: &str, field: &str) -> Vec<Value> {
    d[table].as_array().unwrap().iter().map(|r| r[field].clone()).collect()
}

fn field(v: Value) -> FieldDef {
    serde_json::from_value(v).expect("a field")
}

async fn define(k: &Kernel, admin: &Ctx, def: Value) -> Result<Uuid> {
    let entity: EntityDef = serde_json::from_value(def).expect("an entity definition");
    let name = entity.name.clone();
    k.add_customization(admin, &name, OverlayOp::DefineEntity { entity: Box::new(entity) }, None).await
}

async fn add(k: &Kernel, admin: &Ctx, entity: &str, f: Value) -> Result<Uuid> {
    k.add_customization(admin, entity, OverlayOp::AddField { field: field(f), after: None, column: false }, None).await
}

/// Lines with no record above them: neither a custom record nor a basket.
const ORPHANS: &str = "select count(*) from custom_record c where c.tenant_id = $1 and c.parent_id is not null \
    and not exists (select 1 from custom_record p where p.tenant_id = c.tenant_id and p.id = c.parent_id) \
    and not exists (select 1 from e_basket p where p.tenant_id = c.tenant_id and p.id = c.parent_id)";
const CHECKS: &str = "select count(*) from custom_record where tenant_id = $1 and entity = 'XCheck'";
/// Every custom record of the workspace that is not a basket's line, as text: what nothing here may touch.
const OTHERS: &str = "select coalesce(string_agg(id::text || ':' || entity || ':' || coalesce(parent_id::text, '') || ':' || ext::text, ',' order by id), '') \
    from custom_record where tenant_id = $1 and entity <> 'XCheck'";

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
    async fn text(&self, sql: &str) -> String {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let s: String = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(self.tenant).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        s
    }
    /// The workspace's own customizations: lines on its baskets, and custom records of its own, with lines too.
    async fn customize(&self) {
        let (k, admin) = (&self.k, self.admin().await);
        define(k, &admin, check_def()).await.expect("the checklist's line entity");
        define(k, &admin, json!({"name": "XNote", "label": "Note", "kind": "master", "fields": [{"name": "title", "type": "data"}]})).await.unwrap();
        define(k, &admin, json!({"name": "XOrderLine", "label": "Order Line", "kind": "child", "fields": [{"name": "item", "type": "data"}]})).await.unwrap();
        define(
            k,
            &admin,
            json!({"name": "XOrder", "label": "Order", "kind": "master", "fields": [{"name": "code", "type": "data"}, {"name": "lines", "type": "table", "of": "XOrderLine"}]}),
        )
        .await
        .unwrap();
        let admin = self.admin().await;
        add(k, &admin, "Basket", json!({"name": "x_checks", "type": "table", "of": "XCheck", "label": "Checks"})).await.expect("lines on a package's entity");
        let admin = self.admin().await;
        add(k, &admin, "Basket", json!({"name": "x_score", "type": "currency", "rollup": {"table": "x_checks", "op": "sum", "field": "score"}})).await.unwrap();
        add(k, &admin, "Basket", json!({"name": "x_count", "type": "int", "rollup": {"table": "x_checks", "op": "count"}})).await.unwrap();
    }
    /// Custom records that are not a basket's lines: two notes, an order with two lines.
    async fn others(&self) {
        let admin = self.admin().await;
        self.k.insert(&admin, "XNote", obj(json!({"title": "keep me"}))).await.unwrap();
        self.k.insert(&admin, "XNote", obj(json!({"title": "and me"}))).await.unwrap();
        self.k.insert(&admin, "XOrder", obj(json!({"code": "O-1", "lines": [{"item": "a"}, {"item": "b"}]}))).await.unwrap();
    }
}

fn check_def() -> Value {
    json!({"name": "XCheck", "label": "Check", "kind": "child", "fields": [
        {"name": "step", "type": "data", "required": true},
        {"name": "email", "type": "data"},
        {"name": "score", "type": "currency"},
        {"name": "basket", "type": "link", "to": "Basket"}
    ]})
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

async fn customization(k: &Kernel, admin: &Ctx, name: &str) -> Uuid {
    let rows = k.list_customizations(admin).await.unwrap();
    rows.iter().find(|r| r.op["field"]["name"] == json!(name)).map(|r| r.id).expect("the customization")
}

#[test]
fn a_packaged_entity_holds_lines_of_the_workspaces_own() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db.clone(), catalog(V1), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    k.set_file_store(Arc::new(MemFiles::default()));
    let w = workspace(&k, "acme").await;
    w.customize().await;
    w.others().await;
    let admin = w.admin().await;
    let basket = admin.schema.entity("Basket").unwrap();
    assert_eq!(basket.children["x_checks"].table, "custom_record", "the workspace's lines are custom records");
    assert_eq!(basket.children["items"].table, "e_basket__items", "the package's lines stay in its table");

    // ───────────── what is refused ─────────────
    let refused = |r: Result<Uuid>| r.expect_err("refused").to_string();
    let e = refused(add(&k, &admin, "Basket", json!({"name": "x_more", "type": "table", "of": "BasketItem"})).await);
    assert!(e.contains("must be a custom child entity"), "a package's line entity: {e}");
    let e = refused(add(&k, &admin, "Basket", json!({"name": "x_more", "type": "table", "of": "XNote"})).await);
    assert!(e.contains("XNote is not a child entity"), "a record entity: {e}");
    let e = refused(add(&k, &admin, "Basket", json!({"name": "x_more", "type": "table", "of": "XOrderLine"})).await);
    assert!(e.contains("XOrderLine is already the lines of XOrder.lines"), "{e}");
    let e = refused(add(&k, &admin, "BasketItem", json!({"name": "x_more", "type": "table", "of": "XOrderLine"})).await);
    assert!(e.contains("lines of another entity"), "lines on a package's lines: {e}");
    let e = refused(add(&k, &admin, "XNote", json!({"name": "x_more", "type": "table", "of": "XOrderLine"})).await);
    assert!(e.contains("come with its definition"), "a custom entity's tables: {e}");
    let e = refused(
        define(&k, &admin, json!({"name": "XOther", "label": "Other", "kind": "master", "fields": [{"name": "rows", "type": "table", "of": "XCheck"}]})).await,
    );
    // Refused on the package's side, which falls back to its package's definition rather than the custom entity going.
    assert!(e.contains("Basket: customizations ignored: table field x_checks: XCheck is already the lines of XOther.rows"), "{e}");
    // One level: a basket's checks hold no lines of their own, whichever comes first.
    define(&k, &admin, json!({"name": "XLeaf", "label": "Leaf", "kind": "child", "fields": [{"name": "v", "type": "data"}]})).await.unwrap();
    define(&k, &admin, json!({"name": "XDeep", "label": "Deep", "kind": "child", "fields": [{"name": "rows", "type": "table", "of": "XLeaf"}]})).await.unwrap();
    let admin = w.admin().await;
    let e = refused(add(&k, &admin, "Basket", json!({"name": "x_deep", "type": "table", "of": "XDeep"})).await);
    assert!(e.contains("one level"), "{e}");
    let mut deeper = check_def();
    deeper["fields"].as_array_mut().unwrap().push(json!({"name": "rows", "type": "table", "of": "XLeaf"}));
    let e = refused(k.add_customization(&admin, "XCheck", OverlayOp::RedefineEntity { entity: Box::new(serde_json::from_value(deeper).unwrap()) }, None).await);
    assert!(e.contains("one level"), "{e}");

    // ───────────── saving and loading: ids kept, idx by position, exact totals ─────────────
    let b1 = k
        .insert(
            &admin,
            "Basket",
            obj(json!({"name": "B1", "items": [{"price": "1"}],
                "x_checks": [{"step": "weigh", "score": "0.1", "email": "erin@acme.test"}, {"step": "seal", "score": "0.2"}]})),
        )
        .await
        .expect("a basket with its checks");
    assert_eq!(col(&b1, "x_checks", "step"), [json!("weigh"), json!("seal")]);
    assert_eq!(col(&b1, "x_checks", "idx"), [json!(0), json!(1)]);
    assert_eq!((&b1["x_score"], &b1["x_count"]), (&json!("0.3"), &json!(2)), "exact totals over the workspace's lines");
    let loaded = k.get(&admin, "Basket", id_of(&b1), false).await.unwrap();
    assert_eq!(loaded["x_checks"], b1["x_checks"]);
    assert_eq!(col(&loaded, "items", "price"), [json!("1")], "the package's own lines beside them");
    // Reordered, one removed, one added: kept by id, idx by position.
    let rows = b1["x_checks"].as_array().unwrap().clone();
    let edited =
        k.update(&admin, "Basket", id_of(&b1), obj(json!({"x_checks": [rows[1].clone(), {"step": "label", "score": "5"}]})), None).await.expect("edited");
    assert_eq!(col(&edited, "x_checks", "id")[0], rows[1]["id"], "a kept row keeps its id");
    assert_eq!(col(&edited, "x_checks", "step"), [json!("seal"), json!("label")]);
    assert_eq!(col(&edited, "x_checks", "idx"), [json!(0), json!(1)]);
    assert_eq!(edited["x_score"], json!("5.2"));
    assert_eq!(w.count(CHECKS).await, 2, "the removed row is gone");
    let edited = k
        .update(&admin, "Basket", id_of(&b1), obj(json!({"x_checks": [edited["x_checks"][0].clone(), edited["x_checks"][1].clone(), rows[0].clone()]})), None)
        .await
        .unwrap();
    assert_eq!(col(&edited, "x_checks", "step"), [json!("seal"), json!("label"), json!("weigh")]);
    // A filter on a line's field.
    let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&json!({"x_checks.step": "label"})).unwrap(), ..Default::default() };
    let b2 = k.insert(&admin, "Basket", obj(json!({"name": "B2", "x_checks": [{"step": "weigh"}]}))).await.unwrap();
    let found: Vec<Value> = k.list(&admin, "Basket", &q).await.unwrap().rows.iter().map(|r| r["name"].clone()).collect();
    assert_eq!(found, [json!("B1")]);

    // ───────────── another workspace sees nothing ─────────────
    let other = workspace(&k, "globex").await;
    let them = other.admin().await;
    assert!(!them.schema.entity("Basket").unwrap().children.contains_key("x_checks"), "its baskets have no checks");
    assert!(k.get(&them, "Basket", id_of(&b1), false).await.is_err());
    assert_eq!(other.count(CHECKS).await, 0);
    other.customize().await;
    let them = other.admin().await;
    assert!(k.get(&them, "Basket", id_of(&b1), false).await.is_err(), "the same customization reaches only its own");
    assert_eq!(other.count(CHECKS).await, 0);

    // ───────────── deleting a basket takes its lines, and only its lines ─────────────
    let others = w.text(OTHERS).await;
    let b3 = k.insert(&admin, "Basket", obj(json!({"name": "B3", "x_checks": [{"step": "a"}, {"step": "b"}]}))).await.unwrap();
    assert_eq!(w.count(CHECKS).await, 6);
    k.delete(&admin, "Basket", id_of(&b3), None).await.expect("deleted");
    assert_eq!(w.count(CHECKS).await, 4, "its two lines went with it");
    assert_eq!(w.count(ORPHANS).await, 0);
    assert_eq!(w.text(OTHERS).await, others, "nothing else moved");

    // ───────────── a link in a line: guards a delete, follows a merge ─────────────
    let b4 = k.insert(&admin, "Basket", obj(json!({"name": "B4", "x_checks": [{"step": "compare", "basket": b2["id"]}]}))).await.unwrap();
    let e = k.delete(&admin, "Basket", id_of(&b2), None).await.expect_err("a line links to it");
    assert!(e.to_string().contains("other records link to it"), "{e}");
    k.merge_records(&admin, "Basket", id_of(&b1), id_of(&b2), "the same basket").await.expect("merged");
    let b4 = k.get(&admin, "Basket", id_of(&b4), false).await.unwrap();
    assert_eq!(b4["x_checks"][0]["basket"], b1["id"], "the link follows the merge");
    // The duplicate now reads as the record that stays; its own lines stay under it, the workspace's as the package's.
    let under_b2 = format!("select count(*) from custom_record where tenant_id = $1 and entity = 'XCheck' and parent_id = '{}'", id_of(&b2));
    assert_eq!(w.count(&under_b2).await, 1, "a merge moves no lines");
    assert_eq!(col(&k.get(&admin, "Basket", id_of(&b1), false).await.unwrap(), "x_checks", "step"), [json!("seal"), json!("label"), json!("weigh")]);

    // ───────────── erasure: the history's copies of a line, the line's own value undeclared ─────────────
    k.create_user(&admin, "erin@acme.test", "Erin", None, &[], false).await.unwrap();
    let held = format!("select count(*) from audit_event where tenant_id = $1 and record_id = '{}' and diff::text ilike '%erin@acme.test%'", id_of(&b1));
    assert!(w.count(&held).await > 0, "the basket's history holds its line's address");
    let done = k.erase_personal_data(&admin, "erin@acme.test", "Request 9").await.expect("erased");
    assert_eq!(done["account_erased"], json!(true));
    assert_eq!(w.count(&held).await, 0, "the address went from the basket's history");
    assert_eq!(w.count("select count(*) from custom_record where tenant_id = $1 and ext->>'email' = 'erin@acme.test'").await, 1, "no declaration names it");
    assert_eq!(w.count(ORPHANS).await, 0);

    // ───────────── export, and importing what it holds ─────────────
    let job = k.export_workspace(&admin).await.unwrap();
    assert_eq!(k.run_jobs_once(1).await.unwrap(), 1);
    let v = k.job(&admin, job).await.unwrap();
    assert_eq!(v["status"], json!("done"), "{v}");
    let part: Uuid = v["result"]["parts"][0]["id"].as_str().unwrap().parse().unwrap();
    let (_, bytes) = k.export_part(&admin, job, part).await.unwrap();
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut files = HashMap::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        files.insert(f.name().unwrap().to_string(), s);
    }
    assert!(!files.keys().any(|n| n.starts_with("records/XCheck")), "lines are no records of their own");
    let baskets: Vec<Value> =
        files.iter().filter(|(n, _)| n.starts_with("records/Basket/")).flat_map(|(_, s)| s.lines().map(|l| serde_json::from_str(l).unwrap())).collect();
    let mut exported = baskets.iter().find(|b| b["name"] == json!("B1")).expect("B1").clone();
    assert_eq!(exported["x_checks"].as_array().unwrap().iter().map(|r| r["step"].clone()).collect::<Vec<_>>(), [json!("seal"), json!("label"), json!("weigh")]);
    exported["name"] = json!("B1 again");
    for key in ["id", "number", "version", "created_at", "updated_at", "created_by", "updated_by", "x_score", "x_count"] {
        exported.as_object_mut().unwrap().remove(key);
    }
    for table in ["x_checks", "items"] {
        for r in exported[table].as_array_mut().unwrap() {
            r.as_object_mut().unwrap().remove("id");
        }
    }
    let (made, failed) = k.import_rows(&admin, "Basket", &[exported]).await.unwrap();
    assert!(failed.is_empty(), "{failed:?}");
    let again = k.get(&admin, "Basket", made[0].as_str().unwrap().parse().unwrap(), false).await.unwrap();
    assert_eq!(col(&again, "x_checks", "step"), [json!("seal"), json!("label"), json!("weigh")]);
    assert_eq!((&again["x_score"], &again["x_count"]), (&json!("5.3"), &json!(3)));
    assert_eq!(w.count(ORPHANS).await, 0);

    // ───────────── switching the customization off hides the lines, on shows them again ─────────────
    let lines = w.count(CHECKS).await;
    let before = k.get(&admin, "Basket", id_of(&b1), false).await.unwrap();
    let ids = [customization(&k, &admin, "x_count").await, customization(&k, &admin, "x_score").await, customization(&k, &admin, "x_checks").await];
    for id in ids {
        k.set_customization_active(&w.admin().await, id, false).await.expect("switched off");
    }
    let off = w.admin().await;
    let hidden = k.get(&off, "Basket", id_of(&b1), false).await.unwrap();
    assert!(!hidden.contains_key("x_checks"), "{hidden:?}");
    k.update(&off, "Basket", id_of(&b1), obj(json!({"name": "B1 renamed"})), None).await.expect("saved without them");
    assert_eq!(w.count(CHECKS).await, lines, "a save without them keeps them");
    // Their entity is no table's lines now, but its rows hang off baskets: they cannot become another entity's lines.
    let e = define(&k, &off, json!({"name": "XThief", "label": "Thief", "kind": "master", "fields": [{"name": "rows", "type": "table", "of": "XCheck"}]}))
        .await
        .expect_err("the baskets' lines taken over");
    assert!(e.to_string().contains("Check: its rows are lines of another app's records; they cannot become the lines of XThief.rows"), "{e}");
    // A basket deleted while they are off takes its hidden lines.
    for id in ids.iter().rev() {
        k.set_customization_active(&w.admin().await, *id, true).await.expect("switched on");
    }
    let admin = w.admin().await;
    let b6 = k.insert(&admin, "Basket", obj(json!({"name": "B6", "x_checks": [{"step": "x"}, {"step": "y"}]}))).await.unwrap();
    for id in ids {
        k.set_customization_active(&w.admin().await, id, false).await.unwrap();
    }
    k.delete(&w.admin().await, "Basket", id_of(&b6), None).await.expect("deleted while its lines are hidden");
    assert_eq!(w.count(CHECKS).await, lines, "its hidden lines went with it");
    assert_eq!(w.count(ORPHANS).await, 0);
    for id in ids.iter().rev() {
        k.set_customization_active(&w.admin().await, *id, true).await.unwrap();
    }
    let admin = w.admin().await;
    let shown = k.get(&admin, "Basket", id_of(&b1), false).await.unwrap();
    assert_eq!(shown["x_checks"], before["x_checks"], "back, every row as it was");
    assert_eq!(shown["name"], json!("B1 renamed"));

    // ───────────── a package upgrade that changes the entity leaves them ─────────────
    let k = Kernel::boot(db.clone(), catalog(V2), HookRegistry::default(), KernelSettings::default(), None).await.expect("upgraded");
    k.install_packages(w.tenant, &["shop".to_string()]).await.expect("upgrades");
    let w = W { k: k.clone(), tenant: w.tenant, user: w.user };
    let admin = w.admin().await;
    assert!(admin.schema.warnings.is_empty(), "{:?}", admin.schema.warnings);
    let up = k.update(&admin, "Basket", id_of(&b1), obj(json!({"note": "new field"})), None).await.expect("the new field");
    assert_eq!(up["x_checks"], before["x_checks"], "the lines as they were");
    assert_eq!(w.count(CHECKS).await, lines);

    // ───────────── uninstalling the package ─────────────
    let others = w.text(OTHERS).await;
    assert!(others.matches(":XNote:").count() == 2 && others.matches(":XOrderLine:").count() == 2, "{others}");
    // Keeping its data keeps the lines too; installing it again shows them.
    k.uninstall_packages(&admin, &["shop".into()], false).await.expect("removed, keeping data");
    assert_eq!(w.count(CHECKS).await, lines);
    assert_eq!(w.text(OTHERS).await, others);
    k.install_packages(w.tenant, &["shop".to_string()]).await.unwrap();
    let admin = w.admin().await;
    assert_eq!(k.get(&admin, "Basket", id_of(&b1), false).await.unwrap()["x_checks"], before["x_checks"]);
    // Deleting its data deletes its baskets' lines, and nothing else of the workspace's own.
    k.uninstall_packages(&admin, &["shop".into()], true).await.expect("removed with its data");
    assert_eq!(w.count("select count(*) from e_basket where tenant_id = $1").await, 0);
    assert_eq!(w.count(CHECKS).await, 0, "the baskets' lines went with them");
    assert_eq!(w.text(OTHERS).await, others, "every other custom record of the workspace is as it was");
    assert_eq!(w.count(ORPHANS).await, 0);
    // The other workspace's are its own.
    let other = W { k: k.clone(), tenant: other.tenant, user: other.user };
    let them = other.admin().await;
    let theirs = k.insert(&them, "Basket", obj(json!({"name": "T1", "x_checks": [{"step": "t"}]}))).await;
    assert!(theirs.is_ok(), "{theirs:?}");
    assert_eq!(other.count(CHECKS).await, 1);
}
