//! A customer's session on a site host (docs/builder/decisions.md BD-89, supersedes BD-26), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset by each test, which take their turn): the server as the binary boots
//! it, with two site apps of this test's compiled in. A sign-in on the portal is handed to a site host of the same
//! workspace by a one-use code bound to that host and to the browser that set off; the session it opens works on that
//! host alone, for that host's app alone, as the person's customer self; no other session works there.

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
use serial_test::serial;
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

const PASSWORD: &str = "Correct-Horse-9";
const PORTAL: &str = "my.acme.test";
const SHOP: &str = "shop.acme.test";
const OTHER: &str = "other.acme.test";
const GLOBEX_SHOP: &str = "shop.globex.test";

fn manifest(id: &str) -> &'static str {
    match id {
        "shop" => {
            "id: shop\nname: Shop\nversion: 0.1.0\npublisher: ferrum\nsummary: A site\ncategories: [developer]\nlicense: AGPL-3.0-or-later\nruntime: native\nplatform: \">=0.4, <0.5\"\npackages: [shop]\n"
        }
        _ => {
            "id: other\nname: Other\nversion: 0.1.0\npublisher: ferrum\nsummary: A site\ncategories: [developer]\nlicense: AGPL-3.0-or-later\nruntime: native\nplatform: \">=0.4, <0.5\"\npackages: [other]\n"
        }
    }
}

fn package(name: &str) -> Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    let files: BTreeMap<String, String> = [("package.yaml".to_string(), format!("package: {name}\nversion: 0.1.0\ndepends: [core]\n"))].into_iter().collect();
    Ok(vec![ferrum_meta::catalog::load_package_files(&files)?])
}

/// A site that says who it was told the caller is.
struct Echo(&'static str);

#[async_trait]
impl Site for Echo {
    async fn handle(&self, _: &Kernel, req: SiteRequest) -> ferrum_kernel::Result<SiteResponse> {
        let user = req.user.as_ref().map(|c| json!({"email": c.email, "outside": c.outside, "admin": c.is_admin, "roles": c.roles}));
        Ok(SiteResponse::json(200, &json!({"site": self.0, "path": req.path, "user": user})))
    }
}

fn register(h: &mut HookRegistry) {
    h.add_site("shop", Arc::new(Echo("shop")));
    h.add_site("other", Arc::new(Echo("other")));
}

/// The two apps, compiled in once for every test of this binary.
fn extend() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        extensions::extend(vec![
            Extension {
                id: "shop",
                packages: || package("shop"),
                register,
                reports: Vec::new,
                maintenance: |_, _| Box::pin(async { Ok(()) }),
                manifest: manifest("shop"),
                ui: &[],
                tasks: |_| {},
                metrics: |_| Box::pin(async { Ok(String::new()) }),
            },
            Extension {
                id: "other",
                packages: || package("other"),
                register: |_| {},
                reports: Vec::new,
                maintenance: |_, _| Box::pin(async { Ok(()) }),
                manifest: manifest("other"),
                ui: &[],
                tasks: |_| {},
                metrics: |_| Box::pin(async { Ok(String::new()) }),
            },
        ])
    });
}

struct World {
    k: Arc<Kernel>,
    r: axum::Router,
    /// Acme's administrator's browser session (a member), signed in on the portal host.
    member: String,
}

fn run<F: std::future::Future<Output = ()> + Send + 'static>(f: impl FnOnce() -> F) {
    extend();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(f()).await.unwrap() });
}

/// Acme with a portal host and two site hosts (Shop, Other); Globex with a site host of its own.
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
    for slug in ["acme", "globex"] {
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
        for app in ["shop", "other"] {
            let plan = st.apps.plan(&admin, app, None).await.unwrap_or_else(|e| panic!("plan {app}: {e}"));
            st.apps.install(&admin, app, None, &plan.digest, false).await.unwrap_or_else(|e| panic!("install {app}: {e:?}"));
        }
        k.add_subdomain(t, &format!("{slug}.ferrum.test")).await.unwrap();
        let hosts: &[(&str, &str)] =
            if slug == "acme" { &[(PORTAL, "portal"), (SHOP, "site:shop"), (OTHER, "site:other")] } else { &[(GLOBEX_SHOP, "site:shop")] };
        for (host, target) in hosts {
            k.add_host(&admin, host, target).await.unwrap();
            k.host_checked(host, true).await.unwrap();
        }
    }
    let member = {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/auth/login")
            .header(header::HOST, PORTAL)
            .header("x-forwarded-for", "203.0.113.5")
            .header("sec-fetch-site", "same-origin")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"tenant": "acme", "email": "admin@acme.test", "password": PASSWORD}).to_string()))
            .unwrap();
        let res = r.clone().oneshot(req).await.unwrap();
        cookies(res.headers()).remove(ferrum_server::shell::cookie_name(false)).expect("a page's sign-in sets the session cookie")
    };
    World { k, r, member }
}

/// The cookies a response sets, by name (an emptied one as empty).
fn cookies(h: &HeaderMap) -> BTreeMap<String, String> {
    h.get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|c| c.split(';').next()?.split_once('=').map(|(n, v)| (n.to_string(), v.to_string())))
        .collect()
}

/// One request to `host` with these cookies and headers: status, headers, body.
async fn call(r: &axum::Router, method: Method, host: &str, path: &str, jar: &[(&str, &str)], extra: &[(&str, &str)]) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder().method(method).uri(path).header(header::HOST, host).header("x-forwarded-for", "203.0.113.5");
    if !jar.is_empty() {
        req = req.header(header::COOKIE, jar.iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join("; "));
    }
    for (n, v) in extra {
        req = req.header(*n, *v);
    }
    let res = r.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let (status, headers) = (res.status(), res.headers().clone());
    (status, headers, String::from_utf8_lossy(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).into_owned())
}

/// Where a redirect leads, as a path on its host: `(host, path and query)`.
fn location(h: &HeaderMap) -> (String, String) {
    let to = h.get(header::LOCATION).expect("a redirect").to_str().unwrap();
    let rest = to.split_once("://").map_or(to, |(_, r)| r);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    (host.to_string(), format!("/{path}"))
}

/// The site's answer: who it was told the caller is.
fn user_of(body: &str) -> Value {
    serde_json::from_str::<Value>(body).unwrap_or_else(|_| panic!("{body}"))["user"].clone()
}

const SESSION: fn() -> &'static str = || ferrum_server::shell::cookie_name(false);

/// Steps 1 and 2: the site host sends the browser to the portal with a state it keeps; the portal, signed in, sends it
/// back with a code. `(state cookie, callback path)`.
async fn handed(w: &World) -> ((String, String), String) {
    let (status, h, body) = call(&w.r, Method::GET, SHOP, "/_auth/sign-in?next=/account", &[], &[]).await;
    assert!(status.is_redirection(), "{status} {body}");
    let state_cookie = cookies(&h).into_iter().find(|(n, _)| n != SESSION()).expect("the state cookie");
    let (host, handoff) = location(&h);
    assert_eq!(host, PORTAL, "to the workspace's portal host");
    let (status, h, body) = call(&w.r, Method::GET, PORTAL, &handoff, &[(SESSION(), &w.member)], &[]).await;
    assert!(status.is_redirection(), "{status} {body}");
    let (host, callback) = location(&h);
    assert_eq!(host, SHOP);
    (state_cookie, callback)
}

/// Steps 1 to 4: a site session on Shop.
async fn site_session(w: &World) -> String {
    let ((n, v), callback) = handed(w).await;
    let (status, h, body) = call(&w.r, Method::GET, SHOP, &callback, &[(&n, &v)], &[]).await;
    assert!(status.is_redirection(), "{status} {body}");
    assert_eq!(location(&h).1, "/account");
    cookies(&h).remove(SESSION()).filter(|c| !c.is_empty()).expect("the site session's cookie")
}

#[test]
#[serial]
fn staff_session_refused_on_site_host() {
    run(|| async {
        let w = world().await;
        let (status, _, body) = call(&w.r, Method::GET, SHOP, "/hello", &[(SESSION(), &w.member)], &[]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(user_of(&body), Value::Null, "a member's own session is nobody on a site");
        let (_, _, body) = call(&w.r, Method::GET, SHOP, "/api/site/acme/shop/x", &[(SESSION(), &w.member)], &[("sec-fetch-site", "same-origin")]).await;
        assert_eq!(user_of(&body), Value::Null);
    });
}

#[test]
#[serial]
fn site_session_refused_on_portal_and_other_site() {
    run(|| async {
        let w = world().await;
        let site = site_session(&w).await;
        for host in [PORTAL, "acme.ferrum.test"] {
            let (status, _, body) = call(&w.r, Method::GET, host, "/api/auth/me", &[(SESSION(), &site)], &[("sec-fetch-site", "same-origin")]).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{host}: {body}");
        }
        let (_, _, body) = call(&w.r, Method::GET, OTHER, "/hello", &[(SESSION(), &site)], &[]).await;
        assert_eq!(user_of(&body), Value::Null, "another site of the same workspace");
    });
}

#[test]
#[serial]
fn member_is_customer_on_site() {
    run(|| async {
        let w = world().await;
        let site = site_session(&w).await;
        let (status, _, body) = call(&w.r, Method::GET, SHOP, "/account", &[(SESSION(), &site)], &[]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let user = user_of(&body);
        assert_eq!((user["email"].clone(), user["outside"].clone(), user["admin"].clone()), (json!("admin@acme.test"), json!(true), json!(false)), "{user}");
        assert_eq!(user["roles"], json!([]), "portal roles only, and this workspace has none: {user}");
    });
}

#[test]
#[serial]
fn handoff_refuses_foreign_host() {
    run(|| async {
        let w = world().await;
        for host in [GLOBEX_SHOP, PORTAL, "acme.ferrum.test", "nowhere.test"] {
            let path = format!("/_auth/handoff?host={host}&state=0123456789abcdef");
            let (status, h, body) = call(&w.r, Method::GET, PORTAL, &path, &[(SESSION(), &w.member)], &[]).await;
            assert!(status.is_client_error() && h.get(header::LOCATION).is_none(), "{host}: {status} {body}");
        }
        let mut tx = w.k.db.system_tx().await.unwrap();
        let minted: i64 = sqlx::query_scalar("select count(*) from host_code").fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(minted, 0, "nothing minted");
    });
}

#[test]
#[serial]
fn callback_needs_its_state_cookie() {
    run(|| async {
        let w = world().await;
        let (_, callback) = handed(&w).await;
        for jar in [vec![], vec![("ferrum_auth_x", "nothing")]] {
            let (status, h, body) = call(&w.r, Method::GET, SHOP, &callback, &jar, &[]).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(cookies(&h).get(SESSION()).is_none_or(|c| c.is_empty()), "no session");
        }
    });
}

#[test]
#[serial]
fn code_used_once_under_race() {
    run(|| async {
        let w = world().await;
        let ((n, v), callback) = handed(&w).await;
        let go = || {
            let (r, n, v, callback) = (w.r.clone(), n.clone(), v.clone(), callback.clone());
            tokio::spawn(async move { call(&r, Method::GET, SHOP, &callback, &[(&n, &v)], &[]).await })
        };
        let (a, b) = (go(), go());
        let opened = [a.await.unwrap(), b.await.unwrap()].iter().filter(|(_, h, _)| cookies(h).get(SESSION()).is_some_and(|c| !c.is_empty())).count();
        assert_eq!(opened, 1, "one code, one session");
    });
}

#[test]
#[serial]
fn site_session_reaches_only_its_hosts_app() {
    run(|| async {
        let w = world().await;
        let site = site_session(&w).await;
        let same = [("sec-fetch-site", "same-origin")];
        let (_, _, body) = call(&w.r, Method::GET, SHOP, "/api/site/acme/shop/orders", &[(SESSION(), &site)], &same).await;
        assert_eq!(user_of(&body)["email"], "admin@acme.test", "{body}");
        let (_, _, body) = call(&w.r, Method::GET, SHOP, "/api/site/acme/other/orders", &[(SESSION(), &site)], &same).await;
        assert_eq!(user_of(&body), Value::Null, "another app's routes on the same host");
    });
}

#[test]
#[serial]
fn cross_site_post_refused() {
    run(|| async {
        let w = world().await;
        let site = site_session(&w).await;
        let (status, _, body) = call(&w.r, Method::POST, SHOP, "/api/site/acme/shop/cart", &[(SESSION(), &site)], &[("sec-fetch-site", "cross-site")]).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let (status, _, body) = call(&w.r, Method::POST, SHOP, "/api/site/acme/shop/cart", &[(SESSION(), &site)], &[("sec-fetch-site", "same-origin")]).await;
        assert_eq!((status, user_of(&body)["email"].clone()), (StatusCode::OK, json!("admin@acme.test")), "{body}");
        // Signing out on the site ends that session there.
        let (status, h, _) = call(&w.r, Method::POST, SHOP, "/_auth/sign-out", &[(SESSION(), &site)], &[("sec-fetch-site", "same-origin")]).await;
        assert!(status.is_redirection(), "{status}");
        assert_eq!(cookies(&h).get(SESSION()).map(String::as_str), Some(""), "the cookie is cleared");
        let (_, _, body) = call(&w.r, Method::GET, SHOP, "/hello", &[(SESSION(), &site)], &[]).await;
        assert_eq!(user_of(&body), Value::Null, "the ended session is nobody");
    });
}
