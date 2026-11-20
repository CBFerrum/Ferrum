//! The limiters' own bookkeeping (docs/builder/decisions.md BD-86), over HTTP against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database is reset): the whole server, booted as the binary boots it.
//!
//! A limiter counts a caller by network (an IPv4 /24, an IPv6 /64) and by values already checked: a workspace a caller
//! names counts once found, an unknown one on the network alone, so a caller who makes up a name for each request makes
//! no key and no row of its own; and a request the replica's own count refused writes nothing to the count every
//! replica shares.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use ferrum_kernel::tenant::NewTenant;
use ferrum_server::app::App;
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use serde_json::json;
use serial_test::serial;
use std::sync::LazyLock;
use tokio::sync::OnceCell;
use tower::ServiceExt;

struct Server {
    router: axum::Router,
    st: AppState,
}

static RUNTIME: LazyLock<tokio::runtime::Runtime> =
    LazyLock::new(|| tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().expect("a runtime"));

static SERVER: OnceCell<Server> = OnceCell::const_new();

/// `scenario` on the suite's runtime; its failure is this test's, said again here, where the test's own output is kept.
fn run(scenario: impl std::future::Future<Output = ()> + Send + 'static) {
    if let Err(e) = RUNTIME.block_on(RUNTIME.spawn(scenario)) {
        let p = e.into_panic();
        let said = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
        panic!("{said}");
    }
}

/// The server, with one workspace (`acme`) whose sign-in is tried.
async fn server() -> &'static Server {
    SERVER
        .get_or_init(|| async {
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
            let ctx = AppContext::builder(Environment::Test, base.config.clone()).build();
            ctx.shared_store.insert(st.clone());
            let router = App::routes(&ctx).to_router::<App>(ctx.clone(), axum::Router::new()).expect("router");
            let new = NewTenant {
                slug: "acme".into(),
                name: "Acme".into(),
                admin_email: "admin@acme.test".into(),
                admin_password: "Correct-Horse-9".into(),
                admin_name: None,
                packages: vec![],
                settings: None,
            };
            st.kernel.create_tenant(new).await.expect("a workspace");
            Server { router: App::after_routes(router, &ctx).await.expect("layers"), st }
        })
        .await
}

/// One request from `ip`, as the gateway reports it.
async fn call(ip: &str, method: Method, path: &str, body: Option<serde_json::Value>) -> StatusCode {
    let req = Request::builder().method(method).uri(path).header(header::HOST, "localhost").header("x-forwarded-for", ip);
    let req = match body {
        Some(b) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    server().await.router.clone().oneshot(req).await.unwrap().status()
}

/// The rows of the shared count whose key starts with `prefix`, and their hits.
async fn rows(prefix: &str) -> (i64, i64) {
    let k = &server().await.st.kernel;
    let mut tx = k.db.system_tx().await.unwrap();
    let found = sqlx::query_as("select count(*), coalesce(sum(hits), 0)::int8 from rate_window where starts_with(key, $1)")
        .bind(prefix)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    found
}

#[test]
#[serial]
fn an_unknown_tenant_segment_counts_on_the_network() {
    run(async {
        let max = server().await.st.public_limiter.max() as usize;
        // A workspace made up for each request, from one network: each is no workspace (404) until the network's
        // budget is spent, then the network is refused, whatever name comes next.
        for i in 0..max {
            assert_eq!(call("198.51.100.7", Method::GET, &format!("/api/site/nope{i}/core/x"), None).await, StatusCode::NOT_FOUND, "request {i}");
        }
        assert_eq!(call("198.51.100.7", Method::GET, "/api/site/another/core/x", None).await, StatusCode::TOO_MANY_REQUESTS, "past the network's budget");
        assert_eq!(call("198.51.100.200", Method::GET, "/api/site/yet-another/core/x", None).await, StatusCode::TOO_MANY_REQUESTS, "the same /24");
        assert_eq!(call("192.0.2.1", Method::GET, "/api/site/nope0/core/x", None).await, StatusCode::NOT_FOUND, "another network is not held");
    });
}

#[test]
#[serial]
fn an_ipv6_slash_64_counts_as_one() {
    run(async {
        let max = server().await.st.login_limiter.max();
        let attempt = json!({"tenant": "acme", "email": "nobody@acme.test", "password": "Wrong-Horse-1"});
        // Each attempt from another address of one /64: refused once the /64's budget is spent.
        for i in 1..=max {
            let status = call(&format!("2001:db8:5:6::{i:x}"), Method::POST, "/api/auth/login", Some(attempt.clone())).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {i}");
        }
        let past = call("2001:db8:5:6:ffff:ffff:ffff:ffff", Method::POST, "/api/auth/login", Some(attempt.clone())).await;
        assert_eq!(past, StatusCode::TOO_MANY_REQUESTS, "a /64 counts as one");
        assert_eq!(call("2001:db8:5:7::1", Method::POST, "/api/auth/login", Some(attempt)).await, StatusCode::UNAUTHORIZED, "the next /64 is another");
    });
}

#[test]
#[serial]
fn a_refused_request_writes_no_rate_row() {
    run(async {
        let max = server().await.st.public_limiter.max() as usize;
        for i in 0..max + 50 {
            call("203.0.113.9", Method::GET, &format!("/api/site/made-up-{i}/core/x"), None).await;
        }
        // One count, the network's, holding what the replica admitted; the fifty refused wrote nothing, and no name the
        // caller made up has a row of its own.
        assert_eq!(rows("public|203.0.113.").await, (1, max as i64), "the shared count of a network flooding made-up names");
    });
}

#[test]
#[serial]
fn a_window_longer_than_an_hour_is_swept_by_its_own_length() {
    run(async {
        let k = &server().await.st.kernel;
        // A two-hour window begun ninety minutes ago is still counting; a minute's window that ended an hour ago is not.
        let mut tx = k.db.system_tx().await.unwrap();
        sqlx::query(
            "insert into rate_window (key, window_start, ends_at, hits) values \
             ('it|long', now() - interval '90 minutes', now() + interval '30 minutes', 7), ('it|ended', now() - interval '61 minutes', now() - interval '60 minutes', 3)",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        k.db.sweep_rate_windows().await.unwrap();
        assert_eq!(rows("it|long").await, (1, 7), "a window still counting is kept, whenever it began");
        assert_eq!(rows("it|ended").await, (0, 0), "an ended one goes");
    });
}
