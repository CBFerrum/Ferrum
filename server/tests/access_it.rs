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
use uuid::Uuid;

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

    // ── 2. An API key pinned to networks ──
    let member_token = holds[0].token.clone();
    let (status, answer) =
        call(&r, Method::POST, "/api/auth/api_keys", INSIDE, Cred::Bearer(&member_token), Some(json!({"name": "pinned", "networks": ["10.0.0.1/8"]}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "a network that is not one: {answer}");
    let (status, answer) = call(
        &r,
        Method::POST,
        "/api/auth/api_keys",
        INSIDE,
        Cred::Bearer(&member_token),
        Some(json!({"name": "pinned", "networks": ["10.0.0.0/8", "2001:db8::/32"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let made: Value = serde_json::from_str(&answer).unwrap();
    let pinned = made["key"].as_str().unwrap().to_string();
    for ip in [OUTSIDE, OUTSIDE_V6] {
        let (status, answer) = call(&r, Method::GET, "/api/auth/me", ip, Cred::Bearer(&pinned), None).await;
        assert!(refused_by_network(status, &answer), "a pinned key from {ip}: {status} {answer}");
    }
    for ip in [INSIDE, INSIDE_V6] {
        assert_eq!(call(&r, Method::GET, "/api/auth/me", ip, Cred::Bearer(&pinned), None).await.0, StatusCode::OK, "a pinned key from {ip}");
    }
    assert!(k.verify_api_key(&pinned).await.is_err(), "from no address, a pinned key is refused");
    let (_, list) = call(&r, Method::GET, "/api/auth/api_keys", INSIDE, Cred::Bearer(&member_token), None).await;
    let list: Value = serde_json::from_str(&list).unwrap();
    let listed = list["keys"].as_array().unwrap().iter().find(|k| k["id"] == made["id"]).cloned().unwrap();
    assert_eq!(listed["networks"], json!(["10.0.0.0/8", "2001:db8::/32"]), "listed with its networks");
    let path = format!("/api/auth/api_keys/{}", made["id"].as_str().unwrap());
    let (status, _) = call(&r, Method::PATCH, &path, INSIDE, Cred::Bearer(&member_token), Some(json!({"networks": []}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(call(&r, Method::GET, "/api/auth/me", OUTSIDE, Cred::Bearer(&pinned), None).await.0, StatusCode::OK, "unpinned: from anywhere");
    assert_eq!(call(&r, Method::GET, "/api/auth/me", OUTSIDE, Cred::Bearer(&holds[0].key), None).await.0, StatusCode::OK, "a key with none, as before");
    let mut tx = k.db.tenant_tx(acme).await.unwrap();
    let trail: Vec<String> = sqlx::query_scalar("select action from audit_event where tenant_id = $1 and entity = 'ApiKey' order by id")
        .bind(acme)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(trail, ["create", "networks"], "audited");

    // ── 3. Idle timeout ──
    for bad in [json!(4), json!(-1), json!(20000), json!("15")] {
        let (status, _) = call(&r, Method::PUT, "/api/admin/settings", INSIDE, Cred::Bearer(&admin_token), Some(json!({"idle_minutes": bad}))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "idle_minutes {bad}");
    }
    let (status, answer) = call(&r, Method::PUT, "/api/admin/settings", INSIDE, Cred::Bearer(&admin_token), Some(json!({"idle_minutes": 15}))).await;
    assert_eq!((status, serde_json::from_str::<Value>(&answer).unwrap()["idle_minutes"].clone()), (StatusCode::OK, json!(15)));
    // A browser session, and the page's bearer token bound to it.
    let (_, page, cookie) = sign_in(&r, INSIDE, "member@acme.test", PASSWORD, true).await;
    let (cookie, bound) = (cookie.unwrap(), page["token"].as_str().unwrap().to_string());
    let secret = base.config.auth.as_ref().unwrap().jwt.as_ref().unwrap().secret.clone();
    let sid: Uuid = loco_rs::auth::jwt::JWT::new(&secret).validate(&cookie).unwrap().claims.claims["sid"].as_str().unwrap().parse().unwrap();
    let seen = |ago: i64| {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(acme).await.unwrap();
            sqlx::query("update user_session set last_seen_at = now() - make_interval(mins => $2::int) where id = $1")
                .bind(sid)
                .bind(ago as i32)
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
    };
    let last_seen = || {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(acme).await.unwrap();
            let (at, revoked): (chrono::DateTime<chrono::Utc>, bool) =
                sqlx::query_as("select last_seen_at, revoked_at is not null from user_session where id = $1").bind(sid).fetch_one(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            (chrono::Utc::now() - at, revoked)
        }
    };
    // A socket's recheck is not activity: it leaves the session's last use where it was.
    seen(3).await;
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::HOST, HOST.parse().unwrap());
    headers.insert("x-forwarded-for", INSIDE.parse().unwrap());
    let entry = ferrum_server::extract::socket_entry(&st, &headers).await;
    ferrum_server::extract::ctx_from_token(&st, &bound, "it".into(), &entry, false).await.expect("a socket of a live session");
    assert!(last_seen().await.0 > chrono::Duration::minutes(2), "a recheck does not keep a session alive");
    // A request is: it moves the last use on.
    // (A replica keeps its answer about a session for 20 seconds.)
    tokio::time::sleep(std::time::Duration::from_secs(21)).await;
    assert_eq!(call(&r, Method::GET, "/api/auth/me", INSIDE, Cred::Cookie(&cookie), None).await.0, StatusCode::OK);
    assert!(last_seen().await.0 < chrono::Duration::minutes(1), "a request is activity");
    // Idle longer than allowed: ended, for the socket and every request, and it does not come back.
    seen(16).await;
    tokio::time::sleep(std::time::Duration::from_secs(21)).await;
    let socket = ferrum_server::extract::ctx_from_token(&st, &bound, "it".into(), &entry, false).await;
    assert!(matches!(&socket, Err(e) if e.0.status() == 401), "the socket's recheck stops it: {:?}", socket.err());
    assert!(last_seen().await.1, "the session is ended");
    let (status, answer) = call(&r, Method::GET, "/api/auth/me", INSIDE, Cred::Cookie(&cookie), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{answer}");
    assert!(answer.contains("sign in again"), "the answer the interface already handles: {answer}");
    seen(0).await;
    assert_eq!(call(&r, Method::GET, "/api/auth/me", INSIDE, Cred::Cookie(&cookie), None).await.0, StatusCode::UNAUTHORIZED, "it cannot come back");
    assert_eq!(call(&r, Method::GET, "/api/auth/me", INSIDE, Cred::Bearer(&holds[0].key), None).await.0, StatusCode::OK, "API keys are not sessions");
    let (status, _) = call(&r, Method::PUT, "/api/admin/settings", INSIDE, Cred::Bearer(&admin_token), Some(json!({"idle_minutes": 0}))).await;
    assert_eq!(status, StatusCode::OK);

    // ── 4. Sign-ins on the audit trail ──
    let sign_ins = |address: &'static str| {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(acme).await.unwrap();
            let rows: Vec<(String, Value)> =
                sqlx::query_as("select action, diff from audit_event where tenant_id = $1 and entity = 'SignIn' and diff->>'address' = $2 order by id")
                    .bind(acme)
                    .bind(address)
                    .fetch_all(&mut *tx)
                    .await
                    .unwrap();
            tx.commit().await.unwrap();
            rows
        }
    };
    let (_, _, cookie) = sign_in(&r, "10.4.0.1", "member@acme.test", PASSWORD, true).await;
    let got = sign_ins("10.4.0.1").await;
    assert_eq!(got.len(), 1, "one event: {got:?}");
    assert_eq!(got[0].0, "signed_in");
    assert_eq!((got[0].1["method"].clone(), got[0].1["zone"].clone()), (json!("password"), Value::Null));
    assert!(got[0].1["user_agent"].as_str().unwrap().contains("Firefox"), "{got:?}");
    // Signing out ends the session, on the trail with where it was opened.
    call(&r, Method::POST, "/api/auth/logout", "10.4.0.1", Cred::Cookie(&cookie.unwrap()), None).await;
    let got = sign_ins("10.4.0.1").await;
    assert_eq!(got.iter().map(|g| g.0.as_str()).collect::<Vec<_>>(), ["signed_in", "session_ended"], "{got:?}");
    assert_eq!(got[1].1["reason"], "signed_out");
    // The idle session of item 3 ended on the trail too.
    let mut tx = k.db.tenant_tx(acme).await.unwrap();
    let idle: i64 = sqlx::query_scalar(
        "select count(*) from audit_event where entity = 'SignIn' and action = 'session_ended' and diff->>'reason' = 'idle' and diff->>'session' = $1",
    )
    .bind(sid.to_string())
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(idle, 1, "the idle session's end, once");
    // A wrong password for a known account; nothing by address for an unknown one, only counted.
    // (The member has had more than an hour's few already: someone fresh.)
    k.create_user(&admin, "wrong@acme.test", "Wrong", Some(PASSWORD), &[], false).await.unwrap();
    sign_in(&r, "10.4.0.2", "wrong@acme.test", "Wrong-Horse-99", false).await;
    let got = sign_ins("10.4.0.2").await;
    assert_eq!((got.len(), got[0].0.as_str(), got[0].1["method"].clone()), (1, "failed", json!("password")), "{got:?}");
    let unknown = ferrum_kernel::gate::unknown_failures();
    sign_in(&r, "10.4.0.3", "nobody@acme.test", "Wrong-Horse-99", false).await;
    assert!(sign_ins("10.4.0.3").await.is_empty(), "nothing recorded by address for nobody");
    assert_eq!(ferrum_kernel::gate::unknown_failures(), unknown + 1, "counted");
    assert!(call(&r, Method::GET, "/metrics", INSIDE, Cred::None, None).await.1.contains("ferrum_sign_in_unknown_total"));
    // A refusal by network, before the password.
    let (status, _) = call(&r, Method::PUT, "/api/admin/settings", INSIDE, Cred::Bearer(&admin_token), Some(networks.clone())).await;
    assert_eq!(status, StatusCode::OK);
    sign_in(&r, "198.51.100.4", "wrong@acme.test", PASSWORD, false).await;
    let got = sign_ins("198.51.100.4").await;
    assert_eq!((got.len(), got[0].0.as_str(), got[0].1["by"].clone()), (1, "refused", json!("network")), "{got:?}");
    k.clear_staff_networks("acme", "test").await.unwrap();
    // A flood: a few an hour one by one, then one summary with a count once the hour is over.
    let flood = k.create_user(&admin, "flood@acme.test", "Flood", Some(PASSWORD), &[], false).await.unwrap();
    let from = ferrum_kernel::gate::Arrival { address: "10.4.0.5".into(), ..Default::default() };
    for _ in 0..20 {
        k.record_sign_in_failure(acme, flood, "failed", "password", &from).await.unwrap();
    }
    assert_eq!(sign_ins("10.4.0.5").await.len(), ferrum_kernel::gate::FAILURES_PER_HOUR as usize, "capped");
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update sign_in_failure set hour = hour - interval '1 hour' where user_id = $1").bind(flood).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    k.purge_sessions().await.unwrap();
    k.purge_sessions().await.unwrap();
    let mut tx = k.db.tenant_tx(acme).await.unwrap();
    let summary: Vec<Value> = sqlx::query_scalar("select diff from audit_event where entity = 'SignIn' and action = 'failed_summary' and record_id = $1")
        .bind(flood)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(summary.len(), 1, "one summary");
    assert_eq!(summary[0]["count"], json!(20 - ferrum_kernel::gate::FAILURES_PER_HOUR));
    // An administrator ending a session, and a password change.
    let (_, _, cookie) = sign_in(&r, "10.4.0.6", "member@acme.test", PASSWORD, true).await;
    let msid = loco_rs::auth::jwt::JWT::new(&secret).validate(cookie.as_deref().unwrap()).unwrap().claims.claims["sid"].as_str().unwrap().to_string();
    assert_eq!(call(&r, Method::DELETE, &format!("/api/auth/sessions/{msid}"), INSIDE, Cred::Bearer(&admin_token), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(sign_ins("10.4.0.6").await.last().unwrap().1["reason"], "revoked");
    k.set_password(&admin, flood, None, "Another-Horse-77").await.unwrap();
    let mut tx = k.db.tenant_tx(acme).await.unwrap();
    let changed: i64 =
        sqlx::query_scalar("select count(*) from audit_event where entity = 'SignIn' and record_id = $1 and diff->>'reason' = 'password_changed'")
            .bind(flood)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(changed, 1);
    // Administrators read them, filtered; nobody else.
    let (status, answer) = call(&r, Method::GET, "/api/admin/audit?category=sign_in&limit=500", INSIDE, Cred::Bearer(&admin_token), None).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let events: Value = serde_json::from_str(&answer).unwrap();
    let events = events["events"].as_array().unwrap();
    assert!(events.iter().all(|e| e["category"] == "sign_in" && e["entity"] == "SignIn"), "only sign-ins");
    assert!(events.iter().any(|e| e["action"] == "failed_summary"), "{answer}");
    let one = events.iter().find(|e| e["details"]["address"] == "10.4.0.1").unwrap();
    for field in ["id", "workspace", "at", "record", "actor", "hash"] {
        assert!(!one[field].is_null(), "{field} in {one}");
    }
    assert_eq!(call(&r, Method::GET, "/api/admin/audit?category=nothing", INSIDE, Cred::Bearer(&admin_token), None).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(call(&r, Method::GET, "/api/admin/audit", INSIDE, Cred::Bearer(&holds[0].token), None).await.0, StatusCode::FORBIDDEN, "members do not");
    // Personal data: listed in the person's export, blanked by their erasure (the events stay).
    let gone = k.create_user(&admin, "gone@acme.test", "Gone", Some(PASSWORD), &[], false).await.unwrap();
    sign_in(&r, "10.4.0.9", "gone@acme.test", PASSWORD, false).await;
    let export = k.personal_data(&admin, "gone@acme.test").await.unwrap();
    assert!(export["sign_ins"].as_array().unwrap().iter().any(|e| e["details"]["address"] == "10.4.0.9"), "{export}");
    k.erase_personal_data(&admin, "gone@acme.test", "asked to").await.unwrap();
    assert!(sign_ins("10.4.0.9").await.is_empty(), "the address is erased");
    let mut tx = k.db.tenant_tx(acme).await.unwrap();
    let kept: i64 =
        sqlx::query_scalar("select count(*) from audit_event where entity = 'SignIn' and record_id = $1").bind(gone).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(kept >= 1, "the events stay");

    // ── 5. Sending the audit log elsewhere ──
    let rx = Receiver::start().await;
    // (a) The operator's sink: two replicas of it on this database.
    let sink = ferrum_kernel::audit_sink::Sink::parse(&format!("{}/sink", rx.base), SINK_SECRET).unwrap();
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let replica = || {
        let (url, sink) = (url.clone(), sink.clone());
        async move {
            let db = ferrum_kernel::Db::connect(ferrum_kernel::DbConfig { url, max_connections: 4, ..Default::default() }).await.unwrap();
            let catalog = ferrum_meta::Catalog::load_dir(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages")).unwrap();
            let settings =
                ferrum_kernel::KernelSettings { secret_key: Some(ferrum_kernel::secret::SecretKey::new([7u8; 32])), audit_sink: sink, ..Default::default() };
            Kernel::boot(db, catalog, ferrum_kernel::HookRegistry::default(), settings, None).await.unwrap()
        }
    };
    let (k1, k2) = (replica().await, replica().await);
    assert_eq!(k.audit_sink_once().await.unwrap(), ferrum_kernel::audit_sink::Turn::Off, "off without the setting");
    assert_eq!(k1.audit_sink_once().await.unwrap(), ferrum_kernel::audit_sink::Turn::Sent(0), "the first turn starts the cursor at now");
    let write = |n: usize| {
        let (k, admin) = (k.clone(), admin.clone());
        async move {
            let mut ids = vec![];
            for i in 0..n {
                let tz = if i % 2 == 0 { "Asia/Kolkata" } else { "UTC" };
                k.update_tenant_settings(&admin, json!({"timezone": tz}).as_object().unwrap()).await.unwrap();
                let mut tx = k.db.tenant_tx(acme).await.unwrap();
                let id: i64 = sqlx::query_scalar("select max(id) from audit_event where tenant_id = $1").bind(acme).fetch_one(&mut *tx).await.unwrap();
                tx.commit().await.unwrap();
                ids.push(id);
            }
            ids
        }
    };
    let first = write(5).await;
    let mut sent = 0;
    while let ferrum_kernel::audit_sink::Turn::Sent(n @ 1..) = k1.audit_sink_once().await.unwrap() {
        sent += n;
    }
    assert!(sent >= 5, "{sent}");
    let got = rx.ids("/sink");
    assert!(first.iter().all(|i| got.contains(i)), "every event: {first:?} in {got:?}");
    let mut sorted = got.clone();
    sorted.sort();
    assert_eq!(got, sorted, "in order");
    let lines = rx.lines("/sink");
    let one: Value = serde_json::from_str(&lines[0]).unwrap();
    for field in ["id", "workspace", "at", "category", "entity", "record", "action", "actor", "details", "hash"] {
        assert!(one.get(field).is_some(), "{field} in {one}");
    }
    assert!(rx.all_signed(), "every batch is signed with the secret");
    assert!(!lines.iter().any(|l| l.contains(SINK_SECRET)), "no secret in an event");
    // A crash between the receiver's yes and the cursor's write: the batch comes again (a duplicate id, no gap).
    let cursor = |v: Option<String>| {
        let k = k.clone();
        async move {
            let mut tx = k.db.system_tx().await.unwrap();
            let now: String = sqlx::query_scalar("select value from cluster_state where key = 'audit_sink_cursor'").fetch_one(&mut *tx).await.unwrap();
            if let Some(v) = v {
                sqlx::query("update cluster_state set value = $1 where key = 'audit_sink_cursor'").bind(v).execute(&mut *tx).await.unwrap();
            }
            tx.commit().await.unwrap();
            now
        }
    };
    let before = cursor(None).await;
    let second = write(2).await;
    while let ferrum_kernel::audit_sink::Turn::Sent(1..) = k1.audit_sink_once().await.unwrap() {}
    cursor(Some(before)).await;
    while let ferrum_kernel::audit_sink::Turn::Sent(1..) = k2.audit_sink_once().await.unwrap() {}
    let got = rx.ids("/sink");
    for id in &second {
        assert_eq!(got.iter().filter(|g| *g == id).count(), 2, "{id} sent again after the lost cursor");
    }
    // The receiver down: nothing moves, and it is said; up again: it goes on from where it was.
    rx.set_failing(true);
    let third = write(1).await;
    assert!(k1.audit_sink_once().await.is_err(), "the receiver refused");
    assert!(k1.audit_sink_behind().await.unwrap().unwrap() >= 0.0);
    assert!(!rx.ids("/sink").contains(&third[0]));
    rx.set_failing(false);
    while let ferrum_kernel::audit_sink::Turn::Sent(1..) = k1.audit_sink_once().await.unwrap() {}
    assert_eq!(rx.ids("/sink").iter().filter(|g| **g == third[0]).count(), 1, "sent once it answers");
    assert_eq!(k1.audit_sink_behind().await.unwrap(), Some(0.0), "nothing waits");
    // Two replicas at once: one sends, the other finds the cursor taken; never the same batch twice.
    rx.set_delay(1500);
    let fourth = write(3).await;
    let (a, b) = tokio::join!(k1.audit_sink_once(), k2.audit_sink_once());
    let turns = [a.unwrap(), b.unwrap()];
    assert!(turns.contains(&ferrum_kernel::audit_sink::Turn::Busy), "{turns:?}");
    assert_eq!(rx.max_in_flight(), 1, "one sender at a time");
    rx.set_delay(0);
    while let ferrum_kernel::audit_sink::Turn::Sent(1..) = k2.audit_sink_once().await.unwrap() {}
    for id in &fourth {
        assert_eq!(rx.ids("/sink").iter().filter(|g| *g == id).count(), 1, "{id} once");
    }
    let (_, metrics) = call(&r, Method::GET, "/metrics", INSIDE, Cred::None, None).await;
    assert!(!metrics.contains("ferrum_audit_sink_behind_seconds"), "only where the sink is set");
    assert!(k1.doctor(10).await.unwrap().iter().any(|f| f.check == "audit.sink"), "the doctor reports it");

    // (b) A workspace's own sink: a webhook on its audit topics, only its events, only the chosen categories.
    let other = k
        .create_tenant(NewTenant {
            slug: "other".into(),
            name: "Other".into(),
            admin_email: "admin@other.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec![],
            settings: None,
        })
        .await
        .unwrap();
    let os = k.login("other", "admin@other.test", PASSWORD).await.unwrap();
    let other_admin = k.load_ctx(other, os.user, None, "it".into()).await.unwrap();
    let (status, answer) = call(
        &r,
        Method::POST,
        "/api/admin/webhooks",
        INSIDE,
        Cred::Bearer(&admin_token),
        Some(json!({"name": "siem", "url": format!("{}/hooks", rx.base), "topics": ["audit.sign_in"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    // And a webhook of everything, which must not get them.
    call(
        &r,
        Method::POST,
        "/api/admin/webhooks",
        INSIDE,
        Cred::Bearer(&admin_token),
        Some(json!({"name": "all", "url": format!("{}/all", rx.base), "topics": ["*"]})),
    )
    .await;
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    while k.deliver_webhooks_once().await.unwrap() > 0 {}
    sign_in(&r, "10.5.0.1", "member@acme.test", PASSWORD, false).await;
    k.update_tenant_settings(&admin, json!({"timezone": "UTC"}).as_object().unwrap()).await.unwrap();
    k.update_tenant_settings(&other_admin, json!({"timezone": "UTC"}).as_object().unwrap()).await.unwrap();
    k.login("other", "admin@other.test", PASSWORD).await.unwrap();
    k.record_sign_in(other, os.user, "signed_in", "password", &ferrum_kernel::gate::Arrival { address: "10.5.0.2".into(), ..Default::default() })
        .await
        .unwrap();
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    while k.deliver_webhooks_once().await.unwrap() > 0 {}
    let hooked: Vec<Value> = rx.lines("/hooks").iter().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(hooked.iter().any(|e| e["details"]["address"] == "10.5.0.1"), "acme's sign-in: {hooked:?}");
    assert!(hooked.iter().all(|e| e["workspace"] == json!(acme) && e["category"] == "sign_in"), "only acme's, only sign-ins: {hooked:?}");
    assert!(rx.lines("/all").iter().all(|l| !l.contains("SignIn")), "a webhook of everything gets no audit events");

    // ── 6. Break-glass under enforced single sign-on ──
    let admin2 = k.create_user(&admin, "admin2@acme.test", "Second", Some(PASSWORD), &[], true).await.unwrap();
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update tenant set settings = settings || '{\"require_sso\": true}' where id = $1").bind(acme).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let put = |v: Value| call(&r, Method::PUT, "/api/admin/settings", INSIDE, Cred::Bearer(&admin_token), Some(json!({"break_glass": v})));
    assert_eq!(sign_in(&r, "10.6.0.1", "member@acme.test", PASSWORD, false).await.0, StatusCode::FORBIDDEN, "staff sign in through SSO");
    assert_eq!(sign_in(&r, "10.6.0.2", "admin2@acme.test", PASSWORD, false).await.0, StatusCode::OK, "unset: every administrator keeps a password");
    assert_eq!(sign_ins("10.6.0.2").await.iter().filter(|e| e.0 == "break_glass").count(), 1, "a break-glass sign-in, on its own");
    let (status, answer) = put(json!([])).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && answer.contains("at least one"), "nobody: {answer}");
    let (status, answer) = put(json!([member.to_string()])).await;
    assert!(status == StatusCode::UNPROCESSABLE_ENTITY && answer.contains("not an active administrator"), "a member: {answer}");
    let (status, answer) = put(json!([admin2.to_string()])).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(serde_json::from_str::<Value>(&answer).unwrap()["break_glass"], json!([admin2.to_string()]));
    assert_eq!(sign_in(&r, "10.6.0.3", "admin@acme.test", PASSWORD, false).await.0, StatusCode::FORBIDDEN, "an administrator not named");
    assert_eq!(sign_in(&r, "10.6.0.4", "admin2@acme.test", PASSWORD, false).await.0, StatusCode::OK, "the one named");
    assert_eq!(sign_ins("10.6.0.4").await.iter().filter(|e| e.0 == "break_glass").count(), 1);
    let (status, _) = put(Value::Null).await;
    assert_eq!(status, StatusCode::OK, "back to every administrator");
    assert_eq!(sign_in(&r, "10.6.0.5", "admin@acme.test", PASSWORD, false).await.0, StatusCode::OK);
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update tenant set settings = settings - 'require_sso' where id = $1").bind(acme).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

const SINK_SECRET: &str = "a-sink-secret-of-some-length";

/// A receiver of batches and webhooks: what came, in order, by path; whether each was signed; failing or slow
/// when asked; how many requests were in flight at most.
#[derive(Clone)]
struct Receiver {
    base: String,
    state: std::sync::Arc<std::sync::Mutex<RxState>>,
}

#[derive(Default)]
struct RxState {
    bodies: Vec<(String, String)>,
    unsigned: usize,
    failing: bool,
    delay_ms: u64,
    in_flight: usize,
    max_in_flight: usize,
}

impl Receiver {
    async fn start() -> Receiver {
        let state = std::sync::Arc::new(std::sync::Mutex::new(RxState::default()));
        let st = state.clone();
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri, headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let st = st.clone();
            async move {
                let delay = {
                    let mut s = st.lock().unwrap();
                    s.in_flight += 1;
                    s.max_in_flight = s.max_in_flight.max(s.in_flight);
                    s.delay_ms
                };
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                let mut s = st.lock().unwrap();
                s.in_flight -= 1;
                if s.failing {
                    return StatusCode::SERVICE_UNAVAILABLE;
                }
                let path = uri.path().to_string();
                let sig = headers.get("x-ferrum-signature").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                if path == "/sink" {
                    let ts: i64 = sig.split(',').next().and_then(|t| t.strip_prefix("t=")).and_then(|t| t.parse().ok()).unwrap_or_default();
                    let want = ferrum_kernel::outbox::signature(SINK_SECRET, ts, &body).unwrap();
                    let same = want.len() == sig.len() && want.bytes().zip(sig.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0;
                    if !same {
                        s.unsigned += 1;
                    }
                }
                let text = String::from_utf8_lossy(&body).into_owned();
                s.bodies.push((path, text));
                StatusCode::OK
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Receiver { base, state }
    }

    /// Each event received on `path`, one per line (a webhook's body is one event).
    fn lines(&self, path: &str) -> Vec<String> {
        let s = self.state.lock().unwrap();
        s.bodies.iter().filter(|(p, _)| p == path).flat_map(|(_, b)| b.lines().map(str::to_string).collect::<Vec<_>>()).filter(|l| !l.is_empty()).collect()
    }

    fn ids(&self, path: &str) -> Vec<i64> {
        self.lines(path).iter().map(|l| serde_json::from_str::<Value>(l).unwrap()["id"].as_str().unwrap().parse().unwrap()).collect()
    }

    fn all_signed(&self) -> bool {
        self.state.lock().unwrap().unsigned == 0
    }

    fn set_failing(&self, on: bool) {
        self.state.lock().unwrap().failing = on;
    }

    fn set_delay(&self, ms: u64) {
        let mut s = self.state.lock().unwrap();
        s.delay_ms = ms;
        s.max_in_flight = 0;
    }

    fn max_in_flight(&self) -> usize {
        self.state.lock().unwrap().max_in_flight
    }
}
