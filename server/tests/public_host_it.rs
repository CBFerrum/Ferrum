//! The customer's side on the workspace's own addresses (docs/builder/decisions.md BD-87), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset by each test, which take their turn): the server as the binary
//! boots it. A portal address serves the workspace's public pages, never another workspace's; its `/` can lead to one;
//! links and published lists answer only on their own workspace's addresses; a public page speaks the visitor's
//! language among those the workspace offers, remembered unless the page is framed.

mod common;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Kernel};
use ferrum_server::app::App;
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use serde_json::{Value, json};
use serial_test::serial;
use std::sync::Arc;
use tower::ServiceExt;

const PASSWORD: &str = "Correct-Horse-9";
const ACME: &str = "my.acme.test";
const GLOBEX: &str = "my.globex.test";

struct World {
    k: Arc<Kernel>,
    r: axum::Router,
    acme: Ctx,
    token: String,
}

fn run<F: std::future::Future<Output = ()> + Send + 'static>(f: impl FnOnce() -> F) {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(f()).await.unwrap() });
}

/// Acme and Globex, each with Forms and a portal address; Acme's administrator's API token.
async fn world() -> World {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    {
        let pool = sqlx::PgPool::connect(&url).await.expect("connect");
        common::reset(&pool).await;
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut config = Environment::Test.load_from_folder(&dir.join("config")).expect("config/test.yaml");
    ferrum_server::settings::set_database_url(&mut config, url);
    if let Some(s) = config.settings.as_mut() {
        s["packages_dir"] = json!(dir.join("../packages").to_string_lossy());
    }
    let base = <App as Hooks>::after_context(AppContext::builder(Environment::Test, config).build()).await.expect("boot");
    let st = base.shared_store.get::<AppState>().expect("state");
    let k = st.kernel.clone();
    let router = App::routes(&base).to_router::<App>(base.clone(), axum::Router::new()).expect("router");
    let r = App::after_routes(router, &base).await.expect("layers");
    let version = st.apps.native_manifest("forms").expect("Forms is compiled in").version.to_string();
    let entry = json!({"head": "<!doctype html><html><head>", "body": "</head><body><main id=form></main>", "tail": "</body></html>"});
    k.register_ui_release("forms", &"0123456789abcdef".repeat(4), &version, &json!({}), &entry).await.unwrap();
    let mut admins = vec![];
    for (slug, host) in [("acme", ACME), ("globex", GLOBEX)] {
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
        let s = k.login(slug, &format!("admin@{slug}.test"), PASSWORD).await.unwrap();
        let admin = k.load_ctx(t, s.user, None, "it".into()).await.unwrap();
        let plan = st.apps.plan(&admin, "forms", None).await.unwrap_or_else(|e| panic!("plan forms: {e}"));
        st.apps.install(&admin, "forms", None, &plan.digest, false).await.unwrap_or_else(|e| panic!("install forms: {e:?}"));
        k.add_subdomain(t, &format!("{slug}.ferrum.test")).await.unwrap();
        k.add_host(&admin, host, "portal").await.unwrap();
        k.host_checked(host, true).await.unwrap();
        admins.push(k.load_ctx(t, s.user, None, "it".into()).await.unwrap());
    }
    let acme = admins.remove(0);
    let token = {
        let res = call(
            &r,
            Method::POST,
            "acme.ferrum.test",
            "/api/auth/login",
            None,
            Some(json!({"tenant": "acme", "email": "admin@acme.test", "password": PASSWORD})),
        )
        .await;
        serde_json::from_str::<Value>(&res.2).unwrap()["token"].as_str().unwrap().to_string()
    };
    World { k, r, acme, token }
}

async fn call(r: &axum::Router, method: Method, host: &str, path: &str, bearer: Option<&str>, body: Option<Value>) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder().method(method).uri(path).header(header::HOST, host).header("x-forwarded-for", "203.0.113.5");
    if let Some(t) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let res = r.clone().oneshot(req).await.unwrap();
    let (status, headers) = (res.status(), res.headers().clone());
    (status, headers, String::from_utf8_lossy(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).into_owned())
}

async fn get_with(r: &axum::Router, host: &str, path: &str, extra: &[(&str, &str)]) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder().method(Method::GET).uri(path).header(header::HOST, host).header("x-forwarded-for", "203.0.113.5");
    for (n, v) in extra {
        req = req.header(*n, *v);
    }
    let res = r.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let (status, headers) = (res.status(), res.headers().clone());
    (status, headers, String::from_utf8_lossy(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).into_owned())
}

fn bootstrap(html: &str) -> Value {
    let start = html.find(r#"id="ferrum-bootstrap">"#).map(|i| i + r#"id="ferrum-bootstrap">"#.len()).unwrap_or_else(|| panic!("no bootstrap: {html}"));
    let end = start + html[start..].find("</script>").unwrap();
    serde_json::from_str(&html[start..end].replace("\\u003c", "<").replace("\\u003e", ">").replace("\\u0026", "&")).unwrap()
}

#[test]
#[serial]
fn form_page_served_on_portal_host() {
    run(|| async {
        let w = world().await;
        let (status, h, html) = get_with(&w.r, ACME, "/forms/f/acme/contact", &[]).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        assert_eq!(bootstrap(&html)["public"], json!("forms/f"));
        // Framed by the workspace's own addresses, this one included (S4-P2), and sized when asked to be.
        let csp = h.get(header::CONTENT_SECURITY_POLICY).unwrap().to_str().unwrap();
        assert!(csp.contains("://my.acme.test") && !csp.contains("globex"), "{csp}");
        let (_, _, html) = get_with(&w.r, ACME, "/forms/f/acme/contact?embed=1", &[]).await;
        let origins = bootstrap(&html)["embed"]["origins"].clone();
        assert!(origins.as_array().unwrap().iter().any(|o| o.as_str().unwrap().ends_with("://my.acme.test")), "{origins}");
    });
}

#[test]
#[serial]
fn other_workspaces_form_on_portal_host_is_gone() {
    run(|| async {
        let w = world().await;
        let theirs = get_with(&w.r, GLOBEX, "/forms/f/acme/contact", &[]).await;
        let nobody = get_with(&w.r, GLOBEX, "/forms/f/nobody/contact", &[]).await;
        assert_eq!(theirs.0, StatusCode::NOT_FOUND, "{}", theirs.2);
        assert_eq!((theirs.0, theirs.2), (nobody.0, nobody.2), "the same answer as for a workspace that does not exist");
        // Nor its brand, its lists or its links.
        assert_eq!(get_with(&w.r, GLOBEX, "/public/acme/lists/anything", &[]).await.0, StatusCode::NOT_FOUND);
        assert!(!get_with(&w.r, GLOBEX, "/portal/", &[]).await.2.contains("acme"));
    });
}

#[test]
#[serial]
fn portal_home_redirects_to_public_page() {
    run(|| async {
        let w = world().await;
        let (status, h, _) = get_with(&w.r, ACME, "/", &[]).await;
        assert!(status.is_redirection() && h[header::LOCATION] == "/portal/", "no home: the portal, as before");
        let (status, _, body) =
            call(&w.r, Method::PATCH, "acme.ferrum.test", &format!("/api/admin/domains/{ACME}"), Some(&w.token), Some(json!({"home": "forms"}))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, h, _) = get_with(&w.r, ACME, "/", &[]).await;
        assert_eq!((status, h[header::LOCATION].to_str().unwrap()), (StatusCode::FOUND, "/forms/f/acme/"));
        // An app with no public page is no portal address's home.
        let (status, _, body) =
            call(&w.r, Method::PATCH, "acme.ferrum.test", &format!("/api/admin/domains/{ACME}"), Some(&w.token), Some(json!({"home": "studio"}))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    });
}

#[test]
#[serial]
fn links_and_lists_refuse_another_workspaces_host() {
    run(|| async {
        let w = world().await;
        let brand = w.k.insert(&w.acme, "Brand", json!({"brand_name": "Acme"}).as_object().cloned().unwrap()).await.unwrap();
        let record = brand["id"].as_str().unwrap().parse().unwrap();
        let spec: ferrum_kernel::action_link::LinkSpec = serde_json::from_value(json!({"actions": ["view"], "show": ["brand_name"]})).unwrap();
        let issued = w.k.issue_link_now(&w.acme, "Brand", record, &spec).await.unwrap();
        let path = issued.url.split_once("/api/public/").map(|(_, p)| format!("/api/public/{p}")).unwrap();
        let (status, _, ours) = get_with(&w.r, ACME, &path, &[]).await;
        assert_eq!(status, StatusCode::OK, "on its own workspace's address: {ours}");
        let theirs = get_with(&w.r, GLOBEX, &path, &[]).await;
        let unknown = get_with(&w.r, GLOBEX, &format!("{}x", &path[..path.len() - 1]), &[]).await;
        assert_eq!(theirs.0, StatusCode::NOT_FOUND, "{}", theirs.2);
        assert_eq!(theirs.2, unknown.2, "the same page as an unknown link");
        let (status, _, _) = get_with(&w.r, GLOBEX, "/api/public/acme/lists/anything", &[]).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    });
}

#[test]
#[serial]
fn lang_query_sets_cookie_except_framed() {
    run(|| async {
        let w = world().await;
        w.k.update_tenant_settings(&w.acme, json!({"public_languages": ["en", "de"]}).as_object().unwrap()).await.unwrap();
        // The browser's language among those offered.
        let (_, _, html) = get_with(&w.r, ACME, "/forms/f/acme/contact", &[("accept-language", "de-AT,de;q=0.9,en;q=0.5")]).await;
        let boot = bootstrap(&html);
        assert_eq!((boot["locale"]["language"].clone(), boot["locale"]["languages"].clone()), (json!("de"), json!(["en", "de"])));
        // Asked in the address: kept in a cookie for the next page.
        let (_, h, html) = get_with(&w.r, ACME, "/forms/f/acme/contact?lang=en", &[("accept-language", "de")]).await;
        assert_eq!(bootstrap(&html)["locale"]["language"], json!("en"));
        let cookie = h.get_all(header::SET_COOKIE).iter().map(|v| v.to_str().unwrap().to_string()).find(|c| c.contains("ferrum-lang=en"));
        assert!(cookie.as_deref().is_some_and(|c| c.contains("SameSite=Lax")), "{h:?}");
        let (_, _, html) = get_with(&w.r, ACME, "/forms/f/acme/contact", &[("cookie", "ferrum-lang=en"), ("accept-language", "de")]).await;
        assert_eq!(bootstrap(&html)["locale"]["language"], json!("en"), "the cookie over the browser");
        // Framed: answered in it, remembered nowhere. Not offered: not taken.
        let (_, h, _) = get_with(&w.r, ACME, "/forms/f/acme/contact?lang=en&embed=1", &[]).await;
        assert!(h.get_all(header::SET_COOKIE).iter().all(|v| !v.to_str().unwrap().contains("ferrum-lang")), "{h:?}");
        let (_, h, html) = get_with(&w.r, ACME, "/forms/f/acme/contact?lang=fr", &[]).await;
        assert_eq!(bootstrap(&html)["locale"]["language"], json!("en"));
        assert!(h.get_all(header::SET_COOKIE).iter().all(|v| !v.to_str().unwrap().contains("ferrum-lang")));
    });
}
