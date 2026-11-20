//! Who may change how a person signs in, and where a narrowed key works (docs/builder/decisions.md BD-85), against a real
//! PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset): the whole server, booted as the binary boots it.
//!
//! Every change to how a person signs in (two-factor sign-in, recovery codes, passkeys, the password, signing devices
//! out, and an administrator's changes to someone's) asks for a person at a screen: the browser's session cookie, or
//! the web app's page token bound to it. A key, a bare or OAuth token, a job, the system and the assistant cannot,
//! whoever's they are. The setup routes take no key; the session that may only set up a first second factor sets it
//! up and does nothing else. A key narrowed to apps or operations is refused by every consumer of credentials but the
//! capability door; a whole key works where it did.

mod common;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use base64::Engine as _;
use ferrum_kernel::ctx::Egress;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Error, Kernel};
use ferrum_server::app::App;
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use serde_json::{Value, json};
use serial_test::serial;
use sha2::Digest;
use std::sync::{Arc, LazyLock};
use tokio::sync::OnceCell;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "Correct-Horse-9";
const NEW_PASSWORD: &str = "Battery-Staple-77";

// ───────────── one server for the suite, on one runtime ─────────────

struct Server {
    router: axum::Router,
    st: AppState,
}

static RUNTIME: LazyLock<tokio::runtime::Runtime> =
    LazyLock::new(|| tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().expect("a runtime"));

static SERVER: OnceCell<Server> = OnceCell::const_new();

fn run(scenario: impl std::future::Future<Output = ()> + Send + 'static) {
    if let Err(e) = RUNTIME.block_on(RUNTIME.spawn(scenario)) {
        std::panic::resume_unwind(e.into_panic());
    }
}

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
            Server { router: App::after_routes(router, &ctx).await.expect("layers"), st }
        })
        .await
}

// ───────────── requests ─────────────

#[derive(Clone, Copy)]
enum Cred<'a> {
    Bearer(&'a str),
    Cookie(&'a str),
}

/// One request to `host` from `ip` (as the gateway reports it), carrying `cred`.
async fn call(host: &str, ip: &str, method: Method, path: &str, cred: Cred<'_>, body: Option<Value>) -> (StatusCode, String) {
    let s = server().await;
    let mut req = Request::builder().method(method).uri(path).header(header::HOST, host).header("x-forwarded-for", ip);
    req = match cred {
        Cred::Bearer(t) => req.header(header::AUTHORIZATION, format!("Bearer {t}")),
        Cred::Cookie(t) => req
            .header(header::COOKIE, format!("{}={t}", ferrum_server::shell::cookie_name(false)))
            .header("sec-fetch-site", "same-origin")
            .header(header::ORIGIN, format!("http://{host}")),
    };
    if path == "/mcp" {
        req = req.header("mcp-protocol-version", "2025-11-25");
    }
    let req = match body {
        Some(b) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let res = s.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// A password sign-in at `host` from `ip`: the answer, and the cookie's token when signed in from a page.
async fn sign_in(host: &str, ip: &str, slug: &str, email: &str, password: &str, page: bool) -> (StatusCode, Value, Option<String>) {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri("/api/auth/login")
        .header(header::HOST, host)
        .header("x-forwarded-for", ip)
        .header(header::USER_AGENT, "Mozilla/5.0 (X11; Linux x86_64) Firefox/131.0")
        .header(header::CONTENT_TYPE, "application/json");
    if page {
        req = req.header("sec-fetch-site", "same-origin");
    }
    let req = req.body(Body::from(json!({"tenant": slug, "email": email, "password": password}).to_string())).unwrap();
    let res = server().await.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let cookie = res.headers().get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).find_map(|c| {
        c.strip_prefix(&format!("{}=", ferrum_server::shell::cookie_name(false))).map(|rest| rest.split(';').next().unwrap_or_default().to_string())
    });
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null), cookie)
}

/// A POST from a page of `host` (`sec-fetch-site: same-origin`), with the session cookie when given: the answer, and the
/// session cookie it sets.
async fn post_page(host: &str, ip: &str, path: &str, cookie: Option<&str>, body: Value) -> (StatusCode, Value, Option<String>) {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::HOST, host)
        .header("x-forwarded-for", ip)
        .header(header::USER_AGENT, "Mozilla/5.0 (X11; Linux x86_64) Firefox/131.0")
        .header(header::CONTENT_TYPE, "application/json")
        .header("sec-fetch-site", "same-origin")
        .header(header::ORIGIN, format!("http://{host}"));
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, format!("{}={c}", ferrum_server::shell::cookie_name(false)));
    }
    let res = server().await.router.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = res.status();
    let set = res.headers().get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).find_map(|c| {
        c.strip_prefix(&format!("{}=", ferrum_server::shell::cookie_name(false))).map(|rest| rest.split(';').next().unwrap_or_default().to_string())
    });
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null), set.filter(|c| !c.is_empty()))
}

fn refused(status: StatusCode) -> bool {
    status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

// ───────────── a workspace and what its people hold ─────────────

/// What one person holds: the browser's cookie and the page token bound to it (screens), a bare token from an API
/// sign-in, an OAuth token, a whole API key and one narrowed to an app by SQL (nothing makes one before API-3b).
struct Holds {
    id: Uuid,
    email: String,
    cookie: String,
    page: String,
    token: String,
    oauth: String,
    key: String,
    narrowed: String,
}

impl Holds {
    /// Each credential that is not a person at a screen.
    fn programs(&self) -> [(&'static str, Cred<'_>); 4] {
        [
            ("a whole API key", Cred::Bearer(&self.key)),
            ("a narrowed key", Cred::Bearer(&self.narrowed)),
            ("a bare token", Cred::Bearer(&self.token)),
            ("an OAuth token", Cred::Bearer(&self.oauth)),
        ]
    }
}

struct World {
    k: Arc<Kernel>,
    slug: String,
    host: String,
    tenant: Uuid,
    admin_ctx: Ctx,
    admin: Holds,
    pat: Holds,
    sam: Holds,
}

/// A workspace `slug` on host `{slug}.test` with its administrator, Pat and Sam (members), each holding every
/// credential. `n` keeps each suite's addresses apart.
async fn world(slug: &str, n: u8) -> World {
    let k = server().await.st.kernel.clone();
    let tenant = k
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
        .expect("workspace");
    let host = format!("{slug}.test");
    k.add_operator_host(slug, &host, "shell", None).await.unwrap();
    let s = k.login(slug, &format!("admin@{slug}.test"), PASSWORD).await.unwrap();
    let admin_ctx = k.load_ctx(tenant, s.user, Some(s.token_version), "it".into()).await.unwrap();
    let mut held = vec![];
    for (i, who) in ["admin", "pat", "sam"].into_iter().enumerate() {
        let email = format!("{who}@{slug}.test");
        let id = match who {
            "admin" => s.user,
            _ => k.create_user(&admin_ctx, &email, who, Some(PASSWORD), &[], false).await.unwrap(),
        };
        held.push(holds(&k, &admin_ctx, slug, &host, &format!("10.{n}.{i}.1"), id, &email).await);
    }
    let (sam, pat, admin) = (held.pop().unwrap(), held.pop().unwrap(), held.pop().unwrap());
    World { k, slug: slug.into(), host, tenant, admin_ctx, admin, pat, sam }
}

async fn holds(k: &Kernel, admin: &Ctx, slug: &str, host: &str, ip: &str, id: Uuid, email: &str) -> Holds {
    let (status, body, cookie) = sign_in(host, ip, slug, email, PASSWORD, true).await;
    assert_eq!(status, StatusCode::OK, "{email} signs in from a page: {body}");
    let (cookie, page) = (cookie.expect("a cookie"), body["token"].as_str().expect("a page token").to_string());
    let (status, body, _) = sign_in(host, ip, slug, email, PASSWORD, false).await;
    assert_eq!(status, StatusCode::OK, "{email} signs in from a program: {body}");
    let token = body["token"].as_str().expect("a bearer token").to_string();
    let key = k.create_api_key(admin, id, "whole", None, None).await.unwrap().1;
    let narrowed = k.create_api_key(admin, id, "narrowed", None, None).await.unwrap().1;
    let prefix = narrowed.split('_').nth(1).unwrap().to_string();
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::query("update api_key set apps = array['crm'] where prefix = $1").bind(&prefix).execute(&mut *tx).await.expect("a key narrowed to CRM");
    tx.commit().await.unwrap();
    let me = k.load_ctx(admin.tenant, id, None, "it".into()).await.unwrap();
    let client = k.oauth_register(Some("it".into()), vec!["http://127.0.0.1:9/cb".into()]).await.unwrap();
    let verifier = "v".repeat(50);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    let code = k.oauth_issue_code(&me, &client.id, "http://127.0.0.1:9/cb", &challenge, "S256").await.unwrap();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/oauth/token")
        .header(header::HOST, host)
        .header("x-forwarded-for", ip)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!(
            "grant_type=authorization_code&code={code}&redirect_uri=http%3A%2F%2F127.0.0.1%3A9%2Fcb&code_verifier={verifier}&client_id={}",
            client.id
        )))
        .unwrap();
    let res = server().await.router.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let oauth = serde_json::from_slice::<Value>(&bytes).unwrap()["access_token"].as_str().expect("an OAuth token").to_string();
    Holds { id, email: email.into(), cookie, page, token, oauth, key, narrowed }
}

/// Whether a person's two-factor secret was made, and whether it is on.
async fn two_factor(k: &Kernel, tenant: Uuid, user: Uuid) -> (bool, bool) {
    let mut tx = k.db.system_tx().await.unwrap();
    let row = sqlx::query_as("select totp_set, totp_enabled_at is not null from app_user where tenant_id = $1 and id = $2")
        .bind(tenant)
        .bind(user)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    row
}

/// A socket's handshake at `host` with `token` as the bearer, and the `{"auth": token}` a socket may send first instead
/// (live updates and co-editing alike).
async fn socket(host: &str, ip: &str, token: &str) -> (bool, bool) {
    let st = &server().await.st;
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, host.parse().unwrap());
    headers.insert("x-forwarded-for", ip.parse().unwrap());
    headers.insert(header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
    let entry = ferrum_server::extract::socket_entry(st, &headers).await;
    let shaken = ferrum_server::extract::socket_credential(st, &headers, &entry).await;
    let first = ferrum_server::extract::ctx_from_token(st, token, "it".into(), &entry, false).await;
    (matches!(shaken, Ok(Some(_))), first.is_ok())
}

// ───────────── the tests ─────────────

#[test]
#[serial]
fn a_key_cannot_set_up_two_factor_or_open_a_socket_off_its_door() {
    run(async {
        let w = world("keys2fa", 1).await;
        let (host, ip) = (w.host.as_str(), "10.1.9.9");
        let setup: [(Method, &str, Option<Value>); 3] = [
            (Method::GET, "/api/auth/2fa", None),
            (Method::POST, "/api/auth/2fa/begin", None),
            (Method::POST, "/api/auth/2fa/enable", Some(json!({"code": "123456"}))),
        ];
        // Pat has no second factor. No program credential of Pat's reaches the setup routes, a whole key included.
        for (name, cred) in w.pat.programs() {
            for (method, path, body) in setup.clone() {
                let (status, answer) = call(host, ip, method, path, cred, body).await;
                assert!(refused(status), "{name} reaches {path}: {status} {answer}");
            }
        }
        assert_eq!(two_factor(&w.k, w.tenant, w.pat.id).await, (false, false), "no secret was made for Pat, nothing turned on");
        // Pat at a screen sets it up: the cookie, and the page token bound to it.
        for (name, cred) in [("the cookie", Cred::Cookie(&w.pat.cookie)), ("the page token", Cred::Bearer(&w.pat.page))] {
            let (status, answer) = call(host, ip, Method::GET, "/api/auth/2fa", cred, None).await;
            assert_eq!(status, StatusCode::OK, "{name}: {answer}");
        }
        let (status, answer) = call(host, ip, Method::POST, "/api/auth/2fa/begin", Cred::Cookie(&w.pat.cookie), None).await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        let secret = serde_json::from_str::<Value>(&answer).unwrap()["secret"].as_str().unwrap().to_string();
        let code = ferrum_kernel::twofactor::code(&secret, now()).unwrap();
        let (status, answer) = call(host, ip, Method::POST, "/api/auth/2fa/enable", Cred::Bearer(&w.pat.page), Some(json!({"code": code}))).await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(serde_json::from_str::<Value>(&answer).unwrap()["recovery_codes"].as_array().map(Vec::len), Some(10));

        // Sockets: a whole key opens one, as a machine's, as before; a narrowed key opens none, by either way in.
        assert_eq!(socket(host, ip, &w.pat.key).await, (true, true), "a whole key opens a socket");
        assert_eq!(socket(host, ip, &w.pat.page).await, (true, true), "a page's token opens a socket");
        assert_eq!(socket(host, ip, &w.pat.narrowed).await, (false, false), "a narrowed key opens no socket and no co-editing");
    });
}

#[test]
#[serial]
fn every_sign_in_change_asks_a_person_at_a_screen() {
    run(async {
        let w = world("screens", 2).await;
        let (host, ip) = (w.host.as_str(), "10.2.9.9");
        let (pat, sam, nobody) = (w.pat.id, w.sam.id, Uuid::now_v7());
        // Pat turns two-factor sign-in on at a screen, so that turning it off and new recovery codes can be tried.
        let (_, begun) = call(host, ip, Method::POST, "/api/auth/2fa/begin", Cred::Cookie(&w.pat.cookie), None).await;
        let secret = serde_json::from_str::<Value>(&begun).unwrap()["secret"].as_str().map(str::to_string);
        let secret = secret.expect("Pat at a screen begins two-factor sign-in");
        let enabled_at = now();
        let (status, _) = call(
            host,
            ip,
            Method::POST,
            "/api/auth/2fa/enable",
            Cred::Cookie(&w.pat.cookie),
            Some(json!({"code": ferrum_kernel::twofactor::code(&secret, enabled_at).unwrap()})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let pat_changes = |code: String| -> Vec<(Method, String, Option<Value>)> {
            vec![
                (Method::POST, "/api/auth/2fa/begin".into(), None),
                (Method::POST, "/api/auth/2fa/enable".into(), Some(json!({"code": code}))),
                (Method::POST, "/api/auth/2fa/recovery-codes".into(), Some(json!({"code": code}))),
                (Method::POST, "/api/auth/2fa/disable".into(), Some(json!({"password": PASSWORD}))),
                (Method::POST, "/api/auth/password".into(), Some(json!({"current_password": PASSWORD, "new_password": NEW_PASSWORD}))),
                (Method::POST, "/api/auth/passkeys/options".into(), None),
                (Method::POST, "/api/auth/passkeys".into(), Some(json!({"request": nobody, "credential": {}, "name": "x"}))),
                (Method::PATCH, format!("/api/auth/passkeys/{nobody}"), Some(json!({"name": "x"}))),
                (Method::DELETE, format!("/api/auth/passkeys/{nobody}"), None),
                (Method::DELETE, format!("/api/auth/sessions/{nobody}"), None),
                (Method::POST, "/api/auth/logout_all".into(), None),
            ]
        };
        let admin_changes: Vec<(Method, String, Option<Value>)> = vec![
            (Method::PUT, format!("/api/admin/users/{sam}/password"), Some(json!({"password": NEW_PASSWORD}))),
            (Method::POST, format!("/api/admin/users/{sam}/two-factor/reset"), None),
            (Method::POST, format!("/api/admin/users/{sam}/sso/unlink"), None),
            (Method::DELETE, format!("/api/auth/sessions/{nobody}"), None),
        ];

        // ── no program credential changes how anyone signs in: Pat's own, or the administrator's for Sam ──
        let code = ferrum_kernel::twofactor::code(&secret, now() + 30).unwrap();
        for (name, cred) in w.pat.programs() {
            for (method, path, body) in pat_changes(code.clone()) {
                let (status, answer) = call(host, ip, method, &path, cred, body).await;
                assert!(refused(status), "Pat's {name}: {path}: {status} {answer}");
            }
        }
        for (name, cred) in w.admin.programs() {
            for (method, path, body) in admin_changes.clone() {
                let (status, answer) = call(host, ip, method, &path, cred, body).await;
                assert!(refused(status), "the administrator's {name}: {path}: {status} {answer}");
            }
        }
        // Nothing changed: Pat's factor is on, both still sign in with their passwords, and their screens still work.
        assert_eq!(two_factor(&w.k, w.tenant, pat).await, (true, true), "Pat's second factor is as Pat left it");
        let (status, answer, _) = sign_in(host, "10.2.9.10", &w.slug, &w.sam.email, PASSWORD, false).await;
        assert_eq!(status, StatusCode::OK, "Sam's password is unchanged: {answer}");
        for (who, cookie) in [("Pat", &w.pat.cookie), ("Sam", &w.sam.cookie)] {
            assert_eq!(call(host, ip, Method::GET, "/api/auth/me", Cred::Cookie(cookie), None).await.0, StatusCode::OK, "{who} is still signed in");
        }

        // ── nor does a job, the system, the assistant or an automation's context, in the kernel ──
        let job = w.k.load_ctx(w.tenant, pat, None, "job".into()).await.unwrap();
        let assistant = job.for_provider(Egress::default());
        let system = Ctx::system(w.tenant, job.schema.clone());
        for (name, ctx) in [("a job", &job), ("the assistant", &assistant), ("the system", &system), ("the administrator's job", &w.admin_ctx)] {
            let forbidden = |r: Result<(), Error>, what: &str| assert!(matches!(r, Err(Error::Forbidden(_))), "{name}: {what}: {r:?}");
            forbidden(w.k.begin_two_factor(ctx).await.map(|_| ()), "begin");
            forbidden(w.k.enable_two_factor(ctx, &code).await.map(|_| ()), "enable");
            forbidden(w.k.new_recovery_codes(ctx, &code).await.map(|_| ()), "recovery codes");
            forbidden(w.k.disable_two_factor(ctx, Some(PASSWORD), None).await, "disable");
            forbidden(w.k.set_password(ctx, ctx.user, Some(PASSWORD), NEW_PASSWORD).await, "password");
            forbidden(w.k.passkey_options(ctx, host, "it").await.map(|_| ()), "passkey options");
            forbidden(w.k.change_passkey(ctx, nobody, None).await, "remove a passkey");
            forbidden(w.k.revoke_session(ctx, nobody).await, "sign a device out");
            forbidden(w.k.logout_everywhere(ctx).await, "sign out everywhere");
        }
        for (name, ctx) in [("the system", &system), ("the administrator's job", &w.admin_ctx)] {
            let forbidden = |r: Result<(), Error>, what: &str| assert!(matches!(r, Err(Error::Forbidden(_))), "{name}: {what}: {r:?}");
            forbidden(w.k.set_password(ctx, sam, None, NEW_PASSWORD).await, "Sam's password");
            forbidden(w.k.reset_two_factor(ctx, sam).await, "Sam's second factor");
            forbidden(w.k.unlink_sso(ctx, sam).await, "Sam's single sign-on");
        }
        assert_eq!(two_factor(&w.k, w.tenant, pat).await, (true, true));

        // ── a person at a screen changes each: Pat's own with the cookie, the administrator's for Sam ──
        let at_screen = |r: (StatusCode, String), what: &str| assert!(!refused(r.0), "{what}: {} {}", r.0, r.1);
        let pat_cookie = Cred::Cookie(&w.pat.cookie);
        // A step after the one that turned it on (each is taken once), within one step of now.
        let code = ferrum_kernel::twofactor::code(&secret, now().max(enabled_at + 30)).unwrap();
        let (status, answer) = call(host, ip, Method::POST, "/api/auth/2fa/recovery-codes", pat_cookie, Some(json!({"code": code}))).await;
        assert_eq!(status, StatusCode::OK, "new recovery codes at a screen: {answer}");
        let (status, answer) = call(host, ip, Method::POST, "/api/auth/2fa/disable", pat_cookie, Some(json!({"password": PASSWORD}))).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "two-factor sign-in off at a screen: {answer}");
        assert_eq!(two_factor(&w.k, w.tenant, pat).await, (false, false));
        let (status, answer) = call(host, ip, Method::POST, "/api/auth/passkeys/options", pat_cookie, None).await;
        assert_eq!(status, StatusCode::OK, "a passkey's options at a screen: {answer}");
        // Past the check, these name what is missing.
        at_screen(call(host, ip, Method::PATCH, &format!("/api/auth/passkeys/{nobody}"), pat_cookie, Some(json!({"name": "x"}))).await, "rename");
        at_screen(call(host, ip, Method::DELETE, &format!("/api/auth/passkeys/{nobody}"), pat_cookie, None).await, "remove a passkey");
        at_screen(call(host, ip, Method::DELETE, &format!("/api/auth/sessions/{nobody}"), pat_cookie, None).await, "sign a device out");
        let (status, answer) = call(
            host,
            ip,
            Method::POST,
            "/api/auth/password",
            Cred::Bearer(&w.pat.page),
            Some(json!({"current_password": PASSWORD, "new_password": NEW_PASSWORD})),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "a new password from the page's token: {answer}");
        let (_, _, again) = sign_in(host, "10.2.9.11", &w.slug, &w.pat.email, NEW_PASSWORD, true).await;
        let again = again.expect("Pat signs in with the new password");
        assert_eq!(call(host, ip, Method::POST, "/api/auth/logout_all", Cred::Cookie(&again), None).await.0, StatusCode::NO_CONTENT, "signing out everywhere");
        let admin_cookie = Cred::Cookie(&w.admin.cookie);
        for (method, path, body) in &admin_changes[..3] {
            let (status, answer) = call(host, ip, method.clone(), path, admin_cookie, body.clone()).await;
            assert_eq!(status, StatusCode::NO_CONTENT, "the administrator at a screen: {path}: {answer}");
        }

        // ── the session that may only set up a first second factor sets it up, and does nothing else (API-K2: the
        // page's cookie, a_setup_only_session_is_the_sign_in_pages_cookie) ──
        let w = world("setup2fa", 3).await;
        w.k.update_tenant_settings(&w.admin_ctx, json!({"require_two_factor": true}).as_object().unwrap()).await.unwrap();
        let (status, body, setup) = sign_in(&w.host, "10.3.9.9", &w.slug, &w.sam.email, PASSWORD, true).await;
        assert!(status == StatusCode::OK && body["two_factor_setup"] == json!(true), "{body}");
        let setup = setup.expect("the setup session is the page's cookie");
        let (status, answer) = call(
            &w.host,
            "10.3.9.9",
            Method::POST,
            "/api/auth/password",
            Cred::Cookie(&setup),
            Some(json!({"current_password": PASSWORD, "new_password": NEW_PASSWORD})),
        )
        .await;
        assert!(refused(status), "a setup-only session changes no password: {status} {answer}");
    });
}

/// API-K2 (A11 T7): the session that may only set up a first second factor is the sign-in page's cookie: no bearer, no
/// API client; it begins and finishes the setup and reaches nothing else; finishing gives a full session cookie.
#[test]
fn a_setup_only_session_is_the_sign_in_pages_cookie() {
    run(async {
        let w = world("setupcookie", 7).await;
        w.k.update_tenant_settings(&w.admin_ctx, json!({"require_two_factor": true}).as_object().unwrap()).await.unwrap();
        let ip = "10.7.9.9";
        let (status, body, cookie) = sign_in(&w.host, ip, &w.slug, &w.sam.email, PASSWORD, false).await;
        assert!(refused(status) && body.get("token").is_none() && cookie.is_none(), "an API client gets no setup session: {status} {body}");
        let (status, body, cookie) = sign_in(&w.host, ip, &w.slug, &w.sam.email, PASSWORD, true).await;
        assert!(status == StatusCode::OK && body["two_factor_setup"] == json!(true) && body.get("token").is_none(), "no bearer: {body}");
        let setup = cookie.expect("the setup session is the page's cookie");
        assert_eq!(call(&w.host, ip, Method::GET, "/api/auth/2fa", Cred::Cookie(&setup), None).await.0, StatusCode::OK, "its own status");
        assert!(refused(call(&w.host, ip, Method::POST, "/api/auth/2fa/begin", Cred::Bearer(&setup), None).await.0), "its token as a bearer");
        assert!(refused(call(&w.host, ip, Method::GET, "/api/auth/sessions", Cred::Cookie(&setup), None).await.0), "nothing but the setup");
        let (status, answer) = call(&w.host, ip, Method::POST, "/api/auth/2fa/begin", Cred::Cookie(&setup), None).await;
        assert_eq!(status, StatusCode::OK, "it begins the setup: {answer}");
        let secret = serde_json::from_str::<Value>(&answer).unwrap()["secret"].as_str().unwrap().to_string();
        let code = ferrum_kernel::twofactor::code(&secret, now()).unwrap();
        let (status, body, full) = post_page(&w.host, ip, "/api/auth/2fa/enable", Some(&setup), json!({"code": code})).await;
        assert!(status == StatusCode::OK && body["recovery_codes"].is_array() && body.get("token").is_none(), "{status} {body}");
        let full = full.expect("a full session cookie in exchange");
        assert_eq!(call(&w.host, ip, Method::GET, "/api/auth/sessions", Cred::Cookie(&full), None).await.0, StatusCode::OK, "the full session");
    });
}

/// API-K2: a code sign-in that must set up a second factor gets the same setup-only cookie, not a full session (the
/// accounts host's part: identity_it).
#[test]
fn code_sign_in_and_the_accounts_host_ask_for_setup_the_same_way() {
    run(async {
        let w = world("setupcode", 8).await;
        w.k.update_tenant_settings(&w.admin_ctx, json!({"require_two_factor": true}).as_object().unwrap()).await.unwrap();
        let ann = w.k.create_user(&w.admin_ctx, "ann@setupcode.test", "Ann", None, &[], false).await.unwrap();
        w.k.set_user_kind(&w.admin_ctx, ann, "portal").await.unwrap();
        let ip = "10.8.9.9";
        // This test server mails nothing: the code is kept as `send_sign_in_code` keeps one (salted with its row's id).
        let (id, code) = (uuid::Uuid::new_v4(), "482913");
        let hash = {
            use sha2::Digest;
            hex::encode(sha2::Sha256::digest(format!("{id}:{code}").as_bytes()))
        };
        let mut tx = w.k.db.system_tx().await.unwrap();
        sqlx::query("insert into sign_in_code (id, tenant_id, user_id, code_hash, expires_at) values ($1, $2, $3, $4, now() + interval '10 minutes')")
            .bind(id)
            .bind(w.tenant)
            .bind(ann)
            .bind(hash)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let (status, body, cookie) =
            post_page(&w.host, ip, "/api/auth/code/sign-in", None, json!({"tenant": w.slug, "email": "ann@setupcode.test", "code": code, "session": "cookie"}))
                .await;
        assert!(status == StatusCode::OK && body["two_factor_setup"] == json!(true) && body.get("token").is_none(), "{status} {body}");
        let setup = cookie.expect("the setup session cookie");
        assert!(refused(call(&w.host, ip, Method::GET, "/api/auth/sessions", Cred::Cookie(&setup), None).await.0), "a setup session, not a full one");
        assert_eq!(call(&w.host, ip, Method::GET, "/api/auth/2fa", Cred::Cookie(&setup), None).await.0, StatusCode::OK);
    });
}

#[test]
#[serial]
fn a_narrowed_key_is_refused_by_every_consumer_but_the_capability_door() {
    run(async {
        let w = world("narrowed", 4).await;
        let (host, ip) = (w.host.as_str(), "10.4.9.9");
        // Every family of routes a credential is taken on (crate::extract): records, metadata, apps' endpoints,
        // reports, search, files, MCP, administration, notifications, the devices page, who-am-I, the setup routes,
        // the assistant, work, jobs and the API's document.
        let routes: Vec<(Method, &str, Option<Value>)> = vec![
            (Method::GET, "/api/v1/Company", None),
            (Method::GET, "/api/meta/Company", None),
            (Method::GET, "/api/app/core/anything", None),
            (Method::GET, "/api/reports", None),
            (Method::GET, "/api/search?q=acme", None),
            (Method::GET, "/files/0195f5d0-0000-7000-8000-000000000000", None),
            (Method::POST, "/mcp", Some(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))),
            (Method::GET, "/api/admin/users", None),
            (Method::GET, "/api/notifications", None),
            (Method::GET, "/api/auth/sessions", None),
            (Method::GET, "/api/auth/me", None),
            (Method::GET, "/api/auth/2fa", None),
            (Method::GET, "/api/assistant", None),
            (Method::GET, "/api/work", None),
            (Method::GET, "/api/jobs", None),
            (Method::GET, "/api/openapi.json", None),
        ];
        // The administrator's keys, so that what a whole key reaches is reached for want of nothing but the key's kind.
        for (method, path, body) in routes {
            let (status, answer) = call(host, ip, method.clone(), path, Cred::Bearer(&w.admin.narrowed), body.clone()).await;
            assert!(refused(status), "a narrowed key at {path}: {status} {answer}");
            let (whole, answer) = call(host, ip, method, path, Cred::Bearer(&w.admin.key), body).await;
            let expected = match path {
                // The setup routes take no key at all; the others answer a whole key as before.
                "/api/auth/2fa" => refused(whole),
                _ => whole.is_success() || whole == StatusCode::NOT_FOUND,
            };
            assert!(expected, "a whole key at {path}: {whole} {answer}");
        }
        assert_eq!(socket(host, ip, &w.admin.narrowed).await, (false, false), "a narrowed key's socket");
        assert_eq!(socket(host, ip, &w.admin.key).await, (true, true), "a whole key's socket, as before");
        // The kernel's own reading of a key, which knows no door, refuses a narrowed one too.
        assert!(matches!(w.k.verify_api_key(&w.pat.narrowed).await, Err(Error::Forbidden(_))), "the kernel reads no narrowed key as whole");
        assert!(w.k.verify_api_key(&w.pat.key).await.is_ok());
    });
}
