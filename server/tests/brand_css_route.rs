//! A brand's stylesheet and files (`/_brand/{workspace}/{brand}…`, docs/builder/decisions.md BD-79) against a real
//! PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset): the whole server as the binary boots it, behind its
//! own router. Served to anyone on the workspace's own hosts, never on another workspace's; a brand's logo is read by a
//! stranger only through its brand, typed by its bytes; an address takes only a brand of its own workspace.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use ferrum_kernel::tenant::NewTenant;
use ferrum_server::app::App;
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use serde_json::{Value, json};
use tower::ServiceExt;

const PASSWORD: &str = "Correct-Horse-9";
const ACME: &str = "acme.ferrum.test";
const OTHER: &str = "other.ferrum.test";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89";

#[test]
fn other_workspace_host_is_404() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("{} {}", self.status, String::from_utf8_lossy(&self.body)))
    }
    fn header(&self, name: header::HeaderName) -> &str {
        self.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default()
    }
}

async fn call(router: &axum::Router, method: Method, host: &str, path: &str, bearer: Option<&str>, body: Option<(String, Vec<u8>)>) -> Reply {
    let mut req = Request::builder().method(method).uri(path).header(header::HOST, host).header("x-forwarded-for", "203.0.113.9");
    if let Some(t) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req = match body {
        Some((ctype, bytes)) => req.header(header::CONTENT_TYPE, ctype).body(Body::from(bytes)),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    let (status, headers) = (res.status(), res.headers().clone());
    Reply { status, headers, body: axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec() }
}

fn json_body(v: Value) -> Option<(String, Vec<u8>)> {
    Some(("application/json".into(), v.to_string().into_bytes()))
}

/// An upload of `bytes`, claiming to be `ctype`: its `/files/{id}`.
async fn upload(router: &axum::Router, token: &str, ctype: &str, bytes: &[u8]) -> String {
    let mut body = b"--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"logo\"\r\nContent-Type: ".to_vec();
    body.extend_from_slice(ctype.as_bytes());
    body.extend_from_slice(b"\r\n\r\n");
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n--XyZ--\r\n");
    let r = call(router, Method::POST, ACME, "/api/files", Some(token), Some(("multipart/form-data; boundary=XyZ".into(), body))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", String::from_utf8_lossy(&r.body));
    r.json()["url"].as_str().unwrap().to_string()
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    {
        let pool = sqlx::PgPool::connect(&url).await.expect("connect");
        common::reset(&pool).await;
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut config = Environment::Test.load_from_folder(&dir.join("config")).expect("config/test.yaml");
    ferrum_server::settings::set_database_url(&mut config, url.clone());
    if let Some(s) = config.settings.as_mut() {
        s["packages_dir"] = json!(dir.join("../packages").to_string_lossy());
    }
    let base = <App as Hooks>::after_context(AppContext::builder(Environment::Test, config).build()).await.expect("boot");
    let st = base.shared_store.get::<AppState>().expect("state");
    let router = App::routes(&base).to_router::<App>(base.clone(), axum::Router::new()).expect("router");
    let router = App::after_routes(router, &base).await.expect("layers");
    let k = st.kernel.clone();
    for slug in ["acme", "other"] {
        let t = k
            .create_tenant(NewTenant {
                slug: slug.into(),
                name: slug.into(),
                admin_email: format!("admin@{slug}.test"),
                admin_password: PASSWORD.into(),
                admin_name: None,
                packages: vec![],
                settings: None,
            })
            .await
            .unwrap();
        k.add_subdomain(t, &format!("{slug}.ferrum.test")).await.unwrap();
    }
    let token = {
        let r =
            call(&router, Method::POST, ACME, "/api/auth/login", None, json_body(json!({"tenant": "acme", "email": "admin@acme.test", "password": PASSWORD})))
                .await;
        r.json()["token"].as_str().expect("a token").to_string()
    };

    // A logo uploaded as WebP that is a PNG, and a favicon claiming PNG that is a page.
    let logo = upload(&router, &token, "image/webp", PNG).await;
    let favicon = upload(&router, &token, "image/png", b"<!doctype html><script>alert(1)</script>").await;
    let r = call(
        &router,
        Method::POST,
        ACME,
        "/api/v1/Brand",
        Some(&token),
        json_body(json!({"brand_name": "Acme", "primary_color": "#ffff00", "logo": logo, "favicon": favicon, "is_default": true})),
    )
    .await;
    assert!(r.status.is_success(), "{} {}", r.status, String::from_utf8_lossy(&r.body));
    let brand = r.json();
    assert_eq!(brand["shown_color"], "#707000", "nudged for contrast and shown on the record");
    let id = brand["id"].as_str().unwrap().to_string();

    // On the workspace's own host: the stylesheet, from validated colours only, kept at its version.
    let shown = k.brand_by_id(k.tenant_for_host(ACME).await.unwrap().unwrap(), id.parse().unwrap()).await.unwrap().unwrap();
    let css_path = format!("/_brand/acme/{id}.css?v={}", shown.version);
    let r = call(&router, Method::GET, ACME, &css_path, None, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!((r.header(header::CONTENT_TYPE), r.header(header::X_CONTENT_TYPE_OPTIONS)), ("text/css; charset=utf-8", "nosniff"));
    assert_eq!(r.header(header::CACHE_CONTROL), "public, max-age=31536000, immutable");
    assert!(String::from_utf8_lossy(&r.body).contains("--ferrum-primary:light-dark(#707000, #ffff00)"));
    let r = call(&router, Method::GET, ACME, &format!("/_brand/acme/{id}.css?v=old"), None, None).await;
    assert_eq!((r.status, r.header(header::CACHE_CONTROL)), (StatusCode::OK, "public, max-age=300"));

    // Another workspace's host, an unknown brand, an unknown workspace, a brand named under another workspace: not found.
    for (host, path) in [
        (OTHER, css_path.clone()),
        (OTHER, format!("/_brand/acme/{id}/logo")),
        (ACME, format!("/_brand/acme/{}.css", uuid::Uuid::now_v7())),
        (ACME, format!("/_brand/nobody/{id}.css")),
        (OTHER, format!("/_brand/other/{id}.css")),
        (ACME, format!("/_brand/acme/{id}/brand_name")),
    ] {
        assert_eq!(call(&router, Method::GET, host, &path, None, None).await.status, StatusCode::NOT_FOUND, "{host}{path}");
    }

    // The logo: to a stranger, through its brand only, as the type its bytes are; the page claiming to be a PNG: never.
    let r = call(&router, Method::GET, ACME, &format!("/_brand/acme/{id}/logo?v={}", shown.version), None, None).await;
    assert_eq!((r.status, r.header(header::CONTENT_TYPE), r.body.as_slice()), (StatusCode::OK, "image/png", PNG));
    assert_eq!(r.header(header::X_CONTENT_TYPE_OPTIONS), "nosniff");
    assert_eq!(call(&router, Method::GET, ACME, &format!("/_brand/acme/{id}/favicon"), None, None).await.status, StatusCode::NOT_FOUND);
    let file_id = logo.trim_start_matches("/files/");
    assert_eq!(call(&router, Method::GET, ACME, &format!("/files/public/{file_id}"), None, None).await.status, StatusCode::NOT_FOUND);
    assert!(call(&router, Method::GET, ACME, &logo, None, None).await.status.is_client_error(), "not without signing in");
    // Gone with its brand.
    let r = call(&router, Method::PUT, ACME, &format!("/api/v1/Brand/{id}"), Some(&token), json_body(json!({"logo": null}))).await;
    assert!(r.status.is_success(), "{} {}", r.status, String::from_utf8_lossy(&r.body));
    assert_eq!(call(&router, Method::GET, ACME, &format!("/_brand/acme/{id}/logo"), None, None).await.status, StatusCode::NOT_FOUND);

    // An address takes a brand of its own workspace only; `null` gives it back the default.
    let other_token = {
        let r = call(
            &router,
            Method::POST,
            OTHER,
            "/api/auth/login",
            None,
            json_body(json!({"tenant": "other", "email": "admin@other.test", "password": PASSWORD})),
        )
        .await;
        r.json()["token"].as_str().expect("a token").to_string()
    };
    let r = call(&router, Method::PATCH, OTHER, &format!("/api/admin/domains/{OTHER}"), Some(&other_token), json_body(json!({"brand": id}))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", String::from_utf8_lossy(&r.body));
    let r = call(&router, Method::PATCH, ACME, &format!("/api/admin/domains/{ACME}"), Some(&token), json_body(json!({"brand": id}))).await;
    assert_eq!((r.status, r.json()["brand"].as_str()), (StatusCode::OK, Some(id.as_str())));
    let r = call(&router, Method::PATCH, ACME, &format!("/api/admin/domains/{ACME}"), Some(&token), json_body(json!({"brand": null}))).await;
    assert_eq!((r.status, r.json()["brand"].clone()), (StatusCode::OK, Value::Null));
}
