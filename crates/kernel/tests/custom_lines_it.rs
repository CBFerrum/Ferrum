//! Custom entities that hold a business document (docs/builder/decisions.md BD-06, BF-ENTITY-…), against a
//! real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first). An administrator defines,
//! with no code, an order with two tables of lines, unique keys and exact totals over its lines. Both
//! tables' rows share `custom_record`, so the test tries every way one table's rows could show up in the
//! other's: loading, filtering, every kind of edit, deleting, the workspace's export and a portal path.
//! Unique keys hold under two saves at once, free what a record no longer holds, and are filled or
//! dropped with the definition; totals are exact where floats are not.

use async_trait::async_trait;
use ferrum_kernel::files::FileStore;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, Result};
use ferrum_meta::{Catalog, EntityDef, OverlayOp, PermissionDef};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const ENTITIES: &str = r#"
- name: Shop
  label: Shop
  kind: master
  title_field: name
  fields:
    - {name: name, type: data, required: true}
    - {name: baskets, type: int, label: Baskets}
- name: Basket
  label: Basket
  kind: master
  title_field: name
  fields:
    - {name: name, type: data, required: true}
    - {name: total, type: currency, rollup: {table: items, op: sum, field: price}}
    - {name: items, type: table, of: BasketItem}
- name: BasketItem
  label: Basket Item
  kind: child
  fields:
    - {name: price, type: currency}
"#;

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-custom-lines-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("shop/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("shop/package.yaml"), "package: shop\nversion: 0.1.0\nlabel: Shop\ndepends: [core]\n").unwrap();
    std::fs::write(dir.join("shop/entities/shop.yaml"), ENTITIES).unwrap();
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

/// What a table of a record holds, by row field.
fn col(d: &Doc, table: &str, field: &str) -> Vec<Value> {
    d[table].as_array().unwrap().iter().map(|r| r[field].clone()).collect()
}

fn order_def() -> Value {
    json!({
        "name": "XOrder", "label": "Order", "kind": "document", "submittable": true, "title_field": "code",
        "fields": [
            {"name": "code", "type": "data", "label": "Code", "unique": true},
            {"name": "ref_a", "type": "data", "label": "Ref A"},
            {"name": "ref_b", "type": "data", "label": "Ref B"},
            {"name": "region", "type": "data"},
            {"name": "total", "type": "currency", "rollup": {"table": "lines", "op": "sum", "field": "amount"}},
            {"name": "line_count", "type": "int", "rollup": {"table": "lines", "op": "count"}},
            {"name": "lines", "type": "table", "of": "XOrderLine"},
            {"name": "notes", "type": "table", "of": "XOrderNote"}
        ],
        "unique": [["ref_a", "ref_b"]],
        "rules": [{"name": "small_orders", "check": "doc.total < 1000000.0 || doc.region == \"big\"", "message": "large orders go to region big"}]
    })
}

fn line_def() -> Value {
    json!({
        "name": "XOrderLine", "label": "Order Line", "kind": "child",
        "fields": [
            {"name": "item", "type": "data", "required": true},
            {"name": "email", "type": "data"},
            {"name": "amount", "type": "currency"}
        ],
        "rules": [{"name": "open_region", "check": "doc.amount == 0.0 || parent.region != \"frozen\"", "message": "nothing is sold in a frozen region"}]
    })
}

fn note_def() -> Value {
    json!({
        "name": "XOrderNote", "label": "Order Note", "kind": "child",
        "fields": [{"name": "item", "type": "data"}, {"name": "email", "type": "data"}]
    })
}

async fn define(k: &Kernel, admin: &Ctx, def: Value) -> Result<Uuid> {
    let entity: EntityDef = serde_json::from_value(def).expect("an entity definition");
    let name = entity.name.clone();
    k.add_customization(admin, &name, OverlayOp::DefineEntity { entity: Box::new(entity) }, None).await
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
    async fn count(&self, sql: &str, id: Option<Uuid>) -> i64 {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let mut q = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(self.tenant);
        if let Some(id) = id {
            q = q.bind(id);
        }
        let n: i64 = q.fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        n
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

async fn filtered(k: &Kernel, ctx: &Ctx, filters: Value) -> Vec<String> {
    let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() };
    let mut codes: Vec<String> = k.list(ctx, "XOrder", &q).await.unwrap().rows.iter().map(|r| r["code"].as_str().unwrap().to_string()).collect();
    codes.sort();
    codes
}

#[test]
fn custom_entities_hold_lines_unique_keys_and_exact_totals() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    k.set_file_store(Arc::new(MemFiles::default()));
    let w = workspace(&k, "acme").await;
    let admin = w.admin().await;

    // ───────────── defining it: what is refused, and why ─────────────
    define(&k, &admin, line_def()).await.expect("a child entity of the workspace's own");
    define(&k, &admin, note_def()).await.expect("a second one");
    define(&k, &admin, order_def()).await.expect("an order with two tables, keys and totals");
    let admin = w.admin().await;
    let mut twice = order_def();
    twice["name"] = json!("XQuote");
    twice["fields"] = json!([{"name": "code", "type": "data"}, {"name": "lines", "type": "table", "of": "XOrderLine"}]);
    twice["unique"] = json!([]);
    twice["rules"] = json!([]);
    let e = define(&k, &admin, twice).await.expect_err("one child entity, two table fields");
    assert!(e.to_string().contains("XOrderLine is already the lines of XOrder.lines"), "{e}");
    let mut keyed = note_def();
    keyed["name"] = json!("XKeyedLine");
    keyed["fields"][0]["unique"] = json!(true);
    let e = define(&k, &admin, keyed).await.expect_err("a unique key on lines");
    assert!(e.to_string().contains("child entity: its lines cannot have unique keys"), "{e}");
    let mut packaged_lines = order_def();
    packaged_lines["name"] = json!("XCart");
    packaged_lines["fields"] = json!([{"name": "code", "type": "data"}, {"name": "items", "type": "table", "of": "BasketItem"}]);
    packaged_lines["unique"] = json!([]);
    packaged_lines["rules"] = json!([]);
    let e = define(&k, &admin, packaged_lines).await.expect_err("a package's child entity as a custom entity's lines");
    assert!(e.to_string().contains("must be a custom child entity"), "{e}");
    let table: ferrum_meta::FieldDef = serde_json::from_value(json!({"name": "x_lines", "type": "table", "of": "XOrderNote"})).unwrap();
    let e = k
        .add_customization(&admin, "Shop", OverlayOp::AddField { field: table, after: None, column: false }, None)
        .await
        .expect_err("another table's lines on a package's entity (packaged_lines_it adds its own)");
    assert!(e.to_string().contains("XOrderNote is already the lines of XOrder.notes"), "{e}");
    let mut bad_rollup = order_def();
    bad_rollup["name"] = json!("XBadTotal");
    bad_rollup["fields"] = json!([{"name": "code", "type": "data"}, {"name": "total", "type": "int", "rollup": {"table": "lines", "op": "sum", "field": "amount"}},
        {"name": "lines", "type": "table", "of": "XOrderNote"}]);
    bad_rollup["unique"] = json!([]);
    bad_rollup["rules"] = json!([]);
    let e = define(&k, &admin, bad_rollup).await.expect_err("a sum of a field the rows do not have");
    assert!(e.to_string().contains("amount is not a field of XOrderNote"), "{e}");

    // ───────────── two tables of rows in one shared table, never mixed ─────────────
    let a = k
        .insert(
            &admin,
            "XOrder",
            obj(json!({"code": "A", "region": "north",
                "lines": [{"item": "apple", "email": "lena@home.test", "amount": "0.1"}, {"item": "plum", "amount": "0.2"}],
                "notes": [{"item": "pear", "email": "nora@home.test"}]})),
        )
        .await
        .expect("order A");
    let b = k
        .insert(&admin, "XOrder", obj(json!({"code": "B", "lines": [{"item": "pear", "email": "nora@home.test"}], "notes": [{"item": "fig"}]})))
        .await
        .expect("order B");
    assert_eq!(col(&a, "lines", "item"), [json!("apple"), json!("plum")]);
    assert_eq!(col(&a, "notes", "item"), [json!("pear")]);
    assert_eq!(col(&b, "lines", "item"), [json!("pear")]);
    let loaded = k.get(&admin, "XOrder", id_of(&a), false).await.unwrap();
    assert_eq!((col(&loaded, "lines", "item"), col(&loaded, "notes", "item")), (col(&a, "lines", "item"), col(&a, "notes", "item")));
    // A filter on a row field reads that table only.
    assert_eq!(filtered(&k, &admin, json!({"lines.item": "pear"})).await, ["B"]);
    assert_eq!(filtered(&k, &admin, json!({"notes.item": "pear"})).await, ["A"]);
    assert_eq!(filtered(&k, &admin, json!({"lines.item": "fig"})).await, Vec::<String>::new());
    // Lines are no records of their own.
    assert!(k.list(&admin, "XOrderLine", &ListQuery::default()).await.is_err());
    assert!(k.insert(&admin, "XOrderLine", obj(json!({"item": "loose"}))).await.is_err());

    // Every kind of edit to one table leaves the other as it was.
    let lines = a["lines"].as_array().unwrap().clone();
    let note_ids = col(&a, "notes", "id");
    let edited = k
        .update(
            &admin,
            "XOrder",
            id_of(&a),
            obj(json!({"lines": [
                {"id": lines[1]["id"], "item": "plum", "amount": "0.5"},
                {"item": "kiwi", "amount": "1"},
                // A row id of the other table is no way into it: the row is a new one here.
                {"id": note_ids[0], "item": "intruder"}
            ]})),
            None,
        )
        .await
        .expect("reorder, change, add and remove lines");
    assert_eq!(col(&edited, "lines", "item"), [json!("plum"), json!("kiwi"), json!("intruder")]);
    assert_eq!(col(&edited, "lines", "id")[0], lines[1]["id"], "a kept row keeps its id");
    assert_ne!(col(&edited, "lines", "id")[2], note_ids[0]);
    assert_eq!((col(&edited, "notes", "item"), col(&edited, "notes", "id")), (vec![json!("pear")], note_ids.clone()));
    let again = k.get(&admin, "XOrder", id_of(&a), false).await.unwrap();
    assert_eq!(col(&again, "notes", "item"), [json!("pear")], "the note survives as it was");
    assert_eq!(w.count("select count(*) from custom_record where tenant_id = $1 and parent_id = $2", Some(id_of(&a))).await, 4, "three lines, one note");
    let edited = k.update(&admin, "XOrder", id_of(&a), obj(json!({"notes": []})), None).await.unwrap();
    assert_eq!(edited["lines"].as_array().unwrap().len(), 3, "emptying the notes leaves the lines");

    // ───────────── per-row validation, rules that read the parent, the cap ─────────────
    let e = k.insert(&admin, "XOrder", obj(json!({"code": "R1", "lines": [{"amount": "1"}]}))).await.expect_err("a line without its item");
    let Error::Validation(errs) = &e else { panic!("{e}") };
    assert!(errs.iter().any(|f| f.field == "item" && f.row == Some(("lines".into(), 0))), "{errs:?}");
    let e = k
        .insert(&admin, "XOrder", obj(json!({"code": "R2", "region": "frozen", "lines": [{"item": "ice", "amount": "0"}, {"item": "ice", "amount": "2"}]})))
        .await
        .expect_err("a row rule reading its parent");
    let Error::Validation(errs) = &e else { panic!("{e}") };
    assert_eq!(errs.iter().map(|f| (f.field.as_str(), f.row.clone())).collect::<Vec<_>>(), [("open_region", Some(("lines".to_string(), 1)))]);
    // (The notes, whose rows have no rule reading the parent: such a rule reads all of it per row.)
    let many = |n: usize| Value::Array((0..n).map(|i| json!({"item": format!("i{i}")})).collect());
    let e = k.insert(&admin, "XOrder", obj(json!({"code": "R3", "notes": many(5_001)}))).await.expect_err("more rows than a table holds");
    assert!(e.to_string().contains("at most 5000 rows"), "{e}");
    let full = k.insert(&admin, "XOrder", obj(json!({"code": "R4", "notes": many(5_000), "lines": [{"item": "one"}]}))).await.expect("a full table");
    assert_eq!((full["notes"].as_array().unwrap().len(), full["lines"].as_array().unwrap().len()), (5_000, 1));
    let reloaded = k.get(&admin, "XOrder", id_of(&full), false).await.unwrap();
    assert_eq!((reloaded["notes"].as_array().unwrap().len(), &reloaded["line_count"]), (5_000, &json!(1)));
    k.delete(&admin, "XOrder", id_of(&full), None).await.unwrap();

    // ───────────── exact totals over the lines ─────────────
    assert_eq!((&a["total"], &a["line_count"]), (&json!("0.3"), &json!(2)), "0.1 + 0.2 is 0.3 here, not 0.30000000000000004");
    let again = k.get(&admin, "XOrder", id_of(&a), false).await.unwrap();
    assert_eq!((&again["total"], &again["line_count"]), (&json!("1.5"), &json!(3)), "recomputed on update");
    let big = k
        .insert(
            &admin,
            "XOrder",
            obj(json!({"code": "BIG", "region": "big", "total": "7", "line_count": 99,
                "lines": [{"item": "ship", "amount": "1234567890123456789"}, {"item": "fee", "amount": "0.01"}]})),
        )
        .await
        .expect("a 19-digit order");
    assert_eq!((&big["total"], &big["line_count"]), (&json!("1234567890123456789.01"), &json!(2)), "exact, and what the client sent is ignored");
    let e = k
        .insert(&admin, "XOrder", obj(json!({"code": "HUGE", "region": "south", "lines": [{"item": "ship", "amount": "2000000"}]})))
        .await
        .expect_err("the order's own rule reads its total");
    assert!(e.to_string().contains("large orders go to region big"), "{e}");
    let draft = k
        .preview(&admin, "XOrder", obj(json!({"code": "P", "total": "1", "lines": [{"item": "a", "amount": "0.1"}, {"item": "b", "amount": "0.2"}]})))
        .await
        .unwrap();
    assert_eq!((&draft["total"], &draft["line_count"]), (&json!("0.3"), &json!(2)), "the draft preview shows it");
    // On a package's entity too.
    let basket = k.insert(&admin, "Basket", obj(json!({"name": "b1", "items": [{"price": "0.1"}, {"price": "0.2"}]}))).await.unwrap();
    assert_eq!(basket["total"], json!("0.3"));

    // ───────────── unique keys ─────────────
    let (x, y) = tokio::join!(
        k.insert(&admin, "XOrder", obj(json!({"code": "SAME", "lines": [{"item": "x"}]}))),
        k.insert(&admin, "XOrder", obj(json!({"code": "SAME", "lines": [{"item": "y"}]})))
    );
    let lost = match (&x, &y) {
        (Ok(_), Err(e)) | (Err(e), Ok(_)) => e,
        _ => panic!("exactly one of two saves at once wins: {x:?} {y:?}"),
    };
    assert!(matches!(lost, Error::Conflict(m) if m == "another Order already has this Code"), "{lost:?}");
    let first = k.insert(&admin, "XOrder", obj(json!({"code": "C1", "ref_a": "K", "ref_b": "1"}))).await.unwrap();
    k.insert(&admin, "XOrder", obj(json!({"code": "C2", "ref_a": "K", "ref_b": "2"}))).await.expect("another composite value");
    let e = k.insert(&admin, "XOrder", obj(json!({"code": "C3", "ref_a": "K", "ref_b": "1"}))).await.expect_err("the same composite value");
    assert!(e.to_string().contains("Ref A and Ref B"), "{e}");
    for code in ["E1", "E2"] {
        k.insert(&admin, "XOrder", obj(json!({"code": code, "ref_a": "K"}))).await.expect("an empty part takes no part");
    }
    k.insert(&admin, "XOrder", obj(json!({"ref_a": "only"}))).await.expect("no code at all");
    k.insert(&admin, "XOrder", obj(json!({"ref_a": "only too", "code": ""}))).await.expect("an empty code collides with nothing");
    // A record that changes its value frees the old one.
    k.update(&admin, "XOrder", id_of(&first), obj(json!({"code": "C1b", "ref_b": "9"})), None).await.unwrap();
    k.insert(&admin, "XOrder", obj(json!({"code": "C1", "ref_a": "K", "ref_b": "1"}))).await.expect("the old values are free");
    assert!(k.update(&admin, "XOrder", id_of(&first), obj(json!({"region": "west"})), None).await.is_ok(), "a record keeps its own values");
    // Deleting and cancelling free them too.
    let gone = k.insert(&admin, "XOrder", obj(json!({"code": "D1"}))).await.unwrap();
    k.delete(&admin, "XOrder", id_of(&gone), None).await.unwrap();
    let held = k.insert(&admin, "XOrder", obj(json!({"code": "D1"}))).await.expect("a deleted record's value is free");
    k.submit(&admin, "XOrder", id_of(&held), None).await.unwrap();
    assert!(k.insert(&admin, "XOrder", obj(json!({"code": "D1"}))).await.is_err(), "a submitted document holds its value");
    k.cancel(&admin, "XOrder", id_of(&held), None).await.unwrap();
    k.insert(&admin, "XOrder", obj(json!({"code": "D1"}))).await.expect("a cancelled document's value is free");
    // Derived values never touch a key.
    let mut tx = k.db.tenant_tx(w.tenant).await.unwrap();
    let e = k.set_values_tx(&admin, &mut tx, "XOrder", id_of(&first), obj(json!({"code": "sneaky"}))).await.expect_err("set_values on a key");
    assert!(e.to_string().contains("unique key"), "{e}");
    tx.rollback().await.unwrap();
    // Another workspace has keys of its own.
    let other = workspace(&k, "other").await;
    for def in [line_def(), note_def(), order_def()] {
        define(&k, &other.admin().await, def).await.unwrap();
    }
    let theirs = other.admin().await;
    k.insert(&theirs, "XOrder", obj(json!({"code": "C1", "ref_a": "K", "ref_b": "1"}))).await.expect("the same values in another workspace");
    assert_eq!(filtered(&k, &theirs, json!({})).await, ["C1"], "and nothing of the first");

    // ───────────── keys follow the definition ─────────────
    let tag = |unique: bool| json!({"name": "XTag", "label": "Tag", "kind": "master", "title_field": "name", "fields": [{"name": "name", "type": "data", "label": "Name", "unique": unique}]});
    let plain = define(&k, &admin, tag(false)).await.unwrap();
    let admin = w.admin().await;
    for name in ["dup", "dup", "solo", "dup2", "dup2"] {
        k.insert(&admin, "XTag", obj(json!({"name": name}))).await.unwrap();
    }
    k.set_customization_active(&admin, plain, false).await.unwrap();
    let e = define(&k, &w.admin().await, tag(true)).await.expect_err("a key over records that share values");
    assert!(e.to_string().contains("records already share dup, dup2"), "{e}");
    k.set_customization_active(&admin, plain, true).await.unwrap();
    let admin = w.admin().await;
    let dups = k
        .list(&admin, "XTag", &ListQuery { filters: ferrum_kernel::query::parse_filters(&json!({"name": "dup"})).unwrap(), ..Default::default() })
        .await
        .unwrap();
    k.delete(&admin, "XTag", dups.rows[0]["id"].as_str().unwrap().parse().unwrap(), None).await.unwrap();
    let dups = k
        .list(&admin, "XTag", &ListQuery { filters: ferrum_kernel::query::parse_filters(&json!({"name": "dup2"})).unwrap(), ..Default::default() })
        .await
        .unwrap();
    k.delete(&admin, "XTag", dups.rows[0]["id"].as_str().unwrap().parse().unwrap(), None).await.unwrap();
    k.set_customization_active(&admin, plain, false).await.unwrap();
    let keyed = define(&k, &w.admin().await, tag(true)).await.expect("a key over distinct values");
    assert_eq!(w.count("select count(*) from custom_key where tenant_id = $1 and entity = 'XTag'", None).await, 3, "filled from the records");
    let admin = w.admin().await;
    assert!(k.insert(&admin, "XTag", obj(json!({"name": "solo"}))).await.is_err(), "and enforced");
    k.set_customization_active(&admin, keyed, false).await.unwrap();
    assert_eq!(w.count("select count(*) from custom_key where tenant_id = $1 and entity = 'XTag'", None).await, 0, "a key that goes takes its rows");

    // ───────────── deleting takes the lines and keys with the record ─────────────
    let admin = w.admin().await;
    let id_b = id_of(&b);
    k.delete(&admin, "XOrder", id_b, None).await.unwrap();
    assert_eq!(w.count("select count(*) from custom_record where tenant_id = $1 and parent_id = $2", Some(id_b)).await, 0, "the rows went with it");
    assert_eq!(w.count("select count(*) from custom_key where tenant_id = $1 and record_id = $2", Some(id_b)).await, 0);
    assert_eq!(col(&k.get(&admin, "XOrder", id_of(&a), false).await.unwrap(), "lines", "item").len(), 3, "and only its own");

    // ───────────── a portal path through a custom table ─────────────
    let rule: PermissionDef = serde_json::from_value(json!({"entity": "XOrder", "read": true, "via": "notes.email"})).unwrap();
    k.set_role_permission(&admin, "Order Contacts", &rule).await.expect("a path through custom lines");
    k.set_role_portal(&admin, "Order Contacts", true).await.unwrap();
    k.update(&admin, "XOrder", id_of(&a), obj(json!({"notes": [{"item": "pear", "email": "nora@home.test"}]})), None).await.unwrap();
    let c = k
        .insert(
            &admin,
            "XOrder",
            obj(json!({"code": "LINEMAIL", "lines": [{"item": "pear", "email": "nora@home.test"}], "notes": [{"email": "else@home.test"}]})),
        )
        .await
        .unwrap();
    let nora = k.create_user(&admin, "nora@home.test", "Nora", None, &[], false).await.unwrap();
    k.set_user_kind(&admin, nora, "portal").await.unwrap();
    k.set_user_roles_until(&admin, nora, &["Order Contacts".to_string()], &Default::default()).await.unwrap();
    let nora = k.load_ctx(w.tenant, nora, None, "it".into()).await.unwrap();
    assert_eq!(filtered(&k, &nora, json!({})).await, ["A"], "her email in a line's field of the same name reaches nothing");
    assert!(k.get(&nora, "XOrder", id_of(&a), false).await.is_ok());
    assert!(k.get(&nora, "XOrder", id_of(&c), false).await.is_err());

    // ───────────── the workspace's export ─────────────
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
    let manifest: Value = serde_json::from_str(&files["manifest.json"]).unwrap();
    assert!(!manifest["tables_left_out"].as_array().unwrap().iter().any(|t| t == "custom_record" || t == "custom_key"), "{manifest}");
    assert!(!files.keys().any(|n| n.starts_with("records/XOrderLine") || n.starts_with("records/XOrderNote")), "lines are no records of their own");
    let orders: Vec<Value> =
        files.iter().filter(|(n, _)| n.starts_with("records/XOrder/")).flat_map(|(_, s)| s.lines().map(|l| serde_json::from_str(l).unwrap())).collect();
    let exported = orders.iter().find(|o| o["code"] == json!("A")).expect("order A");
    assert_eq!(exported["lines"].as_array().unwrap().iter().map(|r| r["item"].clone()).collect::<Vec<_>>(), [json!("plum"), json!("kiwi"), json!("intruder")]);
    assert_eq!(exported["notes"].as_array().unwrap().iter().map(|r| r["item"].clone()).collect::<Vec<_>>(), [json!("pear")]);
}
