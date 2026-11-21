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
        if req.path.ends_with("fail") {
            return Err(ferrum_kernel::Error::invalid("this order cannot be read"));
        }
        let mut r = SiteResponse::json(200, &json!({"site": self.0, "path": req.path, "user": user}));
        if req.path.ends_with("nostore") {
            r.headers.push(("cache-control".into(), "no-store".into()));
        }
        if req.path.ends_with("cached") {
            r.headers.push(("cache-control".into(), "public, max-age=60".into()));
        }
        if req.path.ends_with("plant") {
            // A site that tries to set the platform's own cookie, beside one of its own.
            r.headers.push(("set-cookie".into(), format!("{}=planted; Path=/", ferrum_server::shell::cookie_name(false))));
            r.headers.push(("Set-Cookie".into(), "ferrum_auth-0123456789ab=x.y; Path=/".into()));
            r.headers.push(("set-cookie".into(), "app_pref=1; Path=/".into()));
        }
        Ok(r)
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
    st: AppState,
    r: axum::Router,
    /// Acme's administrator's browser session (a member), signed in on the portal host.
    member: String,
    /// Acme's customer's (a portal person, pat@acme.test) browser session on the portal host.
    customer: String,
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
    let s = k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
    let admin = k.load_ctx(s.tenant, s.user, None, "it".into()).await.unwrap();
    let pat = k.create_user(&admin, "pat@acme.test", "Pat", Some(PASSWORD), &[], false).await.unwrap();
    k.set_user_kind(&admin, pat, "portal").await.unwrap();
    let signed_in = |email: &'static str| {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/auth/login")
            .header(header::HOST, PORTAL)
            .header("x-forwarded-for", "203.0.113.5")
            .header("sec-fetch-site", "same-origin")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"tenant": "acme", "email": email, "password": PASSWORD}).to_string()))
            .unwrap();
        let r = r.clone();
        async move {
            let res = r.oneshot(req).await.unwrap();
            cookies(res.headers()).remove(ferrum_server::shell::cookie_name(false)).expect("a page's sign-in sets the session cookie")
        }
    };
    let (member, customer) = (signed_in("admin@acme.test").await, signed_in("pat@acme.test").await);
    World { k, r, member, customer, st: st.clone() }
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
    handed_to(w, "/account").await
}

/// [`handed`], to come back to `next`.
async fn handed_to(w: &World, next: &str) -> ((String, String), String) {
    let (status, h, body) = call(&w.r, Method::GET, SHOP, &format!("/_auth/sign-in?next={next}"), &[], &[]).await;
    assert!(status.is_redirection(), "{status} {body}");
    let state_cookie = cookies(&h).into_iter().find(|(n, _)| n != SESSION()).expect("the state cookie");
    let (host, handoff) = location(&h);
    assert_eq!(host, PORTAL, "to the workspace's portal host");
    let (status, h, body) = call(&w.r, Method::GET, PORTAL, &handoff, &[(SESSION(), &w.customer)], &[]).await;
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
fn a_customer_is_a_customer_on_site() {
    run(|| async {
        let w = world().await;
        let site = site_session(&w).await;
        let (status, _, body) = call(&w.r, Method::GET, SHOP, "/account", &[(SESSION(), &site)], &[]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let user = user_of(&body);
        assert_eq!((user["email"].clone(), user["outside"].clone(), user["admin"].clone()), (json!("pat@acme.test"), json!(true), json!(false)), "{user}");
        assert_eq!(user["roles"], json!([]), "portal roles only, and this workspace has none: {user}");
    });
}

/// A member of staff gets no site session (S4-P4-fix-2, the lead's answer 1): the handoff refuses, pointing to the
/// workspace's own address, and mints nothing.
#[test]
#[serial]
fn a_member_gets_no_site_session() {
    run(|| async {
        let w = world().await;
        let (_, h, _) = call(&w.r, Method::GET, SHOP, "/_auth/sign-in?next=/account", &[], &[]).await;
        let (_, handoff) = location(&h);
        let (status, h, body) = call(&w.r, Method::GET, PORTAL, &handoff, &[(SESSION(), &w.member)], &[]).await;
        assert!(status.is_client_error() && h.get(header::LOCATION).is_none(), "{status} {body}");
        assert!(body.contains("acme.ferrum.test"), "sent to the workspace's own address: {body}");
        let mut tx = w.k.db.system_tx().await.unwrap();
        let minted: i64 = sqlx::query_scalar("select count(*) from host_code").fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(minted, 0, "nothing minted");
    });
}

#[test]
#[serial]
fn handoff_refuses_foreign_host() {
    run(|| async {
        let w = world().await;
        for host in [GLOBEX_SHOP, PORTAL, "acme.ferrum.test", "nowhere.test"] {
            let path = format!("/_auth/handoff?host={host}&state=0123456789abcdef");
            let (status, h, body) = call(&w.r, Method::GET, PORTAL, &path, &[(SESSION(), &w.customer)], &[]).await;
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
        assert_eq!(user_of(&body)["email"], "pat@acme.test", "{body}");
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
        assert_eq!((status, user_of(&body)["email"].clone()), (StatusCode::OK, json!("pat@acme.test")), "{body}");
        // Signing out on the site ends that session there.
        let (status, h, _) = call(&w.r, Method::POST, SHOP, "/_auth/sign-out", &[(SESSION(), &site)], &[("sec-fetch-site", "same-origin")]).await;
        assert!(status.is_redirection(), "{status}");
        assert_eq!(cookies(&h).get(SESSION()).map(String::as_str), Some(""), "the cookie is cleared");
        let (_, _, body) = call(&w.r, Method::GET, SHOP, "/hello", &[(SESSION(), &site)], &[]).await;
        assert_eq!(user_of(&body), Value::Null, "the ended session is nobody");
    });
}

/// Only the browser's own session starts a site sign-in (S4-P4 review M1): a junk cookie beside an API key or a bare
/// sign-in token mints nothing.
#[test]
#[serial]
fn handoff_takes_only_the_browsers_session() {
    run(|| async {
        let w = world().await;
        let s = w.k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
        let admin = w.k.load_ctx(s.tenant, s.user, None, "it".into()).await.unwrap();
        let key = w.k.create_api_key(&admin, s.user, "it", None, None).await.unwrap().1;
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/auth/login")
            .header(header::HOST, PORTAL)
            .header("x-forwarded-for", "203.0.113.5")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"tenant": "acme", "email": "admin@acme.test", "password": PASSWORD}).to_string()))
            .unwrap();
        let res = w.r.clone().oneshot(req).await.unwrap();
        let body: Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap();
        let token = body["token"].as_str().unwrap_or_else(|| panic!("a bare token: {body}")).to_string();
        let (_, h, _) = call(&w.r, Method::GET, SHOP, "/_auth/sign-in?next=/account", &[], &[]).await;
        let (_, handoff) = location(&h);
        for bearer in [key, token] {
            let auth = format!("Bearer {bearer}");
            let (status, h, body) = call(&w.r, Method::GET, PORTAL, &handoff, &[(SESSION(), "x")], &[("authorization", &auth)]).await;
            assert!(status.is_client_error() && h.get(header::LOCATION).is_none(), "{status} {body}");
            let (status, h, body) = call(&w.r, Method::GET, PORTAL, &handoff, &[(SESSION(), &w.customer)], &[("authorization", &auth)]).await;
            assert!(status.is_client_error() && h.get(header::LOCATION).is_none(), "not preferred beside the cookie either: {status} {body}");
        }
        let mut tx = w.k.db.system_tx().await.unwrap();
        let minted: i64 = sqlx::query_scalar("select count(*) from host_code").fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(minted, 0, "nothing minted");
    });
}

/// A code is spent by any callback that names it, refused or not (S4-P4 review L1).
#[test]
#[serial]
fn a_refused_callback_spends_its_code() {
    run(|| async {
        let w = world().await;
        let ((n, v), callback) = handed(&w).await;
        let (status, _, body) = call(&w.r, Method::GET, SHOP, &callback, &[], &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, h, body) = call(&w.r, Method::GET, SHOP, &callback, &[(&n, &v)], &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(cookies(&h).get(SESSION()).is_none_or(|c| c.is_empty()), "no session");
    });
}

/// The role list shows which roles every portal person holds (S4-P4 review M4).
#[test]
#[serial]
fn role_list_shows_the_default_mark() {
    run(|| async {
        let w = world().await;
        let s = w.k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
        let mut tx = w.k.db.system_tx().await.unwrap();
        sqlx::query("insert into role (tenant_id, name, portal, portal_default) values ($1, 'Patrons', true, true)")
            .bind(s.tenant)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let (status, _, body) = call(&w.r, Method::GET, PORTAL, "/api/admin/roles", &[(SESSION(), &w.member)], &[("sec-fetch-site", "same-origin")]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let roles: Value = serde_json::from_str(&body).unwrap();
        let patrons = roles["roles"].as_array().and_then(|a| a.iter().find(|r| r["name"] == "Patrons")).cloned().unwrap_or_else(|| panic!("{body}"));
        assert_eq!(patrons["portal_default"], json!(true), "{patrons}");
    });
}

/// A site app never sets the platform's own cookies on its host (S4-P4 review L4); its own pass.
#[test]
#[serial]
fn site_cannot_set_the_platforms_cookies() {
    run(|| async {
        let w = world().await;
        let (status, h, body) = call(&w.r, Method::GET, SHOP, "/plant", &[], &[]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let set: Vec<String> = cookies(&h).into_keys().collect();
        assert_eq!(set, vec!["app_pref".to_string()], "{h:?}");
    });
}

/// A session that may only set up the second factor the workspace requires starts no site sign-in (S4-P4 review M1,
/// with API-K2's setup-only cookie).
#[test]
#[serial]
fn handoff_refuses_a_setup_only_session() {
    run(|| async {
        let w = world().await;
        let s = w.k.login("acme", "pat@acme.test", PASSWORD).await.unwrap();
        let mut claims = serde_json::Map::new();
        claims.insert("tenant".into(), json!(s.tenant.to_string()));
        claims.insert("tv".into(), json!(s.token_version));
        claims.insert("setup_2fa".into(), json!(true));
        // A browser's session (its `sid`), as API-K2 makes the setup-only one.
        let sid = w.k.open_session(&s, std::time::Duration::from_secs(600), "it", "203.0.113.5", None).await.unwrap();
        claims.insert("sid".into(), json!(sid.to_string()));
        let setup = w.st.sessions.sign(600, s.user.to_string(), claims).unwrap();
        let (_, h, _) = call(&w.r, Method::GET, SHOP, "/_auth/sign-in?next=/account", &[], &[]).await;
        let (_, handoff) = location(&h);
        let (status, h, body) = call(&w.r, Method::GET, PORTAL, &handoff, &[(SESSION(), &setup)], &[]).await;
        assert!(h.get(header::LOCATION).is_none_or(|l| !l.to_str().unwrap().contains("/_auth/callback")), "{status} {body}");
        let mut tx = w.k.db.system_tx().await.unwrap();
        let minted: i64 = sqlx::query_scalar("select count(*) from host_code").fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(minted, 0, "nothing minted");
    });
}

/// Whether a role is a portal role, or held by every portal person, is changed by an administrator at a screen: never
/// by a key (S4-P4-fix-2, the lead's answer 2).
#[test]
#[serial]
fn portal_marks_are_set_at_a_screen() {
    run(|| async {
        let w = world().await;
        let s = w.k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
        let mut tx = w.k.db.system_tx().await.unwrap();
        sqlx::query("insert into role (tenant_id, name) values ($1, 'Patrons')").bind(s.tenant).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        let admin = w.k.load_ctx(s.tenant, s.user, None, "it".into()).await.unwrap();
        let key = w.k.create_api_key(&admin, s.user, "it", None, None).await.unwrap().1;
        let put = |auth: Option<String>, jar: Vec<(&'static str, String)>| {
            let mut req = Request::builder()
                .method(Method::PUT)
                .uri("/api/admin/roles/Patrons/portal")
                .header(header::HOST, PORTAL)
                .header("x-forwarded-for", "203.0.113.5")
                .header("sec-fetch-site", "same-origin")
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(a) = auth {
                req = req.header(header::AUTHORIZATION, format!("Bearer {a}"));
            }
            if !jar.is_empty() {
                req = req.header(header::COOKIE, jar.iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join("; "));
            }
            let r = w.r.clone();
            let req = req.body(Body::from(json!({"portal": true, "portal_default": true}).to_string())).unwrap();
            async move { r.oneshot(req).await.unwrap().status() }
        };
        assert_eq!(put(Some(key), vec![]).await, StatusCode::FORBIDDEN, "a key");
        let mut tx = w.k.db.system_tx().await.unwrap();
        let marked: (bool, bool) = sqlx::query_as("select portal, portal_default from role where tenant_id = $1 and name = 'Patrons'")
            .bind(s.tenant)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(marked, (false, false), "nothing changed");
        assert_eq!(put(None, vec![(SESSION(), w.member.clone())]).await, StatusCode::NO_CONTENT, "an administrator at a screen");
    });
}

/// An answer to a request carrying a site session is private to that browser, whatever the app set (S4-P4-fix-2,
/// the lead's answer 4); one without is as the app set it.
#[test]
#[serial]
fn a_signed_in_answer_is_never_cached_for_others() {
    run(|| async {
        let w = world().await;
        let (_, h, _) = call(&w.r, Method::GET, SHOP, "/cached", &[], &[]).await;
        assert_eq!(h.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()), Some("public, max-age=60"), "nobody signed in: as the app said");
        let site = site_session(&w).await;
        let same = [("sec-fetch-site", "same-origin")];
        for path in ["/cached", "/api/site/acme/shop/cached"] {
            let (status, h, body) = call(&w.r, Method::GET, SHOP, path, &[(SESSION(), &site)], &same).await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
            assert_eq!(h.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()), Some("private"), "{path}: {h:?}");
            assert!(h.get_all(header::VARY).iter().any(|v| v.to_str().unwrap_or_default().to_ascii_lowercase().contains("cookie")), "{path}: {h:?}");
        }
    });
}

/// Every answer to a request that carried a site session is private, an app's error and the platform's error page
/// included; an app's `no-store` stays (BD-89, second read, 5).
#[test]
#[serial]
fn every_signed_in_answer_is_private() {
    run(|| async {
        let w = world().await;
        let site = site_session(&w).await;
        let same = [("sec-fetch-site", "same-origin")];
        for (path, cache) in [
            ("/api/site/acme/shop/fail", "private"),
            ("/fail", "private"),
            ("/nostore", "private, no-store"),
            ("/api/site/acme/shop/nostore", "private, no-store"),
        ] {
            let (status, h, body) = call(&w.r, Method::GET, SHOP, path, &[(SESSION(), &site)], &same).await;
            assert_eq!(h.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()), Some(cache), "{path}: {status} {body}");
            assert!(h.get_all(header::VARY).iter().any(|v| v.to_str().unwrap_or_default().to_ascii_lowercase().contains("cookie")), "{path}: {h:?}");
        }
    });
}

/// The handoff mints a code for a portal person alone: a guest gets none (BD-89, second read, 6).
#[test]
#[serial]
fn handoff_mints_nothing_for_a_guest() {
    run(|| async {
        let w = world().await;
        let s = w.k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
        let admin = w.k.load_ctx(s.tenant, s.user, None, "it".into()).await.unwrap();
        let gus = w.k.create_user(&admin, "gus@acme.test", "Gus", Some(PASSWORD), &[], false).await.unwrap();
        w.k.set_user_kind(&admin, gus, "guest").await.unwrap();
        let guest = w.k.login("acme", "gus@acme.test", PASSWORD).await.unwrap();
        let sid = w.k.open_session(&guest, std::time::Duration::from_secs(600), "it", "203.0.113.5", None).await.unwrap();
        let mut claims = serde_json::Map::new();
        claims.insert("tenant".into(), json!(guest.tenant.to_string()));
        claims.insert("tv".into(), json!(guest.token_version));
        claims.insert("sid".into(), json!(sid.to_string()));
        let cookie = w.st.sessions.sign(600, gus.to_string(), claims).unwrap();
        let (_, h, _) = call(&w.r, Method::GET, SHOP, "/_auth/sign-in?next=/account", &[], &[]).await;
        let (_, handoff) = location(&h);
        let (status, h, body) = call(&w.r, Method::GET, PORTAL, &handoff, &[(SESSION(), &cookie)], &[]).await;
        assert!(h.get(header::LOCATION).is_none_or(|l| !l.to_str().unwrap().contains("/_auth/callback")), "{status} {body}");
        let mut tx = w.k.db.system_tx().await.unwrap();
        let minted: i64 = sqlx::query_scalar("select count(*) from host_code").fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(minted, 0, "nothing minted");
    });
}

/// The site callback sits behind the sign-in limiter (BD-89, second read, 6).
#[test]
#[serial]
fn the_site_callback_is_limited() {
    run(|| async {
        let w = world().await;
        let mut limited = false;
        for i in 0..30 {
            let (status, _, _) = call(&w.r, Method::GET, SHOP, &format!("/_auth/callback?code=c{i}&state=abc"), &[], &[]).await;
            limited |= status == StatusCode::TOO_MANY_REQUESTS;
        }
        assert!(limited, "thirty tries from one network are not all answered");
    });
}

/// Signing out everywhere, or an administrator disabling the person, ends an open site session (BD-89, second read, 7).
#[test]
#[serial]
fn signing_out_everywhere_ends_a_site_session() {
    run(|| async {
        let w = world().await;
        let site = site_session(&w).await;
        let (_, _, body) = call(&w.r, Method::GET, SHOP, "/hello", &[(SESSION(), &site)], &[]).await;
        assert_eq!(user_of(&body)["email"], "pat@acme.test", "{body}");
        let (status, _, body) =
            call(&w.r, Method::POST, PORTAL, "/api/auth/logout_all", &[(SESSION(), &w.customer)], &[("sec-fetch-site", "same-origin")]).await;
        assert!(status.is_success(), "{status} {body}");
        let (_, _, body) = call(&w.r, Method::GET, SHOP, "/hello", &[(SESSION(), &site)], &[]).await;
        assert_eq!(user_of(&body), Value::Null, "signed out everywhere: {body}");
    });
}

#[test]
#[serial]
fn disabling_the_person_ends_a_site_session() {
    run(|| async {
        let w = world().await;
        let site = site_session(&w).await;
        let s = w.k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
        let admin = w.k.load_ctx(s.tenant, s.user, None, "it".into()).await.unwrap();
        let pat = w.k.login("acme", "pat@acme.test", PASSWORD).await.unwrap().user;
        w.k.set_user_active(&admin, pat, false).await.unwrap();
        let (_, _, body) = call(&w.r, Method::GET, SHOP, "/hello", &[(SESSION(), &site)], &[]).await;
        assert_eq!(user_of(&body), Value::Null, "disabled: {body}");
    });
}

/// The site callback sends the browser on only to a path of its own host (BD-89, second read, 7).
#[test]
#[serial]
fn the_site_callback_keeps_to_its_host() {
    run(|| async {
        let w = world().await;
        let ((n, v), callback) = handed_to(&w, "//evil.example/steal").await;
        let (status, h, body) = call(&w.r, Method::GET, SHOP, &callback, &[(&n, &v)], &[]).await;
        assert!(status.is_redirection(), "{status} {body}");
        let to = h.get(header::LOCATION).unwrap().to_str().unwrap();
        assert!(to.starts_with('/') && !to.starts_with("//") && !to.contains("evil"), "{to}");
    });
}

/// A site address released, or given later to another app, takes its sessions with it (BD-89, second read, 7).
#[test]
#[serial]
fn a_released_site_address_ends_its_sessions() {
    run(|| async {
        let w = world().await;
        let site = site_session(&w).await;
        w.k.release_host(SHOP, Some(0)).await.unwrap();
        let mut tx = w.k.db.system_tx().await.unwrap();
        let open: i64 =
            sqlx::query_scalar("select count(*) from user_session where host = $1 and revoked_at is null").bind(SHOP).fetch_one(&mut *tx).await.unwrap();
        // Long ago, so the address may be given again: to the workspace's other app.
        sqlx::query("update tenant_host set released_at = now() - interval '400 days', leaving_until = null where host = $1")
            .bind(SHOP)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(open, 0, "the released address's sessions are ended");
        let s = w.k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
        let admin = w.k.load_ctx(s.tenant, s.user, None, "it".into()).await.unwrap();
        w.k.add_host(&admin, SHOP, "site:other").await.unwrap();
        w.k.host_checked(SHOP, true).await.unwrap();
        let (status, _, body) = call(&w.r, Method::GET, SHOP, "/hello", &[(SESSION(), &site)], &[]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!((serde_json::from_str::<Value>(&body).unwrap()["site"].clone(), user_of(&body)), (json!("other"), Value::Null), "{body}");
    });
}

/// The screen is told the roles a portal person holds by default, which their records' roles count (S4-P4-fix-3b, 3).
#[test]
#[serial]
fn me_names_the_roles_held_by_default() {
    run(|| async {
        let w = world().await;
        let s = w.k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
        let mut tx = w.k.db.system_tx().await.unwrap();
        sqlx::query("insert into role (tenant_id, name, portal, portal_default) values ($1, 'Patrons', true, true)")
            .bind(s.tenant)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        w.k.invalidate_tenant_users(s.tenant);
        let (status, _, body) = call(&w.r, Method::GET, PORTAL, "/api/auth/me", &[(SESSION(), &w.customer)], &[("sec-fetch-site", "same-origin")]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let me: Value = serde_json::from_str(&body).unwrap();
        assert_eq!((me["user"]["roles"].clone(), me["user"]["default_roles"].clone()), (json!([]), json!(["Patrons"])), "{body}");
    });
}
