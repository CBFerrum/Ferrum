//! Ferrum as an OpenID Connect provider for the programs beside it, over HTTP (server/src/controllers/oidc.rs), against
//! a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset): the whole server, booted as the binary boots
//! it, with a stand-in for the Cloud app registering the forge's companion (`EXTRA_APPS`).
//!
//! Discovery, keys and user-info answer only on one workspace's own addresses, each its own issuer. A member signs in
//! through the authorization endpoint with no consent page, and the companion takes its tokens with its secret (HTTP
//! Basic or the form); its access token reaches the user-info and nothing else. Refusals are pages until the return
//! address is known to be the client's, and the specification's errors at that address after; the token endpoint says
//! the same thing whatever was wrong, and counts failures. `prompt=login` waits on a page for the person to confirm it is
//! them through the step-up. Signing out ends what the companion holds. API clients go on as before.

mod common;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use base64::Engine as _;
use ferrum_kernel::oidc::Companion;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::vault::{Scope, Vault};
use ferrum_kernel::{Action, HookRegistry, Kernel};
use ferrum_server::app::{App, EXTRA_APPS};
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use serde_json::{Map, Value, json};
use std::sync::{Arc, OnceLock};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "Correct-Horse-9";
const ACME: &str = "acme.test";
const SALES: &str = "sales.acme.test";
const BETA: &str = "beta.test";
const BACK: &str = "https://forge.example/user/oauth2/acme/callback";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

static FORGE: OnceLock<Vault> = OnceLock::new();

struct Forge;

#[async_trait]
impl Companion for Forge {
    fn right(&self) -> (&str, Action) {
        ("LetterHead", Action::Read)
    }

    async fn claims(&self, _k: &Kernel, _tenant: Uuid, user: Uuid) -> ferrum_kernel::Result<Map<String, Value>> {
        Ok(json!({"preferred_username": format!("u-{}", &user.simple().to_string()[..12])}).as_object().cloned().unwrap_or_default())
    }
}

fn stand_in(h: &mut HookRegistry) {
    h.app("forgeapp", &[], |h| {
        let _ = FORGE.set(h.vault());
        h.companion("forge", Arc::new(Forge));
    });
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    fn location(&self) -> String {
        self.headers.get(header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string()
    }
}

/// One request to `host`, from 10.9.8.7 as the gateway reports it.
async fn call(r: &axum::Router, method: Method, host: &str, path: &str, headers: &[(&str, &str)], body: Option<String>) -> Answer {
    let mut req = Request::builder().method(method).uri(path).header(header::HOST, host).header("x-forwarded-for", "10.9.8.7");
    for (h, v) in headers {
        req = req.header(*h, *v);
    }
    let res = r.clone().oneshot(req.body(body.map(Body::from).unwrap_or_else(Body::empty)).unwrap()).await.unwrap();
    let (status, headers) = (res.status(), res.headers().clone());
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    Answer { status, headers, body: String::from_utf8_lossy(&bytes).into_owned() }
}

/// A browser's cookie for `email` signed in at `host`.
async fn cookie(r: &axum::Router, host: &str, slug: &str, email: &str) -> String {
    let body = json!({"tenant": slug, "email": email, "password": PASSWORD, "session": "cookie"}).to_string();
    let res = call(r, Method::POST, host, "/api/auth/login", &[("content-type", "application/json"), ("sec-fetch-site", "same-origin")], Some(body)).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let name = ferrum_server::shell::cookie_name(false);
    let token = res
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| c.strip_prefix(&format!("{name}=")).map(|rest| rest.split(';').next().unwrap_or_default().to_string()))
        .expect("a session cookie");
    format!("{name}={token}")
}

fn authorize_path(client: &str, extra: &str) -> String {
    format!(
        "/oauth/authorize?response_type=code&client_id={client}&redirect_uri={}&scope=openid%20profile%20email&state=s1&code_challenge={CHALLENGE}&code_challenge_method=S256{extra}",
        urlencode(BACK)
    )
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The parameter `name` of an address's query.
fn param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|kv| kv.strip_prefix(&format!("{name}="))).map(|v| {
        let mut out = Vec::new();
        let mut b = v.bytes();
        while let Some(c) = b.next() {
            if c == b'%' {
                let h = [b.next().unwrap(), b.next().unwrap()];
                out.push(u8::from_str_radix(std::str::from_utf8(&h).unwrap(), 16).unwrap());
            } else {
                out.push(c);
            }
        }
        String::from_utf8(out).unwrap()
    })
}

/// The tokens endpoint asked by the companion with its secret as HTTP Basic credentials.
async fn redeem(r: &axum::Router, host: &str, client: &str, secret: &str, code: &str) -> Answer {
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{client}:{secret}"));
    let form = format!("grant_type=authorization_code&code={code}&redirect_uri={}&code_verifier={VERIFIER}", urlencode(BACK));
    call(
        r,
        Method::POST,
        host,
        "/oauth/token",
        &[("authorization", &format!("Basic {basic}")), ("content-type", "application/x-www-form-urlencoded")],
        Some(form),
    )
    .await
}

/// A code for the person of `cookie` at `host`, through the authorization endpoint.
async fn code(r: &axum::Router, host: &str, cookie: &str, client: &str, extra: &str) -> String {
    let res = call(r, Method::GET, host, &authorize_path(client, extra), &[("cookie", cookie)], None).await;
    assert!(res.status.is_redirection(), "{} {}", res.status, res.body);
    let to = res.location();
    assert!(to.starts_with(BACK), "back to the client: {to}");
    assert_eq!(param(&to, "iss").as_deref(), Some(format!("http://{host}").as_str()), "it says who answered");
    param(&to, "code").expect("a code")
}

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

#[test]
fn programs_beside_ferrum_sign_in_through_it() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    {
        let pool = sqlx::PgPool::connect(&url()).await.expect("connect");
        common::reset(&pool).await;
    }
    let _ = EXTRA_APPS.set(stand_in);
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut config = Environment::Test.load_from_folder(&dir.join("config")).expect("config/test.yaml");
    config.database.uri = url();
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
    let forge = FORGE.get().expect("the stand-in registered");

    let new = |slug: &str| NewTenant {
        slug: slug.into(),
        name: slug.into(),
        admin_email: format!("admin@{slug}.test"),
        admin_password: PASSWORD.into(),
        admin_name: None,
        packages: vec![],
        settings: None,
    };
    let acme = k.create_tenant(new("acme")).await.unwrap();
    let beta = k.create_tenant(new("beta")).await.unwrap();
    k.add_operator_host("acme", ACME, "shell", None).await.unwrap();
    k.add_operator_host("acme", SALES, "shell", None).await.unwrap();
    k.add_operator_host("beta", BETA, "shell", None).await.unwrap();
    let client = forge.companion(&k, acme, "forge", &[BACK.into()]).await.unwrap();
    let secret = String::from_utf8(forge.reveal(&k, Scope::Workspace(acme), &client.secret).await.unwrap().to_vec()).unwrap();
    let beta_client = forge.companion(&k, beta, "forge", &[BACK.into()]).await.unwrap();
    let beta_secret = String::from_utf8(forge.reveal(&k, Scope::Workspace(beta), &beta_client.secret).await.unwrap().to_vec()).unwrap();

    // ── discovery and keys: on the workspace's own addresses, each its own issuer; nowhere else ──
    let d = call(&r, Method::GET, ACME, "/.well-known/openid-configuration", &[], None).await;
    assert_eq!(d.status, StatusCode::OK, "{}", d.body);
    let d = d.json();
    assert_eq!(d["issuer"], json!("http://acme.test"));
    assert_eq!(d["token_endpoint"], json!("http://acme.test/oauth/token"));
    assert_eq!(d["id_token_signing_alg_values_supported"], json!(["EdDSA"]));
    assert_eq!(call(&r, Method::GET, SALES, "/.well-known/openid-configuration", &[], None).await.json()["issuer"], json!("http://sales.acme.test"));
    let gz = call(&r, Method::GET, ACME, "/.well-known/openid-configuration", &[("accept-encoding", "gzip")], None).await;
    assert!(gz.status == StatusCode::OK && gz.headers.get(header::CONTENT_ENCODING).is_some_and(|v| v == "gzip"), "as a browser asks: {:?}", gz.headers);
    for host in ["localhost", "unknown.test"] {
        assert_eq!(
            call(&r, Method::GET, host, "/.well-known/openid-configuration", &[], None).await.status,
            StatusCode::NOT_FOUND,
            "{host}: no workspace's own"
        );
        assert_eq!(call(&r, Method::GET, host, "/oauth/jwks", &[], None).await.status, StatusCode::NOT_FOUND);
    }
    let keys = call(&r, Method::GET, ACME, "/oauth/jwks", &[("accept-encoding", "gzip")], None).await;
    assert_eq!(keys.status, StatusCode::OK);
    let keys = call(&r, Method::GET, ACME, "/oauth/jwks", &[], None).await.json();
    assert_eq!(keys["keys"].as_array().map(Vec::len), Some(1));
    assert!(keys["keys"][0].get("d").is_none() && keys["keys"][0]["crv"] == json!("Ed25519"));

    // ── a member signs in with no consent page; the companion takes its tokens with its secret ──
    let admin = cookie(&r, ACME, "acme", "admin@acme.test").await;
    let c = code(&r, ACME, &admin, &client.client_id, "&nonce=n-1").await;
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{}:{secret}", client.client_id));
    let form = format!("grant_type=authorization_code&code={c}&redirect_uri={}&code_verifier={VERIFIER}", urlencode(BACK));
    let gzip = [("authorization", format!("Basic {basic}")), ("content-type", "application/x-www-form-urlencoded".into()), ("accept-encoding", "gzip".into())];
    let gzip: Vec<(&str, &str)> = gzip.iter().map(|(h, v)| (*h, v.as_str())).collect();
    let t = call(&r, Method::POST, ACME, "/oauth/token", &gzip, Some(form)).await;
    assert!(
        t.status == StatusCode::OK && t.headers.get(header::CONTENT_ENCODING).is_some_and(|v| v == "gzip"),
        "as a browser asks: {} {:?}",
        t.status,
        t.headers
    );
    assert_eq!(t.headers.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()), Some("no-store"));
    let c = code(&r, ACME, &admin, &client.client_id, "").await;
    let form = format!(
        "grant_type=authorization_code&code={c}&redirect_uri={}&code_verifier={VERIFIER}&client_id={}&client_secret={secret}",
        urlencode(BACK),
        client.client_id
    );
    let t = call(&r, Method::POST, ACME, "/oauth/token", &[("content-type", "application/x-www-form-urlencoded")], Some(form)).await;
    assert_eq!(t.status, StatusCode::OK, "the secret in the form: {}", t.body);
    let tokens = t.json();
    assert_eq!((tokens["token_type"].as_str(), tokens["expires_in"].as_i64(), tokens.get("refresh_token")), (Some("Bearer"), Some(300), None));
    let access = tokens["access_token"].as_str().unwrap().to_string();
    let info = call(&r, Method::GET, ACME, "/oauth/userinfo", &[("authorization", &format!("Bearer {access}")), ("accept-encoding", "gzip")], None).await;
    assert_eq!(info.status, StatusCode::OK);
    let info = call(&r, Method::GET, ACME, "/oauth/userinfo", &[("authorization", &format!("Bearer {access}"))], None).await.json();
    assert!(info["sub"].is_string() && info["email"] == json!("admin@acme.test") && info["preferred_username"].is_string(), "{info}");
    let api = call(&r, Method::GET, ACME, "/api/auth/me", &[("authorization", &format!("Bearer {access}"))], None).await;
    assert_eq!(api.status, StatusCode::UNAUTHORIZED, "the access token reaches the user-info only");
    let elsewhere = call(&r, Method::GET, SALES, "/oauth/userinfo", &[("authorization", &format!("Bearer {access}"))], None).await;
    assert_eq!(elsewhere.status, StatusCode::UNAUTHORIZED, "nor at another issuer");
    assert!(elsewhere.headers.get(header::WWW_AUTHENTICATE).is_some_and(|v| v.to_str().unwrap().contains("invalid_token")));

    // ── refused before the return address is known to be the client's: a page, nothing sent anywhere ──
    let page = |path: String, host: &'static str| {
        let (r, admin) = (r.clone(), admin.clone());
        async move { call(&r, Method::GET, host, &path, &[("cookie", &admin)], None).await }
    };
    for (what, res) in [
        ("an unknown client", page(authorize_path("fo_nobody00000000000000000", ""), ACME).await),
        ("another workspace's client at this one's address", page(authorize_path(&beta_client.client_id, ""), ACME).await),
        ("a return address one character off", page(authorize_path(&client.client_id, "").replace("callback", "callbacK"), ACME).await),
        ("a return address one character longer", page(authorize_path(&client.client_id, "").replace("callback", "callbackx"), ACME).await),
    ] {
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{what}: {}", res.body);
        assert!(res.headers.get(header::LOCATION).is_none() && res.body.contains("<html"), "{what}: a page");
        assert!(!res.body.contains(BACK), "{what}: nothing about where it would have gone");
    }
    // ── after: the specification's errors at the client's own address ──
    let told = |res: &Answer| param(&res.location(), "error").unwrap_or_default();
    let reused = page(authorize_path(&client.client_id, "&nonce=n-1"), ACME).await;
    assert_eq!(told(&reused), "invalid_request", "a nonce used again");
    assert_eq!(param(&reused.location(), "state").as_deref(), Some("s1"));
    let nobody = call(&r, Method::GET, ACME, &authorize_path(&client.client_id, "&prompt=none"), &[], None).await;
    assert_eq!(told(&nobody), "login_required", "prompt=none with nobody signed in");
    let to_sign_in = call(&r, Method::GET, ACME, &authorize_path(&client.client_id, ""), &[], None).await;
    assert!(to_sign_in.location().starts_with("/login?next=%2Foauth%2Fauthorize"), "anyone else signs in first: {}", to_sign_in.location());
    assert_eq!(
        told(&page(authorize_path(&client.client_id, "").replace("code_challenge_method=S256", "code_challenge_method=plain"), ACME).await),
        "invalid_request"
    );
    let admin_ctx = k.load_ctx(acme, k.login("acme", "admin@acme.test", PASSWORD).await.unwrap().user, None, "it".into()).await.unwrap();
    k.create_user(&admin_ctx, "plain@acme.test", "Plain", Some(PASSWORD), &[], false).await.unwrap();
    let plain = cookie(&r, ACME, "acme", "plain@acme.test").await;
    let denied = call(&r, Method::GET, ACME, &authorize_path(&client.client_id, ""), &[("cookie", &plain)], None).await;
    assert_eq!(told(&denied), "access_denied", "a member the app does not let in");

    // ── the token endpoint: the same refusal whatever was wrong, a mixed-up issuer and another client included ──
    let c = code(&r, ACME, &admin, &client.client_id, "").await;
    let wrong = redeem(&r, ACME, &client.client_id, "not-the-secret", &c).await;
    assert_eq!((wrong.status, wrong.json()["error"].clone()), (StatusCode::UNAUTHORIZED, json!("invalid_client")));
    assert!(wrong.headers.get(header::WWW_AUTHENTICATE).is_some());
    let unknown = redeem(&r, ACME, "fo_nobody00000000000000000", &secret, &c).await;
    assert_eq!((unknown.status, unknown.json()), (wrong.status, wrong.json()), "nobody's client: the same answer");
    let mixed = redeem(&r, SALES, &client.client_id, &secret, &c).await;
    assert_eq!(mixed.json()["error"], json!("invalid_grant"), "a code from one issuer redeemed at another");
    let other = redeem(&r, BETA, &beta_client.client_id, &beta_secret, &c).await;
    assert_eq!(other.json()["error"], json!("invalid_grant"), "redeemed by another workspace's client");
    let ok = redeem(&r, ACME, &client.client_id, &secret, &c).await;
    assert_eq!(ok.status, StatusCode::OK, "none of those used it up: {}", ok.body);
    let replay = redeem(&r, ACME, &client.client_id, &secret, &c).await;
    assert_eq!((replay.status, replay.json()["error"].clone()), (StatusCode::BAD_REQUEST, json!("invalid_grant")), "a code used twice");
    let refresh = call(
        &r,
        Method::POST,
        ACME,
        "/oauth/token",
        &[
            ("authorization", &format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{}:{secret}", client.client_id)))),
            ("content-type", "application/x-www-form-urlencoded"),
        ],
        Some("grant_type=refresh_token&refresh_token=fr_x".into()),
    )
    .await;
    assert_eq!(refresh.json()["error"], json!("unsupported_grant_type"), "a companion has no refresh token");

    // ── prompt=login: a page asks the person to confirm it is them, through the step-up, then on to the client ──
    let res = call(&r, Method::GET, ACME, &authorize_path(&client.client_id, "&prompt=login"), &[("cookie", &admin)], None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body.contains("companion.js") && res.body.contains("runtime.js"), "the step-up's dialog comes with the shell's runtime");
    assert!(res.headers.get("content-security-policy").is_some_and(|v| v.to_str().unwrap().contains("frame-ancestors 'none'")));
    let waiting = res.body.split("\"waiting\":\"").nth(1).and_then(|s| s.split('"').next()).expect("the request it waits on").to_string();
    let same = [("cookie", admin.as_str()), ("sec-fetch-site", "same-origin"), ("content-type", "application/json")];
    let ask = json!({"waiting": waiting}).to_string();
    let first = call(&r, Method::POST, ACME, "/api/oauth/authorize/confirm", &same, Some(ask.clone())).await;
    assert_eq!((first.status, first.json()["error"].clone()), (StatusCode::FORBIDDEN, json!("step_up")), "{}", first.body);
    assert!(first.json()["ways"].as_array().is_some_and(|w| w.iter().any(|w| w["way"] == json!("password"))), "with the person's ways: {}", first.body);
    let stepped = call(&r, Method::POST, ACME, "/api/auth/step-up", &same, Some(json!({"password": PASSWORD}).to_string())).await;
    assert_eq!(stepped.status, StatusCode::NO_CONTENT, "{}", stepped.body);
    let done = call(&r, Method::POST, ACME, "/api/oauth/authorize/confirm", &same, Some(ask)).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    let to = done.json()["redirect_to"].as_str().unwrap().to_string();
    assert!(to.starts_with(BACK) && param(&to, "code").is_some() && param(&to, "state").as_deref() == Some("s1"));

    // ── signing out ends what the companion holds ──
    let c = code(&r, ACME, &admin, &client.client_id, "").await;
    let held = redeem(&r, ACME, &client.client_id, &secret, &c).await.json()["access_token"].as_str().unwrap().to_string();
    let out = call(&r, Method::POST, ACME, "/api/auth/logout", &[("cookie", &admin), ("sec-fetch-site", "same-origin")], None).await;
    assert!(out.status.is_success(), "{} {}", out.status, out.body);
    assert_eq!(call(&r, Method::GET, ACME, "/oauth/userinfo", &[("authorization", &format!("Bearer {held}"))], None).await.status, StatusCode::UNAUTHORIZED);
    assert!(k.openid_sweep().await.unwrap() >= 1, "and its app is told");

    // ── failures at the token endpoint are counted like failed sign-ins ──
    let c = code(&r, ACME, &cookie(&r, ACME, "acme", "admin@acme.test").await, &client.client_id, "").await;
    let mut last = StatusCode::OK;
    for _ in 0..25 {
        last = redeem(&r, ACME, &client.client_id, "guess", &c).await.status;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS, "a flood of guesses is cut off");
}
