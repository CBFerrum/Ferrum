//! Studio over a custom entity's lines (BD-06), against a real PostgreSQL (`FERRUM_STUDIO_TEST_DATABASE_URL`;
//! the database is reset first). An order defined by an administrator has two tables whose rows share
//! `custom_record` and a field of the same name: grouping, measuring and filtering by one table's field
//! never reads the other's rows.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, EntityDef, OverlayOp};
use ferrum_studio::spec::Spec;
use serde_json::{Value, json};

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

#[test]
fn studio_reads_one_custom_table_at_a_time() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_STUDIO_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_studio_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::from_packages(ferrum_domain::packages().expect("platform packages")).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "lines".into(),
            name: "Lines".into(),
            admin_email: "admin@lines.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("lines", "admin@lines.test", "Correct-Horse-9").await.expect("login");
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.expect("ctx");
    for def in [
        json!({"name": "XLine", "label": "Line", "kind": "child", "fields": [{"name": "item", "type": "data"}, {"name": "amount", "type": "currency"}]}),
        json!({"name": "XNote", "label": "Note", "kind": "child", "fields": [{"name": "item", "type": "data"}, {"name": "amount", "type": "currency"}]}),
        json!({"name": "XOrder", "label": "Order", "kind": "master", "title_field": "code", "fields": [
            {"name": "code", "type": "data"}, {"name": "lines", "type": "table", "of": "XLine"}, {"name": "notes", "type": "table", "of": "XNote"}]}),
    ] {
        let entity: EntityDef = serde_json::from_value(def).unwrap();
        let name = entity.name.clone();
        k.add_customization(&admin, &name, OverlayOp::DefineEntity { entity: Box::new(entity) }, None).await.expect("defined");
    }
    let admin = k.load_ctx(tenant, session.user, None, "it".into()).await.expect("ctx");
    for (code, lines, notes) in [
        ("A", json!([{"item": "apple", "amount": "1"}, {"item": "apple", "amount": "2"}]), json!([{"item": "pear", "amount": "100"}])),
        ("B", json!([{"item": "pear", "amount": "5"}]), json!([{"item": "apple", "amount": "1000"}])),
    ] {
        k.insert(&admin, "XOrder", obj(json!({"code": code, "lines": lines, "notes": notes}))).await.expect("order");
    }
    let q = |spec: Value| {
        let k = k.clone();
        let admin = admin.clone();
        async move { ferrum_studio::run::run(&k, &admin, &Spec::parse(&spec).unwrap()).await.unwrap_or_else(|e| panic!("query {spec}: {e:?}")) }
    };

    let by_item =
        q(json!({"entity": "XOrder", "group_by": [{"field": "lines.item"}], "measures": [{"fn": "sum", "field": "lines.amount"}, {"fn": "count"}]})).await;
    assert_eq!(by_item["meta"]["grain"], json!("lines"), "{by_item}");
    let mut got: Vec<(Value, Value)> = by_item["rows"].as_array().unwrap().iter().map(|r| (r["lines.item"].clone(), r["sum(lines.amount)"].clone())).collect();
    got.sort_by_key(|p| p.0.to_string());
    assert_eq!(got, [(json!("apple"), json!("3")), (json!("pear"), json!("5"))], "{by_item}");
    assert_eq!(by_item["totals"]["sum(lines.amount)"], json!("8"), "no note's amount among the lines");

    let notes = q(json!({"entity": "XOrder", "measures": [{"fn": "sum", "field": "notes.amount"}]})).await;
    assert_eq!(notes["totals"]["sum(notes.amount)"], json!("1100"), "{notes}");

    // Filtering by one table's field picks the documents with such a row in that table only.
    let with_pear_line =
        q(json!({"entity": "XOrder", "filters": [["lines.item", "=", "pear"]], "group_by": [{"field": "code"}], "measures": [{"fn": "count"}]})).await;
    let codes: Vec<&Value> = with_pear_line["rows"].as_array().unwrap().iter().map(|r| &r["code"]).collect();
    assert_eq!(codes, [&json!("B")], "{with_pear_line}");
    let with_pear_note =
        q(json!({"entity": "XOrder", "filters": [["notes.item", "=", "pear"]], "group_by": [{"field": "code"}], "measures": [{"fn": "count"}]})).await;
    let codes: Vec<&Value> = with_pear_note["rows"].as_array().unwrap().iter().map(|r| &r["code"]).collect();
    assert_eq!(codes, [&json!("A")], "{with_pear_note}");
    // At the lines' grain, a filter on the notes still asks the notes.
    let apples_where_noted_pear =
        q(json!({"entity": "XOrder", "filters": [["notes.item", "=", "pear"]], "group_by": [{"field": "lines.item"}], "measures": [{"fn": "sum", "field": "lines.amount"}]}))
            .await;
    assert_eq!(apples_where_noted_pear["totals"]["sum(lines.amount)"], json!("3"), "{apples_where_noted_pear}");
}
