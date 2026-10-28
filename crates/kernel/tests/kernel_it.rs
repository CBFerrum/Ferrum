//! Kernel integration test against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`). The database is reset first.

use ferrum_kernel::query::parse_filters;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::{Catalog, OverlayOp};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

fn obj(v: Value) -> ferrum_kernel::Doc {
    v.as_object().cloned().unwrap()
}

async fn fresh_kernel() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages")).expect("catalog");
    Kernel::boot(db, catalog, HookRegistry::default(), KernelSettings::default(), None).await.expect("boot")
}

#[tokio::test]
async fn kernel_end_to_end() {
    let k = fresh_kernel().await;

    let t1 = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme Corp".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into()],
            settings: Some(json!({"fiscal_year_start_month": 4})),
        })
        .await
        .expect("tenant");

    let session = k.login("acme", "ADMIN@acme.test", "Correct-Horse-9").await.expect("login");
    assert!(k.login("acme", "admin@acme.test", "wrong-password").await.is_err());
    let ctx = k.load_ctx(t1, session.user, Some(session.token_version), "it".into()).await.expect("ctx");

    // Seeds ran: currencies and the item-group tree.
    let inr = k.list(&ctx, "Currency", &ListQuery { filters: parse_filters(&json!({"code": "INR"})).unwrap(), ..Default::default() }).await.unwrap();
    assert_eq!(inr.rows.len(), 1);
    let inr_id = inr.rows[0]["id"].as_str().unwrap().to_string();

    // Company + rule validation (bad currency code is rejected by the CEL rule).
    let bad = k.insert(&ctx, "Currency", obj(json!({"code": "rupee", "currency_name": "x"}))).await;
    assert!(matches!(bad, Err(Error::Validation(_))), "{bad:?}");
    let company = k.insert(&ctx, "Company", obj(json!({"company_name": "Acme India", "abbr": "AI", "default_currency": inr_id}))).await.expect("company");
    let company_id = company["id"].as_str().unwrap().to_string();
    assert_eq!(company["fy_start_month"], json!(4), "literal default applied");

    // Numbered master with defaults; unknown fields rejected.
    assert!(k.insert(&ctx, "Customer", obj(json!({"customer_name": "X", "nope": 1}))).await.is_err());
    let c = k.insert(&ctx, "Customer", obj(json!({"customer_name": "Globex", "credit_limit": "1,000.50"}))).await.expect("customer");
    assert_eq!(c["number"], json!("CUST-00001"));
    assert_eq!(c["credit_limit"], json!("1000.5"));
    assert_eq!(c["customer_type"], json!("Company"));
    let cid: uuid::Uuid = c["id"].as_str().unwrap().parse().unwrap();

    // Optimistic concurrency.
    let upd = k.update(&ctx, "Customer", cid, obj(json!({"email": "AP@GLOBEX.test"})), Some(1)).await.expect("update");
    assert_eq!(upd["email"], json!("ap@globex.test"));
    assert_eq!(upd["version"], json!(2));
    let stale = k.update(&ctx, "Customer", cid, obj(json!({"phone": "+91 99"})), Some(1)).await;
    assert!(matches!(stale, Err(Error::Conflict(_))), "{stale:?}");

    // Customization: custom field lands in ext, is filterable and survives.
    k.add_customization(
        &ctx,
        "Customer",
        serde_json::from_value::<OverlayOp>(json!({"op": "add_field", "field": {"name": "x_tier", "type": "select", "options": ["Gold", "Silver"]}})).unwrap(),
        None,
    )
    .await
    .expect("customize");
    let bad_custom = k
        .add_customization(
            &ctx,
            "Customer",
            serde_json::from_value::<OverlayOp>(json!({"op": "add_field", "field": {"name": "tier", "type": "data"}})).unwrap(),
            None,
        )
        .await;
    assert!(bad_custom.is_err(), "custom fields need the x_ prefix");
    let ctx = k.load_ctx(t1, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let up2 = k.update(&ctx, "Customer", cid, obj(json!({"x_tier": "Gold"})), None).await.expect("set custom");
    assert_eq!(up2["x_tier"], json!("Gold"));
    let gold = k
        .list(&ctx, "Customer", &ListQuery { filters: parse_filters(&json!([["x_tier", "=", "Gold"]])).unwrap(), with_count: true, ..Default::default() })
        .await
        .unwrap();
    assert_eq!(gold.total, Some(1));

    // Tree cycle prevention and link filters (parent must be a group).
    let root = k
        .list(&ctx, "ItemGroup", &ListQuery { filters: parse_filters(&json!({"group_name": "All Item Groups"})).unwrap(), ..Default::default() })
        .await
        .unwrap();
    let root_id: uuid::Uuid = root.rows[0]["id"].as_str().unwrap().parse().unwrap();
    let leaf =
        k.list(&ctx, "ItemGroup", &ListQuery { filters: parse_filters(&json!({"group_name": "Products"})).unwrap(), ..Default::default() }).await.unwrap();
    let leaf_id = leaf.rows[0]["id"].as_str().unwrap().to_string();
    let cyc = k.update(&ctx, "ItemGroup", root_id, obj(json!({"parent_group": leaf_id})), None).await;
    assert!(cyc.is_err(), "leaf is not a group, and the link would create a cycle: {cyc:?}");

    // Foreign keys: a company in use cannot be deleted; customer deletion works.
    let fy = k
        .insert(&ctx, "FiscalYear", obj(json!({"year_name": "2026-27", "company": company_id, "start_date": "2026-04-01", "end_date": "2027-03-31"})))
        .await
        .expect("fiscal year");
    let overlap =
        k.insert(&ctx, "FiscalYear", obj(json!({"year_name": "X", "company": company_id, "start_date": "2026-06-01", "end_date": "2027-05-31"}))).await;
    assert!(matches!(overlap, Err(Error::Conflict(_))), "exclusion constraint: {overlap:?}");
    let del = k.delete(&ctx, "Company", company_id.parse().unwrap(), None).await;
    assert!(matches!(del, Err(Error::Conflict(_))), "{del:?}");
    let _ = fy;

    // Pagination with cursors.
    for i in 0..25 {
        k.insert(&ctx, "Customer", obj(json!({"customer_name": format!("Bulk {i:02}")}))).await.unwrap();
    }
    let p1 = k.list(&ctx, "Customer", &ListQuery { limit: Some(10), sort: Some("created_at desc".into()), ..Default::default() }).await.unwrap();
    assert_eq!(p1.rows.len(), 10);
    let p2 = k
        .list(&ctx, "Customer", &ListQuery { limit: Some(10), sort: Some("created_at desc".into()), cursor: p1.next_cursor.clone(), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(p2.rows.len(), 10);
    assert!(p1.rows.iter().all(|a| p2.rows.iter().all(|b| a["id"] != b["id"])));

    // Tenant isolation: a second tenant sees none of the first tenant's data, even by id.
    let t2 = k
        .create_tenant(NewTenant {
            slug: "globex".into(),
            name: "Globex".into(),
            admin_email: "admin@globex.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into()],
            settings: None,
        })
        .await
        .unwrap();
    let s2 = k.login("globex", "admin@globex.test", "Correct-Horse-9").await.unwrap();
    let ctx2 = k.load_ctx(t2, s2.user, Some(s2.token_version), "it2".into()).await.unwrap();
    assert!(matches!(k.get(&ctx2, "Customer", cid, false).await, Err(Error::NotFound(_))));
    let listed = k.list(&ctx2, "Customer", &ListQuery::default()).await.unwrap();
    assert!(listed.rows.is_empty());
    // Raw SQL in tenant 2's transaction cannot see tenant 1's rows either (RLS).
    let mut tx = k.db.tenant_tx(t2).await.unwrap();
    let n: i64 = sqlx::query_scalar("select count(*) from e_customer").fetch_one(&mut *tx).await.unwrap();
    assert_eq!(n, 0);
    tx.rollback().await.unwrap();

    // Audit trail is chained and verifiable; history cannot be edited.
    let mut tx = k.db.tenant_tx(t1).await.unwrap();
    assert_eq!(ferrum_kernel::audit::verify_record(&mut tx, t1, "Customer", cid).await.unwrap(), None);
    let tamper = sqlx::query("update audit_event set action = 'x'").execute(&mut *tx).await;
    assert!(tamper.is_err());
    tx.rollback().await.unwrap();

    // Outbox recorded the events and the dispatcher drains them.
    let drained = k.dispatch_outbox_once().await.unwrap();
    assert!(drained > 0);

    // Print renders.
    let html = k.render_print(&ctx, "Customer", cid).await.unwrap();
    assert!(html.contains("Globex"));
}
