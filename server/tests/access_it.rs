//! A workspace's own access controls (docs/operations.md, "Staff networks", "Sending the audit log
//! elsewhere"; docs/security.md), against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`; the database is reset): the whole server,
//! booted as the binary boots it, behind its own router.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine as _;
use ferrum_kernel::Kernel;
use ferrum_kernel::tenant::NewTenant;
use ferrum_server::app::App;
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use serde_json::{Value, json};
use sha2::Digest;
use tower::ServiceExt;

const PASSWORD: &str = "Correct-Horse-9";
const HOST: &str = "erp.acme.test";
/// Inside the workspace's networks, and outside them.
const INSIDE: &str = "10.1.2.3";
const INSIDE_V6: &str = "2001:db8::5";
const OUTSIDE: &str = "203.0.113.9";
const OUTSIDE_V6: &str = "2001:db9::5";

#[test]
fn workspace_access() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

#[derive(Clone, Copy)]
enum Cred<'a> {
    None,
    Bearer(&'a str),
    Cookie(&'a str),
    Basic(&'a str, &'a str),
}

/// One request from `ip` (as the gateway reports it), carrying `cred`.
async fn call(r: &axum::Router, method: Method, path: &str, ip: &str, cred: Cred<'_>, body: Option<Value>) -> (StatusCode, String) {
    let mut req = Request::builder().method(method).uri(path).header(header::HOST, HOST).header("x-forwarded-for", ip);
    match cred {
        Cred::None => {}
        Cred::Bearer(t) => req = req.header(header::AUTHORIZATION, format!("Bearer {t}")),
        Cred::Cookie(t) => {
            req = req
                .header(header::COOKIE, format!("{}={t}", ferrum_server::shell::cookie_name(false)))
                .header("sec-fetch-site", "same-origin")
                .header(header::ORIGIN, format!("http://{HOST}"))
        }
        Cred::Basic(u, p) => req = req.header(header::AUTHORIZATION, format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}")))),
    }
    let req = match body {
        Some(b) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let res = r.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// A password sign-in: status, answer, and the cookie's token when signed in from a page.
async fn sign_in(r: &axum::Router, ip: &str, email: &str, password: &str, page: bool) -> (StatusCode, Value, Option<String>) {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri("/api/auth/login")
        .header(header::HOST, HOST)
        .header("x-forwarded-for", ip)
        .header(header::USER_AGENT, "Mozilla/5.0 (X11; Linux x86_64) Firefox/131.0")
        .header(header::CONTENT_TYPE, "application/json");
    if page {
        req = req.header("sec-fetch-site", "same-origin");
    }
    let req = req.body(Body::from(json!({"tenant": "acme", "email": email, "password": password}).to_string())).unwrap();
    let res = r.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let cookie = res.headers().get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).find_map(|c| {
        c.strip_prefix(&format!("{}=", ferrum_server::shell::cookie_name(false))).map(|rest| rest.split(';').next().unwrap_or_default().to_string())
    });
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null), cookie)
}

async fn token_endpoint(r: &axum::Router, ip: &str, form: String) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/oauth/token")
        .header(header::HOST, HOST)
        .header("x-forwarded-for", ip)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let res = r.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// An OAuth code for `ctx`'s person: (client, code, verifier).
async fn oauth_code(k: &Kernel, ctx: &ferrum_kernel::Ctx) -> (String, String, String) {
    let client = k.oauth_register(Some("it".into()), vec!["http://127.0.0.1:9/cb".into()]).await.unwrap();
    let verifier = "v".repeat(50);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    let code = k.oauth_issue_code(ctx, &client.id, "http://127.0.0.1:9/cb", &challenge, "S256").await.unwrap();
    (client.id, code, verifier)
}

async fn redeem(r: &axum::Router, ip: &str, (client, code, verifier): &(String, String, String)) -> (StatusCode, Value) {
    token_endpoint(
        r,
        ip,
        format!("grant_type=authorization_code&code={code}&redirect_uri=http%3A%2F%2F127.0.0.1%3A9%2Fcb&code_verifier={verifier}&client_id={client}"),
    )
    .await
}

/// The refusal of a credential from outside the networks: 403, `network`, naming no network.
fn refused_by_network(status: StatusCode, body: &str) -> bool {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    status == StatusCode::FORBIDDEN && v["error"] == "network" && !body.contains("10.0.0.0") && !body.contains("2001:db8")
}

async fn failed_logins(k: &Kernel, email: &str) -> (i32, bool) {
    let mut tx = k.db.system_tx().await.unwrap();
    let row = sqlx::query_as("select failed_logins, locked_until is not null from app_user where email = $1").bind(email).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    row
}

fn counted(mode: &str) -> u64 {
    ferrum_kernel::gate::refusals().iter().filter(|(m, _, _)| *m == mode).map(|r| r.2).sum()
}

/// What one person holds.
#[derive(Default)]
struct Holds {
    email: String,
    token: String,
    cookie: String,
    key: String,
    dav: String,
    oauth: String,
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
    k.add_operator_host("acme", HOST, "shell", None).await.unwrap();
    let s = k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
    let admin = k.load_ctx(acme, s.user, Some(s.token_version), "it".into()).await.unwrap();
    let member = k.create_user(&admin, "member@acme.test", "Member", Some(PASSWORD), &[], false).await.unwrap();
    let portal = k.create_user(&admin, "portal@acme.test", "Portal", Some(PASSWORD), &[], false).await.unwrap();
    k.set_user_kind(&admin, portal, "portal").await.unwrap();

    // Every people's credential of a member and a portal person, made from inside.
    let mut holds = vec![];
    for (email, id) in [("member@acme.test", member), ("portal@acme.test", portal)] {
        let (status, body, _) = sign_in(&r, INSIDE, email, PASSWORD, false).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, _, cookie) = sign_in(&r, INSIDE, email, PASSWORD, true).await;
        let me = k.load_ctx(acme, id, None, "it".into()).await.unwrap();
        let (status, oauth) = redeem(&r, INSIDE, &oauth_code(&k, &me).await).await;
        assert_eq!(status, StatusCode::OK, "{oauth}");
        holds.push(Holds {
            email: email.into(),
            token: body["token"].as_str().unwrap().into(),
            cookie: cookie.unwrap(),
            key: k.create_api_key(&admin, id, "it", None, None).await.unwrap().1,
            dav: k.create_api_key(&admin, id, "dav", None, Some("dav")).await.unwrap().1,
            oauth: oauth["access_token"].as_str().unwrap().into(),
        });
    }
    let (_, body, _) = sign_in(&r, INSIDE, "admin@acme.test", PASSWORD, false).await;
    let admin_token = body["token"].as_str().unwrap().to_string();

    // ── 1. Staff networks ──
    let networks = json!({"staff_networks": {"networks": ["10.0.0.0/8", "2001:db8::/32"], "mode": "enforce"}});
    // No self-lockout: saving from outside the new list is refused, saying why.
    let (status, answer) = call(&r, Method::PUT, "/api/admin/settings", OUTSIDE, Cred::Bearer(&admin_token), Some(networks.clone())).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert!(answer.contains("shut you out") && answer.contains(OUTSIDE), "{answer}");
    let (status, answer) = call(&r, Method::PUT, "/api/admin/settings", INSIDE, Cred::Bearer(&admin_token), Some(networks.clone())).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(serde_json::from_str::<Value>(&answer).unwrap()["staff_networks"], networks["staff_networks"]);
    let trail: i64 = {
        let mut tx = k.db.tenant_tx(acme).await.unwrap();
        let n = sqlx::query_scalar(
            "select count(*) from audit_event where tenant_id = $1 and entity = 'Tenant' and action = 'settings' and diff ? 'staff_networks'",
        )
        .bind(acme)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        n
    };
    assert_eq!(trail, 1, "the change is on the audit trail");

    let before = counted("enforce");
    for h in &holds {
        let staff = h.email.starts_with("member");
        let creds = [("token", Cred::Bearer(&h.token)), ("cookie", Cred::Cookie(&h.cookie)), ("oauth", Cred::Bearer(&h.oauth))];
        for (name, cred) in creds {
            for ip in [OUTSIDE, OUTSIDE_V6] {
                let (status, answer) = call(&r, Method::GET, "/api/auth/me", ip, cred, None).await;
                if staff {
                    assert!(refused_by_network(status, &answer), "{name} from {ip}: {status} {answer}");
                } else {
                    assert_eq!(status, StatusCode::OK, "a portal person's {name}: {answer}");
                }
            }
            for ip in [INSIDE, INSIDE_V6] {
                assert_eq!(call(&r, Method::GET, "/api/auth/me", ip, cred, None).await.0, StatusCode::OK, "{} {name} from {ip}", h.email);
            }
        }
        // An app password.
        let propfind = Method::from_bytes(b"PROPFIND").unwrap();
        let (status, answer) = call(&r, propfind.clone(), "/api/dav/comms/", OUTSIDE, Cred::Basic(&h.email, &h.dav), None).await;
        let inside = call(&r, propfind.clone(), "/api/dav/comms/", INSIDE, Cred::Basic(&h.email, &h.dav), None).await.0;
        if staff {
            assert!(refused_by_network(status, &answer), "app password: {status} {answer}");
        } else {
            assert_eq!(status, inside, "a portal person's app password");
        }
        assert_ne!(inside, StatusCode::FORBIDDEN, "app password from inside");
        // A socket's handshake.
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::HOST, HOST.parse().unwrap());
        headers.insert("x-forwarded-for", OUTSIDE.parse().unwrap());
        headers.insert(header::AUTHORIZATION, format!("Bearer {}", h.token).parse().unwrap());
        let entry = ferrum_server::extract::socket_entry(&st, &headers).await;
        let shaken = ferrum_server::extract::socket_credential(&st, &headers, &entry).await;
        assert_eq!(
            matches!(shaken, Err(ferrum_server::error::ApiError(ferrum_kernel::Error::Refused { code: "network", .. }))),
            staff,
            "a socket of {}",
            h.email
        );
        // An API key is a machine's: the workspace's networks do not hold it.
        let (status, answer) = call(&r, Method::GET, "/api/auth/me", OUTSIDE, Cred::Bearer(&h.key), None).await;
        assert_eq!(status, StatusCode::OK, "{}'s API key: {answer}", h.email);
    }
    assert!(counted("enforce") > before, "refusals are counted");
    let (_, metrics) = call(&r, Method::GET, "/metrics", INSIDE, Cred::None, None).await;
    assert!(metrics.contains("ferrum_staff_network_refusals_total{mode=\"enforce\",credential=\"token\"}"), "{metrics}");

    // Sign-in from outside: one answer for a right and a wrong password and nobody, no lockout counted.
    let was = failed_logins(&k, "member@acme.test").await;
    let wrong = sign_in(&r, OUTSIDE, "member@acme.test", "Wrong-Horse-99", false).await;
    let right = sign_in(&r, OUTSIDE, "member@acme.test", PASSWORD, false).await;
    let nobody = sign_in(&r, OUTSIDE, "nobody@acme.test", PASSWORD, false).await;
    assert_eq!(wrong.0, StatusCode::UNAUTHORIZED);
    assert_eq!((wrong.0, &wrong.1["message"]), (right.0, &right.1["message"]), "a wrong and a right password: one answer");
    assert_eq!((right.0, &right.1["message"]), (nobody.0, &nobody.1["message"]), "and that of nobody");
    for _ in 0..6 {
        sign_in(&r, OUTSIDE_V6, "member@acme.test", "Wrong-Horse-99", false).await;
    }
    assert_eq!(failed_logins(&k, "member@acme.test").await, was, "nothing counts toward the lockout");
    assert_eq!(sign_in(&r, INSIDE_V6, "member@acme.test", PASSWORD, false).await.0, StatusCode::OK, "inside, they sign in");
    assert_eq!(sign_in(&r, OUTSIDE, "portal@acme.test", PASSWORD, false).await.0, StatusCode::OK, "a portal person from anywhere");
    // An OAuth code of staff from outside: the unknown answer, and not used up.
    let member_ctx = k.load_ctx(acme, member, None, "it".into()).await.unwrap();
    let code = oauth_code(&k, &member_ctx).await;
    let (status, answer) = redeem(&r, OUTSIDE, &code).await;
    assert_eq!((status, answer["error"].clone()), (StatusCode::BAD_REQUEST, json!("invalid_grant")), "{answer}");
    assert_eq!(redeem(&r, INSIDE, &code).await.0, StatusCode::OK, "it was left unused");

    // Report mode: admitted, and counted. Switching back to enforce is checked again.
    let report = json!({"staff_networks": {"networks": ["10.0.0.0/8"], "mode": "report"}});
    let (status, answer) = call(&r, Method::PUT, "/api/admin/settings", OUTSIDE, Cred::Bearer(&admin_token), Some(report)).await;
    assert!(refused_by_network(status, &answer), "the administrator is staff too: {answer}");
    let (status, answer) = call(
        &r,
        Method::PUT,
        "/api/admin/settings",
        INSIDE,
        Cred::Bearer(&admin_token),
        Some(json!({"staff_networks": {"networks": ["192.0.2.0/24"], "mode": "report"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "report mode may leave the saver outside: {answer}");
    let before = counted("report");
    assert_eq!(call(&r, Method::GET, "/api/auth/me", OUTSIDE, Cred::Bearer(&holds[0].token), None).await.0, StatusCode::OK, "report admits");
    assert_eq!(sign_in(&r, OUTSIDE, "member@acme.test", PASSWORD, false).await.0, StatusCode::OK, "and signs in");
    assert!(counted("report") > before, "and counts");
    let (status, answer) = call(
        &r,
        Method::PUT,
        "/api/admin/settings",
        INSIDE,
        Cred::Bearer(&admin_token),
        Some(json!({"staff_networks": {"networks": ["192.0.2.0/24"], "mode": "enforce"}})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "report to enforce is checked again: {answer}");

    // The operator's task clears it, audited; everyone is back from anywhere.
    let (status, _) = call(&r, Method::PUT, "/api/admin/settings", INSIDE, Cred::Bearer(&admin_token), Some(networks.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert!(refused_by_network(
        call(&r, Method::GET, "/api/auth/me", OUTSIDE, Cred::Bearer(&holds[0].token), None).await.0,
        &call(&r, Method::GET, "/api/auth/me", OUTSIDE, Cred::Bearer(&holds[0].token), None).await.1
    ));
    assert!(k.clear_staff_networks("acme", " ").await.is_err(), "a reason is required");
    k.clear_staff_networks("acme", "the office moved").await.unwrap();
    assert_eq!(call(&r, Method::GET, "/api/auth/me", OUTSIDE, Cred::Bearer(&holds[0].token), None).await.0, StatusCode::OK, "cleared at once");
    let mut tx = k.db.tenant_tx(acme).await.unwrap();
    let why: Value = sqlx::query_scalar("select diff from audit_event where tenant_id = $1 and action = 'clear_staff_networks'")
        .bind(acme)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(why["reason"], "the office moved");
}
