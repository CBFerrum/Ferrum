//! Published lists (BD-15) over HTTP without a session, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`,
//! reset first), on the server's own handlers mounted at their addresses beside the plugins' public route:
//! only the listed fields leave, the filter and "submitted only" hold, the CSV defuses formulas and the
//! page escapes markup, everything not published answers one 404, the limiter turns a flood away, and a
//! list follows its entity's later changes without showing more.

mod common;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{any, get as route_get};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, OverlayOp};
use ferrum_server::controllers::lists;
use ferrum_server::state::RateLimiter;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

type S = (Arc<Kernel>, Arc<RateLimiter>);

/// The handlers at the addresses the server gives them, with a plugin's public route beside them.
async fn serve(k: Arc<Kernel>, limit: u32) -> String {
    let state: S = (k, Arc::new(RateLimiter::local_only("public", Duration::from_secs(60), limit)));
    let app = Router::new()
        .route(
            "/api/public/{tenant}/lists/{name}",
            route_get(|State(s): State<S>, Path((w, n)): Path<(String, String)>, h: HeaderMap| async move { lists::answer(&s.0, &s.1, &h, &w, &n).await }),
        )
        .route("/api/public/{tenant}/{plugin}/{*path}", any(|| async { "a plugin" }))
        .route(
            "/public/{*rest}",
            route_get(|State(s): State<S>, Path(rest): Path<String>, h: HeaderMap| async move { lists::page(&s.0, &s.1, &h, &rest, None).await }),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn op(v: Value) -> OverlayOp {
    serde_json::from_value(v).unwrap()
}

fn doc(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

async fn workspace(k: &Kernel, slug: &str) -> Ctx {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: format!("{slug} & Co <Council>"),
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
    for (e, o) in [
        (
            "XCategory",
            json!({"name": "XCategory", "label": "Category", "kind": "master", "title_field": "category_name", "fields": [{"name": "category_name", "type": "data"}]}),
        ),
        (
            "XPermit",
            json!({"name": "XPermit", "label": "Permit", "kind": "document", "submittable": true, "title_field": "holder", "fields": [
                {"name": "holder", "type": "text"}, {"name": "kind", "type": "data", "label": "Type"}, {"name": "fee", "type": "currency"},
                {"name": "contact_email", "type": "email"}, {"name": "category", "type": "link", "to": "XCategory"}]}),
        ),
    ] {
        k.add_customization(&admin, e, op(json!({"op": "define_entity", "entity": o})), None).await.unwrap();
    }
    fresh(k, &admin).await
}

async fn fresh(k: &Kernel, ctx: &Ctx) -> Ctx {
    k.load_ctx(ctx.tenant, ctx.user, None, "it".into()).await.unwrap()
}

async fn permit(k: &Kernel, ctx: &Ctx, holder: &str, kind: &str, category: Option<&str>, state: &str) -> Uuid {
    let d = k
        .insert(ctx, "XPermit", doc(json!({"holder": holder, "kind": kind, "fee": "12.50", "contact_email": "holder@example.com", "category": category})))
        .await
        .unwrap();
    let id: Uuid = d["id"].as_str().unwrap().parse().unwrap();
    if state != "draft" {
        k.submit(ctx, "XPermit", id, None).await.unwrap();
    }
    if state == "cancelled" {
        k.cancel(ctx, "XPermit", id, None).await.unwrap();
    }
    id
}

async fn fetch(url: &str) -> (u16, HeaderMap, String) {
    let r = reqwest::get(url).await.unwrap();
    let (status, headers) = (r.status().as_u16(), r.headers().clone());
    (status, headers, r.text().await.unwrap())
}

fn keys(rows: &Value) -> Vec<BTreeSet<String>> {
    rows.as_array().unwrap().iter().map(|r| r.as_object().unwrap().keys().cloned().collect()).collect()
}

#[test]
fn published_lists_over_http() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    common::reset(&db.pool).await;
    let core = ferrum_meta::catalog::load_package_dir(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages/core")).expect("core");
    let k = Kernel::boot(db, Catalog::from_packages(vec![core]).unwrap(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let base = serve(k.clone(), 1000).await;

    let acme = workspace(&k, "acme").await;
    let shops = k.insert(&acme, "XCategory", doc(json!({"category_name": "Shops"}))).await.unwrap();
    let shops = shops["id"].as_str().unwrap().to_string();
    let dangerous = ["=HYPERLINK(\"http://x\")", "+1+cmd", "-2+3", "@SUM(A1)", "\tTabbed", "\rReturned", "Line one\nsaid \"two\"", "<b>Bold</b> & co"];
    for h in dangerous {
        permit(&k, &acme, h, "Trade", Some(&shops), "submitted").await;
    }
    permit(&k, &acme, "Draft Holder", "Trade", None, "draft").await;
    permit(&k, &acme, "Cancelled Holder", "Trade", None, "cancelled").await;
    permit(&k, &acme, "Fence Holder", "Fence", None, "submitted").await;
    let list = json!({"name": "permits", "label": "Trade permits", "description": "Issued <now>", "fields": ["holder", "kind", "category"],
        "filter": [["kind", "=", "Trade"], ["docstatus", "in", [0, 1, 2]]], "sort": ["holder", "asc"], "csv": true});
    k.add_customization(&acme, "XPermit", op(json!({"op": "publish_list", "list": list})), None).await.unwrap();

    // Exactly the listed fields, a link as its title; the filter holds, and only submitted records,
    // whatever the filter says.
    let (status, headers, body) = fetch(&format!("{base}/api/public/acme/lists/permits")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(headers["cache-control"], "public, max-age=60");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((v["label"].clone(), v["description"].clone(), v["more"].clone()), (json!("Trade permits"), json!("Issued <now>"), json!(false)));
    assert_eq!(
        v["fields"],
        json!([{"name": "holder", "label": "Holder", "type": "text"}, {"name": "kind", "label": "Type", "type": "data"}, {"name": "category", "label": "Category", "type": "link"}])
    );
    let want: BTreeSet<String> = ["holder", "kind", "category"].map(String::from).into();
    assert!(keys(&v["rows"]).iter().all(|k| *k == want), "{body}");
    let holders: BTreeSet<&str> = v["rows"].as_array().unwrap().iter().map(|r| r["holder"].as_str().unwrap()).collect();
    assert_eq!(holders, dangerous.into_iter().collect(), "no draft, no cancelled, no Fence");
    assert!(v["rows"].as_array().unwrap().iter().all(|r| r["category"] == json!("Shops") && r["kind"] == json!("Trade")));
    assert!(!body.contains(&shops) && !body.contains("holder@example.com") && !body.contains("12.5"), "{body}");

    // The CSV: labels for a header, formulas defused, quotes and line breaks kept inside quotes.
    let (status, headers, csv) = fetch(&format!("{base}/api/public/acme/lists/permits.csv")).await;
    assert_eq!((status, headers["content-type"].to_str().unwrap()), (200, "text/csv; charset=utf-8"));
    assert!(csv.starts_with("Holder,Type,Category\r\n"), "{csv}");
    for cell in ["\"'=HYPERLINK(\"\"http://x\"\")\"", "'+1+cmd", "'-2+3", "'@SUM(A1)", "'\tTabbed", "\"'\rReturned\"", "\"Line one\nsaid \"\"two\"\"\""] {
        assert!(csv.contains(&format!("\r\n{cell},Trade,Shops\r\n")), "{cell:?} in {csv:?}");
    }

    // The page: escaped, no script, a strict policy, not for search engines.
    let (status, headers, html) = fetch(&format!("{base}/public/acme/lists/permits")).await;
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("&lt;b&gt;Bold&lt;/b&gt; &amp; co") && !html.contains("<b>Bold"), "{html}");
    assert!(html.contains("acme &amp; Co &lt;Council&gt;") && html.contains("Issued &lt;now&gt;") && !html.contains("<script"), "{html}");
    assert!(html.contains("/api/public/acme/lists/permits.csv"));
    let csp = headers["content-security-policy"].to_str().unwrap();
    assert!(csp.starts_with("default-src 'none'") && !csp.contains("script-src"), "{csp}");
    assert_eq!(headers["x-robots-tag"], "noindex");

    // A plugin's public route is still a plugin's.
    assert_eq!(fetch(&format!("{base}/api/public/acme/forms/submit")).await.2, "a plugin");

    // Another workspace's list of the same name is its own; one without CSV has none.
    let other = workspace(&k, "other").await;
    permit(&k, &other, "Other Holder", "Trade", None, "submitted").await;
    k.add_customization(&other, "XPermit", op(json!({"op": "publish_list", "list": {"name": "permits", "label": "Theirs", "fields": ["holder"]}})), None)
        .await
        .unwrap();
    let theirs: Value = serde_json::from_str(&fetch(&format!("{base}/api/public/other/lists/permits")).await.2).unwrap();
    assert_eq!(theirs["rows"], json!([{"holder": "Other Holder"}]));

    // Everything not published answers the same 404: no such workspace, an inactive one, no such list, one
    // taken down, CSV not offered.
    let sleepy = workspace(&k, "sleepy").await;
    permit(&k, &sleepy, "Sleepy Holder", "Trade", None, "submitted").await;
    k.add_customization(
        &sleepy,
        "XPermit",
        op(json!({"op": "publish_list", "list": {"name": "permits", "label": "P", "fields": ["holder"], "csv": true}})),
        None,
    )
    .await
    .unwrap();
    assert_eq!(fetch(&format!("{base}/api/public/sleepy/lists/permits")).await.0, 200);
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update tenant set status = 'suspended' where slug = 'sleepy'").execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let acme = fresh(&k, &acme).await;
    k.add_customization(&acme, "XPermit", op(json!({"op": "publish_list", "list": {"name": "gone", "label": "Gone", "fields": ["holder"]}})), None)
        .await
        .unwrap();
    let acme = fresh(&k, &acme).await;
    k.add_customization(&acme, "XPermit", op(json!({"op": "unpublish_list", "name": "gone"})), None).await.unwrap();
    let mut answers = BTreeSet::new();
    let mut pages = BTreeSet::new();
    for (ws, name) in [("nobody", "permits"), ("sleepy", "permits"), ("acme", "nothing"), ("acme", "gone"), ("other", "permits.csv"), ("sleepy", "permits.csv")]
    {
        let (status, _, body) = fetch(&format!("{base}/api/public/{ws}/lists/{name}")).await;
        answers.insert((status, body));
        let (status, _, body) = fetch(&format!("{base}/public/{ws}/lists/{name}")).await;
        pages.insert((status, body));
    }
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_eq!(answers.first().unwrap().0, 404);
    assert_eq!(pages.len(), 1, "{pages:?}");
    assert_eq!(pages.first().unwrap().0, 404);

    // A field added to the entity later is not shown until it is listed.
    let acme = fresh(&k, &acme).await;
    let added = k.add_customization(&acme, "XPermit", op(json!({"op": "add_field", "field": {"name": "x_room", "type": "data"}})), None).await.unwrap();
    let acme = fresh(&k, &acme).await;
    permit(&k, &acme, "Roomy", "Trade", None, "submitted").await;
    let mut tx = k.db.tenant_tx(acme.tenant).await.unwrap();
    sqlx::query("update custom_record set ext = ext || '{\"x_room\": \"Room 4\"}' where tenant_id = $1 and entity = 'XPermit'")
        .bind(acme.tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let v: Value = serde_json::from_str(&fetch(&format!("{base}/api/public/acme/lists/permits")).await.2).unwrap();
    assert!(keys(&v["rows"]).iter().all(|k| *k == want) && !v.to_string().contains("Room 4"), "{v}");
    // Listed, it shows; the field gone from the entity drops out of the list, which stands, with a warning.
    let rooms = json!({"name": "rooms", "label": "Rooms", "fields": ["holder", "x_room"], "filter": [["holder", "=", "Roomy"]]});
    k.add_customization(&acme, "XPermit", op(json!({"op": "publish_list", "list": rooms})), None).await.unwrap();
    let v: Value = serde_json::from_str(&fetch(&format!("{base}/api/public/acme/lists/rooms")).await.2).unwrap();
    assert_eq!(v["rows"], json!([{"holder": "Roomy", "x_room": "Room 4"}]));
    k.set_customization_active(&fresh(&k, &acme).await, added, false).await.unwrap();
    let v: Value = serde_json::from_str(&fetch(&format!("{base}/api/public/acme/lists/rooms")).await.2).unwrap();
    assert_eq!(v["rows"], json!([{"holder": "Roomy"}]));
    let acme = fresh(&k, &acme).await;
    assert!(acme.schema.warnings.iter().any(|w| w.message == "published list rooms: x_room is not a field of XPermit"), "{:?}", acme.schema.warnings);

    // Past the limit for an address and a workspace: 429; another workspace has its own budget.
    let small = serve(k.clone(), 2).await;
    for _ in 0..2 {
        assert_eq!(fetch(&format!("{small}/api/public/acme/lists/permits")).await.0, 200);
    }
    assert_eq!(fetch(&format!("{small}/api/public/acme/lists/permits")).await.0, 429);
    assert_eq!(fetch(&format!("{small}/public/acme/lists/permits")).await.0, 429);
    assert_eq!(fetch(&format!("{small}/api/public/nobody/lists/permits")).await.0, 404);
}
