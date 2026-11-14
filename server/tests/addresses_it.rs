//! An address that goes away (docs/operations.md, "Changing or removing an address"), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_test`; the database is
//! reset): the whole server as the binary boots it, behind its own router. A removed address keeps serving the
//! people signed in there for its leaving period, sends everyone else on, starts no session, and says so in the
//! bootstrap; then it redirects as before.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use ferrum_kernel::Kernel;
use ferrum_kernel::shell::HostRoute;
use ferrum_kernel::tenant::NewTenant;
use ferrum_server::app::App;
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use loco_rs::task::Task;
use serde_json::{Value, json};
use tower::ServiceExt;

const PASSWORD: &str = "Correct-Horse-9";
/// The workspace's address that goes away, its main one until then.
const OLD: &str = "old.acme.test";
/// The address it hands its place to.
const NEW: &str = "acme.ferrum.test";

#[test]
fn an_address_goes_away() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

/// One request to `host`: (status, `Location`, body, whether it set the session cookie).
async fn call(
    router: &axum::Router,
    method: Method,
    host: &str,
    path: &str,
    cookie: Option<&str>,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, String, String, bool) {
    let mut req = Request::builder().method(method).uri(path).header(header::HOST, host).header("x-forwarded-for", "203.0.113.7");
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, format!("{}={c}", ferrum_server::shell::cookie_name(false))).header("sec-fetch-site", "same-origin");
    }
    if let Some(t) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let location = res.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    let cookie_set = res
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|c| c.starts_with(&format!("{}=", ferrum_server::shell::cookie_name(false))) && !c.contains("Max-Age=0"));
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, location, String::from_utf8_lossy(&bytes).into_owned(), cookie_set)
}

/// A page's bootstrap.
fn bootstrap(html: &str) -> Value {
    let start = html.find(r#"id="ferrum-bootstrap">"#).map(|i| i + r#"id="ferrum-bootstrap">"#.len()).unwrap_or_else(|| panic!("no bootstrap: {html}"));
    let end = start + html[start..].find("</script>").unwrap();
    serde_json::from_str(&html[start..end].replace("\\u003c", "<")).unwrap()
}

/// A page sign-in on `host` (the browser's own page asks): the cookie's token, if one was set.
async fn sign_in_page(router: &axum::Router, host: &str, cookie: Option<&str>) -> (StatusCode, Option<String>) {
    sign_in_as(router, host, cookie, "acme").await
}

/// [`sign_in_page`] as the administrator of workspace `slug` (`admin@{slug}.test`).
async fn sign_in_as(router: &axum::Router, host: &str, cookie: Option<&str>, slug: &str) -> (StatusCode, Option<String>) {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri("/api/auth/login")
        .header(header::HOST, host)
        .header("x-forwarded-for", "203.0.113.7")
        .header("sec-fetch-site", "same-origin")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, format!("{}={c}", ferrum_server::shell::cookie_name(false)));
    }
    let req = req.body(Body::from(json!({"tenant": slug, "email": format!("admin@{slug}.test"), "password": PASSWORD}).to_string())).unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let cookie = res.headers().get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).find_map(|c| {
        c.strip_prefix(&format!("{}=", ferrum_server::shell::cookie_name(false))).map(|rest| rest.split(';').next().unwrap_or_default().to_string())
    });
    (status, cookie.filter(|c| !c.is_empty()))
}

async fn sql(k: &Kernel, q: &str) {
    let q = format!("begin; set local ferrum.system = 'on'; {q}; commit;");
    sqlx::raw_sql(sqlx::AssertSqlSafe(q.clone())).execute(&k.db.pool).await.expect(&q);
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    {
        let pool = sqlx::PgPool::connect(&url).await.expect("connect");
        common::reset(&pool).await;
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut config = Environment::Test.load_from_folder(&dir.join("config")).expect("config/test.yaml");
    config.database.uri = url.clone();
    if let Some(s) = config.settings.as_mut() {
        s["packages_dir"] = json!(dir.join("../packages").to_string_lossy());
        assert_eq!(s["host_leaving_days"], json!(14), "the default leaving period");
    }
    let base = <App as Hooks>::after_context(AppContext::builder(Environment::Test, Default::default(), config).build()).await.expect("boot");
    let st = base.shared_store.get::<AppState>().expect("state");
    let router = App::routes(&base).to_router::<App>(base.clone(), axum::Router::new()).expect("router");
    let router = App::after_routes(router, &base).await.expect("layers");
    let k = st.kernel.clone();

    let acme = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    let s = k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
    let admin = k.load_ctx(acme, s.user, Some(s.token_version), "it".into()).await.unwrap();
    k.add_subdomain(acme, NEW).await.unwrap();
    k.add_host(&admin, OLD, "shell").await.unwrap();
    k.host_checked(OLD, true).await.unwrap();
    k.make_primary_host(&admin, OLD).await.unwrap();

    // Signed in on the old address, in a browser and as an API client.
    let (status, cookie) = sign_in_page(&router, OLD, None).await;
    let cookie = cookie.unwrap_or_else(|| panic!("a cookie: {status}"));
    let token = {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/auth/login")
            .header(header::HOST, OLD)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"tenant": "acme", "email": "admin@acme.test", "password": PASSWORD}).to_string()))
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        let v: Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap();
        v["token"].as_str().expect("an API client's token").to_string()
    };
    let (status, ..) = call(&router, Method::GET, OLD, "/", Some(&cookie), None, None).await;
    // The shell's interface is not built here: its page is the status page, with the bar and bootstrap (503).
    assert!(!status.is_redirection() && status != StatusCode::UNAUTHORIZED, "served before: {status}");

    // The administrator removes it: the answer says until when it is leaving.
    let (status, _, body, _) = call(&router, Method::DELETE, OLD, &format!("/api/admin/domains/{OLD}"), None, Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let answer: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(answer["host"], OLD);
    let until: chrono::DateTime<chrono::Utc> = serde_json::from_value(answer["leaving_until"].clone()).expect("until when");
    assert!((until - chrono::Utc::now()).num_days() >= 13, "{until}");
    assert_eq!(k.primary_host(acme, "shell").await.unwrap().as_deref(), Some(NEW), "the main place was handed on first");
    let (_, _, body, _) = call(&router, Method::GET, NEW, "/api/admin/domains", None, Some(&token), None).await;
    let listed: Value = serde_json::from_str(&body).unwrap();
    assert!(listed["domains"].as_array().unwrap().iter().any(|d| d["host"] == OLD && d["status"] == "leaving"), "{listed}");

    // In its leaving period: the person signed in there goes on as before, pages, API and all, and the bootstrap
    // says where the workspace is now and until when.
    let (status, _, html, _) = call(&router, Method::GET, OLD, "/", Some(&cookie), None, None).await;
    assert!(!status.is_redirection() && status != StatusCode::UNAUTHORIZED, "a signed-in page still served: {status}");
    let boot = bootstrap(&html);
    assert_eq!(boot["host"]["leaving"]["to"], format!("http://{NEW}"), "{}", boot["host"]);
    assert_eq!(serde_json::from_value::<chrono::DateTime<chrono::Utc>>(boot["host"]["leaving"]["until"].clone()).unwrap(), until);
    assert_eq!(
        (boot["host"]["bound"].clone(), boot["workspace"]["id"].clone()),
        (json!(true), json!(acme)),
        "the same workspace: the origin guard keeps its storage"
    );
    assert_eq!(call(&router, Method::GET, OLD, "/api/auth/me", Some(&cookie), None, None).await.0, StatusCode::OK, "the API with the cookie");
    assert_eq!(call(&router, Method::GET, OLD, "/api/auth/me", None, Some(&token), None).await.0, StatusCode::OK, "an API client, as before");
    // The new address has no notice.
    let (new_cookie_status, new_cookie) = sign_in_page(&router, NEW, None).await;
    let new_cookie = new_cookie.unwrap_or_else(|| panic!("signs in at the new address: {new_cookie_status}"));
    let (_, _, html, _) = call(&router, Method::GET, NEW, "/", Some(&new_cookie), None, None).await;
    assert!(bootstrap(&html)["host"].get("leaving").is_none(), "only a leaving address says so");

    // Nobody starts anything there: a visit without a session goes on to the new address, as a moved one does;
    // its sign-in page is not offered, even to a browser whose cookie no longer opens anything; no session starts.
    let (status, location, ..) = call(&router, Method::GET, OLD, "/crm/deals?x=1", None, None, None).await;
    assert_eq!((status, location.as_str()), (StatusCode::PERMANENT_REDIRECT, "http://acme.ferrum.test/crm/deals?x=1"));
    let (status, location, ..) = call(&router, Method::GET, OLD, "/login?next=/crm/", Some("not-a-session"), None, None).await;
    assert!(status.is_redirection() && location.starts_with("http://acme.ferrum.test/login?next="), "{status} {location}");
    let (status, location, ..) = call(&router, Method::GET, OLD, "/login", None, None, None).await;
    assert!(status.is_redirection() && location.starts_with("http://acme.ferrum.test/"), "{status} {location}");
    assert_eq!(sign_in_page(&router, OLD, None).await, (StatusCode::UNAUTHORIZED, None), "no session starts there");
    assert_eq!(sign_in_page(&router, OLD, Some("not-a-session")).await, (StatusCode::UNAUTHORIZED, None), "nor with a dead cookie");
    let (status, .., set) = call(&router, Method::GET, OLD, "/_auth/callback?code=x&state=y", Some(&cookie), None, None).await;
    assert!(!set && status.is_client_error(), "no handoff from the accounts host lands there: {status}");
    // The session holds for its own workspace only, as on any host.
    let globex = k
        .create_tenant(NewTenant {
            slug: "globex".into(),
            name: "Globex".into(),
            admin_email: "admin@globex.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    k.add_subdomain(globex, "globex.ferrum.test").await.unwrap();
    assert_eq!(call(&router, Method::GET, "globex.ferrum.test", "/api/auth/me", Some(&cookie), None, None).await.0, StatusCode::UNAUTHORIZED);

    // After its leaving period: as a released address was before (a redirect for pages; 401 for the cookie on the
    // API). An API client's token is not the address's: it goes on wherever the address still reaches the server.
    sql(&k, &format!("update tenant_host set leaving_until = now() - interval '1 second' where host = '{OLD}'")).await;
    k.release_hosts(uuid::Uuid::nil()).await.unwrap(); // clears this replica's host cache
    let (status, location, ..) = call(&router, Method::GET, OLD, "/crm/", Some(&cookie), None, None).await;
    assert_eq!((status, location.as_str()), (StatusCode::PERMANENT_REDIRECT, "http://acme.ferrum.test/crm/"));
    assert_eq!(call(&router, Method::GET, OLD, "/api/auth/me", Some(&cookie), None, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&router, Method::GET, OLD, "/api/auth/me", None, Some(&token), None).await.0, StatusCode::OK);

    // The operator's task: any address, a subdomain too, with the leaving period it is given.
    k.add_subdomain(acme, "acme-2.ferrum.test").await.unwrap();
    let vars = loco_rs::task::Vars::from_cli_args(vec![("host".into(), "acme-2.ferrum.test".into()), ("leaving_days".into(), "3".into())]);
    ferrum_server::tasks::ReleaseHost.run(&base, &vars).await.expect("released");
    match k.route_host("acme-2.ferrum.test").await.unwrap() {
        HostRoute::Leaving { to, until, .. } => assert!(to.as_deref() == Some(NEW) && (until - chrono::Utc::now()).num_hours() > 70, "{to:?} {until}"),
        other => panic!("{other:?}"),
    }
    let vars = loco_rs::task::Vars::from_cli_args(vec![("host".into(), "acme-2.ferrum.test".into())]);
    assert!(ferrum_server::tasks::ReleaseHost.run(&base, &vars).await.is_err(), "released already");

    // No leaving period (FERRUM_HOST_LEAVING_DAYS=0, or leaving_days:0): the address redirects at once, even for
    // the person signed in there, as before the setting.
    k.add_host(&admin, "x.acme.test", "shell").await.unwrap();
    k.host_checked("x.acme.test", true).await.unwrap();
    let (_, x_cookie) = sign_in_page(&router, "x.acme.test", None).await;
    let vars = loco_rs::task::Vars::from_cli_args(vec![("host".into(), "x.acme.test".into()), ("leaving_days".into(), "0".into())]);
    ferrum_server::tasks::ReleaseHost.run(&base, &vars).await.expect("released");
    let (status, location, ..) = call(&router, Method::GET, "x.acme.test", "/crm/", x_cookie.as_deref(), None, None).await;
    assert_eq!((status, location.as_str()), (StatusCode::PERMANENT_REDIRECT, "http://acme.ferrum.test/crm/"));

    // Hosts the operator vouches for, and no other (a private network): the first is the workspace's main host, so
    // releasing it hands that place to the other, the leaving notice points there and a visitor is redirected.
    let ops = k
        .create_tenant(NewTenant {
            slug: "ops".into(),
            name: "Ops".into(),
            admin_email: "admin@ops.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    for host in ["erp.ops.internal", "erp2.ops.internal"] {
        let vars = loco_rs::task::Vars::from_cli_args(vec![
            ("tenant".into(), "ops".into()),
            ("host".into(), host.into()),
            ("target".into(), "shell".into()),
            ("zone".into(), "none".into()),
        ]);
        ferrum_server::tasks::AddHost.run(&base, &vars).await.expect("added");
    }
    assert_eq!(k.primary_host(ops, "shell").await.unwrap().as_deref(), Some("erp.ops.internal"), "the first is the main one");
    let (_, ops_cookie) = sign_in_as(&router, "erp.ops.internal", None, "ops").await;
    let ops_cookie = ops_cookie.expect("signed in on the operator's host");
    let vars = loco_rs::task::Vars::from_cli_args(vec![("host".into(), "erp.ops.internal".into())]);
    ferrum_server::tasks::ReleaseHost.run(&base, &vars).await.expect("released");
    let (_, _, html, _) = call(&router, Method::GET, "erp.ops.internal", "/", Some(&ops_cookie), None, None).await;
    assert_eq!(bootstrap(&html)["host"]["leaving"]["to"], "http://erp2.ops.internal", "the notice points at the other");
    let (status, location, ..) = call(&router, Method::GET, "erp.ops.internal", "/crm/deals?x=1", None, None, None).await;
    assert_eq!((status, location.as_str()), (StatusCode::PERMANENT_REDIRECT, "http://erp2.ops.internal/crm/deals?x=1"), "a visitor goes on");
    // On a port of its own (a development server, an installation not on 443) the redirect keeps the port.
    let (status, location, ..) = call(&router, Method::GET, "erp.ops.internal:8443", "/crm/deals?x=1", None, None, None).await;
    assert_eq!((status, location.as_str()), (StatusCode::PERMANENT_REDIRECT, "http://erp2.ops.internal:8443/crm/deals?x=1"), "and keeps the port");
    // On purpose, by the task: the other way round.
    let vars = loco_rs::task::Vars::from_cli_args(vec![
        ("tenant".into(), "ops".into()),
        ("host".into(), "erp3.ops.internal".into()),
        ("target".into(), "shell".into()),
        ("zone".into(), "none".into()),
        ("primary".into(), "yes".into()),
    ]);
    ferrum_server::tasks::AddHost.run(&base, &vars).await.expect("added as the main host");
    assert_eq!(k.primary_host(ops, "shell").await.unwrap().as_deref(), Some("erp3.ops.internal"));
    let vars = loco_rs::task::Vars::from_cli_args(vec![("host".into(), "erp2.ops.internal".into())]);
    ferrum_server::tasks::SetPrimaryHost.run(&base, &vars).await.expect("made the main host");
    assert_eq!(k.primary_host(ops, "shell").await.unwrap().as_deref(), Some("erp2.ops.internal"));
    let vars = loco_rs::task::Vars::from_cli_args(vec![("host".into(), "erp.ops.internal".into())]);
    assert!(ferrum_server::tasks::SetPrimaryHost.run(&base, &vars).await.is_err(), "not a released host");

    // A workspace with one host, released: nothing takes its place, and a visitor is told the workspace has moved (410).
    let solo = k
        .create_tenant(NewTenant {
            slug: "solo".into(),
            name: "Solo".into(),
            admin_email: "admin@solo.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    k.add_operator_host("solo", "erp.solo.internal", "shell", None).await.unwrap();
    assert_eq!(k.host_successor("erp.solo.internal").await.unwrap(), None, "the task says so before it releases it");
    let vars = loco_rs::task::Vars::from_cli_args(vec![("host".into(), "erp.solo.internal".into())]);
    ferrum_server::tasks::ReleaseHost.run(&base, &vars).await.expect("released");
    assert_eq!(k.primary_host(solo, "shell").await.unwrap(), None);
    assert_eq!(call(&router, Method::GET, "erp.solo.internal", "/crm/", None, None, None).await.0, StatusCode::GONE);

    Box::pin(shapes(&base, &st)).await;
}

/// The workspace's main address, its subdomain of the base domain, in [`shapes`].
const MAIN: &str = "beta.ferrum.test";
/// A name it gives itself under the base domain.
const SALES: &str = "sales--beta.ferrum.test";

/// A person's API token and browser cookie, signing in on `host` (`{email}` of workspace `slug`).
async fn signed_in(router: &axum::Router, host: &str, slug: &str, email: &str) -> (String, String) {
    let mut out = Vec::new();
    for page in [false, true] {
        let mut req =
            Request::builder().method(Method::POST).uri("/api/auth/login").header(header::HOST, host).header(header::CONTENT_TYPE, "application/json");
        if page {
            req = req.header("sec-fetch-site", "same-origin");
        }
        let req = req.body(Body::from(json!({"tenant": slug, "email": email, "password": PASSWORD}).to_string())).unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        let cookie = res.headers().get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).find_map(|c| {
            c.strip_prefix(&format!("{}=", ferrum_server::shell::cookie_name(false))).map(|rest| rest.split(';').next().unwrap_or_default().to_string())
        });
        let v: Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap_or_default();
        out.push(if page {
            cookie.unwrap_or_else(|| panic!("a cookie for {email} on {host}: {v}"))
        } else {
            v["token"].as_str().expect("a token").to_string()
        });
    }
    (out.remove(0), out.remove(0))
}

/// `SHOW=1` prints the answers the Administration screen reads, as the tests see them.
fn show(what: &str, body: &str) {
    if std::env::var("SHOW").is_ok() {
        println!("── {what}\n{body}");
    }
}

/// The addresses a workspace shapes for itself (docs/operations.md, "Addresses of a workspace"): on a cluster with a base
/// domain under which isolated apps get their own origins too, and two zones.
async fn shapes(base: &AppContext, st: &AppState) {
    use ferrum_kernel::zones::Zones;
    use std::sync::Arc;
    let k = st.kernel.clone();
    let zones = Arc::new(
        ferrum_server::zone::resolve(Zones::parse(r#"{"office": {}, "field": {"staff": ["crm"], "admin": true}}"#, "", "", "").unwrap(), &k).await.unwrap(),
    );
    k.set_zones(zones.clone());
    let settings = ferrum_server::state::Settings { base_domain: "ferrum.test".into(), apps_base: "ferrum.test".into(), ..st.settings.clone() };
    let storage = Arc::new(loco_rs::storage::Storage::single(loco_rs::storage::drivers::mem::new()));
    let shell = Arc::new(ferrum_server::shell::Shell::new(k.clone(), storage, &settings, "http://localhost:5150"));
    let st = AppState { settings, shell, zones, ..st.clone() };
    let ctx = AppContext::builder(Environment::Test, Default::default(), base.config.clone()).build();
    ctx.shared_store.insert(st.clone());
    let router = App::routes(&ctx).to_router::<App>(ctx.clone(), axum::Router::new()).expect("router");
    let router = App::after_routes(router, &ctx).await.expect("layers");

    // A workspace with CRM and Helpdesk, its subdomain, and an address the operator put in a zone.
    let beta = k
        .create_tenant(NewTenant {
            slug: "beta".into(),
            name: "Beta".into(),
            admin_email: "admin@beta.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    let root = k.login("beta", "admin@beta.test", PASSWORD).await.unwrap().user;
    let admin = k.load_ctx(beta, root, None, "it".into()).await.unwrap();
    for app in ["crm", "helpdesk"] {
        let plan = st.apps.plan(&admin, app, None).await.unwrap_or_else(|e| panic!("plan {app}: {e}"));
        st.apps.install(&admin, app, None, &plan.digest, false).await.unwrap_or_else(|e| panic!("install {app}: {e:?}"));
    }
    let admin = k.load_ctx(beta, root, None, "it".into()).await.unwrap();
    k.ensure_subdomains("ferrum.test").await.unwrap();
    k.add_operator_host("beta", "erp.beta.internal", "shell", Some("office")).await.unwrap();
    k.create_user(&admin, "nina@beta.test", "Nina", Some(PASSWORD), &["Sales User".into(), "Support Agent".into()], false).await.unwrap();
    k.create_user(&admin, "pat@beta.test", "Pat", Some(PASSWORD), &[], false).await.unwrap();
    let (token, _) = signed_in(&router, MAIN, "beta", "admin@beta.test").await;
    let get = async |host: &str, path: &str, cookie: Option<&str>| call(&router, Method::GET, host, path, cookie, None, None).await;
    let api = async |method: Method, host: &str, path: &str, body: Option<Value>| {
        let (status, _, body, _) = call(&router, method, host, path, None, Some(&token), body).await;
        (status, body)
    };
    let row =
        |listed: &Value, host: &str| listed["domains"].as_array().unwrap().iter().find(|d| d["host"] == host).cloned().unwrap_or_else(|| panic!("{host}"));

    // ── What the screen is given to offer the right choices. ──
    let (status, body) = api(Method::GET, MAIN, "/api/admin/domains", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    show("GET /api/admin/domains", &body);
    let listed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((listed["base"].clone(), listed["name_form"].clone()), (json!("ferrum.test"), json!("{name}--beta.ferrum.test")));
    let reserved: Vec<&str> = listed["reserved_names"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    for kept in ["accounts", "hooks", "www", "api", "admin", "login", "mail"] {
        assert!(reserved.contains(&kept), "{kept}: {reserved:?}");
    }
    let homes: Vec<(&str, &str)> = listed["homes"].as_array().unwrap().iter().map(|h| (h["id"].as_str().unwrap(), h["route"].as_str().unwrap())).collect();
    assert!(homes.contains(&("crm", "crm")) && homes.contains(&("helpdesk", "helpdesk")), "{homes:?}");
    assert_eq!((listed["limits"].clone(), listed["used"].clone()), (json!({"addresses": 20, "paths": 50}), json!({"addresses": 2, "paths": 0})));
    let main = row(&listed, MAIN);
    assert_eq!(
        (main["kind"].clone(), main["primary"].clone(), main["is_primary"].clone(), main["status"].clone()),
        (json!("subdomain"), json!(true), json!(true), json!("verified"))
    );
    assert_eq!(main["can"], json!({"edit": true, "remove": false, "make_primary": false}), "the workspace's own subdomain stays");
    let op = row(&listed, "erp.beta.internal");
    assert_eq!((op["kind"].clone(), op["zone"].clone()), (json!("operator"), json!("office")), "the operator's address and its zone, shown as they are");
    assert_eq!(op["can"], json!({"edit": false, "remove": false, "make_primary": true}));

    // ── A name under the base: its form, live at once, a certificate on demand, sign-ins handed to it. ──
    let (status, body) = api(Method::POST, MAIN, "/api/admin/domains", Some(json!({"name": "Sales", "home": "crm", "apps": ["crm"], "label": "Sales"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    show("POST /api/admin/domains {name}", &body);
    let added: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (added["host"].clone(), added["kind"].clone(), added["status"].clone(), added["home"].clone(), added["apps"].clone(), added["label"].clone()),
        (json!(SALES), json!("subdomain"), json!("verified"), json!("crm"), json!(["crm"]), json!("Sales"))
    );
    assert_eq!(added["can"], json!({"edit": true, "remove": true, "make_primary": true}));
    assert_eq!(k.route_host(SALES).await.unwrap(), HostRoute::Serve { tenant: beta, target: "shell".into() }, "live at once");
    assert_eq!(get("localhost", &format!("/_tls/ask?domain={SALES}"), None).await.0, StatusCode::OK, "the edge may get it a certificate");
    assert_eq!(get("localhost", "/_tls/ask?domain=nothing--beta.ferrum.test", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(st.shell.workspace_at(SALES).await.unwrap(), Some(beta), "the accounts host hands sign-ins to it");
    let (status, location, ..) = get(SALES, "/login?next=/crm/", None).await;
    assert!(status.is_redirection() && location.starts_with(&format!("http://accounts.ferrum.test/authorize?host={SALES}&")), "{status} {location}");
    for (refused, says) in [
        (json!({"name": "accounts"}), "kept for the service"),
        (json!({"name": "crm"}), "an app's name"),
        (json!({"name": "sa--les"}), "single hyphens"),
        (json!({"host": "x.ferrum.test"}), "give a name instead"),
        (json!({"name": "x", "zone": "office"}), "only the operator"),
        (json!({"name": "x", "host": "x.example"}), "either host"),
        (json!({"name": "help", "target": "portal", "home": "crm"}), "addresses that open the workspace"),
        (json!({"name": "x", "home": "nosuch"}), "nosuch is no app of this workspace"),
    ] {
        let (status, body) = api(Method::POST, MAIN, "/api/admin/domains", Some(refused.clone())).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && body.contains(says), "{refused}: {status} {body}");
    }
    // Another workspace can never have it: its names end in its own slug, no new slug has `--`, and the base is no
    // domain of anyone's own.
    assert!(
        k.create_tenant(NewTenant {
            slug: "sales--beta".into(),
            name: "x".into(),
            admin_email: "a@x.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec![],
            settings: None
        })
        .await
        .is_err()
    );

    // An address wins over an isolated app's origin of the same form (the apps base is the base domain here): its
    // cookie is the workspace's session, where an app's origin reads none.
    assert!(st.shell.names_like_app_origins());
    assert_eq!(st.shell.app_host(SALES).await.unwrap(), None);
    assert_eq!(st.shell.app_host("cards--beta.ferrum.test").await.unwrap(), Some(("cards".into(), "beta".into())));
    let (_, admin_cookie) = signed_in(&router, SALES, "beta", "admin@beta.test").await;
    assert_eq!(get(SALES, "/api/auth/me", Some(&admin_cookie)).await.0, StatusCode::OK);
    assert_eq!(get("cards--beta.ferrum.test", "/api/auth/me", Some(&admin_cookie)).await.0, StatusCode::NOT_FOUND, "an app's origin keeps to its paths");

    // ── `home`: `/` lands on the app for whoever may open it; everyone else gets Home. The bootstrap says what the
    // address shows, as far as the person may open it there. ──
    let (_, nina) = signed_in(&router, SALES, "beta", "nina@beta.test").await;
    let (_, pat) = signed_in(&router, SALES, "beta", "pat@beta.test").await;
    assert_eq!(get(SALES, "/", Some(&nina)).await.0, StatusCode::FOUND);
    assert_eq!(get(SALES, "/?view=mine", Some(&nina)).await.1, "/crm/?view=mine", "the query kept");
    let (status, location, html, _) = get(SALES, "/", Some(&pat)).await;
    assert!(!status.is_redirection(), "Pat may not open CRM: Home, as anywhere ({status} {location})");
    let boot = bootstrap(&html);
    show("bootstrap host, on the name, for Pat", &boot["host"].to_string());
    assert_eq!(
        (
            boot["host"]["home"].clone(),
            boot["host"]["apps"].clone(),
            boot["host"]["label"].clone(),
            boot["host"]["main"].clone(),
            boot["host"]["main_url"].clone()
        ),
        (json!(null), json!(["crm"]), json!("Sales"), json!(false), json!(format!("http://{MAIN}")))
    );
    let (_, _, html, _) = get(SALES, "/data/", Some(&nina)).await;
    assert_eq!(bootstrap(&html)["host"]["home"], "crm", "Nina may open it");
    let (_, nina_main) = signed_in(&router, MAIN, "beta", "nina@beta.test").await;
    let (_, _, html, _) = get(MAIN, "/data/", Some(&nina_main)).await;
    assert_eq!(
        (bootstrap(&html)["host"]["main"].clone(), bootstrap(&html)["host"]["apps"].clone()),
        (json!(true), json!(null)),
        "the main address lists every app"
    );
    // Not a wall: another app's page opens on the name as on the main address.
    let (here, there) = (get(SALES, "/helpdesk/", Some(&nina)).await.0, get(MAIN, "/helpdesk/", Some(&nina_main)).await.0);
    assert!(here == there && here != StatusCode::NOT_FOUND && !here.is_redirection(), "{here} {there}");

    // ── Changing what it shows, part by part; never its zone, never an operator's address. ──
    let patch = async |host: &str, body: Value| api(Method::PATCH, MAIN, &format!("/api/admin/domains/{host}"), Some(body)).await;
    let (status, body) = patch(SALES, json!({"label": "Sales team"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    show("PATCH /api/admin/domains/{host}", &body);
    let changed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((changed["label"].clone(), changed["home"].clone()), (json!("Sales team"), json!("crm")), "the rest stays");
    assert_eq!(serde_json::from_str::<Value>(&patch(SALES, json!({"home": null})).await.1).unwrap()["home"], json!(null));
    for (refused, says) in [(json!({"zone": "office"}), "only the operator"), (json!({"colour": "red"}), "not something an address shows")] {
        let (status, body) = patch(SALES, refused.clone()).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && body.contains(says), "{refused}: {status} {body}");
    }
    let (status, body) = patch("erp.beta.internal", json!({"label": "ERP"})).await;
    assert!(status == StatusCode::FORBIDDEN && body.contains("operator's"), "{status} {body}");
    let (status, body) = api(Method::DELETE, MAIN, "/api/admin/domains/erp.beta.internal", None).await;
    assert!(status == StatusCode::FORBIDDEN && body.contains("operator's"), "{status} {body}");
    assert_eq!(k.host_zone("erp.beta.internal").await.unwrap().as_deref(), Some("office"));

    // ── Short paths: a redirect on every `shell` address, the query kept; an app's route always wins. ──
    let paths = json!({"paths": [{"alias": "q4-sales", "target": "/crm/"}, {"alias": "my-leads", "target": "/crm/e/Lead?view=mine"}]});
    let (status, body) = api(Method::PUT, MAIN, "/api/admin/paths", Some(paths)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for (host, cookie) in [(MAIN, &nina_main), (SALES, &nina)] {
        let (status, location, ..) = get(host, "/q4-sales?x=1", Some(cookie)).await;
        assert_eq!((status, location.as_str()), (StatusCode::FOUND, "/crm/?x=1"), "{host}");
    }
    assert_eq!(get(MAIN, "/my-leads/42?q=a", Some(&nina_main)).await.1, "/crm/e/Lead/42?view=mine&q=a");
    let (status, location, ..) = get(MAIN, "/q4-sales", None).await;
    assert!(status.is_redirection() && location.starts_with("/login?next="), "signed out: sign in first, then on: {location}");
    let (status, body) = api(Method::GET, MAIN, "/api/admin/paths", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    show("GET /api/admin/paths", &body);
    let listed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(listed["paths"][0], json!({"alias": "q4-sales", "target": "/crm/", "works": true}));
    assert_eq!(listed["limit"], 50);
    let taken: Vec<&str> = listed["taken"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(taken.contains(&"crm") && taken.contains(&"api") && taken.contains(&"portal"), "{taken:?}");
    let many: Vec<Value> = (0..51).map(|i| json!({"alias": format!("p{i}"), "target": "/crm/"})).collect();
    for (refused, says) in [
        (json!([{"alias": "api", "target": "/crm/"}]), "/api is one of the service's own addresses"),
        (json!([{"alias": "crm", "target": "/crm/"}]), "/crm is an app's address"),
        (json!([{"alias": "help", "target": "/portal/"}]), "/help must lead to a page of this workspace"),
        (json!([{"alias": "away", "target": "//evil.example/"}]), "/away must lead"),
        (json!([{"alias": "twice", "target": "/crm/"}, {"alias": "twice", "target": "/"}]), "/twice is given twice"),
        (json!(many), "at most 50"),
    ] {
        let (status, body) = api(Method::PUT, MAIN, "/api/admin/paths", Some(json!({"paths": refused}))).await;
        assert!(status == StatusCode::UNPROCESSABLE_ENTITY && body.contains(says), "{says}: {status} {body}");
    }
    // A short path saved before an app came to claim its segment: the app's page opens there, and the doctor says so.
    sql(&k, &format!("update tenant set settings = settings || '{{\"paths\": [{{\"alias\": \"helpdesk\", \"target\": \"/crm/\"}}]}}' where id = '{beta}'"))
        .await;
    let (status, location, ..) = get(MAIN, "/helpdesk/", Some(&nina_main)).await;
    assert!(status != StatusCode::FOUND || location != "/crm/", "the app wins: {status} {location}");
    let shadowed = ferrum_server::controllers::domains::shadowed_paths(&st).await.unwrap();
    assert!(shadowed.contains(&("beta".into(), "helpdesk".into(), "helpdesk".into())), "{shadowed:?}");
    let listed: Value = serde_json::from_str(&api(Method::GET, MAIN, "/api/admin/paths", None).await.1).unwrap();
    assert_eq!(listed["paths"][0]["works"], false);
    assert_eq!(api(Method::PUT, MAIN, "/api/admin/paths", Some(json!({"paths": []}))).await.0, StatusCode::OK);

    // ── Nothing widens a zone. An address the operator put in a zone keeps that zone whatever is edited; what it shows
    // never names an app the zone leaves out, and `/` never leads to one. ──
    const FIELD: &str = "roam--beta.ferrum.test";
    let (status, body) =
        api(Method::POST, MAIN, "/api/admin/domains", Some(json!({"name": "roam", "home": "helpdesk", "apps": ["crm", "helpdesk"], "label": "Field"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    k.set_host_zone(FIELD, Some("field")).await.unwrap();
    let (_, nina_field) = signed_in(&router, FIELD, "beta", "nina@beta.test").await;
    let (status, location, html, _) = get(FIELD, "/", Some(&nina_field)).await;
    assert!(!status.is_redirection(), "Helpdesk is not opened in this zone: Home ({status} {location})");
    let boot = bootstrap(&html);
    assert_eq!((boot["host"]["home"].clone(), boot["host"]["apps"].clone()), (json!(null), json!(["crm"])), "{}", boot["host"]);
    let (_, field_admin) = signed_in(&router, FIELD, "beta", "admin@beta.test").await;
    let (status, _, body, _) = call(&router, Method::GET, FIELD, "/api/admin/domains", Some(&field_admin), None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed: Value = serde_json::from_str(&body).unwrap();
    let homes: Vec<&str> = listed["homes"].as_array().unwrap().iter().filter_map(|h| h["id"].as_str()).collect();
    assert_eq!(homes, ["crm"], "the zone's apps only");
    assert_eq!(
        (row(&listed, FIELD)["home"].clone(), row(&listed, FIELD)["apps"].clone(), row(&listed, FIELD)["zone"].clone()),
        (json!(null), json!(["crm"]), json!("field"))
    );
    let (status, _, body, _) =
        call(&router, Method::PATCH, FIELD, &format!("/api/admin/domains/{FIELD}"), Some(&field_admin), None, Some(json!({"home": "helpdesk"}))).await;
    assert!(
        status == StatusCode::UNPROCESSABLE_ENTITY && body.contains("helpdesk is no app of this workspace"),
        "refused as an app it does not have: {status} {body}"
    );
    let (status, _, body, _) =
        call(&router, Method::PATCH, FIELD, &format!("/api/admin/domains/{FIELD}"), Some(&field_admin), None, Some(json!({"zone": null}))).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && body.contains("only the operator"), "{status} {body}");
    assert_eq!(patch(FIELD, json!({"home": "crm", "label": "Out and about"})).await.0, StatusCode::OK);
    assert_eq!(k.host_zone(FIELD).await.unwrap().as_deref(), Some("field"), "its zone, whatever is edited");
    k.release_hosts(uuid::Uuid::nil()).await.unwrap(); // clears this replica's host cache
    assert_eq!(get(FIELD, "/", Some(&nina_field)).await.1, "/crm/", "an app the zone shows is a home there");

    // ── The limit, and removing a name (its leaving period, as any address). ──
    sql(&k, &format!("update tenant set quotas = '{{\"addresses\": 4}}' where id = '{beta}'")).await;
    let (status, body) = api(Method::POST, MAIN, "/api/admin/domains", Some(json!({"name": "spare"}))).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && body.contains("at most 4 addresses"), "{status} {body}");
    let (status, body) = api(Method::DELETE, MAIN, &format!("/api/admin/domains/{SALES}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(serde_json::from_str::<Value>(&body).unwrap()["leaving_until"].is_string());
    let listed: Value = serde_json::from_str(&api(Method::GET, MAIN, "/api/admin/domains", None).await.1).unwrap();
    assert_eq!(
        (row(&listed, SALES)["status"].clone(), row(&listed, SALES)["can"].clone()),
        (json!("leaving"), json!({"edit": false, "remove": false, "make_primary": false}))
    );

    // Every change is on the audit trail, as the administrator's.
    let mut tx = k.db.system_tx().await.unwrap();
    let audited: Vec<(String, String)> = sqlx::query_as(
        "select entity, action from audit_event where tenant_id = $1 and actor = $2 and entity in ('WorkspaceDomain', 'WorkspacePaths') order by id",
    )
    .bind(beta)
    .bind(root)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    for (entity, action) in [("WorkspaceDomain", "add"), ("WorkspaceDomain", "shape"), ("WorkspaceDomain", "remove"), ("WorkspacePaths", "set")] {
        assert!(audited.contains(&(entity.into(), action.into())), "{entity} {action}: {audited:?}");
    }
}
