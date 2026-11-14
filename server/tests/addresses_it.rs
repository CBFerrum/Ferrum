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
}
