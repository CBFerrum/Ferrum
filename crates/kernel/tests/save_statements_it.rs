//! K10: statements per save and per preview depend on the linked records, not on the number of lines, against a real
//! PostgreSQL (`FERRUM_SAVES_TEST_DATABASE_URL`; the database is reset first). An order's lines link an item that has a
//! filter, fetch its price and unit, and fetch (through a second fetch that reads the first) its category's name. The
//! same order is saved and previewed with 4 lines and with 400 that link the same four items, and the statements of
//! each (every query sqlx runs is an event of target `sqlx::query`) are counted: equal at both sizes. Errors of a
//! filtered link are reported in line order, as before.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use uuid::Uuid;

static COUNTING: AtomicBool = AtomicBool::new(false);
static STATEMENTS: AtomicUsize = AtomicUsize::new(0);

struct Statements;

impl tracing::Subscriber for Statements {
    fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
        m.target() == "sqlx::query"
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        if COUNTING.load(Ordering::SeqCst) {
            STATEMENTS.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// The statements `f` runs.
async fn counted<T>(f: impl std::future::Future<Output = T>) -> (T, usize) {
    STATEMENTS.store(0, Ordering::SeqCst);
    COUNTING.store(true, Ordering::SeqCst);
    let out = f.await;
    COUNTING.store(false, Ordering::SeqCst);
    (out, STATEMENTS.load(Ordering::SeqCst))
}

const ENTITIES: &str = r#"
- name: ShopCategory
  label: Category
  kind: master
  fields:
    - {name: name, type: data}
- name: ShopItem
  label: Item
  kind: master
  fields:
    - {name: name, type: data}
    - {name: price, type: currency}
    - {name: unit, type: data}
    - {name: active, type: check}
    - {name: category, type: link, to: ShopCategory}
- name: ShopOrder
  label: Order
  kind: master
  fields:
    - {name: name, type: data}
    - {name: lines, type: table, of: ShopOrderLine}
- name: ShopOrderLine
  label: Order Line
  kind: child
  fields:
    - {name: item, type: link, to: ShopItem, link_filters: {active: true}}
    - {name: price, type: currency, fetch_from: item.price}
    - {name: unit, type: data, fetch_from: item.unit}
    - {name: category, type: link, to: ShopCategory, fetch_from: item.category}
    - {name: category_name, type: data, fetch_from: category.name}
"#;

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-save-statements-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("shop/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("shop/package.yaml"), "package: shop\nversion: 0.1.0\nlabel: Shop\ndepends: [core]\n").unwrap();
    std::fs::write(dir.join("shop/entities/shop.yaml"), ENTITIES).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> String {
    d["id"].as_str().unwrap().to_string()
}

#[test]
fn lines_do_not_multiply_queries() {
    tracing::subscriber::set_global_default(Statements).expect("the statement counter");
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

fn order(code: &str, items: &[String], n: usize) -> Doc {
    let lines: Vec<Value> = (0..n).map(|i| json!({"item": items[i % items.len()]})).collect();
    obj(json!({"name": code, "lines": lines}))
}

async fn scenario() {
    let url = std::env::var("FERRUM_SAVES_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_saves_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k: Arc<Kernel> = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["shop".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin: Ctx = k.load_ctx(tenant, session.user, None, "it".into()).await.unwrap();

    let mut cats = Vec::new();
    for c in ["red", "blue"] {
        cats.push(id_of(&k.insert(&admin, "ShopCategory", obj(json!({"name": c}))).await.unwrap()));
    }
    let mut items = Vec::new();
    for i in 0..4 {
        let item = json!({"name": format!("i{i}"), "price": format!("{}.50", i + 1), "unit": format!("u{i}"), "active": true, "category": cats[i % 2]});
        items.push(id_of(&k.insert(&admin, "ShopItem", obj(item)).await.unwrap()));
    }
    let off = id_of(&k.insert(&admin, "ShopItem", obj(json!({"name": "off", "price": "9", "unit": "x", "active": false}))).await.unwrap());

    // The values fetched, through the chain (item, then its category's name), and the statements, at two sizes.
    let mut inserts = Vec::new();
    let mut previews = Vec::new();
    let mut updates = Vec::new();
    for n in [4usize, 400] {
        let (made, saved) = counted(k.insert(&admin, "ShopOrder", order(&format!("O{n}"), &items, n))).await;
        let made = made.expect("an order of lines linking active items");
        let lines = made["lines"].as_array().unwrap();
        assert_eq!(lines.len(), n);
        for (i, l) in lines.iter().enumerate() {
            let (price, unit, cat) = (format!("{}.5", i % 4 + 1), format!("u{}", i % 4), if i % 2 == 0 { "red" } else { "blue" });
            assert_eq!(
                (l["price"].as_str().map(|p| p.trim_end_matches('0')), l["unit"].as_str(), l["category_name"].as_str()),
                (Some(price.as_str()), Some(unit.as_str()), Some(cat)),
                "line {i}: {l}"
            );
        }
        let (shown, previewed) = counted(k.preview(&admin, "ShopOrder", order(&format!("P{n}"), &items, n))).await;
        let shown = shown.expect("a preview");
        assert_eq!(
            (shown["lines"][n - 1]["unit"].clone(), shown["lines"][n - 1]["category_name"].clone()),
            (json!(format!("u{}", (n - 1) % 4)), json!(if (n - 1) % 2 == 0 { "red" } else { "blue" }))
        );
        // An edit that adds as many lines as the order has and changes none of the others.
        let id: Uuid = made["id"].as_str().unwrap().parse().unwrap();
        let mut all = lines.clone();
        all.extend(order("x", &items, n)["lines"].as_array().unwrap().clone());
        let (edited, updated) = counted(k.update(&admin, "ShopOrder", id, obj(json!({"lines": all})), None)).await;
        assert_eq!(edited.expect("more lines")["lines"].as_array().unwrap().len(), 2 * n);
        println!("STATEMENTS {n} lines: insert {saved}, preview {previewed}, update {updated}");
        inserts.push(saved);
        previews.push(previewed);
        updates.push(updated);
    }
    assert_eq!(inserts[0], inserts[1], "a save's statements do not grow with its lines");
    assert_eq!(previews[0], previews[1], "a preview's statements do not grow with its lines");
    assert_eq!(updates[0], updates[1], "an edit's statements do not grow with its lines");

    // A filtered link that is refused, on lines 2 and 5 of 6: both reported, in line order, as the lines they are.
    let mut bad = order("BAD", &items, 6);
    for i in [2, 5] {
        bad["lines"][i]["item"] = json!(off);
    }
    let e = k.insert(&admin, "ShopOrder", bad).await.expect_err("an inactive item");
    let Error::Validation(fields) = &e else { panic!("{e}") };
    let said: Vec<String> = fields.iter().map(|f| format!("{:?} {} {}", f.row, f.field, f.message)).collect();
    println!("ERRORS {said:#?}");
    assert_eq!(fields.len(), 2, "{said:?}");
    assert!(said[0].contains("(\"lines\", 2)") && said[1].contains("(\"lines\", 5)") && said.iter().all(|s| s.contains("active must be true")), "{said:?}");
    let e = k.preview(&admin, "ShopOrder", order("GONE", &[Uuid::now_v7().to_string()], 3)).await;
    println!("PREVIEW of a missing item: {}", e.as_ref().map(|_| "ok".to_string()).unwrap_or_else(|e| e.to_string()));
    let e = k.insert(&admin, "ShopOrder", order("GONE", &[Uuid::now_v7().to_string()], 3)).await.expect_err("a missing item");
    let Error::Validation(fields) = &e else { panic!("{e}") };
    assert_eq!(fields.len(), 3);
    assert!(fields.iter().all(|f| f.message.contains("does not exist")), "{fields:?}");
}
