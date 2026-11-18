//! Doors out of a workspace, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`; the database is reset): the whole server, booted as the
//! binary boots it with an app of this test's compiled in, behind its own router.
//!
//! A reply the server did not write, a plugin's (`/api/public/{workspace}/{plugin}/…`, `/api/ext/{plugin}/…`) or an app
//! site's (`/api/site/{workspace}/{package}/…`, `/api/app/{package}/…`), runs nothing on the application's origin unless
//! a native app declares a policy of its own; and a route that names its workspace answers on that workspace's hosts,
//! not on another's. An AI client at `/mcp` reads under the workspace's policy for the assistant, with a bearer credential
//! only and from no page of another origin. A body past its
//! route's limit is refused as too large (413) wherever the server refuses one. Administration's list of packages is
//! the administrators'.

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
platform: \">=0.4, <0.5\"
packages: [doors]
";

/// Its package: a person's record whose pay is in the `payroll` field group.
fn packages() -> Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    let files: BTreeMap<String, String> = [
        ("package.yaml", "package: doors\nversion: 0.1.0\ndepends: [core]\n"),
        (
            "entities/employee.yaml",
            "name: DoorsEmployee\nlabel: Employee\nkind: master\ntitle_field: full_name\nfields:\n  - {name: full_name, type: data}\n  - {name: salary, type: data, perm_group: payroll}\n",
        ),
    ]
    .into_iter()
    .map(|(p, d)| (p.to_string(), d.to_string()))
    .collect();
    Ok(vec![ferrum_meta::catalog::load_package_files(&files)?])
}

/// An app's site: a page declaring nothing, one with its own policy, JSON and an attachment, signed in or not; and a
/// form that takes 16 KB at most.
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
            "/form" => SiteResponse::json(201, &json!({"taken": req.body.len()})),
            _ => return Err(ferrum_kernel::Error::NotFound("route".into())),
        };
        match req.path.as_str() {
            "/own" => r.headers.push(("content-security-policy".into(), OWN_POLICY.into())),
            "/file" => r.headers.push(("content-disposition".into(), "attachment; filename=\"page.html\"".into())),
            _ => {}
        }
        Ok(r)
    }

    fn max_body(&self) -> usize {
        16 << 10
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
        maintenance: |_, _| Box::pin(async { Ok(()) }),
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

/// A POST of `body` to `/mcp` on acme with these headers.
async fn mcp_post(r: &axum::Router, headers: &[(&str, &str)], body: String) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder().method(Method::POST).uri("/mcp").header(header::HOST, ACME).header("x-forwarded-for", "203.0.113.7");
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let res = r.clone().oneshot(req.header(header::CONTENT_TYPE, "application/json").body(Body::from(body)).unwrap()).await.unwrap();
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
    ferrum_server::settings::set_database_url(&mut config, url);
    config.server.host = format!("http://{PUBLIC}");
    if let Some(s) = config.settings.as_mut() {
        s["packages_dir"] = json!(dir.join("../packages").to_string_lossy());
        // Files of 64 KB at most, so that one past it is small to send.
        s["upload_max_bytes"] = json!(64 << 10);
    }
    let base = <App as Hooks>::after_context(AppContext::builder(Environment::Test, config).build()).await.expect("boot");
    let st = base.shared_store.get::<AppState>().expect("state");
    let k = st.kernel.clone();
    let ctx = AppContext::builder(Environment::Test, base.config.clone()).build();
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

    // ── An AI client at /mcp ──
    // Its provider gets what it reads, so the workspace's policy for the assistant is its: what may not go to a
    // provider (the payroll field group, here) is not read, and where the assistant is off the door is shut.
    let pat = k.insert(&admin, "DoorsEmployee", json!({"full_name": "Pat Doe", "salary": "SALARY-123456"}).as_object().cloned().unwrap()).await.unwrap();
    let get_record =
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "get_record", "arguments": {"entity": "DoorsEmployee", "id": pat["id"]}}});
    let mcp = |policy: Value| {
        let (k, r, admin, key, get_record) = (k.clone(), r.clone(), admin.clone(), key.clone(), get_record.clone());
        async move {
            k.update_tenant_settings(&admin, policy.as_object().unwrap()).await.unwrap();
            let bearer = format!("Bearer {key}");
            let (status, _, body) = mcp_post(&r, &[("authorization", &bearer), ("mcp-protocol-version", "2025-11-25")], get_record.to_string()).await;
            (status, body)
        }
    };
    let (status, body) = mcp(json!({"assistant_egress": {"groups": [], "entities": []}})).await;
    assert!(status == StatusCode::OK && body.contains("Pat Doe"), "{status} {body}");
    assert!(!body.contains("SALARY-123456"), "payroll is withheld from AI providers: {body}");
    let (status, body) = mcp(json!({"assistant_egress": {"groups": ["payroll"], "entities": []}})).await;
    assert!(status == StatusCode::OK && body.contains("SALARY-123456"), "and goes where the workspace lets it: {status} {body}");
    let (status, body) = mcp(json!({"assistant": false})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the assistant turned off: {body}");
    k.update_tenant_settings(&admin, json!({"assistant": true}).as_object().unwrap()).await.unwrap();
    // An agent is a program: a bearer credential only, never the browser's cookie, and no page of another origin.
    let bearer = format!("Bearer {key}");
    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string();
    let legacy: &[(&str, &str)] = &[("authorization", &bearer), ("mcp-protocol-version", "2025-11-25")];
    let challenged =
        |h: &HeaderMap| said(h, "www-authenticate").iter().any(|c| c.contains("resource_metadata=\"") && c.contains("/.well-known/oauth-protected-resource"));
    let (status, h, body) = mcp_post(&r, &[("mcp-protocol-version", "2025-11-25")], list.clone()).await;
    assert!(status == StatusCode::UNAUTHORIZED && challenged(&h), "no credential: {status} {h:?} {body}");
    let cookie = format!("{}=anything", ferrum_server::shell::cookie_name(false));
    let (status, h, body) = mcp_post(&r, &[("cookie", &cookie), ("sec-fetch-site", "same-origin"), ("mcp-protocol-version", "2025-11-25")], list.clone()).await;
    assert!(status == StatusCode::UNAUTHORIZED && challenged(&h) && body.contains("Bearer"), "the cookie is not taken: {status} {body}");
    let (status, _, body) = mcp_post(&r, &[legacy, &[("origin", "https://evil.example")]].concat(), list.clone()).await;
    let refused: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN, "a foreign Origin: {body}");
    assert!(refused["error"]["code"] == json!(-32600) && refused.get("id").is_none(), "{refused}");
    assert_eq!(mcp_post(&r, &[legacy, &[("origin", "null")]].concat(), list.clone()).await.0, StatusCode::FORBIDDEN, "an opaque Origin");
    let (status, _, body) = mcp_post(&r, &[legacy, &[("origin", "http://erp.acme.test")]].concat(), list.clone()).await;
    assert!(status == StatusCode::OK && body.contains("ferrum_overview"), "this host's own Origin: {status} {body}");
    let (status, _, body) = mcp_post(&r, &[("authorization", &bearer)], list.clone()).await;
    assert!(status == StatusCode::BAD_REQUEST && body.contains("-32020"), "no MCP-Protocol-Version: {body}");
    let (status, _, _) = mcp_post(&r, legacy, format!("[{list}]")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "one message per POST");
    let (status, _, body) = mcp_post(&r, &[("authorization", &bearer), ("mcp-protocol-version", "1900-01-01")], list.clone()).await;
    assert!(status == StatusCode::BAD_REQUEST && body.contains("-32022") && body.contains("2026-07-28"), "{body}");
    assert_eq!(mcp_post(&r, legacy, " ".repeat(1 << 20) + &list).await.0, StatusCode::PAYLOAD_TOO_LARGE, "a body past a mebibyte");

    // ── A body past its route's limit ──
    // Too large (413) wherever the server refuses one: a site's route that takes 16 KB, signed in or not, a plugin's
    // route past its megabyte, a file past the largest the server takes. What fits is taken.
    let too_large = |(status, _, body): (StatusCode, HeaderMap, String)| {
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        (status, v["error"].as_str().unwrap_or_default().to_string())
    };
    let refused = (StatusCode::PAYLOAD_TOO_LARGE, "too_large".to_string());
    for (path, bearer) in [("/api/site/acme/doors/form", None), ("/api/app/doors/form", Some(key.as_str()))] {
        assert_eq!(too_large(call(&r, Method::POST, ACME, path, bearer, Body::from(vec![b'x'; 1 << 20])).await), refused, "{path}");
        assert_eq!(call(&r, Method::POST, ACME, path, bearer, Body::from(vec![b'x'; 1 << 10])).await.0, StatusCode::CREATED, "{path}");
    }
    assert_eq!(too_large(call(&r, Method::POST, ACME, "/api/public/acme/pages/page", None, Body::from(vec![b'x'; 2 << 20])).await), refused, "a plugin's");
    let boundary = "doors-form";
    let mut form =
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"big.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n")
            .into_bytes();
    form.extend(vec![0u8; 100 << 10]);
    form.extend(format!("\r\n--{boundary}--\r\n").into_bytes());
    let upload = Request::post("/api/files")
        .header(header::HOST, ACME)
        .header("x-forwarded-for", "203.0.113.7")
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
        .body(Body::from(form))
        .unwrap();
    let res = r.clone().oneshot(upload).await.unwrap();
    let (status, headers) = (res.status(), res.headers().clone());
    let body = String::from_utf8_lossy(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).into_owned();
    assert_eq!(too_large((status, headers, body)), refused, "a file past upload_max_bytes");

    // ── Administration's list of packages ──
    // Only the Administration page reads it, which administrators alone open: a member is refused, as on every other
    // administration route.
    let member = k.create_user(&admin, "member@acme.test", "Member", Some(PASSWORD), &[], false).await.unwrap();
    let member_key = k.create_api_key(&admin, member, "it", None, None).await.unwrap().1;
    let (status, _, body) = get(&r, ACME, "/api/admin/packages", Some(&member_key)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a member: {body}");
    let (status, _, body) = get(&r, ACME, "/api/admin/packages", Some(&key)).await;
    assert!(status == StatusCode::OK && body.contains("\"doors\""), "an administrator: {status}");
}
