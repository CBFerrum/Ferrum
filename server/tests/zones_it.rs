//! The wall of zones (docs/operations.md, "Zones"), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_test`; the
//! database is reset): the whole server, booted as the binary boots it, behind its own router. Every
//! kind of credential and every family of routes is tried, by a staff account and by outside people
//! and machines, in a zone that admits no staff, placed every way a request can be placed, enforced
//! and reported. Outside people and machines must get exactly what a server without zones gives them.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine as _;
use ferrum_kernel::Kernel;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::zones::{Credential, Zones};
use ferrum_server::app::App;
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use serde_json::{Value, json};
use sha2::Digest;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "Correct-Horse-9";
/// The workspace's staff address: a host in the zone that admits staff.
const STAFF_HOST: &str = "erp.acme.test";
/// A host whose zone admits no staff.
const PUBLIC_HOST: &str = "www.acme.test";
/// A host with no zone of its own: the caller's network or the default decides.
const FREE_HOST: &str = "free.acme.test";

#[test]
fn the_wall() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

/// A server: the booted state with `zones` in place of the configured ones, behind the full router.
struct Server {
    router: axum::Router,
    st: AppState,
}

async fn server(base: &AppContext, st: &AppState, zones: Zones) -> Server {
    let st = AppState { zones: Arc::new(zones), ..st.clone() };
    let ctx = AppContext::builder(Environment::Test, Default::default(), base.config.clone()).build();
    ctx.shared_store.insert(st.clone());
    let router = App::routes(&ctx).to_router::<App>(ctx.clone(), axum::Router::new()).expect("router");
    Server { router: App::after_routes(router, &ctx).await.expect("layers"), st }
}

fn zones(json: &str, pin: &str, default: &str, mode: &str) -> Zones {
    Zones::parse(json, pin, default, mode).expect("zones")
}

#[derive(Clone, Copy)]
enum Cred<'a> {
    None,
    Bearer(&'a str),
    Cookie(&'a str),
    Basic(&'a str, &'a str),
}

/// One request: `host`, from `ip` (as the gateway reports it), carrying `cred`.
async fn call(s: &Server, method: Method, host: &str, path: &str, ip: &str, cred: Cred<'_>, body: Option<Value>) -> (StatusCode, String) {
    let mut req = Request::builder().method(method).uri(path).header(header::HOST, host).header("x-forwarded-for", ip);
    match cred {
        Cred::None => {}
        Cred::Bearer(t) => req = req.header(header::AUTHORIZATION, format!("Bearer {t}")),
        Cred::Cookie(t) => {
            req = req
                .header(header::COOKIE, format!("{}={t}", ferrum_server::shell::cookie_name(false)))
                .header("sec-fetch-site", "same-origin")
                .header(header::ORIGIN, format!("http://{host}"))
        }
        Cred::Basic(u, p) => req = req.header(header::AUTHORIZATION, format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}")))),
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

/// A sign-in on `s`: the bearer token, and the cookie's token when signed in from a page.
async fn sign_in(s: &Server, host: &str, ip: &str, email: &str, password: &str, page: bool) -> (StatusCode, Value, Option<String>) {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri("/api/auth/login")
        .header(header::HOST, host)
        .header("x-forwarded-for", ip)
        .header(header::CONTENT_TYPE, "application/json");
    if page {
        req = req.header("sec-fetch-site", "same-origin");
    }
    let req = req.body(Body::from(json!({"tenant": "acme", "email": email, "password": password}).to_string())).unwrap();
    let res = s.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let cookie = res.headers().get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).find_map(|c| {
        c.strip_prefix(&format!("{}=", ferrum_server::shell::cookie_name(false))).map(|rest| rest.split(';').next().unwrap_or_default().to_string())
    });
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null), cookie)
}

/// What one person holds: a bearer token, a cookie, API keys of each scope, an OAuth token.
#[derive(Default, Clone)]
struct Holds {
    email: String,
    token: Option<String>,
    cookie: Option<String>,
    key: Option<String>,
    dav: Option<String>,
    scim: Option<String>,
    oauth: Option<String>,
}

impl Holds {
    /// Each credential this person holds, with a name for messages.
    fn each(&self) -> Vec<(&'static str, Cred<'_>)> {
        let mut out = vec![];
        if let Some(t) = &self.token {
            out.push(("token", Cred::Bearer(t)));
        }
        if let Some(c) = &self.cookie {
            out.push(("cookie", Cred::Cookie(c)));
        }
        if let Some(k) = &self.key {
            out.push(("api key", Cred::Bearer(k)));
        }
        if let Some(o) = &self.oauth {
            out.push(("oauth", Cred::Bearer(o)));
        }
        out
    }
}

/// The families of routes a credential is tried on: (method, path, body).
fn routes() -> Vec<(Method, &'static str, Option<Value>)> {
    vec![
        (Method::GET, "/api/auth/me", None),
        (Method::GET, "/api/v1/Company", None),
        (Method::GET, "/api/meta/Company", None),
        (Method::GET, "/api/app/core/anything", None),
        (Method::GET, "/api/reports", None),
        (Method::GET, "/api/search?q=acme", None),
        (Method::GET, "/files/0195f5d0-0000-7000-8000-000000000000", None),
        (Method::POST, "/mcp", Some(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))),
        (Method::GET, "/api/admin/users", None),
        (Method::GET, "/api/admin/domains", None),
    ]
}

/// The pages a credential is tried on (the cookie).
const PAGES: &[&str] = &["/", "/admin/", "/data/", "/store/", "/assistant", "/contacts/"];

/// An OAuth access token (and the code that was not yet redeemed) for `ctx`'s person, through the
/// token endpoint of `s`.
async fn oauth_code(k: &Kernel, ctx: &ferrum_kernel::Ctx) -> (String, String, String) {
    let client = k.oauth_register(Some("it".into()), vec!["http://127.0.0.1:9/cb".into()]).await.unwrap();
    let verifier = "v".repeat(50);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    let code = k.oauth_issue_code(ctx, &client.id, "http://127.0.0.1:9/cb", &challenge, "S256").await.unwrap();
    (client.id, code, verifier)
}

async fn redeem(s: &Server, host: &str, ip: &str, (client, code, verifier): &(String, String, String)) -> (StatusCode, Value) {
    token_endpoint(
        s,
        host,
        ip,
        format!("grant_type=authorization_code&code={code}&redirect_uri=http%3A%2F%2F127.0.0.1%3A9%2Fcb&code_verifier={verifier}&client_id={client}"),
    )
    .await
}

async fn refresh(s: &Server, host: &str, ip: &str, client: &str, token: &str) -> (StatusCode, Value) {
    token_endpoint(s, host, ip, format!("grant_type=refresh_token&refresh_token={token}&client_id={client}")).await
}

async fn token_endpoint(s: &Server, host: &str, ip: &str, form: String) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri("/oauth/token")
        .header(header::HOST, host)
        .header("x-forwarded-for", ip)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form))
        .unwrap();
    let res = s.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// A refusal by zone, on the API: 403, `zone`, naming the staff address.
fn refused_by_zone(status: StatusCode, body: &str) -> bool {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    status == StatusCode::FORBIDDEN && v["error"] == "zone" && v["staff_address"] == STAFF_HOST
}

/// The plain page of a staff page asked for where staff are refused.
fn plain_page(status: StatusCode, body: &str) -> bool {
    status == StatusCode::FORBIDDEN && body.contains("Not opened here") && body.contains(STAFF_HOST)
}

async fn failed_logins(k: &Kernel, email: &str) -> (i32, bool) {
    let mut tx = k.db.system_tx().await.unwrap();
    let row = sqlx::query_as("select failed_logins, locked_until is not null from app_user where email = $1").bind(email).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    row
}

/// One value, read as the system.
async fn read<T: Send + Unpin + for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>>(k: &Kernel, sql: &'static str, bind: &str) -> T {
    let mut tx = k.db.system_tx().await.unwrap();
    let v = sqlx::query_scalar(sql).bind(bind).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    v
}

fn counted(zone: &str, mode: &str) -> u64 {
    ferrum_kernel::zones::refusals().iter().filter(|(z, m, _, _)| z == zone && *m == mode).map(|r| r.3).sum()
}

async fn scenario() {
    // The server as the binary boots it, on the test database.
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
        assert_eq!(s["zones"], json!(""), "the test configuration renders FERRUM_ZONES (unset) as an empty string");
    }
    let base = <App as Hooks>::after_context(AppContext::builder(Environment::Test, Default::default(), config).build()).await.expect("boot");
    let st = base.shared_store.get::<AppState>().expect("state");
    assert!(st.zones.is_off(), "no zone settings: no zones");
    let k = st.kernel.clone();

    // A workspace with staff, outside people and a machine; hosts in each zone.
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
    for (host, zone) in [(STAFF_HOST, Some("office")), (PUBLIC_HOST, Some("public")), (FREE_HOST, None)] {
        k.add_operator_host("acme", host, "shell", zone).await.unwrap();
    }
    let mut people = vec![];
    for kind in ["member", "portal", "guest", "service"] {
        let email = format!("{kind}@acme.test");
        let id = k.create_user(&admin, &email, kind, Some(PASSWORD), &[], false).await.unwrap();
        if kind != "member" {
            k.set_user_kind(&admin, id, kind).await.unwrap();
        }
        people.push((kind, email, id));
    }

    // Credentials, made where every zone admits everyone (no zones at all).
    let open = server(&base, &st, Zones::default()).await;
    let mut holds = vec![];
    let mut n = 0;
    for (kind, email, id) in people.iter().map(|(k, e, i)| (*k, e.clone(), *i)).chain([("admin", "admin@acme.test".to_string(), s.user)]) {
        let mut h = Holds { email: email.clone(), ..Default::default() };
        n += 1;
        if kind != "service" {
            let ip = format!("203.0.113.{n}");
            let (status, body, _) = sign_in(&open, STAFF_HOST, &ip, &email, PASSWORD, false).await;
            assert_eq!(status, StatusCode::OK, "{kind} signs in without zones: {body}");
            h.token = body["token"].as_str().map(str::to_string);
            let (_, _, cookie) = sign_in(&open, STAFF_HOST, &ip, &email, PASSWORD, true).await;
            h.cookie = Some(cookie.expect("a cookie"));
        }
        let me = k.load_ctx(acme, id, None, "it".into()).await.unwrap();
        h.key = Some(k.create_api_key(&admin, id, "it", None, None).await.unwrap().1);
        h.dav = Some(k.create_api_key(&admin, id, "dav", None, Some("dav")).await.unwrap().1);
        if kind == "admin" {
            h.scim = Some(k.create_api_key(&admin, id, "scim", None, Some("scim")).await.unwrap().1);
        }
        if kind != "service" {
            let (status, token) = redeem(&open, STAFF_HOST, "203.0.113.99", &oauth_code(&k, &me).await).await;
            assert_eq!(status, StatusCode::OK, "{kind}: an OAuth token without zones: {token}");
            h.oauth = token["access_token"].as_str().map(str::to_string);
        }
        holds.push((kind, h));
    }

    // What everyone gets without zones, route by route and page by page.
    let mut baseline = std::collections::BTreeMap::new();
    for (kind, h) in &holds {
        for (name, cred) in h.each() {
            for (method, path, body) in routes() {
                let got = call(&open, method.clone(), PUBLIC_HOST, path, "203.0.113.50", cred, body).await;
                baseline.insert((kind.to_string(), name, path.to_string()), got.0);
            }
        }
        if let Some(c) = &h.cookie {
            for page in PAGES {
                baseline.insert(
                    (kind.to_string(), "page", page.to_string()),
                    call(&open, Method::GET, PUBLIC_HOST, page, "203.0.113.50", Cred::Cookie(c), None).await.0,
                );
            }
        }
    }

    // Every way a request can be in the zone `public`, which admits no staff.
    let by_host = r#"{"public": {"staff": "none"}, "office": {"networks": ["10.0.0.0/8", "fd00::/8"]}}"#;
    let by_network = r#"{"public": {"staff": "none", "networks": ["198.51.100.0/24", "2001:db8::/32"]}, "office": {}}"#;
    let placements: Vec<(&str, Zones, &str, &str)> = vec![
        ("pinned", zones(by_host, "public", "", ""), STAFF_HOST, "10.1.2.3"),
        ("by host", zones(by_host, "", "", ""), PUBLIC_HOST, "10.1.2.3"),
        ("by IPv4 network", zones(by_network, "", "", ""), FREE_HOST, "198.51.100.7"),
        ("by IPv6 network", zones(by_network, "", "", ""), FREE_HOST, "2001:db8::7"),
        ("by default", zones(by_host, "", "public", ""), FREE_HOST, "203.0.113.200"),
    ];
    for (how, z, host, ip) in placements {
        let walled = server(&base, &st, z).await;
        let (status, here) = call(&walled, Method::GET, host, "/_zone", ip, Cred::None, None).await;
        let here: Value = serde_json::from_str(&here).unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            (here["zone"].clone(), here["staff"].clone(), here["admin"].clone(), here["address"].clone()),
            (json!("public"), json!("none"), json!(false), json!(ip)),
            "{how}"
        );
        let before = counted("public", "enforce");
        for (kind, h) in &holds {
            let staff = matches!(*kind, "member" | "admin");
            for (name, cred) in h.each() {
                // The full matrix where the zone comes from the host; every placement on a few routes.
                let tried: Vec<_> = if how == "by host" { routes() } else { routes().into_iter().take(3).collect() };
                for (method, path, body) in tried {
                    let (status, answer) = call(&walled, method, host, path, ip, cred, body).await;
                    if staff {
                        assert!(refused_by_zone(status, &answer), "{how}: {kind}'s {name} on {path}: {status} {answer}");
                    } else {
                        assert_eq!(
                            status,
                            baseline[&(kind.to_string(), name, path.to_string())],
                            "{how}: {kind}'s {name} on {path} as without zones: {answer}"
                        );
                    }
                }
            }
            if let Some(c) = &h.cookie {
                for page in PAGES {
                    let (status, answer) = call(&walled, Method::GET, host, page, ip, Cred::Cookie(c), None).await;
                    if staff {
                        assert!(plain_page(status, &answer), "{how}: {kind}'s page {page}: {status}");
                    } else {
                        assert_eq!(status, baseline[&(kind.to_string(), "page", page.to_string())], "{how}: {kind}'s page {page} as without zones");
                    }
                }
            }
            // An app password of calendar and contacts apps.
            let (status, answer) =
                call(&walled, Method::from_bytes(b"PROPFIND").unwrap(), host, "/api/dav/comms/", ip, Cred::Basic(&h.email, h.dav.as_deref().unwrap()), None)
                    .await;
            let (open_status, _) = call(
                &open,
                Method::from_bytes(b"PROPFIND").unwrap(),
                host,
                "/api/dav/comms/",
                "203.0.113.50",
                Cred::Basic(&h.email, h.dav.as_deref().unwrap()),
                None,
            )
            .await;
            if staff {
                assert!(refused_by_zone(status, &answer), "{how}: {kind}'s app password: {status} {answer}");
            } else {
                assert_eq!(status, open_status, "{how}: {kind}'s app password as without zones");
            }
            // A socket's handshake, with a bearer token.
            if let Some(t) = &h.token {
                let mut headers = axum::http::HeaderMap::new();
                headers.insert(header::HOST, host.parse().unwrap());
                headers.insert("x-forwarded-for", ip.parse().unwrap());
                headers.insert(header::AUTHORIZATION, format!("Bearer {t}").parse().unwrap());
                let entry = ferrum_server::extract::socket_entry(&walled.st, &headers).await;
                assert_eq!(entry.credential, Credential::Socket);
                let shaken = ferrum_server::extract::socket_credential(&walled.st, &headers, &entry).await;
                if staff {
                    assert!(
                        matches!(shaken, Err(ferrum_server::error::ApiError(ferrum_kernel::Error::Refused { code: "zone", .. }))),
                        "{how}: {kind}'s socket"
                    );
                } else {
                    assert!(
                        shaken.is_ok_and(|c| c.is_some_and(|(_, ctx)| ctx.zone.as_ref().and_then(|e| e.name().map(str::to_string)) == Some("public".into()))),
                        "{how}: {kind}'s socket, in the zone"
                    );
                }
            }
        }
        // An identity provider's key: refused where `scim` is not true.
        let scim = holds.iter().find(|(k, _)| *k == "admin").and_then(|(_, h)| h.scim.clone()).unwrap();
        let (status, answer) = call(&walled, Method::GET, host, "/api/scim/v2/Users", ip, Cred::Bearer(&scim), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{how}: a SCIM key: {answer}");
        assert!(counted("public", "enforce") > before, "{how}: refusals are counted");
    }

    let walled = server(&base, &st, zones(by_host, "", "", "")).await;
    let (ip, host) = ("10.1.2.3", PUBLIC_HOST);

    // A SCIM key where the rule says so.
    let scim = holds.iter().find(|(k, _)| *k == "admin").and_then(|(_, h)| h.scim.clone()).unwrap();
    let scim_ok = server(&base, &st, zones(r#"{"public": {"staff": "none", "scim": true}, "office": {}}"#, "", "", "")).await;
    let (status, answer) = call(&scim_ok, Method::GET, host, "/api/scim/v2/Users", ip, Cred::Bearer(&scim), None).await;
    assert_eq!(status, StatusCode::OK, "scim: true admits the identity provider's key: {answer}");
    let (status, _) = call(&scim_ok, Method::GET, host, "/api/auth/me", ip, Cred::Bearer(holds.last().unwrap().1.key.as_deref().unwrap()), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "and nothing else of staff");

    // The staff address is where staff work; the same credentials still work there.
    let admin_token = holds.iter().find(|(k, _)| *k == "admin").and_then(|(_, h)| h.token.clone()).unwrap();
    let (status, _) = call(&walled, Method::GET, STAFF_HOST, "/api/auth/me", ip, Cred::Bearer(&admin_token), None).await;
    assert_eq!(status, StatusCode::OK, "at the staff address");
    let (status, _) = call(&walled, Method::GET, FREE_HOST, "/api/auth/me", "203.0.113.9", Cred::Bearer(&admin_token), None).await;
    assert_eq!(status, StatusCode::OK, "a host in no zone, from no zone's network, without a default: as without zones");

    // Sign-in: refused before the password is read, the same whatever it is, with no lockout.
    let member = "member@acme.test";
    let before = failed_logins(&k, member).await;
    let wrong = sign_in(&walled, host, "198.18.0.1", member, "Wrong-Horse-99", false).await;
    let right = sign_in(&walled, host, "198.18.0.2", member, PASSWORD, false).await;
    let nobody = sign_in(&walled, host, "198.18.0.3", "nobody@acme.test", PASSWORD, false).await;
    assert_eq!(wrong.0, StatusCode::UNAUTHORIZED);
    assert_eq!((wrong.0, &wrong.1["message"]), (right.0, &right.1["message"]), "a wrong and a right password: one answer");
    assert_eq!((right.0, &right.1["message"]), (nobody.0, &nobody.1["message"]), "and that of an address nobody has");
    assert_eq!(failed_logins(&k, member).await, before, "nothing counts toward the lockout");
    for _ in 0..6 {
        sign_in(&walled, host, "198.18.0.4", member, "Wrong-Horse-99", false).await;
    }
    assert_eq!(failed_logins(&k, member).await, before, "however often");
    let (status, _, _) = sign_in(&open, STAFF_HOST, "203.0.113.77", member, PASSWORD, false).await;
    assert_eq!(status, StatusCode::OK, "and the staff address still lets them in");
    let (status, body, _) = sign_in(&walled, host, "198.18.0.5", "portal@acme.test", PASSWORD, false).await;
    assert_eq!(status, StatusCode::OK, "a portal person signs in: {body}");
    // OAuth: a staff code or refresh token is answered as an unknown one, and left as it was: each
    // still works afterwards where staff are admitted. An outside person's is redeemed.
    let staff_ctx = k.load_ctx(acme, people[0].2, None, "it".into()).await.unwrap();
    let unknown = redeem(&walled, host, ip, &(String::from("nobody"), String::from("no-such-code"), "v".repeat(50))).await;
    assert_eq!((unknown.0, unknown.1["error"].clone()), (StatusCode::BAD_REQUEST, json!("invalid_grant")));
    let code = oauth_code(&k, &staff_ctx).await;
    let (status, answer) = redeem(&walled, host, ip, &code).await;
    assert_eq!((status, &answer), (unknown.0, &unknown.1), "a staff code: the unknown answer");
    let (status, granted) = redeem(&open, STAFF_HOST, "10.1.2.3", &code).await;
    assert_eq!(status, StatusCode::OK, "and it was not used up: {granted}");
    let staff_refresh = granted["refresh_token"].as_str().unwrap().to_string();
    let unknown = refresh(&walled, host, ip, &code.0, "fr_no-such-token").await;
    assert_eq!(refresh(&walled, host, ip, &code.0, &staff_refresh).await, unknown, "a staff refresh token: the unknown answer");
    assert_eq!(refresh(&walled, host, ip, &code.0, &staff_refresh).await, unknown, "again: no reuse detection, nothing revoked");
    let (status, rotated) = refresh(&open, STAFF_HOST, "10.1.2.3", &code.0, &staff_refresh).await;
    assert_eq!(status, StatusCode::OK, "it still works where staff are admitted: {rotated}");
    let portal_ctx = k.load_ctx(acme, people[1].2, None, "it".into()).await.unwrap();
    assert_eq!(redeem(&walled, host, ip, &oauth_code(&k, &portal_ctx).await).await.0, StatusCode::OK);
    // The sign-in page for staff is the plain page, the portal's is not.
    assert!(plain_page(
        call(&walled, Method::GET, host, "/login", ip, Cred::None, None).await.0,
        &call(&walled, Method::GET, host, "/login", ip, Cred::None, None).await.1
    ));
    assert_eq!(call(&walled, Method::GET, host, "/login?next=/portal/", ip, Cred::None, None).await.0, StatusCode::OK);

    // What the internet uses works as without zones.
    for path in [
        "/api/public/acme/lists/nothing",
        "/api/public/acme/links/nothing",
        "/api/public/acme/some-plugin/callback",
        "/public/acme/nothing",
        "/_shell/v1/icon.svg",
        "/api/version",
    ] {
        for method in [Method::GET, Method::POST] {
            let a = call(&walled, method.clone(), host, path, ip, Cred::None, Some(json!({}))).await.0;
            let b = call(&open, method.clone(), host, path, "203.0.113.50", Cred::None, Some(json!({}))).await.0;
            assert_eq!(a, b, "{method} {path}");
        }
    }
    let inbound = format!("/connections/inbound/{}", Uuid::now_v7());
    assert_eq!(
        call(&walled, Method::POST, host, &inbound, ip, Cred::None, Some(json!({}))).await.0,
        call(&open, Method::POST, host, &inbound, "203.0.113.50", Cred::None, Some(json!({}))).await.0
    );
    // Work with no request behind it (a job, as its creator) is inside: no zone applies.
    for (_, _, id) in &people {
        k.load_ctx(acme, *id, None, "job-1".into()).await.expect("inside");
    }

    // Staff on two more credentials: an app token, and support access.
    let admin_ctx = k.load_ctx(acme, s.user, None, "it".into()).await.unwrap();
    let secret = base.config.auth.as_ref().unwrap().jwt.as_ref().unwrap().secret.clone();
    let admin_cookie = holds.iter().find(|(k, _)| *k == "admin").and_then(|(_, h)| h.cookie.clone()).unwrap();
    let sid = loco_rs::auth::jwt::JWT::new(&secret).validate(&admin_cookie).unwrap().claims.claims["sid"].clone();
    let mut claims = serde_json::Map::new();
    claims.insert("tenant".into(), json!(acme.to_string()));
    claims.insert("tv".into(), json!(k.token_version(acme, s.user).await.unwrap()));
    claims.insert("bsid".into(), sid);
    claims.insert("app".into(), json!("core"));
    let app_token = loco_rs::auth::jwt::JWT::new(&secret).generate_token(300, s.user.to_string(), claims).unwrap();
    let (status, answer) = call(&walled, Method::GET, host, "/api/v1/Company", ip, Cred::Bearer(&app_token), None).await;
    assert!(refused_by_zone(status, &answer), "an app token: {status} {answer}");
    assert!(!refused_by_zone(call(&open, Method::GET, host, "/api/v1/Company", ip, Cred::Bearer(&app_token), None).await.0, ""), "works elsewhere");
    let grant = serde_json::from_value(json!({"level": "health", "grantee": "Support desk", "reason": "the wall", "hours": 1})).unwrap();
    let (_, support) = k.grant_support(&admin_ctx, grant).await.unwrap();
    let (status, answer) = call(&walled, Method::GET, host, "/api/support/report", ip, Cred::Bearer(&support), None).await;
    assert!(refused_by_zone(status, &answer), "support access: {status} {answer}");
    let (status, answer) = call(&open, Method::GET, host, "/api/support/report", ip, Cred::Bearer(&support), None).await;
    assert_eq!(status, StatusCode::OK, "support access without zones: {answer}");

    // Outside people, for real, in the zone: a published list gives its rows.
    k.insert(&admin_ctx, "Currency", json!({"code": "XTS", "currency_name": "Testing Coin"}).as_object().cloned().unwrap()).await.unwrap();
    let publish: ferrum_meta::OverlayOp =
        serde_json::from_value(json!({"op": "publish_list", "list": {"name": "currencies", "label": "Currencies", "fields": ["code", "currency_name"]}}))
            .unwrap();
    k.add_customization(&admin_ctx, "Currency", publish, None).await.unwrap();
    let (status, list) = call(&walled, Method::GET, host, "/api/public/acme/lists/currencies", ip, Cred::None, None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let rows = serde_json::from_str::<Value>(&list).unwrap()["rows"].clone();
    assert!(rows.as_array().unwrap().contains(&json!({"code": "XTS", "currency_name": "Testing Coin"})), "{list}");
    assert!(call(&walled, Method::GET, host, "/public/acme/lists/currencies", ip, Cred::None, None).await.1.contains("Testing Coin"), "and its page");
    // An action link does what it says.
    let quote = json!({"name": "XQuote", "label": "Quotation", "kind": "master", "title_field": "title",
        "fields": [{"name": "title", "type": "data", "required": true}, {"name": "rating", "type": "int"}],
        "actions": [{"name": "rate", "label": "Rate us", "permission": "write",
            "params": [{"name": "rating", "label": "Rating", "type": "int", "required": true}],
            "effects": [{"type": "set_fields", "values": {"rating": "params.rating"}}]}]});
    let define: ferrum_kernel::customize::NewCustomization =
        serde_json::from_value(json!({"entity": "XQuote", "op": {"op": "define_entity", "entity": quote}})).unwrap();
    k.batch_customizations(&admin_ctx, vec![define], false).await.map_err(|(_, e)| e.to_string()).unwrap();
    let admin_ctx = k.load_ctx(acme, s.user, None, "it".into()).await.unwrap();
    let q: Uuid = k.insert(&admin_ctx, "XQuote", json!({"title": "Q-1"}).as_object().cloned().unwrap()).await.unwrap()["id"].as_str().unwrap().parse().unwrap();
    let spec: ferrum_kernel::action_link::LinkSpec = serde_json::from_value(json!({"actions": ["rate"]})).unwrap();
    let link = k.issue_link_now(&admin_ctx, "XQuote", q, &spec).await.unwrap();
    let path = format!("/api/public/acme/links/{}", link.url.rsplit('/').next().unwrap());
    let req = Request::builder()
        .method(Method::POST)
        .uri(&path)
        .header(header::HOST, host)
        .header("x-forwarded-for", ip)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("_action=rate&rating=4"))
        .unwrap();
    let res = walled.router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "the link's action");
    assert_eq!(k.get(&admin_ctx, "XQuote", q, false).await.unwrap()["rating"], json!(4), "done to the record");
    // A plugin's public callback runs as its service account and does its work.
    // The plugins' own fixture, without what reaches the selling app's Customer (not in this server's packages).
    let mut manifest: Value = serde_json::from_str(include_str!("../../crates/plugins/tests/fixtures/device_gateway.json")).unwrap();
    manifest["hooks"] = json!([]);
    manifest["subscriptions"] = json!([]);
    manifest["permissions"].as_array_mut().unwrap().retain(|p| p["entity"] != "Customer");
    let manifest: ferrum_plugins::NewPlugin = serde_json::from_value(manifest).unwrap();
    st.plugins.install(&k, &admin_ctx, manifest, include_bytes!("../../crates/plugins/tests/fixtures/device_gateway.wasm")).await.unwrap();
    let (status, answer) = call(&walled, Method::GET, host, "/api/public/acme/device-gateway/status", ip, Cred::None, None).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(serde_json::from_str::<Value>(&answer).unwrap()["online"], json!(0), "it read its devices as its service account");
    // A job a portal person queues from the zone is picked up and completes.
    let rule: ferrum_meta::PermissionDef = serde_json::from_value(json!({"entity": "Currency", "read": true})).unwrap();
    k.set_role_permission(&admin_ctx, "Shoppers", &rule).await.unwrap();
    k.set_role_portal(&admin_ctx, "Shoppers", true).await.unwrap();
    k.set_user_roles_until(&admin_ctx, people[1].2, &["Shoppers".into()], &Default::default()).await.unwrap();
    let portal_token = holds.iter().find(|(k, _)| *k == "portal").and_then(|(_, h)| h.token.clone()).unwrap();
    let (status, queued) = call(
        &walled,
        Method::POST,
        host,
        "/api/v1/Currency/_bulk/jobs",
        ip,
        Cred::Bearer(&portal_token),
        Some(json!({"filters": {}, "op": "update", "values": {}})),
    )
    .await;
    assert!(status.is_success(), "queued from the zone: {status} {queued}");
    let job: Uuid = serde_json::from_str::<Value>(&queued).unwrap()["job"].as_str().unwrap().parse().unwrap();
    assert!(k.run_jobs_once(10).await.unwrap() >= 1, "picked up");
    let mut tx = k.db.tenant_tx(acme).await.unwrap();
    let (done, created_by): (String, Uuid) = sqlx::query_as("select status, created_by from job where id = $1").bind(job).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!((done.as_str(), created_by), ("done", people[1].2), "and completed, as the portal person");

    // Report mode: everything as without zones, and counted.
    let report = server(&base, &st, zones(by_host, "", "", "report")).await;
    let before = counted("public", "report");
    for (kind, h) in &holds {
        for (name, cred) in h.each() {
            for (method, path, body) in routes().into_iter().take(4) {
                let (status, answer) = call(&report, method, host, path, ip, cred, body).await;
                assert_eq!(status, baseline[&(kind.to_string(), name, path.to_string())], "report: {kind}'s {name} on {path}: {answer}");
            }
        }
    }
    assert!(counted("public", "report") >= before + 4 * 2, "staff were counted");
    let (status, metrics) = call(&report, Method::GET, host, "/metrics", ip, Cred::None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(metrics.contains("ferrum_zone_refusals_total{zone=\"public\",mode=\"report\",credential=\"token\"}"), "{metrics}");
    let (status, _, cookie) = sign_in(&report, host, "198.18.0.6", member, PASSWORD, true).await;
    assert_eq!(status, StatusCode::OK, "report mode lets staff sign in");
    assert!(cookie.is_some());

    // The zone is on each session.
    let (_, _, cookie) = sign_in(&walled, STAFF_HOST, "10.9.9.9", "admin@acme.test", PASSWORD, true).await;
    let (status, sessions) = call(&walled, Method::GET, STAFF_HOST, "/api/auth/sessions", "10.9.9.9", Cred::Cookie(&cookie.unwrap()), None).await;
    assert_eq!(status, StatusCode::OK);
    let sessions: Value = serde_json::from_str(&sessions).unwrap();
    assert!(sessions["sessions"].as_array().unwrap().iter().any(|s| s["zone"] == "office"), "{sessions}");
    assert!(sessions["sessions"].as_array().unwrap().iter().any(|s| s["zone"].is_null()), "opened without zones: none");

    // A workspace's administrator cannot put a host in a zone; only the operator's tasks can.
    let (status, answer) = call(
        &open,
        Method::POST,
        STAFF_HOST,
        "/api/admin/domains",
        ip,
        Cred::Bearer(&admin_token),
        Some(json!({"host": "new.acme.example", "target": "shell", "zone": "office"})),
    )
    .await;
    assert!(status.is_success(), "{status} {answer}");
    let zone: Option<String> = read(&k, "select zone from tenant_host where host = $1", "new.acme.example").await;
    assert_eq!(zone, None, "the admin API ignores a zone");
    k.set_host_zone("new.acme.example", Some("public")).await.unwrap();
    assert_eq!(k.host_zone("new.acme.example").await.unwrap(), None, "a host not yet verified serves nothing, zone or not");
    assert!(k.set_host_zone("nowhere.acme.example", None).await.is_err(), "no such host");
    let kind: String = read(&k, "select kind from tenant_host where host = $1", PUBLIC_HOST).await;
    assert_eq!(kind, "operator");

    // The operator's way back in: audited, with a reason; never the last administrator.
    assert!(k.operator_admin("acme", "admin@acme.test", false, "test").await.is_err(), "the last administrator stays");
    assert!(k.operator_admin("acme", member, true, " ").await.is_err(), "a reason is required");
    assert!(k.operator_admin("acme", "portal@acme.test", true, "test").await.is_err(), "only a member");
    k.operator_admin("acme", member, true, "the administrator left").await.unwrap();
    k.operator_admin("acme", "admin@acme.test", false, "left the company").await.unwrap();
    assert!(k.operator_admin("acme", member, false, "test").await.is_err(), "now the member is the last one");
    let mut tx = k.db.tenant_tx(acme).await.unwrap();
    let trail: Vec<(String, Value)> =
        sqlx::query_as("select action, diff from audit_event where entity = 'User' and action in ('grant_admin', 'revoke_admin') order by id")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(trail.iter().map(|t| t.0.as_str()).collect::<Vec<_>>(), ["grant_admin", "revoke_admin"]);
    assert_eq!(trail[0].1["reason"], "the administrator left");
    // Two operators at once, each removing the other of the last two: one of them is refused.
    k.operator_admin("acme", "admin@acme.test", true, "back").await.unwrap();
    let (a, b) = tokio::join!(k.operator_admin("acme", "admin@acme.test", false, "race"), k.operator_admin("acme", member, false, "race"));
    assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(), 1, "exactly one is removed: {a:?} {b:?}");
}
