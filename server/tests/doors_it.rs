//! Doors out of a workspace, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`; the database is reset): the whole server, booted as the
//! binary boots it with an app of this test's compiled in, behind its own router.
//!
//! A reply the server did not write, a plugin's (`/api/public/{workspace}/{plugin}/…`, `/api/ext/{plugin}/…`) or an app
//! site's (`/api/site/{workspace}/{package}/…`, `/api/app/{package}/…`), runs nothing on the application's origin unless
//! a native app declares a policy of its own; and a route that names its workspace answers on that workspace's hosts,
//! not on another's.

mod common;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{HookRegistry, Kernel, Site, SiteRequest, SiteResponse};
use ferrum_server::app::App;
use ferrum_server::extensions::{self, Extension};
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

const PASSWORD: &str = "Correct-Horse-9";
const ACME: &str = "erp.acme.test";
const OTHER: &str = "erp.other.test";
/// A host no workspace registered: with neither a base domain nor a default workspace, every workspace's (shape A′).
const SHARED: &str = "erp.shared.test";
/// The server's public address (`PUBLIC_URL`), where callbacks come with no hooks host set: one of acme's hosts too.
const PUBLIC: &str = "erp.public.test";
const PAGE: &str = "<!doctype html><title>Hello</title><script>fetch('/api/auth/token', {method: 'POST'})</script>";
const OWN_POLICY: &str = "default-src 'none'; script-src 'nonce-abc'; style-src 'unsafe-inline'";

const MANIFEST: &str = "id: doors
name: Doors
version: 0.1.0
publisher: ferrum
summary: Pages of a site, as an app answers them
categories: [developer]
license: AGPL-3.0-or-later
runtime: native
platform: \">=0.3, <0.4\"
packages: [doors]
";

fn packages() -> Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    let files: BTreeMap<String, String> =
        [("package.yaml", "package: doors\nversion: 0.1.0\ndepends: [core]\n")].into_iter().map(|(p, d)| (p.to_string(), d.to_string())).collect();
    Ok(vec![ferrum_meta::catalog::load_package_files(&files)?])
}

/// An app's site: a page declaring nothing, one with its own policy, JSON and an attachment, signed in or not.
struct Doors;

#[async_trait]
impl Site for Doors {
    async fn handle(&self, _: &Kernel, req: SiteRequest) -> ferrum_kernel::Result<SiteResponse> {
        let page = || SiteResponse::text(200, "text/html; charset=utf-8", PAGE);
        let mut r = match req.path.as_str() {
            "/page" => page(),
            "/own" => page(),
            "/data" => SiteResponse::json(200, &json!({"ok": true})),
            "/file" => page(),
            _ => return Err(ferrum_kernel::Error::NotFound("route".into())),
        };
        match req.path.as_str() {
            "/own" => r.headers.push(("content-security-policy".into(), OWN_POLICY.into())),
            "/file" => r.headers.push(("content-disposition".into(), "attachment; filename=\"page.html\"".into())),
            _ => {}
        }
        Ok(r)
    }
}

fn register(h: &mut HookRegistry) {
    h.add_site("doors", Arc::new(Doors));
}

#[test]
fn doors_out_of_a_workspace() {
    extensions::extend(vec![Extension {
        id: "doors",
        packages,
        register,
        reports: Vec::new,
        maintenance: |_| Box::pin(async { Ok(()) }),
        manifest: MANIFEST,
        ui: &[],
        tasks: |_| {},
        metrics: |_| Box::pin(async { Ok(String::new()) }),
    }]);
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

/// One request to `host`, as the gateway hands it on: status, headers and body.
async fn call(r: &axum::Router, method: Method, host: &str, path: &str, bearer: Option<&str>, body: Body) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder().method(method).uri(path).header(header::HOST, host).header("x-forwarded-for", "203.0.113.7");
    if let Some(t) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let res = r.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let (status, headers) = (res.status(), res.headers().clone());
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, headers, String::from_utf8_lossy(&bytes).into_owned())
}

async fn get(r: &axum::Router, host: &str, path: &str, bearer: Option<&str>) -> (StatusCode, HeaderMap, String) {
    call(r, Method::GET, host, path, bearer, Body::empty()).await
}

/// Status and JSON body, without the operation id each answer has of its own.
fn answer((status, _, body): (StatusCode, HeaderMap, String)) -> (StatusCode, Value) {
    let mut v: Value = serde_json::from_str(&body).unwrap_or(Value::String(body));
    if let Some(o) = v.as_object_mut() {
        o.remove("operation");
    }
    (status, v)
}

fn said(h: &HeaderMap, name: &str) -> Vec<String> {
    h.get_all(name).iter().map(|v| v.to_str().unwrap_or_default().to_string()).collect()
}

/// A page that runs nothing here: the served files' policy, and never sniffed.
fn sandboxed(h: &HeaderMap) -> bool {
    said(h, "content-security-policy") == [ferrum_server::controllers::system::FILE_SANDBOX] && said(h, "x-content-type-options") == ["nosniff"]
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    {
        let pool = sqlx::PgPool::connect(&url).await.expect("connect");
        common::reset(&pool).await;
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut config = Environment::Test.load_from_folder(&dir.join("config")).expect("config/test.yaml");
    config.database.uri = url;
    config.server.host = format!("http://{PUBLIC}");
    if let Some(s) = config.settings.as_mut() {
        s["packages_dir"] = json!(dir.join("../packages").to_string_lossy());
    }
    let base = <App as Hooks>::after_context(AppContext::builder(Environment::Test, Default::default(), config).build()).await.expect("boot");
    let st = base.shared_store.get::<AppState>().expect("state");
    let k = st.kernel.clone();
    let ctx = AppContext::builder(Environment::Test, Default::default(), base.config.clone()).build();
    ctx.shared_store.insert(st.clone());
    let router = App::routes(&ctx).to_router::<App>(ctx.clone(), axum::Router::new()).expect("router");
    let r = App::after_routes(router, &ctx).await.expect("layers");

    let mut keys = vec![];
    for (slug, host) in [("acme", ACME), ("other", OTHER)] {
        k.create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec!["doors".into()],
            settings: None,
        })
        .await
        .unwrap();
        k.add_operator_host(slug, host, "shell", None).await.unwrap();
        if slug == "acme" {
            k.add_operator_host(slug, PUBLIC, "shell", None).await.unwrap();
        }
        let s = k.login(slug, &format!("admin@{slug}.test"), PASSWORD).await.unwrap();
        let (tenant, _) = k.tenant_by_slug(slug).await.unwrap();
        let admin = k.load_ctx(tenant, s.user, None, "it".into()).await.unwrap();
        keys.push((admin.clone(), k.create_api_key(&admin, s.user, "it", None, None).await.unwrap().1));
    }
    let (admin, key) = keys[0].clone();

    // ── A plugin's page ──
    // It says it is HTML, brings a policy and framing of its own, and a cookie: none of that is the plugin's to say.
    let reply = json!({"status": 200, "headers": {"content-type": "text/html; charset=utf-8", "content-security-policy": "script-src *",
        "x-frame-options": "ALLOWALL", "set-cookie": "taken=1; Path=/"}, "body": PAGE});
    let plugin: ferrum_plugins::NewPlugin =
        serde_json::from_value(json!({"name": "pages", "routes": [{"path": "/page", "public": true}, {"path": "/mine"}]})).unwrap();
    st.plugins.install(&k, &admin, plugin, ferrum_plugins::constant_plugin_wat(&reply.to_string()).as_bytes()).await.unwrap();
    let (status, h, body) = get(&r, ACME, "/api/public/acme/pages/page", None).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, PAGE), "the plugin's page, as it wrote it");
    assert!(sandboxed(&h), "runs nothing on the workspace's origin: {h:?}");
    assert!(said(&h, "x-frame-options").is_empty() && said(&h, "set-cookie").is_empty(), "{h:?}");
    let (status, h, _) = get(&r, ACME, "/api/ext/pages/mine", Some(&key)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(sandboxed(&h), "signed in too: {h:?}");
    // The address names acme: another workspace's host does not answer it, as for a workspace that does not exist; a
    // host that is every workspace's (shape A′) does.
    let nobody = answer(get(&r, ACME, "/api/public/nobody/pages/page", None).await);
    assert_eq!(nobody.0, StatusCode::NOT_FOUND);
    assert_eq!(answer(get(&r, OTHER, "/api/public/acme/pages/page", None).await), nobody, "another workspace's host");
    let (status, h, _) = get(&r, SHARED, "/api/public/acme/pages/page", None).await;
    assert!(status == StatusCode::OK && sandboxed(&h), "a shared host: {status} {h:?}");

    // ── An app site's page ──
    // One that declares no policy runs nothing; a native app's own policy is kept; JSON and attachments are as they were.
    for (path, signed_in) in [("/api/site/acme/doors/page", None), ("/api/app/doors/page", Some(key.as_str()))] {
        let (status, h, body) = get(&r, ACME, path, signed_in).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, PAGE), "{path}");
        assert!(sandboxed(&h), "{path}: {h:?}");
    }
    let (_, h, _) = get(&r, ACME, "/api/site/acme/doors/own", None).await;
    assert_eq!((said(&h, "content-security-policy"), said(&h, "x-content-type-options")), (vec![OWN_POLICY.to_string()], vec!["nosniff".to_string()]));
    for path in ["/api/site/acme/doors/data", "/api/site/acme/doors/file"] {
        let (status, h, _) = get(&r, ACME, path, None).await;
        assert!(status == StatusCode::OK && said(&h, "content-security-policy").is_empty(), "{path}: {h:?}");
    }
    let nobody = answer(get(&r, ACME, "/api/site/nobody/doors/page", None).await);
    assert_eq!(nobody.0, StatusCode::NOT_FOUND);
    assert_eq!(answer(get(&r, OTHER, "/api/site/acme/doors/page", None).await), nobody, "another workspace's host");
    assert_eq!(get(&r, OTHER, "/api/site/other/doors/page", None).await.0, StatusCode::OK, "its own");
    assert_eq!(get(&r, SHARED, "/api/site/acme/doors/page", None).await.0, StatusCode::OK, "a shared host");
    // The address the platform gives outside callers (callback_base: here PUBLIC_URL's host, which is acme's too) answers
    // for every workspace, sandboxed as anywhere; another workspace's own host still does not.
    let (status, h, _) = get(&r, PUBLIC, "/api/site/other/doors/page", None).await;
    assert!(status == StatusCode::OK && sandboxed(&h), "another workspace's route where callbacks come: {status} {h:?}");
    assert_eq!(answer(get(&r, ACME, "/api/site/other/doors/page", None).await), nobody, "acme's own other host");
}
