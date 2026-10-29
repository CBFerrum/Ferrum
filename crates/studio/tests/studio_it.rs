//! End-to-end Studio test against a real PostgreSQL (`FERRUM_STUDIO_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_studio_test`). The database is reset first.
//!
//! A trading company sells to three customers in two customer groups over three months (plus a
//! draft, a cancelled invoice and a second company). A sales rep sees only their own invoices, a
//! sales user lacks the credit field group, a scoped user sees one company, a manager sees all.
//! Every number is asserted exactly: sums, counts, distinct counts, buckets (also in the
//! workspace's time zone), child-table measures, pivot cells and totals, previous periods; then
//! saved reports, charts, cards, dashboards and spaces are validated, run, shared and exported.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, SiteRequest};
use ferrum_meta::{Catalog, OverlayOp, PermissionDef};
use ferrum_studio::spec::Spec;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn uid(doc: &Doc) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

struct T {
    k: Arc<Kernel>,
    tenant: Uuid,
    admin: Ctx,
}

/// A row of a result whose `col` equals `v` (or whose title does).
fn row<'a>(data: &'a Value, col: &str, v: &str) -> &'a Value {
    data["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r[col] == json!(v) || r["_titles"][col] == json!(v))
        .unwrap_or_else(|| panic!("no row with {col} = {v} in {}", data["rows"]))
}

fn rows(data: &Value) -> &Vec<Value> {
    data["rows"].as_array().unwrap()
}

impl T {
    async fn insert(&self, ctx: &Ctx, entity: &str, v: Value) -> Doc {
        self.k.insert(ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let page = self
            .k
            .list(&self.admin, entity, &ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(page.rows.len(), 1, "{entity} {filters}");
        page.rows[0].clone()
    }
    async fn user(&self, email: &str, roles: &[&str]) -> Uuid {
        let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
        self.k.create_user(&self.admin, email, email, Some("Correct-Horse-9"), &roles, false).await.expect("user")
    }
    async fn ctx(&self, user: Uuid) -> Ctx {
        self.k.load_ctx(self.tenant, user, None, "it".into()).await.expect("ctx")
    }
    async fn q(&self, ctx: &Ctx, spec: Value) -> Value {
        let s = Spec::parse(&spec).unwrap_or_else(|e| panic!("spec {spec}: {e:?}"));
        ferrum_studio::run::run(&self.k, ctx, &s).await.unwrap_or_else(|e| panic!("query {spec}: {e:?}"))
    }
    async fn q_err(&self, ctx: &Ctx, spec: Value) -> Error {
        ferrum_studio::run::run(&self.k, ctx, &Spec::parse(&spec).unwrap()).await.expect_err("the query should be refused")
    }
    async fn call(&self, ctx: &Ctx, method: &str, path: &str, query: &[(&str, &str)], body: Value) -> ferrum_kernel::Result<ferrum_kernel::SiteResponse> {
        let site = self.k.hooks.site("studio").expect("studio endpoints are registered").clone();
        let req = SiteRequest {
            tenant: self.tenant,
            user: Some(ctx.clone()),
            method: method.into(),
            path: path.into(),
            query: query.iter().map(|(k, v)| (k.to_string(), json!(v))).collect(),
            headers: Map::new(),
            body: if body.is_null() { vec![] } else { body.to_string().into_bytes() },
            client_ip: "127.0.0.1".into(),
        };
        site.handle(&self.k, req).await
    }
    async fn api(&self, ctx: &Ctx, method: &str, path: &str, query: &[(&str, &str)], body: Value) -> Value {
        let r = self.call(ctx, method, path, query, body).await.unwrap_or_else(|e| panic!("{method} {path}: {e:?}"));
        assert_eq!(r.status, 200);
        serde_json::from_slice(&r.body).unwrap()
    }
}

#[test]
fn studio_end_to_end() {
    // Same runtime shape as the server: unoptimized async hook chains need more than 2 MiB.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_STUDIO_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_studio_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("platform packages");
    packages.extend(ferrum_studio::packages().expect("studio packages"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_studio::register(&mut hooks);
    for source in ferrum_studio::reports() {
        ferrum_domain::reports::add_source(source);
    }
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("partitions");
    ferrum_studio::maintenance(&k).await.expect("studio upkeep");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "studio".into(),
            name: "Studio Traders".into(),
            admin_email: "admin@studio.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["selling".into(), "buying".into(), "studio".into()],
            settings: Some(json!({"fiscal_year_start_month": 4, "timezone": "Asia/Kolkata"})),
        })
        .await
        .expect("tenant");
    let session = k.login("studio", "admin@studio.test", "Correct-Horse-9").await.expect("login");
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    // A custom date-time field (stored in `ext`) to bucket in the workspace's time zone.
    let field = serde_json::from_value(json!({"name": "x_delivered_at", "type": "datetime", "label": "Delivered At"})).unwrap();
    k.add_customization(&admin, "SalesInvoice", OverlayOp::AddField { field, after: None }, None).await.expect("custom field");
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    let t = T { k: k.clone(), tenant, admin };
    let a = t.admin.clone();

    // ───────────── masters ─────────────
    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let acme = uid(&t.insert(&a, "Company", json!({"company_name": "Acme Traders", "abbr": "AT", "default_currency": inr["id"]})).await);
    let beta_co = uid(&t.insert(&a, "Company", json!({"company_name": "Beta Works", "abbr": "BW", "default_currency": inr["id"]})).await);
    let commercial = uid(&t.find_one("CustomerGroup", json!({"group_name": "Commercial"})).await);
    let individual = uid(&t.find_one("CustomerGroup", json!({"group_name": "Individual"})).await);
    let alpha =
        uid(&t.insert(&a, "Customer", json!({"customer_name": "Alpha Traders", "customer_group": commercial.to_string(), "credit_limit": "50000"})).await);
    let beta = uid(&t.insert(&a, "Customer", json!({"customer_name": "Beta Retail", "customer_group": commercial.to_string()})).await);
    let gamma = uid(&t.insert(&a, "Customer", json!({"customer_name": "Gamma Home", "customer_group": individual.to_string()})).await);
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let raw = t.find_one("ItemGroup", json!({"group_name": "Raw Material"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let item = |code: &str, group: &Doc, rate: &str| json!({"item_code": code, "item_name": code.to_lowercase(), "item_group": group["id"], "stock_uom": nos["id"], "standard_rate": rate, "is_stock_item": false});
    let widget = uid(&t.insert(&a, "Item", item("WIDGET", &products, "100")).await);
    let gadget = uid(&t.insert(&a, "Item", item("GADGET", &products, "250")).await);
    let screw = uid(&t.insert(&a, "Item", item("SCREW", &raw, "0.1")).await);
    let bolt = uid(&t.insert(&a, "Item", item("BOLT", &raw, "2.5")).await);

    // ───────────── people ─────────────
    // A sales rep: own invoices only, every master readable (no projects).
    let own = PermissionDef { entity: "SalesInvoice".into(), read: true, write: true, create: true, if_owner: true, ..Default::default() };
    t.k.set_role_permission(&a, "Sales Rep", &own).await.unwrap();
    for e in [
        "Customer",
        "CustomerGroup",
        "Item",
        "ItemGroup",
        "Company",
        "Currency",
        "PriceList",
        "ItemPrice",
        "Uom",
        "Warehouse",
        "Account",
        "TaxTemplate",
        "CostCenter",
        "PricingRule",
    ] {
        t.k.set_role_permission(&a, "Sales Rep", &PermissionDef { entity: e.into(), read: true, ..Default::default() }).await.unwrap();
    }
    let rep_id = t.user("rep@studio.test", &["Sales Rep", "Studio User"]).await;
    let manager_id = t.user("manager@studio.test", &["Sales Manager", "Accounts Manager", "Studio Manager"]).await;
    let viewer_id = t.user("viewer@studio.test", &["Sales User", "Studio User"]).await;
    let scoped_id = t.user("scoped@studio.test", &["Sales User"]).await;
    t.k.set_user_scopes(&a, scoped_id, "Company", &[acme], Some(acme)).await.unwrap();
    let (rep, manager, viewer, scoped) = (t.ctx(rep_id).await, t.ctx(manager_id).await, t.ctx(viewer_id).await, t.ctx(scoped_id).await);

    // ───────────── invoices ─────────────
    let invoice = |ctx: Ctx, company: Uuid, customer: Uuid, date: &'static str, lines: Vec<(Uuid, &'static str)>, extra: Value| {
        let t = &t;
        async move {
            let mut v = json!({"company": company.to_string(), "customer": customer.to_string(), "posting_date": date,
                "items": lines.iter().map(|(i, q)| json!({"item": i.to_string(), "qty": q})).collect::<Vec<_>>()});
            for (key, x) in extra.as_object().unwrap() {
                v[key] = x.clone();
            }
            uid(&t.insert(&ctx, "SalesInvoice", v).await)
        }
    };
    let submit = |id: Uuid| {
        let (t, m) = (&t, &manager);
        async move {
            t.k.submit(m, "SalesInvoice", id, None).await.unwrap_or_else(|e| panic!("submit: {e:?}"));
        }
    };
    let inv1 = invoice(rep.clone(), acme, alpha, "2026-08-05", vec![(widget, "2"), (screw, "3")], json!({})).await;
    let inv2 = invoice(manager.clone(), acme, gamma, "2026-08-20", vec![(gadget, "1")], json!({})).await;
    let inv3 = invoice(rep.clone(), acme, alpha, "2026-09-02", vec![(gadget, "2"), (bolt, "4")], json!({"x_delivered_at": "2026-09-30T20:00:00Z"})).await;
    let inv4 = invoice(manager.clone(), acme, beta, "2026-09-15", vec![(widget, "1"), (screw, "7")], json!({})).await;
    let inv5 = invoice(rep.clone(), acme, gamma, "2026-09-28", vec![(widget, "3")], json!({"x_delivered_at": "2026-09-30T10:00:00Z"})).await;
    let _draft = invoice(rep.clone(), acme, beta, "2026-10-01", vec![(bolt, "10")], json!({})).await;
    let inv7 = invoice(manager.clone(), acme, beta, "2026-09-10", vec![(gadget, "1")], json!({})).await;
    let inv8 = invoice(manager.clone(), beta_co, alpha, "2026-07-15", vec![(widget, "1")], json!({})).await;
    for id in [inv1, inv2, inv3, inv4, inv5, inv7, inv8] {
        submit(id).await;
    }
    t.k.cancel(&manager, "SalesInvoice", inv7, None).await.expect("cancel");
    let g1 = t.k.get(&a, "SalesInvoice", inv1, false).await.unwrap();
    assert_eq!(g1["grand_total"], json!("200.3"), "no taxes, exact decimals: {g1:?}");
    t.k.set_tags(&a, "SalesInvoice", inv3, &["priority".into()]).await.unwrap();

    let aug_sep = json!({"field": "posting_date", "from": "2026-08-01", "to": "2026-09-30"});
    let base = json!([["docstatus", "=", 1], ["company", "=", acme.to_string()]]);

    // ───────────── aggregates ─────────────
    let all = t
        .q(
            &manager,
            json!({"entity": "SalesInvoice", "filters": base, "date_range": aug_sep, "measures": [
                {"fn": "count"}, {"fn": "sum", "field": "grand_total"}, {"fn": "avg", "field": "grand_total"},
                {"fn": "count_distinct", "field": "customer"}, {"fn": "min", "field": "posting_date"}, {"fn": "max", "field": "posting_date"},
                {"fn": "sum", "field": "customer.credit_limit"}]}),
        )
        .await;
    let tot = &all["totals"];
    assert_eq!(
        (&tot["count"], &tot["sum(grand_total)"], &tot["avg(grand_total)"], &tot["count_distinct(customer)"]),
        (&json!(5), &json!("1361"), &json!("272.2"), &json!(3)),
        "{all}"
    );
    assert_eq!((&tot["min(posting_date)"], &tot["max(posting_date)"]), (&json!("2026-08-05"), &json!("2026-09-28")));
    assert_eq!(tot["sum(customer.credit_limit)"], json!("100000"), "the manager reads the credit field group");
    assert_eq!(rows(&all).len(), 1, "measures alone make one row");
    assert_eq!(all["meta"]["timezone"], json!("Asia/Kolkata"));
    assert!(all["meta"]["sql_ms"].as_f64().unwrap() >= 0.0);

    // Group by a link's field, titled; default order: first measure, descending.
    let by_group = t
        .q(
            &manager,
            json!({"entity": "SalesInvoice", "filters": base, "date_range": aug_sep, "group_by": [{"field": "customer.customer_group"}],
            "measures": [{"fn": "count"}, {"fn": "sum", "field": "grand_total"}]}),
        )
        .await;
    assert_eq!(rows(&by_group)[0]["_titles"]["customer.customer_group"], json!("Commercial"), "{by_group}");
    assert_eq!(rows(&by_group)[0]["customer.customer_group"], json!(commercial.to_string()));
    assert_eq!((&rows(&by_group)[0]["count"], &rows(&by_group)[0]["sum(grand_total)"]), (&json!(3), &json!("811")));
    assert_eq!((&rows(&by_group)[1]["count"], &rows(&by_group)[1]["sum(grand_total)"]), (&json!(2), &json!("550")));
    assert_eq!(by_group["columns"][0]["label"], json!("Customer › Customer Group"));
    assert_eq!(by_group["columns"][2]["type"], json!("currency"));

    // Users show by name.
    let by_creator = t
        .q(
            &manager,
            json!({"entity": "SalesInvoice", "filters": base, "date_range": aug_sep, "group_by": [{"field": "created_by"}], "measures": [{"fn": "count"}]}),
        )
        .await;
    assert_eq!(row(&by_creator, "created_by", "rep@studio.test")["count"], json!(3), "{by_creator}");
    assert_eq!(row(&by_creator, "created_by", "manager@studio.test")["count"], json!(2));

    // Date buckets, including the April fiscal year.
    let bucket = |b: &'static str| {
        let (t, m, base, aug_sep) = (&t, &manager, base.clone(), aug_sep.clone());
        async move {
            t.q(
                m,
                json!({"entity": "SalesInvoice", "filters": base, "date_range": aug_sep, "group_by": [{"field": "posting_date", "bucket": b}],
                "measures": [{"fn": "count"}, {"fn": "sum", "field": "grand_total"}]}),
            )
            .await
        }
    };
    let months = bucket("month").await;
    assert_eq!(
        rows(&months).iter().map(|r| (r["month(posting_date)"].clone(), r["count"].clone(), r["sum(grand_total)"].clone())).collect::<Vec<_>>(),
        vec![(json!("2026-08-01"), json!(2), json!("450.3")), (json!("2026-09-01"), json!(3), json!("910.7"))]
    );
    let weeks = bucket("week").await;
    assert_eq!(row(&weeks, "week(posting_date)", "2026-08-31")["sum(grand_total)"], json!("510"), "ISO weeks start on Monday");
    assert_eq!(rows(&bucket("quarter").await)[0]["quarter(posting_date)"], json!("2026-07-01"));
    assert_eq!(rows(&bucket("year").await)[0]["year(posting_date)"], json!("2026-01-01"));
    assert_eq!(rows(&bucket("fiscal_year").await)[0]["fiscal_year(posting_date)"], json!("2026-04-01"));
    let fq = bucket("fiscal_quarter").await;
    assert_eq!((rows(&fq).len(), &rows(&fq)[0]["fiscal_quarter(posting_date)"], &rows(&fq)[0]["count"]), (1, &json!("2026-07-01"), &json!(5)));

    // Date-times bucket in the workspace's time zone (Asia/Kolkata, UTC+5:30): 20:00 UTC on
    // 30 September is already 1 October there.
    let local = t
        .q(&manager, json!({"entity": "SalesInvoice", "filters": [["x_delivered_at", "is", "set"]], "group_by": [{"field": "x_delivered_at", "bucket": "month"}], "measures": [{"fn": "count"}]}))
        .await;
    assert_eq!(
        rows(&local).iter().map(|r| (r["month(x_delivered_at)"].clone(), r["count"].clone())).collect::<Vec<_>>(),
        vec![(json!("2026-09-01"), json!(1)), (json!("2026-10-01"), json!(1))],
        "{local}"
    );
    let october = t.q(&manager, json!({"entity": "SalesInvoice", "date_range": {"field": "x_delivered_at", "from": "2026-10-01", "to": "2026-10-31"}, "measures": [{"fn": "count"}]})).await;
    assert_eq!(october["totals"]["count"], json!(1), "local days bound date-time ranges");

    // Child-table measures through joins: line amounts by item group.
    let by_item_group = t
        .q(
            &manager,
            json!({"entity": "SalesInvoice", "filters": base, "date_range": aug_sep, "group_by": [{"field": "items.item.item_group"}],
            "measures": [{"fn": "sum", "field": "items.amount"}, {"fn": "count_distinct", "field": "items.item"}, {"fn": "count_distinct", "field": "id"}]}),
        )
        .await;
    assert_eq!(by_item_group["meta"]["grain"], json!("items"));
    assert_eq!(row(&by_item_group, "items.item.item_group", "Products")["sum(items.amount)"], json!("1350"), "{by_item_group}");
    assert_eq!(row(&by_item_group, "items.item.item_group", "Raw Material")["sum(items.amount)"], json!("11"));
    assert_eq!(row(&by_item_group, "items.item.item_group", "Raw Material")["count_distinct(id)"], json!(3), "invoices with raw material");
    assert_eq!(by_item_group["totals"]["count_distinct(items.item)"], json!(4));
    assert_eq!(by_item_group["totals"]["sum(items.amount)"], json!("1361"));
    // At the child grain a child filter picks rows; otherwise it picks documents with such a row.
    let widgets_only = t
        .q(&manager, json!({"entity": "SalesInvoice", "filters": [["docstatus", "=", 1], ["items.item", "=", widget.to_string()]], "group_by": [{"field": "items.item.item_group"}],
            "measures": [{"fn": "sum", "field": "items.amount"}]}))
        .await;
    assert_eq!((rows(&widgets_only).len(), &rows(&widgets_only)[0]["sum(items.amount)"]), (1, &json!("700")), "{widgets_only}");
    let with_screws = t
        .q(
            &manager,
            json!({"entity": "SalesInvoice", "filters": [["docstatus", "=", 1], ["items.item", "=", screw.to_string()]], "measures": [{"fn": "count"}]}),
        )
        .await;
    assert_eq!(with_screws["totals"]["count"], json!(2));
    let with_raw = t
        .q(
            &manager,
            json!({"entity": "SalesInvoice", "filters": [["docstatus", "=", 1], ["items.item.item_group", "=", raw["id"]]], "measures": [{"fn": "count"}]}),
        )
        .await;
    assert_eq!(with_raw["totals"]["count"], json!(3));
    let tagged = t.q(&manager, json!({"entity": "SalesInvoice", "filters": [["_tag", "=", "priority"]], "measures": [{"fn": "count"}]})).await;
    assert_eq!(tagged["totals"]["count"], json!(1));
    let searched = t.q(&manager, json!({"entity": "SalesInvoice", "filters": base, "search": "gamma", "measures": [{"fn": "count"}]})).await;
    assert_eq!(searched["totals"]["count"], json!(2), "search reads the title field");

    // Sort by a measure, both ways; truncation is reported.
    let by_customer = |dir: &'static str, limit: u32| {
        let (t, m, base, aug_sep) = (&t, &manager, base.clone(), aug_sep.clone());
        async move {
            t.q(
                m,
                json!({"entity": "SalesInvoice", "filters": base, "date_range": aug_sep, "group_by": [{"field": "customer"}],
                "measures": [{"fn": "sum", "field": "grand_total", "label": "Revenue"}], "sort": [{"by": "Revenue", "dir": dir}], "limit": limit}),
            )
            .await
        }
    };
    let titles = |d: &Value| rows(d).iter().map(|r| r["_titles"]["customer"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(titles(&by_customer("desc", 10).await), ["Alpha Traders", "Gamma Home", "Beta Retail"]);
    assert_eq!(titles(&by_customer("asc", 10).await), ["Beta Retail", "Gamma Home", "Alpha Traders"]);
    let top2 = by_customer("desc", 2).await;
    assert_eq!(
        (rows(&top2).len(), &top2["meta"]["truncated"], &top2["totals"]["sum(grand_total)"]),
        (2, &json!(true), &json!("1361")),
        "totals cover every row"
    );

    // ───────────── pivots ─────────────
    let pivot = t
        .q(
            &manager,
            json!({"entity": "SalesInvoice", "filters": base, "date_range": aug_sep, "group_by": [{"field": "customer.customer_group"}],
            "pivot": {"field": "posting_date", "bucket": "month"}, "measures": [{"fn": "sum", "field": "grand_total"}]}),
        )
        .await;
    let names: Vec<&str> = pivot["columns"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["customer.customer_group", "sum(grand_total)@0", "sum(grand_total)@1", "sum(grand_total)"], "{pivot}");
    assert_eq!((&pivot["columns"][1]["pivot_value"], &pivot["columns"][2]["pivot_value"]), (&json!("2026-08-01"), &json!("2026-09-01")));
    let com = row(&pivot, "customer.customer_group", "Commercial");
    assert_eq!((&com["sum(grand_total)@0"], &com["sum(grand_total)@1"], &com["sum(grand_total)"]), (&json!("200.3"), &json!("610.7"), &json!("811")));
    let ind = row(&pivot, "customer.customer_group", "Individual");
    assert_eq!((&ind["sum(grand_total)@0"], &ind["sum(grand_total)@1"], &ind["sum(grand_total)"]), (&json!("250"), &json!("300"), &json!("550")));
    let pt = &pivot["totals"];
    assert_eq!((&pt["sum(grand_total)@0"], &pt["sum(grand_total)@1"], &pt["sum(grand_total)"]), (&json!("450.3"), &json!("910.7"), &json!("1361")));
    // Categories as columns: the biggest first, capped.
    let capped = t
        .q(&manager, json!({"entity": "SalesInvoice", "filters": base, "date_range": aug_sep, "pivot": {"field": "customer", "max_columns": 2}, "measures": [{"fn": "sum", "field": "grand_total"}]}))
        .await;
    assert_eq!((&capped["meta"]["pivot_truncated"], &capped["meta"]["pivot_columns"]), (&json!(true), &json!(2)), "{capped}");
    assert_eq!((&capped["columns"][0]["pivot_label"], &capped["columns"][1]["pivot_label"]), (&json!("Alpha Traders"), &json!("Gamma Home")));
    assert_eq!((&rows(&capped)[0]["sum(grand_total)@0"], &rows(&capped)[0]["sum(grand_total)@1"]), (&json!("710.3"), &json!("550")));

    // ───────────── time series and previous periods ─────────────
    let series = t
        .q(
            &manager,
            json!({"entity": "SalesInvoice", "filters": base, "measures": [{"fn": "count"}, {"fn": "sum", "field": "grand_total"}],
            "time_series": {"field": "posting_date", "bucket": "month", "from": "2026-07-01", "to": "2026-10-31", "fill_zero": true}}),
        )
        .await;
    assert_eq!(
        rows(&series).iter().map(|r| (r["month(posting_date)"].clone(), r["count"].clone(), r["sum(grand_total)"].clone())).collect::<Vec<_>>(),
        vec![
            (json!("2026-07-01"), json!(0), json!("0")),
            (json!("2026-08-01"), json!(2), json!("450.3")),
            (json!("2026-09-01"), json!(3), json!("910.7")),
            (json!("2026-10-01"), json!(0), json!("0"))
        ],
        "{series}"
    );
    // September against August, per customer group.
    let sep = json!({"field": "posting_date", "from": "2026-09-01", "to": "2026-09-30"});
    let vs = t
        .q(
            &manager,
            json!({"entity": "SalesInvoice", "filters": base, "date_range": sep, "group_by": [{"field": "customer.customer_group"}],
            "measures": [{"fn": "sum", "field": "grand_total"}], "compare": "previous_period"}),
        )
        .await;
    assert_eq!(vs["meta"]["compare"]["previous_from"], json!("2026-08-01"));
    assert_eq!(vs["meta"]["compare"]["previous_to"], json!("2026-08-31"));
    let c = row(&vs, "customer.customer_group", "Commercial");
    assert_eq!((&c["sum(grand_total)"], &c["previous(sum(grand_total))"]), (&json!("610.7"), &json!("200.3")), "{vs}");
    let i = row(&vs, "customer.customer_group", "Individual");
    assert_eq!((&i["sum(grand_total)"], &i["previous(sum(grand_total))"]), (&json!("300"), &json!("250")));
    assert_eq!((&vs["totals"]["sum(grand_total)"], &vs["totals"]["previous(sum(grand_total))"]), (&json!("910.7"), &json!("450.3")));
    // A monthly series against the two months before, bucket by bucket (July holds the other company's sale).
    let trend = t
        .q(
            &manager,
            json!({"entity": "SalesInvoice", "filters": [["docstatus", "=", 1]], "measures": [{"fn": "count"}, {"fn": "sum", "field": "grand_total"}],
            "time_series": {"field": "posting_date", "bucket": "month", "from": "2026-08-01", "to": "2026-09-30"}, "compare": "previous_period"}),
        )
        .await;
    assert_eq!(
        rows(&trend).iter().map(|r| (r["sum(grand_total)"].clone(), r["previous(sum(grand_total))"].clone(), r["previous(count)"].clone())).collect::<Vec<_>>(),
        vec![(json!("450.3"), json!("0"), json!(0)), (json!("910.7"), json!("100"), json!(1))],
        "{trend}"
    );

    // ───────────── permissions ─────────────
    let count = |ctx: Ctx, filters: Value| {
        let t = &t;
        async move {
            let d =
                t.q(&ctx, json!({"entity": "SalesInvoice", "filters": filters, "measures": [{"fn": "count"}, {"fn": "sum", "field": "grand_total"}]})).await;
            (d["totals"]["count"].clone(), d["totals"]["sum(grand_total)"].clone())
        }
    };
    let submitted = json!([["docstatus", "=", 1]]);
    assert_eq!(count(rep.clone(), submitted.clone()).await, (json!(3), json!("1010.3")), "the rep sees their own invoices only");
    assert_eq!(count(rep.clone(), Value::Null).await.0, json!(4), "their draft too");
    assert_eq!(count(manager.clone(), submitted.clone()).await, (json!(6), json!("1461")));
    assert_eq!(count(scoped.clone(), submitted.clone()).await, (json!(5), json!("1361")), "company scope");
    let refused = |e: Error| matches!(e, Error::Forbidden(_));
    assert!(refused(t.q_err(&viewer, json!({"entity": "SalesInvoice", "measures": [{"fn": "sum", "field": "customer.credit_limit"}]})).await));
    assert!(refused(t.q_err(&viewer, json!({"entity": "SalesInvoice", "filters": [["customer.credit_limit", ">", 1]], "measures": [{"fn": "count"}]})).await));
    assert!(refused(t.q_err(&viewer, json!({"entity": "Customer", "fields": [{"field": "customer_name"}], "sort": [{"by": "credit_limit"}]})).await));
    assert!(refused(t.q_err(&rep, json!({"entity": "SalesInvoice", "group_by": [{"field": "project.project_name"}], "measures": [{"fn": "count"}]})).await));
    assert!(refused(t.q_err(&rep, json!({"entity": "PurchaseInvoice", "measures": [{"fn": "count"}]})).await));
    // The field list shows what each caller may use.
    let viewer_fields = t.api(&viewer, "GET", "/fields/SalesInvoice", &[], Value::Null).await;
    let paths: Vec<&str> = viewer_fields["fields"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert!(paths.contains(&"items.item.item_group") && paths.contains(&"x_delivered_at") && !paths.contains(&"customer.credit_limit"));

    // ───────────── plain rows over the API ─────────────
    let listed = t
        .api(
            &manager,
            "POST",
            "/query",
            &[],
            json!({"entity": "SalesInvoice", "filters": base, "date_range": aug_sep,
            "fields": [{"field": "number"}, {"field": "customer"}, {"field": "grand_total"}, {"field": "customer.customer_group", "label": "Segment"}],
            "sort": [{"by": "posting_date", "dir": "asc"}]}),
        )
        .await;
    assert_eq!(rows(&listed).len(), 5);
    assert_eq!(rows(&listed)[0]["_titles"]["customer"], json!("Alpha Traders"));
    assert_eq!(rows(&listed)[0]["grand_total"], json!("200.3"));
    assert_eq!(rows(&listed)[0]["_id"], json!(inv1.to_string()));
    assert_eq!(listed["totals"]["grand_total"], json!("1361"));
    assert_eq!(listed["columns"][3]["label"], json!("Segment"));
    let bad = t.call(&manager, "POST", "/query", &[], json!({"entity": "SalesInvoice", "grup_by": []})).await.unwrap_err();
    assert!(matches!(&bad, Error::Validation(f) if f[0].message.contains("unknown field `grup_by`")), "{bad:?}");
    let sources = t.api(&viewer, "GET", "/sources", &[], Value::Null).await;
    let names: Vec<&str> = sources["builtin"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"sales_register") && names.contains(&"studio_query"));
    assert!(!sources["entities"].as_array().unwrap().iter().any(|e| e["name"] == json!("PurchaseInvoice")), "only readable entities");

    // The platform's report API and MCP's run_report reach the engine too.
    let params: HashMap<String, String> =
        [("spec".to_string(), json!({"entity": "SalesInvoice", "filters": submitted, "measures": [{"fn": "count"}]}).to_string())].into();
    let via_reports = ferrum_domain::reports::run(&t.k, &manager, "studio_query", &params).await.unwrap();
    assert_eq!(via_reports["totals"]["count"], json!(6));

    // ───────────── saved reports ─────────────
    let invalid = t
        .k
        .insert(&manager, "Report", obj(json!({"label": "Broken", "kind": "group_by", "spec": {"entity": "SalesInvoice", "group_by": [{"field": "nope"}], "measures": [{"fn": "count"}]}})))
        .await
        .unwrap_err();
    assert!(
        matches!(&invalid, Error::Validation(f) if f[0].field == "spec" && f[0].message.starts_with("group_by[0].field: unknown field 'nope'")),
        "{invalid:?}"
    );
    let wrong_kind =
        t.k.insert(&manager, "Report", obj(json!({"label": "Odd", "kind": "table", "spec": {"entity": "SalesInvoice", "measures": [{"fn": "count"}]}})))
            .await
            .unwrap_err();
    assert!(matches!(&wrong_kind, Error::Validation(f) if f.iter().any(|x| x.field == "kind")), "{wrong_kind:?}");
    let report_spec = json!({"entity": "SalesInvoice", "filters": base, "date_range": aug_sep, "group_by": [{"field": "customer.customer_group"}],
        "measures": [{"fn": "count"}, {"fn": "sum", "field": "grand_total"}]});
    let bad_columns =
        t.k.insert(&manager, "Report", obj(json!({"label": "Cols", "kind": "group_by", "spec": report_spec, "columns": [{"name": "nope"}]})))
            .await
            .unwrap_err();
    assert!(matches!(&bad_columns, Error::Validation(f) if f[0].field == "columns"), "{bad_columns:?}");
    let r1 = t
        .insert(
            &manager,
            "Report",
            json!({"label": "Sales by segment", "kind": "group_by", "spec": report_spec, "sharing": "everyone", "chart": {"type": "bar", "y": ["sum(grand_total)"]},
                "columns": [{"name": "sum(grand_total)", "label": "Revenue", "width": 160}, {"name": "count", "hidden": true}]}),
        )
        .await;
    assert_eq!(r1["base_entity"], json!("SalesInvoice"));
    let r1_id = uid(&r1);
    let ran =
        t.api(&viewer, "POST", &format!("/reports/{r1_id}/run"), &[], json!({"filters": [["customer.customer_group", "=", commercial.to_string()]]})).await;
    assert_eq!(ran["report"]["label"], json!("Sales by segment"));
    let cols: Vec<&str> = ran["data"]["columns"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(cols, ["sum(grand_total)", "customer.customer_group"], "configured order, hidden count");
    assert_eq!((&ran["data"]["columns"][0]["label"], &ran["data"]["columns"][0]["width"]), (&json!("Revenue"), &json!(160)));
    assert_eq!((rows(&ran["data"]).len(), &rows(&ran["data"])[0]["sum(grand_total)"]), (1, &json!("811")), "runtime filters narrow it");
    let september = t.api(&viewer, "GET", &format!("/reports/{r1_id}/run"), &[("from", "2026-09-01"), ("to", "2026-09-30")], Value::Null).await;
    assert_eq!(september["data"]["totals"]["sum(grand_total)"], json!("910.7"));
    let csv = t.call(&viewer, "GET", &format!("/reports/{r1_id}/export"), &[("format", "csv")], Value::Null).await.unwrap();
    assert!(csv.content_type.starts_with("text/csv"));
    assert!(csv.headers.iter().any(|(k, v)| k == "content-disposition" && v.contains("Sales_by_segment-")));
    let text = String::from_utf8(csv.body).unwrap();
    assert_eq!(text, "\u{feff}Revenue,Customer › Customer Group\r\n811,Commercial\r\n550,Individual\r\n1361,\r\n");
    let xlsx = t.call(&viewer, "GET", &format!("/reports/{r1_id}/export"), &[("format", "xlsx")], Value::Null).await.unwrap_err();
    assert!(matches!(&xlsx, Error::Validation(f) if f[0].field == "format"));
    let no_export = t.call(&rep, "GET", &format!("/reports/{r1_id}/export"), &[], Value::Null).await.unwrap_err();
    assert!(matches!(no_export, Error::Forbidden(_)), "exports need export permission on the data");
    let action = t.k.run_action(&manager, "Report", r1_id, "run", json!({"from": "2026-09-01", "to": "2026-09-30"})).await.unwrap();
    assert_eq!(action["data"]["totals"]["sum(grand_total)"], json!("910.7"), "the run action of the document API");
    let via_saved: HashMap<String, String> = [("report".to_string(), r1_id.to_string())].into();
    assert_eq!(ferrum_domain::reports::run(&t.k, &manager, "studio_report", &via_saved).await.unwrap()["totals"]["sum(grand_total)"], json!("1361"));

    // Sharing: private to the creator, a role, everyone; managers see all.
    let private = uid(&t.insert(&rep, "Report", json!({"label": "My sales", "spec": {"entity": "SalesInvoice", "fields": [{"field": "number"}]}})).await);
    let for_sales = uid(&t
        .insert(&rep, "Report", json!({"label": "Team sales", "spec": {"entity": "SalesInvoice"}, "sharing": "roles", "roles": [{"role": "Sales User"}]}))
        .await);
    let no_roles = t.k.insert(&rep, "Report", obj(json!({"label": "Nobody", "spec": {"entity": "SalesInvoice"}, "sharing": "roles"}))).await.unwrap_err();
    assert!(matches!(&no_roles, Error::Validation(f) if f[0].field == "roles"));
    let ghost =
        t.k.insert(&rep, "Report", obj(json!({"label": "Ghost", "spec": {"entity": "SalesInvoice"}, "sharing": "roles", "roles": [{"role": "Ghosts"}]})))
            .await
            .unwrap_err();
    assert!(matches!(&ghost, Error::Validation(f) if f[0].message.contains("Ghosts")));
    let hidden = t.call(&viewer, "POST", &format!("/reports/{private}/run"), &[], Value::Null).await.unwrap_err();
    assert!(matches!(hidden, Error::NotFound(_)), "private reports are not found by others");
    assert_eq!(rows(&t.api(&viewer, "POST", &format!("/reports/{for_sales}/run"), &[], Value::Null).await["data"]).len(), 8, "shared with the viewer's role");
    assert_eq!(
        rows(&t.api(&scoped, "POST", &format!("/reports/{for_sales}/run"), &[], Value::Null).await["data"]).len(),
        7,
        "with the viewer's own scope: company A"
    );
    assert_eq!(rows(&t.api(&rep, "POST", &format!("/reports/{private}/run"), &[], Value::Null).await["data"]).len(), 4, "the creator, with their own rows");
    assert!(t.call(&rep, "POST", &format!("/reports/{for_sales}/run"), &[], Value::Null).await.is_ok(), "creators see what they share");
    t.api(&manager, "POST", &format!("/reports/{private}/run"), &[], Value::Null).await;
    let listed_for = |ctx: Ctx| {
        let t = &t;
        async move {
            let l = t.api(&ctx, "GET", "/reports", &[], Value::Null).await;
            l["rows"].as_array().unwrap().iter().map(|r| r["label"].as_str().unwrap().to_string()).collect::<Vec<_>>()
        }
    };
    assert_eq!(listed_for(viewer.clone()).await, ["Sales by segment", "Team sales"]);
    assert_eq!(listed_for(manager.clone()).await, ["My sales", "Sales by segment", "Team sales"]);
    let by_entity = t.api(&viewer, "GET", "/reports", &[("entity", "PurchaseInvoice")], Value::Null).await;
    assert!(by_entity["rows"].as_array().unwrap().is_empty());
    // Records stay editable by their owners only (own-records permission).
    assert!(t.k.update(&viewer, "Report", private, obj(json!({"label": "Mine now"})), None).await.is_err());

    // ───────────── charts ─────────────
    let c1 = uid(&t
        .insert(
            &manager,
            "DashboardChart",
            json!({"label": "Monthly sales", "chart_type": "line", "sharing": "everyone",
                "spec": {"entity": "SalesInvoice", "filters": [["docstatus", "=", 1]], "measures": [{"fn": "sum", "field": "grand_total"}],
                         "time_series": {"field": "posting_date", "bucket": "month", "from": "2026-07-01", "to": "2026-10-31", "fill_zero": true}}}),
        )
        .await);
    let c2 = uid(&t
        .insert(
            &manager,
            "DashboardChart",
            json!({"label": "Segments", "chart_type": "bar", "source": "report", "report": r1_id.to_string(), "sharing": "everyone"}),
        )
        .await);
    let c3 = uid(&t
        .insert(
            &manager,
            "DashboardChart",
            json!({"label": "Register", "chart_type": "bar", "source": "builtin", "builtin_report": "sales_register", "x_field": "party", "y_fields": ["grand_total"],
                "builtin_params": {"from_date": "2026-08-01", "to_date": "2026-09-30"}}),
        )
        .await);
    let c4 = uid(&t
        .insert(
            &a,
            "DashboardChart",
            json!({"label": "Purchases", "chart_type": "number", "spec": {"entity": "PurchaseInvoice", "measures": [{"fn": "count"}]}}),
        )
        .await);
    let pie = t
        .k
        .insert(&manager, "DashboardChart", obj(json!({"label": "Pie", "chart_type": "pie", "spec": {"entity": "SalesInvoice", "group_by": [{"field": "customer"}], "measures": [{"fn": "count"}, {"fn": "sum", "field": "grand_total"}]}})))
        .await
        .unwrap_err();
    assert!(matches!(&pie, Error::Validation(f) if f[0].field == "chart_type"), "{pie:?}");
    let no_builtin = t.k.insert(&manager, "DashboardChart", obj(json!({"label": "X", "source": "builtin", "builtin_report": "nope"}))).await.unwrap_err();
    assert!(matches!(&no_builtin, Error::Validation(f) if f[0].field == "builtin_report"));
    assert_eq!(t.k.get(&a, "DashboardChart", c2, false).await.unwrap()["base_entity"], json!("SalesInvoice"), "from the report");
    let line = t.api(&viewer, "GET", &format!("/charts/{c1}/data"), &[], Value::Null).await;
    assert_eq!((&line["chart"]["x"], &line["chart"]["y"]), (&json!("month(posting_date)"), &json!(["sum(grand_total)"])));
    assert_eq!(
        rows(&line["data"]).iter().map(|r| r["sum(grand_total)"].clone()).collect::<Vec<_>>(),
        [json!("100"), json!("450.3"), json!("910.7"), json!("0")]
    );
    assert_eq!(
        line["drill_down"],
        json!({"entity": "SalesInvoice", "filters": [["docstatus", "=", 1], ["posting_date", "between", ["2026-07-01", "2026-10-31"]]]})
    );
    let narrowed = t.api(&viewer, "GET", &format!("/charts/{c1}/data"), &[("from", "2026-09-01"), ("to", "2026-09-30")], Value::Null).await;
    assert_eq!(rows(&narrowed["data"]).len(), 1, "a runtime period replaces the saved one");
    let unshared = t.call(&viewer, "GET", &format!("/charts/{c3}/data"), &[], Value::Null).await.unwrap_err();
    assert!(matches!(unshared, Error::NotFound(_)), "a private chart on its own");
    let segments = t.api(&viewer, "POST", &format!("/charts/{c2}/data"), &[], Value::Null).await;
    assert_eq!(segments["data"]["totals"]["sum(grand_total)"], json!("1361"), "charts over saved reports");
    let preview = t
        .api(&viewer, "POST", "/charts/preview", &[], json!({"record": {"chart_type": "bar", "spec": {"entity": "SalesInvoice", "filters": submitted, "group_by": [{"field": "status"}], "measures": [{"fn": "count"}]}}}))
        .await;
    assert_eq!(preview["chart"]["x"], json!("status"));
    // Someone else's private report can neither be previewed nor charted.
    let peek = t.call(&viewer, "POST", "/charts/preview", &[], json!({"record": {"source": "report", "report": private.to_string()}})).await.unwrap_err();
    assert!(matches!(peek, Error::NotFound(_)), "{peek:?}");
    let borrowed =
        t.k.insert(&viewer, "DashboardChart", obj(json!({"label": "Borrowed", "source": "report", "report": private.to_string()}))).await.unwrap_err();
    assert!(matches!(&borrowed, Error::Validation(f) if f[0].field == "report" && f[0].message.contains("not found")), "{borrowed:?}");

    // ───────────── number cards ─────────────
    let k1 = uid(&t
        .insert(
            &manager,
            "NumberCard",
            json!({"label": "September sales", "format": "currency", "currency": inr["id"], "sparkline": "week", "sharing": "everyone",
                "spec": {"entity": "SalesInvoice", "filters": base, "measures": [{"fn": "sum", "field": "grand_total"}], "date_range": sep, "compare": "previous_period"}}),
        )
        .await);
    let two =
        t.k.insert(
            &manager,
            "NumberCard",
            obj(json!({"label": "Two", "spec": {"entity": "SalesInvoice", "measures": [{"fn": "count"}, {"fn": "sum", "field": "grand_total"}]}})),
        )
        .await
        .unwrap_err();
    assert!(matches!(&two, Error::Validation(f) if f[0].field == "spec" && f[0].message.contains("one value")), "{two:?}");
    let spark =
        t.k.insert(&manager, "NumberCard", obj(json!({"label": "S", "sparkline": "day", "spec": {"entity": "SalesInvoice", "measures": [{"fn": "count"}]}})))
            .await
            .unwrap_err();
    assert!(matches!(&spark, Error::Validation(f) if f[0].field == "sparkline"));
    let card = t.api(&viewer, "GET", &format!("/cards/{k1}/data"), &[], Value::Null).await;
    assert_eq!(
        (&card["value"], &card["previous"], &card["delta"], &card["delta_percent"], &card["direction"]),
        (&json!("910.7"), &json!("450.3"), &json!("460.4"), &json!("102.24"), &json!("up")),
        "{card}"
    );
    assert_eq!(card["card"]["currency"], json!("INR"));
    assert_eq!(card["sparkline"], json!([["2026-08-31", "510"], ["2026-09-07", "0"], ["2026-09-14", "100.7"], ["2026-09-21", "0"], ["2026-09-28", "300"]]));
    assert_eq!(card["drill_down"]["filters"][2], json!(["posting_date", "between", ["2026-09-01", "2026-09-30"]]));
    let rep_card = t.api(&rep, "GET", &format!("/cards/{k1}/data"), &[], Value::Null).await;
    assert_eq!((&rep_card["value"], &rep_card["previous"]), (&json!("810"), &json!("200.3")), "the same card shows each viewer their own data");

    // ───────────── dashboards ─────────────
    let layout = json!([
        {"i": "kpi", "type": "card", "card": k1.to_string(), "x": 0, "y": 0, "w": 4, "h": 2},
        {"i": "trend", "type": "chart", "chart": c1.to_string(), "x": 4, "y": 0, "w": 8, "h": 4},
        {"type": "markdown", "content": "Numbers exclude drafts.", "x": 0, "y": 2, "w": 4, "h": 2},
        {"i": "register", "type": "chart", "chart": c3.to_string(), "x": 0, "y": 4, "w": 12, "h": 4},
        {"i": "buying", "type": "chart", "chart": c4.to_string(), "x": 0, "y": 8, "w": 6, "h": 2},
        {"i": "latest", "type": "list", "entity": "SalesInvoice", "filters": [["docstatus", "=", 1]], "fields": ["number", "customer", "grand_total"], "limit": 3, "x": 6, "y": 8, "w": 6, "h": 4}
    ]);
    let filters = json!([{"name": "company", "type": "link", "to": "Company"}, {"name": "period", "type": "date_range", "default": {"from": "2026-08-01", "to": "2026-09-30"}}]);
    let overlap = t
        .k
        .insert(&manager, "Dashboard", obj(json!({"label": "Bad", "layout": [{"type": "markdown", "content": "a", "x": 0, "y": 0, "w": 6, "h": 2}, {"type": "markdown", "content": "b", "x": 5, "y": 1, "w": 4, "h": 2}]})))
        .await
        .unwrap_err();
    assert!(matches!(&overlap, Error::Validation(f) if f[0].field == "layout" && f[0].message.contains("overlaps")), "{overlap:?}");
    let missing =
        t.k.insert(
            &manager,
            "Dashboard",
            obj(json!({"label": "Missing", "layout": [{"type": "chart", "chart": Uuid::now_v7().to_string(), "x": 0, "y": 0, "w": 6, "h": 2}]})),
        )
        .await
        .unwrap_err();
    assert!(matches!(&missing, Error::Validation(f) if f[0].message.contains("Dashboard Chart not found")), "{missing:?}");
    let d1_doc =
        t.insert(&manager, "Dashboard", json!({"label": "Sales", "layout": layout, "filters": filters, "sharing": "everyone", "refresh_interval": 300})).await;
    assert_eq!(d1_doc["layout"][2]["i"], json!("w2"), "grid items get ids");
    let d1 = uid(&d1_doc);
    let dash = t
        .api(
            &manager,
            "POST",
            &format!("/dashboards/{d1}/data"),
            &[],
            json!({"values": {"company": acme.to_string(), "period": {"from": "2026-09-01", "to": "2026-09-30"}}}),
        )
        .await;
    let item = |d: &Value, i: &str| d["items"].as_array().unwrap().iter().find(|x| x["i"] == json!(i)).cloned();
    let kpi = item(&dash, "kpi").unwrap();
    assert_eq!((&kpi["widget"]["value"], &kpi["widget"]["previous"]), (&json!("910.7"), &json!("450.3")), "{kpi}");
    let trend_w = item(&dash, "trend").unwrap();
    assert_eq!(
        rows(&trend_w["widget"]["data"]).iter().map(|r| r["sum(grand_total)"].clone()).collect::<Vec<_>>(),
        [json!("910.7")],
        "period and company applied"
    );
    let register = item(&dash, "register").unwrap();
    assert_eq!(rows(&register["widget"]["data"]).len(), 3, "built-in report parameters from dashboard filters: {register}");
    assert_eq!(register["widget"]["chart"]["x"], json!("party"));
    assert!(item(&dash, "buying").is_some(), "the manager reads purchases");
    assert_eq!(rows(&item(&dash, "latest").unwrap()["widget"]).len(), 3);
    assert_eq!(item(&dash, "w2").unwrap()["widget"]["content"], json!("Numbers exclude drafts."));
    // The rep: own data, and no purchase widget at all.
    let rep_dash = t.api(&rep, "GET", &format!("/dashboards/{d1}/data"), &[("values", &json!({"company": acme.to_string()}).to_string())], Value::Null).await;
    assert!(item(&rep_dash, "buying").is_none(), "widgets the viewer cannot read are left out");
    let rep_kpi = &item(&rep_dash, "kpi").unwrap()["widget"];
    assert_eq!((&rep_kpi["value"], &rep_kpi["previous"]), (&json!("1010.3"), &json!("0")), "the default period: August and September against June and July");
    assert_eq!(rep_dash["filters"]["period"]["from"], json!("2026-08-01"), "defaults apply");
    let unknown = t.call(&rep, "POST", &format!("/dashboards/{d1}/data"), &[], json!({"values": {"region": "x"}})).await.unwrap_err();
    assert!(matches!(&unknown, Error::Validation(f) if f[0].message.contains("unknown dashboard filter 'region'")));
    // One widget refreshed with the dashboard's filters.
    let one = t
        .api(
            &rep,
            "POST",
            &format!("/cards/{k1}/data"),
            &[],
            json!({"dashboard": d1.to_string(), "values": {"period": {"from": "2026-08-01", "to": "2026-08-31"}}}),
        )
        .await;
    assert_eq!(one["value"], json!("200.3"));
    // Deleting a chart a dashboard shows is refused.
    let in_use = t.k.delete(&manager, "DashboardChart", c1, None).await.unwrap_err();
    assert!(matches!(&in_use, Error::Conflict(m) if m.contains("Sales")), "{in_use:?}");
    let report_in_use = t.k.delete(&manager, "Report", r1_id, None).await.unwrap_err();
    assert!(matches!(report_in_use, Error::Conflict(_)), "a chart reads it");

    // ───────────── spaces ─────────────
    let blocks = json!([
        {"type": "heading", "text": "Selling", "level": 1},
        {"type": "shortcut", "label": "Submitted invoices", "entity": "SalesInvoice", "filters": [["docstatus", "=", 1]], "show_count": true, "width": 3},
        {"type": "shortcut", "label": "My sales", "report": private.to_string()},
        {"type": "links", "label": "Documents", "items": [{"label": "Invoices", "entity": "SalesInvoice"}, {"label": "Purchases", "entity": "PurchaseInvoice"}, {"label": "Register", "builtin": "sales_register"}]},
        {"type": "chart", "chart": c1.to_string()},
        {"type": "card", "card": k1.to_string()},
        {"type": "chart", "chart": c4.to_string()},
        {"type": "list", "label": "Latest", "entity": "SalesInvoice", "limit": 2},
        {"type": "markdown", "content": "**Welcome**"},
        {"type": "onboarding", "title": "Get started", "steps": [{"label": "Add a customer", "entity": "Customer"}, {"label": "Record a purchase", "entity": "PurchaseInvoice"}]}
    ]);
    let bad_block =
        t.k.insert(
            &manager,
            "Space",
            obj(json!({"label": "X", "blocks": [{"type": "shortcut", "label": "Two targets", "entity": "SalesInvoice", "url": "/app"}]})),
        )
        .await
        .unwrap_err();
    assert!(matches!(&bad_block, Error::Validation(f) if f[0].field == "blocks" && f[0].message.contains("exactly one")), "{bad_block:?}");
    let not_mine = t.k.insert(&viewer, "Space", obj(json!({"label": "X", "blocks": [{"type": "chart", "chart": c4.to_string()}]}))).await.unwrap_err();
    assert!(
        matches!(&not_mine, Error::Validation(f) if f[0].message.contains("Dashboard Chart not found")),
        "a private chart cannot be placed by others: {not_mine:?}"
    );
    let bad_filter =
        t.k.insert(&manager, "Space", obj(json!({"label": "X", "blocks": [{"type": "list", "entity": "SalesInvoice", "filters": [["nope", "=", 1]]}]})))
            .await
            .unwrap_err();
    assert!(matches!(&bad_filter, Error::Validation(f) if f[0].message.contains("unknown field 'nope'")), "{bad_filter:?}");
    let s1 = uid(&t
        .insert(&manager, "Space", json!({"label": "Selling", "module": "selling", "icon": "shopping-cart", "blocks": blocks, "sharing": "everyone"}))
        .await);
    let types = |d: &Value| d["blocks"].as_array().unwrap().iter().map(|b| b["type"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let mine = t.api(&rep, "GET", &format!("/spaces/{s1}/data"), &[], Value::Null).await;
    assert_eq!(types(&mine), ["heading", "shortcut", "shortcut", "links", "chart", "card", "list", "markdown", "onboarding"], "{mine}");
    assert_eq!((&mine["blocks"][1]["count"], &mine["blocks"][1]["width"]), (&json!(3), &json!(3)), "live counts with the viewer's permissions");
    assert_eq!(mine["blocks"][3]["items"].as_array().unwrap().len(), 2, "links the viewer cannot open are left out");
    assert_eq!(mine["blocks"][8]["steps"], json!([{"label": "Add a customer", "description": null, "url": null, "entity": "Customer", "done": true}]));
    let viewers = t.api(&viewer, "GET", &format!("/spaces/{s1}/data"), &[], Value::Null).await;
    assert_eq!(
        types(&viewers),
        ["heading", "shortcut", "links", "chart", "card", "list", "markdown", "onboarding"],
        "the rep's private report is not offered to others"
    );
    assert_eq!(viewers["blocks"][1]["count"], json!(6));
    let admins = t.api(&a, "GET", &format!("/spaces/{s1}/data"), &[], Value::Null).await;
    assert_eq!(admins["blocks"].as_array().unwrap().len(), 10);
    let spaces = t.api(&viewer, "GET", "/spaces", &[("module", "selling")], Value::Null).await;
    assert_eq!(spaces["rows"][0]["label"], json!("Selling"));

    // Standard records (shipped by packages) are read-only for everyone else.
    let system = Ctx::system(tenant, t.admin.schema.clone());
    let standard = uid(&t
        .insert(&system, "Space", json!({"label": "Home", "is_standard": true, "sharing": "everyone", "blocks": [{"type": "markdown", "content": "hi"}]}))
        .await);
    let changed = t.k.update(&manager, "Space", standard, obj(json!({"label": "Mine"})), None).await.unwrap_err();
    assert!(matches!(changed, Error::Forbidden(_)), "{changed:?}");
    assert!(matches!(t.k.delete(&a, "Space", standard, None).await.unwrap_err(), Error::Forbidden(_)));
    let made_standard = t.insert(&manager, "Space", json!({"label": "Sneaky", "is_standard": true})).await;
    assert_eq!(made_standard["is_standard"], json!(false), "only packages ship standard records");
}
