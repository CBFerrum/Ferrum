//! The platform's secrets in the vault (docs/operations.md, "The platform's secrets"), against a real PostgreSQL
//! (`FERRUM_TEST_DATABASE_URL`; the database and its vault are reset): the server set up as the binary sets it up, two
//! replicas on one database. With the vault closed everything is as before. Open, what a replica's environment sets is
//! stored once however many store it at once, and the environment still wins. Without `JWT_SECRET` the vault's keys sign
//! sessions: one made between replicas starting together, replaced under traffic with no session refused, a forged or
//! unknown key refused. Tokens the server compares are kept as digests and changed over with an overlap. A secret
//! handed to a library goes only to the host it was set for, and moving that address takes it again or clears it. People
//! and the operator manage secrets through routes that never answer with a value. No table holds one.

mod common;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use ferrum_kernel::vault::{Scope, Zeroizing};
use ferrum_server::app::App;
use ferrum_server::secrets::{Given, Token};
use ferrum_server::settings::Change;
use ferrum_server::state::AppState;
use loco_rs::app::{AppContext, Hooks};
use loco_rs::config::Config;
use loco_rs::environment::Environment;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tower::ServiceExt;

const PASSWORD: &str = "Correct-Horse-9";
/// What an older installation's environment set as `JWT_SECRET`.
const LEGACY: &str = "c2VjcmV0cy1pdC1sZWdhY3ktand0LXNlY3JldC0wMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFi";

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

/// The test configuration on `address`, `JWT_SECRET` as given (empty: none), with a mail server whose password nothing
/// sets.
fn config(address: &str, jwt: &str) -> Config {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut config = Environment::Test.load_from_folder(&dir.join("config")).expect("config/test.yaml");
    ferrum_server::settings::set_database_url(&mut config, address.to_string());
    if let Some(s) = config.settings.as_mut() {
        s["packages_dir"] = json!(packages().to_string_lossy());
    }
    config.auth.as_mut().unwrap().jwt.as_mut().unwrap().secret = jwt.to_string();
    config.mailer = Some(
        serde_json::from_value(
            json!({"stub": false, "smtp": {"enable": true, "host": "localhost", "port": 587, "secure": true, "auth": {"user": "mailer", "password": ""}}}),
        )
        .unwrap(),
    );
    config
}

/// The platform's packages, and `keep`, whose `Safe` holds a `secret` field (the records door's receipts).
fn packages() -> std::path::PathBuf {
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages").canonicalize().unwrap();
    let dir = std::env::temp_dir().join(format!("ferrum-secrets-it-{}", std::process::id()));
    if !dir.exists() {
        std::fs::create_dir_all(dir.join("keep/entities")).unwrap();
        for p in std::fs::read_dir(&here).unwrap().flatten() {
            std::os::unix::fs::symlink(p.path(), dir.join(p.file_name())).unwrap();
        }
        let package = "package: keep\nversion: 0.1.0\ndepends: [core]\n";
        std::fs::write(dir.join("keep/package.yaml"), package).unwrap();
        std::fs::write(
            dir.join("keep/entities/safe.yaml"),
            "name: Safe\nlabel: Safe\nkind: master\nfields:\n  - {name: label, type: data}\n  - {name: pin, type: secret}\n",
        )
        .unwrap();
    }
    dir
}

async fn replica(config: Config) -> AppContext {
    <App as Hooks>::after_context(AppContext::builder(Environment::Test, config).build()).await.expect("set-up")
}

fn state(ctx: &AppContext) -> AppState {
    ctx.shared_store.get::<AppState>().expect("state")
}

fn find(key: &str) -> &'static ferrum_server::settings::Def {
    ferrum_server::secrets::find(key).unwrap()
}

fn claims() -> Map<String, Value> {
    json!({"tenant": "00000000-0000-0000-0000-000000000000"}).as_object().cloned().unwrap()
}

/// The mail password a replica's configuration ends with.
fn mail_password(ctx: &AppContext) -> String {
    ctx.config.mailer.as_ref().unwrap().smtp.as_ref().unwrap().auth.as_ref().unwrap().password.clone()
}

/// Writes a row of the settings table by hand, as an operator's tool (or an older build) would.
async fn row(st: &AppState, key: &str, value: &str) {
    let mut tx = st.kernel.db.system_tx().await.unwrap();
    sqlx::query(
        "insert into setting (scope, scope_name, key, value, updated_by) values ('installation', '', $1, to_jsonb($2::text), 'test') \
         on conflict (scope, scope_name, key) do update set value = excluded.value, version = nextval('setting_version')",
    )
    .bind(key)
    .bind(value)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

struct Served {
    router: axum::Router,
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
}

/// The server's routes and layers, as the binary serves them in `environment` (`ferrum -e <environment> start`), with
/// `zones` (`FERRUM_ZONES`) in place of the configured.
async fn serve_in(ctx: &AppContext, environment: Environment, zones: &str) -> Served {
    let st = AppState { zones: Arc::new(ferrum_kernel::zones::Zones::parse(zones, "", "", "").expect("zones")), ..state(ctx) };
    let ctx = AppContext::builder(environment, ctx.config.clone()).build();
    ctx.shared_store.insert(st);
    let routes = <App as Hooks>::routes(&ctx).to_router::<App>(ctx.clone(), axum::Router::new()).expect("router");
    Served { router: <App as Hooks>::after_routes(routes, &ctx).await.expect("layers") }
}

/// A zone that has the operator's routes, which every [`call`] is in by its address.
const OPERATIONS: &str = r#"{"operations": {"operator": true, "networks": ["10.9.0.0/16"]}}"#;

/// [`serve_in`] the test environment and [`OPERATIONS`].
async fn serve(ctx: &AppContext) -> Served {
    serve_in(ctx, Environment::Test, OPERATIONS).await
}

/// A request with `headers` (an `x-forwarded-for` among them names the address it comes from); a body as JSON.
async fn call(s: &Served, method: Method, path: &str, headers: &[(&str, &str)], body: Option<Value>) -> Answer {
    let mut req = Request::builder().method(method).uri(path).header(header::HOST, "localhost").header("x-forwarded-for", "10.9.8.7");
    for (h, v) in headers {
        req = req.header(*h, *v);
    }
    let req = match body {
        Some(b) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let res = s.router.clone().oneshot(req).await.unwrap();
    let (status, headers) = (res.status(), res.headers().clone());
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    Answer { status, headers, body: String::from_utf8_lossy(&bytes).into_owned() }
}

#[test]
fn the_platforms_secrets() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    {
        let pool = sqlx::PgPool::connect(&url()).await.expect("connect");
        common::reset(&pool).await;
        sqlx::raw_sql("drop schema if exists vault cascade").execute(&pool).await.expect("reset the vault");
    }
    ferrum_server::settings::EVERY_MS.store(300, Ordering::Relaxed);

    // ── The vault closed (no password, no key file, no key service): everything as before ──
    let closed = url().replacen("ferrum:ferrum@", "ferrum@", 1);
    let said = <App as Hooks>::after_context(AppContext::builder(Environment::Test, config(&closed, "")).build()).await.err().unwrap().to_string();
    assert!(said.contains("JWT_SECRET is not set, and the vault is closed"), "{said}");
    let c = state(&replica(config(&closed, LEGACY)).await);
    assert!(!c.vault_open);
    let env = |name: &str| (name == "FERRUM_METRICS_TOKEN").then(|| "Mt-0000-closed".to_string());
    assert!(ferrum_server::secrets::adopt(&c, &env).await.is_empty(), "nothing stored while the vault is closed");
    assert_eq!(ferrum_server::secrets::token(&c, "metrics_token", "", "anything").await, Token::NotSet, "/metrics open, as without a token");
    let token = c.sessions.sign(60, "p".into(), claims()).unwrap();
    assert!(loco_rs::auth::jwt::JWT::new(LEGACY).validate(&token).is_ok(), "JWT_SECRET signs as before");
    let mut found = Vec::new();
    ferrum_server::secrets::findings(&c, &mut found).await;
    assert!(found.iter().any(|f| f.check == "secrets.vault" && f.level == ferrum_kernel::doctor::Level::Warning), "{found:?}");
    c.kernel.db.close().await;

    // ── Open (the database password makes its first slot): replicas started together without JWT_SECRET make one key ──
    let (a_ctx, b_ctx) = tokio::join!(replica(config(&url(), "")), replica(config(&url(), "")));
    let (a, b) = (state(&a_ctx), state(&b_ctx));
    assert!(a.vault_open && b.vault_open);
    assert_eq!((a.sessions.vault_signs(), b.sessions.vault_signs()), (Some(1), Some(1)), "one key between them");
    <App as Hooks>::before_run(&a_ctx).await.expect("serving");
    <App as Hooks>::before_run(&b_ctx).await.expect("serving");
    let mut needles: Vec<String> = vec![LEGACY.into()];

    // ── /metrics while no token is set: open where people develop and test, nobody's in production ──
    assert_eq!(ferrum_server::secrets::token(&a, "metrics_token", "", "").await, Token::NotSet, "nothing sets one yet");
    assert_eq!(call(&serve(&a_ctx).await, Method::GET, "/metrics", &[], None).await.status, StatusCode::OK, "in development and tests");
    let production = call(&serve_in(&a_ctx, Environment::Production, OPERATIONS).await, Method::GET, "/metrics", &[], None).await;
    assert_eq!(production.status, StatusCode::UNAUTHORIZED, "in production: {}", production.body);

    adopted(&a, &b, &mut needles).await;
    sessions(&a, &b, &mut needles).await;
    tokens(&a, &a_ctx, &b_ctx, &mut needles).await;
    library(&a, &mut needles).await;
    people(&a, &a_ctx, &mut needles).await;
    records_door(&a, &a_ctx).await;

    // ── No table holds a value or a token, the settings table no secret ──
    for name in ["session.1", "session.2", "session.3"] {
        if let Ok(v) = a.vault.reveal(&a.kernel, Scope::Installation, name).await {
            needles.push(String::from_utf8(v.to_vec()).unwrap());
        }
    }
    let needles: Vec<String> = needles.into_iter().filter(|n| n.len() >= 12).collect();
    for schema in ["public", "vault"] {
        assert_eq!(holding(&a, schema, &needles).await, Vec::<String>::new(), "{schema}");
    }
    let _ = a.shutdown.send(true);
    let _ = b.shutdown.send(true);
    for st in [&a, &b] {
        let handles: Vec<_> = st.background.lock().await.drain(..).collect();
        futures_util::future::join_all(handles).await;
    }
    a.kernel.db.close().await;
    b.kernel.db.close().await;
}

/// What a replica's environment sets is stored once, however many store it at once, and the environment still wins.
async fn adopted(a: &AppState, b: &AppState, needles: &mut Vec<String>) {
    let env = |name: &str| match name {
        "FERRUM_METRICS_TOKEN" => Some("Mt-1234-from-the-environment".to_string()),
        "MAILER_PASSWORD" => Some("Mp-5678-from-the-environment".to_string()),
        "JWT_SECRET" => Some(LEGACY.to_string()),
        "FERRUM_OPERATOR_TOKEN" => Some(String::new()),
        "FERRUM_SESSION_DAYS" => Some("3".to_string()),
        _ => None,
    };
    needles.extend(["Mt-1234-from-the-environment".to_string(), "Mp-5678-from-the-environment".to_string()]);
    let (one, two) = tokio::join!(ferrum_server::secrets::adopt(a, &env), ferrum_server::secrets::adopt(b, &env));
    let mut stored = [one, two].concat();
    stored.sort();
    assert_eq!(stored, ["jwt_secret", "mailer_password", "metrics_token"], "each once; set to nothing is not stored; a setting is not a secret");
    let again = |name: &str| (name == "FERRUM_METRICS_TOKEN").then(|| "Mt-9999-another-value".to_string());
    assert!(ferrum_server::secrets::adopt(a, &again).await.is_empty(), "what the vault holds stays");
    let events = ferrum_kernel::vault::events(&a.kernel.db, None, 500).await.unwrap();
    let adopts: Vec<_> = events.iter().filter(|e| e.action == "adopt" && e.details["by"] == "environment").collect();
    assert_eq!(adopts.len(), 3, "recorded once each");
    assert!(adopts.iter().all(|e| e.owner == "platform"));
    assert_eq!(events.iter().filter(|e| e.action == "adopt" && e.details["by"] == "start").count(), 1, "the first key, made once");
    let record = serde_json::to_string(&events).unwrap();
    assert!(!record.contains("Mt-1234") && !record.contains("Mp-5678"), "never a value in the record");
    // Where each comes from, as `ferrum task settings` and the operator's routes list it; never a value.
    let listed = ferrum_server::settings::listing(a).await.unwrap();
    let source = |key: &str| listed["settings"].as_array().unwrap().iter().find(|s| s["key"] == key).unwrap()["source"].clone();
    assert_eq!(source("metrics_token"), json!("vault"));
    assert_eq!(source("bootstrap_token"), json!("environment"), "the configuration sets it: it wins");
    assert_eq!(source("jwt_secret"), json!("vault"), "the vault's key signs");
    assert_eq!(source("operator_token"), json!("default"), "nothing sets it");
    assert!(!listed.to_string().contains("Mt-1234") && !listed.to_string().contains("Mp-5678"));
    let mut found = Vec::new();
    ferrum_server::secrets::findings(a, &mut found).await;
    let env_decides = found.iter().find(|f| f.check == "secrets.environment").unwrap();
    assert!(env_decides.detail.contains("bootstrap_token") && !env_decides.detail.contains("metrics_token"), "{env_decides:?}");
    assert!(found.iter().any(|f| f.check == "secrets.jwt" && f.detail.contains("vault's key 1")), "{found:?}");
}

/// The keys that sign sessions: each named by the tokens it signs; replaced under traffic on two replicas, no valid token
/// refused; a forged or unknown key refused; a key past its end verifies nothing.
async fn sessions(a: &AppState, b: &AppState, needles: &mut Vec<String>) {
    let first = a.sessions.sign(3600, "p1".into(), claims()).unwrap();
    assert!(b.sessions.validate(&first).await.is_some(), "one replica's token, the other's to check");
    // The environment's JWT_SECRET, copied into the vault: what it signed still verifies here, where it is not set.
    let legacy = loco_rs::auth::jwt::JWT::new(LEGACY).generate_token(3600, "old".into(), claims()).unwrap();
    assert!(a.sessions.validate(&legacy).await.is_some(), "the copy verifies what the environment's secret signed");

    // Under traffic: each replica signs and the other checks, while one rotates the key.
    let (stop, refused, checked) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let traffic = {
        let (a, b, stop, refused, checked, first) = (a.clone(), b.clone(), stop.clone(), refused.clone(), checked.clone(), first.clone());
        tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                for (signer, checker) in [(&a, &b), (&b, &a)] {
                    let t = signer.sessions.sign(600, "traffic".into(), claims()).unwrap();
                    for token in [&t, &first] {
                        if checker.sessions.validate(token).await.is_none() {
                            refused.fetch_add(1, Ordering::Relaxed);
                        }
                        checked.fetch_add(1, Ordering::Relaxed);
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    ferrum_server::secrets::set(a, find("jwt_secret"), Given::Made, 0).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    stop.store(true, Ordering::Relaxed);
    traffic.await.unwrap();
    assert_eq!(refused.load(Ordering::Relaxed), 0, "no valid token refused, of {}", checked.load(Ordering::Relaxed));
    assert!(checked.load(Ordering::Relaxed) > 40);
    assert_eq!((a.sessions.vault_signs(), b.sessions.vault_signs()), (Some(2), Some(2)), "both sign with the new key");
    let one = a.vault.describe(&a.kernel, Scope::Installation, "session.1").await.unwrap().unwrap();
    let end = one.rule.expires_at.expect("the key before has an end");
    assert!(end > chrono::Utc::now() + chrono::Duration::days(2), "as far ahead as the longest session: {end}");
    let copy = a.vault.describe(&a.kernel, Scope::Installation, "jwt_secret").await.unwrap().unwrap();
    assert!(copy.rule.expires_at.is_some(), "with JWT_SECRET out of this environment, its copy gets an end too");

    // Forged: another key's id on a token, claims changed, a key nobody holds, no algorithm.
    let second = a.sessions.sign(600, "p2".into(), claims()).unwrap();
    let mut parts: Vec<String> = second.split('.').map(str::to_string).collect();
    let b64 = |v: Value| base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, v.to_string());
    for header in [
        json!({"typ": "JWT", "alg": "HS512", "kid": "1"}),
        json!({"typ": "JWT", "alg": "HS512", "kid": "77"}),
        json!({"typ": "JWT", "alg": "none", "kid": "2"}),
    ] {
        let forged = format!("{}.{}.{}", b64(header.clone()), parts[1], parts[2]);
        assert!(b.sessions.validate(&forged).await.is_none(), "{header}");
    }
    parts[1] = b64(json!({"pid": "someone-else", "exp": 4_000_000_000u64, "tenant": "x"}));
    assert!(b.sessions.validate(&parts.join(".")).await.is_none(), "claims changed");
    assert!(b.sessions.validate(&second).await.is_some());

    // A key past its end verifies nothing: the next rotation gives the one before a second.
    let third = ferrum_server::sessions::rotate(&a.kernel, &a.vault, Duration::from_secs(1), false).await.unwrap();
    assert_eq!(third, 3);
    a.sessions.reload().await.unwrap();
    assert!(a.sessions.validate(&second).await.is_some(), "within its end");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    a.sessions.reload().await.unwrap();
    assert!(a.sessions.validate(&second).await.is_none(), "past its end");
    assert!(a.sessions.validate(&a.sessions.sign(60, "p3".into(), claims()).unwrap()).await.is_some());

    // ── a_new_key_is_taken_at_once_by_a_replica_that_has_just_read_its_keys (session-key-race) ──
    // The second replica reads its keys now (a token naming a key nobody holds); the first then makes a new key and signs
    // with it: the second takes that token at once, not a second later.
    let made_up = format!("{}.{}.{}", b64(json!({"typ": "JWT", "alg": "HS512", "kid": "4000"})), parts[1], parts[2]);
    let read = std::time::Instant::now();
    assert!(b.sessions.validate(&made_up).await.is_none(), "a key nobody holds");
    let fourth = ferrum_server::sessions::rotate(&a.kernel, &a.vault, Duration::from_secs(3600), false).await.unwrap();
    a.sessions.reload().await.unwrap();
    let fresh = a.sessions.sign(600, "p4".into(), claims()).unwrap();
    let after = read.elapsed();
    assert!(b.sessions.validate(&fresh).await.is_some(), "key {fourth}'s token, {after:?} after the second replica read its keys");
    needles.push(second);
}

/// Tokens the server compares: kept as digests; the vault's taken where the configuration sets none; changed over with
/// an overlap on every replica at once.
async fn tokens(a: &AppState, a_ctx: &AppContext, b_ctx: &AppContext, needles: &mut Vec<String>) {
    // The operator's routes are not there where no zone has them, as when an installation starts: what any path the
    // server does not have answers, before a token is looked at.
    let bare = serve_in(a_ctx, Environment::Test, "").await;
    let unknown = call(&bare, Method::GET, "/api/nothing-here", &[], None).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    let as_unknown = |path: &str| {
        let said = unknown.json()["message"].as_str().unwrap_or_default().replace("/api/nothing-here", path);
        (StatusCode::NOT_FOUND, unknown.json()["error"].clone(), json!(said))
    };
    let answer = |a: &Answer| (a.status, a.json()["error"].clone(), a.json()["message"].clone());
    let asked = call(&bare, Method::GET, "/api/operator/vault", &[("x-ferrum-operator-token", "a-guess-of-a-token")], None).await;
    assert_eq!(answer(&asked), as_unknown("/api/operator/vault"), "{}", asked.body);
    let (sa, sb) = (serve(a_ctx).await, serve(b_ctx).await);
    let bearer = |t: &str| format!("Bearer {t}");
    let metrics = |t: String| {
        let sa = &sa;
        async move { call(sa, Method::GET, "/metrics", &[("authorization", &t)], None).await.status }
    };
    assert_eq!(metrics(bearer("Mt-1234-from-the-environment")).await, StatusCode::OK, "the vault's token (stored from the environment)");
    assert_eq!(metrics(bearer("Mt-9999-another-value")).await, StatusCode::UNAUTHORIZED);
    let stored = hex::encode(ring::digest::digest(&ring::digest::SHA256, b"Mt-1234-from-the-environment"));
    assert_eq!(metrics(bearer(&stored)).await, StatusCode::UNAUTHORIZED, "what is stored is not the token");
    let production = serve_in(a_ctx, Environment::Production, OPERATIONS).await;
    let auth = bearer("Mt-1234-from-the-environment");
    assert_eq!(call(&production, Method::GET, "/metrics", &[("authorization", &auth)], None).await.status, StatusCode::OK, "in production, with it");

    let operator = |t: &str, s: &Served| {
        let t = t.to_string();
        let router = s.router.clone();
        async move { call(&Served { router }, Method::GET, "/api/operator/vault", &[("x-ferrum-operator-token", &t)], None).await }
    };
    assert_eq!(operator("a-guess-of-a-token", &sa).await.status, StatusCode::FORBIDDEN, "nothing sets it: nobody");
    let made = |overlap: i64| async move {
        let done = ferrum_server::secrets::set(a, find("operator_token"), Given::Made, overlap).await.unwrap();
        assert!(done["secret"].is_object());
        done["shown"].as_str().unwrap().to_string()
    };
    let one = made(0).await;
    let asked = call(&sa, Method::GET, "/api/operator/vault", &[("x-ferrum-operator-token", &one), ("accept-encoding", "gzip, br")], None).await;
    assert!(asked.status == StatusCode::OK && asked.headers.contains_key(header::CONTENT_ENCODING), "as a browser asks: {}", asked.status);
    // Even the right token finds nothing where no zone has the routes: no zones, a zone without `operator`, or an
    // address outside the operator's zone.
    let other = serve_in(a_ctx, Environment::Test, r#"{"operations": {"networks": ["10.9.8.0/24"]}}"#).await;
    for (s, from) in [(&bare, "10.9.8.7"), (&other, "10.9.8.7"), (&sa, "203.0.113.50")] {
        let r = call(s, Method::PUT, "/api/operator/settings/session_days", &[("x-ferrum-operator-token", &one), ("x-forwarded-for", from)], None).await;
        assert_eq!(answer(&r), as_unknown("/api/operator/settings/session_days"), "from {from}: {}", r.body);
    }
    // Tries are counted by address before the token is compared, as sign-ins are: 20 a minute, then refused whatever
    // the token; another network (an IPv4 /24) has its own.
    let from = |ip: &'static str, t: &str| {
        let (sa, t) = (&sa, t.to_string());
        async move { call(sa, Method::GET, "/api/operator/vault", &[("x-ferrum-operator-token", &t), ("x-forwarded-for", ip)], None).await.status }
    };
    for _ in 0..20 {
        assert_eq!(from("10.9.66.66", "a-guess-of-a-token").await, StatusCode::FORBIDDEN);
    }
    assert_eq!(from("10.9.66.66", "a-guess-of-a-token").await, StatusCode::TOO_MANY_REQUESTS, "the 21st in a minute");
    assert_eq!(from("10.9.66.66", &one).await, StatusCode::TOO_MANY_REQUESTS, "the right one too, from there");
    assert_eq!(from("10.9.67.67", &one).await, StatusCode::OK, "another network");
    let status = operator(&one, &sb).await;
    assert_eq!((status.status, status.json()["open"].clone()), (StatusCode::OK, json!(true)), "the other replica takes it at once");
    let two = made(600).await;
    for t in [&one, &two] {
        assert_eq!(operator(t, &sb).await.status, StatusCode::OK, "within the overlap both work");
    }
    let three = made(0).await;
    assert_eq!(operator(&two, &sb).await.status, StatusCode::FORBIDDEN, "no overlap asked: the one before stops");
    assert_eq!(operator(&one, &sa).await.status, StatusCode::FORBIDDEN);
    assert_eq!(operator(&three, &sa).await.status, StatusCode::OK);
    // A copy of the operator's token left in a replica's environment is refused once the vault holds one: changing it
    // there revokes the copy.
    let mut with_copy = config(&url(), "");
    with_copy.settings.as_mut().unwrap()["operator_token"] = json!("Op-0000-a-copy-in-the-environment");
    let copy_ctx = replica(with_copy).await;
    let sc = serve(&copy_ctx).await;
    let copied = |t: &str| {
        let (sc, t) = (&sc, t.to_string());
        async move { call(sc, Method::GET, "/api/operator/vault", &[("x-ferrum-operator-token", &t), ("x-forwarded-for", "10.9.8.8")], None).await.status }
    };
    assert_eq!(copied("Op-0000-a-copy-in-the-environment").await, StatusCode::FORBIDDEN, "the environment's copy");
    assert_eq!(copied(&three).await, StatusCode::OK, "the vault's");
    state(&copy_ctx).kernel.db.close().await;
    // So too the bootstrap token: the configuration's is taken only until the vault holds one.
    let vaults = ferrum_server::secrets::set(a, find("bootstrap_token"), Given::Made, 0).await.unwrap()["shown"].as_str().unwrap().to_string();
    let tenant = json!({"slug": "boot", "name": "Boot", "admin_email": "admin@boot.test", "admin_password": PASSWORD});
    assert_eq!(
        call(&sa, Method::POST, "/api/tenants", &[("x-ferrum-bootstrap-token", "test-bootstrap-token")], Some(tenant.clone())).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(call(&sa, Method::POST, "/api/tenants", &[("x-ferrum-bootstrap-token", &vaults)], Some(tenant)).await.status, StatusCode::CREATED);
    needles.extend([one, two, three.clone(), vaults]);
    // Kept for the routes below.
    OPERATOR.set(three).unwrap();
}

static OPERATOR: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// A secret handed to a library goes only to the host it was set for; moving that address takes it again or clears it.
async fn library(a: &AppState, needles: &mut Vec<String>) {
    // Stored from the environment while the mail server was `localhost`: not given to another host the table names now.
    row(a, "mailer_host", "smtp.example.com").await;
    let ctx = replica(config(&url(), "")).await;
    assert_eq!(mail_password(&ctx), "", "bound to localhost: not given to smtp.example.com");
    state(&ctx).kernel.db.close().await;
    // Set again for it (the task reads it from standard input): given.
    let set = Zeroizing::new("Mp-2468-for-smtp-example".to_string());
    ferrum_server::secrets::set(a, find("mailer_password"), Given::Value(set), 0).await.unwrap();
    let ctx = replica(config(&url(), "")).await;
    assert_eq!(mail_password(&ctx), "Mp-2468-for-smtp-example");
    state(&ctx).kernel.db.close().await;
    // The address moved without it: cleared. With it: set for the new host.
    let host = |value: &str| vec![Change { name: "mailer_host".into(), pool: None, value: Some(value.into()) }];
    let (_, tied) = ferrum_server::settings::change_with(a, &host("smtp.other.example"), &[], "task").await.unwrap();
    assert_eq!(tied, [("mailer_password".to_string(), "cleared")]);
    let ctx = replica(config(&url(), "")).await;
    assert_eq!(mail_password(&ctx), "", "cleared with the move");
    state(&ctx).kernel.db.close().await;
    let given = [("MAILER_PASSWORD".to_string(), Zeroizing::new("Mp-1357-for-the-third".to_string()))];
    let (_, tied) = ferrum_server::settings::change_with(a, &host("smtp.third.example"), &given, "task").await.unwrap();
    assert_eq!(tied, [("mailer_password".to_string(), "set")]);
    let d = a.vault.describe(&a.kernel, Scope::Installation, "mailer_password").await.unwrap().unwrap();
    assert_eq!((d.rule.hosts, d.rule.reveal), (vec!["smtp.third.example".to_string()], true));
    let ctx = replica(config(&url(), "")).await;
    assert_eq!(mail_password(&ctx), "Mp-1357-for-the-third");
    state(&ctx).kernel.db.close().await;
    let refused = ferrum_server::settings::change_with(a, &[Change { name: "session_days".into(), pool: None, value: Some("4".into()) }], &given, "task").await;
    assert!(refused.is_err_and(|e| e.to_string().contains("moves none")), "a secret goes with the address it is sent to");
    // The object store's keys and the model provider's key: revealed at start to the platform alone.
    for (key, value) in
        [("aws_access_key_id", "AKIA-SECRETS-IT-1"), ("aws_secret_access_key", "Sk-1122-object-store"), ("assistant_api_key", "Ak-3344-model-key")]
    {
        ferrum_server::secrets::set(a, find(key), Given::Value(Zeroizing::new(value.into())), 0).await.unwrap();
        needles.push(value.into());
    }
    let ctx = replica(config(&url(), "")).await;
    let st = state(&ctx);
    assert_eq!(st.settings.revealed.get("aws_secret_access_key"), Some("Sk-1122-object-store"));
    assert_eq!(st.settings.revealed.get("assistant_api_key"), Some("Ak-3344-model-key"), "bound to the provider's host, as it is now");
    assert!(!format!("{:?}", st.settings).contains("Sk-1122"), "never shown");
    st.kernel.db.close().await;
    // A replica that started before a change runs the older value, until it starts again: listed as settings are.
    let listed = ferrum_server::settings::listing(a).await.unwrap();
    let mine = listed["replicas"].as_array().unwrap().iter().find(|r| r["id"] == json!(a.live_settings.replica)).cloned().unwrap();
    assert!(mine["older"].as_array().unwrap().contains(&json!("mailer_password")), "{mine}");
    // The settings table and its record never hold a secret.
    let mut tx = a.kernel.db.system_tx().await.unwrap();
    let held: i64 = sqlx::query_scalar("select count(*) from setting s where s::text ~ '(Mp|Mt|Sk|Ak)-[0-9]{4}'").fetch_one(&mut *tx).await.unwrap();
    let recorded: i64 = sqlx::query_scalar("select count(*) from setting_change s where s::text ~ '(Mp|Mt|Sk|Ak)-[0-9]{4}'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!((held, recorded), (0, 0));
    needles.extend(["Mp-2468-for-smtp-example".to_string(), "Mp-1357-for-the-third".to_string()]);
}

/// People and the operator manage secrets through routes that never answer with a value.
async fn people(a: &AppState, a_ctx: &AppContext, needles: &mut Vec<String>) {
    let s = serve(a_ctx).await;
    let tenant = a
        .kernel
        .create_tenant(ferrum_kernel::tenant::NewTenant {
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
    let admin = a.kernel.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
    let admin_ctx = a.kernel.load_ctx(tenant, admin.user, None, "it".into()).await.unwrap();
    a.kernel.create_user(&admin_ctx, "clerk@acme.test", "Clerk", Some(PASSWORD), &[], false).await.unwrap();
    // Signed in from a page: the session's cookie.
    let page = [("sec-fetch-site", "same-origin"), ("origin", "http://localhost")];
    let cookie_of = |email: &'static str| {
        let s = &s;
        async move {
            let a = call(s, Method::POST, "/api/auth/login", &page, Some(json!({"tenant": "acme", "email": email, "password": PASSWORD, "session": "cookie"})))
                .await;
            let name = format!("{}=", ferrum_server::shell::cookie_name(false));
            a.headers
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .find_map(|c| c.strip_prefix(&name).map(|r| format!("{name}{}", r.split(';').next().unwrap_or_default())))
                .unwrap_or_else(|| panic!("{email} signs in: {} {}", a.status, a.body))
        }
    };
    let (boss, clerk) = (cookie_of("admin@acme.test").await, cookie_of("clerk@acme.test").await);
    let as_browser = |cookie: &str| vec![("cookie", cookie.to_string()), ("sec-fetch-site", "same-origin".into()), ("origin", "http://localhost".into())];
    let ask = |cookie: &str, method: Method, path: &str, body: Option<Value>| {
        let (s, h) = (&s, as_browser(cookie));
        let path = path.to_string();
        async move {
            let h: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
            call(s, method, &path, &h, body).await
        }
    };
    assert_eq!(ask(&clerk, Method::GET, "/api/admin/secrets", None).await.status, StatusCode::FORBIDDEN, "not an administrator");
    let listed = ask(&boss, Method::GET, "/api/admin/secrets", None).await;
    assert_eq!((listed.status, listed.json()["owners"].clone()), (StatusCode::OK, json!([])));
    // To change one, confirm it is you: an hour after signing in, the step-up is asked for, with its ways.
    let mut tx = a.kernel.db.system_tx().await.unwrap();
    let aged: Vec<uuid::Uuid> = sqlx::query_scalar("update user_session set step_up_at = now() - interval '1 hour' where tenant_id = $1 returning user_id")
        .bind(tenant)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // What this replica holds of them goes, as a notice from another replica would make it.
    for user in aged {
        a.kernel.invalidate_user(tenant, user);
    }
    let body = json!({"random": 32, "rule": {"overlap_secs": 600}});
    let asked = ask(&boss, Method::PUT, "/api/admin/secrets/platform/webhook-key", Some(body.clone())).await;
    assert_eq!((asked.status, asked.json()["error"].clone()), (StatusCode::FORBIDDEN, json!("step_up")), "{}", asked.body);
    assert!(asked.json()["ways"].is_array(), "with its ways: {}", asked.body);
    assert!(ask(&boss, Method::POST, "/api/auth/step-up", Some(json!({"password": PASSWORD}))).await.status.is_success(), "confirmed");
    let made = ask(&boss, Method::PUT, "/api/admin/secrets/platform/webhook-key", Some(body)).await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let shown = made.json()["shown"].as_str().unwrap().to_string();
    assert_eq!(made.json()["secret"]["rule"]["overlap_secs"], json!(600));
    let rotated = ask(&boss, Method::POST, "/api/admin/secrets/platform/webhook-key/rotate", Some(json!({"random": 32}))).await;
    let shown_again = rotated.json()["shown"].as_str().unwrap().to_string();
    assert!(rotated.status == StatusCode::OK && shown_again != shown);
    let gzip =
        call(&s, Method::GET, "/api/admin/secrets", &[("cookie", boss.as_str()), ("sec-fetch-site", "same-origin"), ("accept-encoding", "gzip, br")], None)
            .await;
    assert!(gzip.status == StatusCode::OK && gzip.headers.contains_key(header::CONTENT_ENCODING), "as a browser asks: {}", gzip.status);
    // Its rule alone: the same right and confirmed identity, the value and version kept, on the trail as `rule`.
    let ruled =
        ask(&boss, Method::PUT, "/api/admin/secrets/platform/webhook-key/rule", Some(json!({"rule": {"overlap_secs": 60, "hosts": ["hooks.example.com"]}})))
            .await;
    assert_eq!(ruled.status, StatusCode::OK, "{}", ruled.body);
    assert_eq!((ruled.json()["secret"]["version"].clone(), ruled.json()["secret"]["rule"]["hosts"].clone()), (json!(2), json!(["hooks.example.com"])));
    assert!(!ruled.body.contains(&shown_again), "no value in it");
    let bad = ask(&boss, Method::PUT, "/api/admin/secrets/platform/webhook-key/rule", Some(json!({"rule": {"hosts": ["Not A Host"]}}))).await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", bad.body);
    assert_eq!(ask(&clerk, Method::PUT, "/api/admin/secrets/platform/webhook-key/rule", Some(json!({"rule": {}}))).await.status, StatusCode::FORBIDDEN);
    let listed = ask(&boss, Method::GET, "/api/admin/secrets", None).await;
    assert_eq!(listed.json()["owners"][0]["owner"], json!("platform"));
    assert_eq!(listed.json()["owners"][0]["secrets"][0]["name"], json!("webhook-key"));
    assert!(listed.json()["owners"][0]["secrets"][0]["set_by_name"].is_string(), "who set it, by name beside the id: {}", listed.body);
    assert!(!listed.body.contains(&shown) && !listed.body.contains(&shown_again), "shown once, never listed");
    // Managed by its owner's code (here the platform's handle stands for an app's): each route refuses, saying so.
    let ws = ferrum_kernel::vault::Scope::Workspace(tenant);
    a.vault.manage(&a.kernel, ws, "webhook-key", true).await.unwrap();
    for (method, path, body) in [
        (Method::PUT, "/api/admin/secrets/platform/webhook-key", Some(json!({"random": 32}))),
        (Method::POST, "/api/admin/secrets/platform/webhook-key/rotate", Some(json!({"random": 32}))),
        (Method::PUT, "/api/admin/secrets/platform/webhook-key/rule", Some(json!({"rule": {}}))),
        (Method::DELETE, "/api/admin/secrets/platform/webhook-key", None),
    ] {
        let r = ask(&boss, method.clone(), path, body.clone()).await;
        assert_eq!(r.status, StatusCode::CONFLICT, "{method} {path}: {}", r.body);
        // A code and the app's id of their own, for a screen to say it in the person's language, and the sentence beside.
        assert_eq!((r.json()["error"].clone(), r.json()["app"].clone()), (json!("managed_by_app"), json!("platform")), "{}", r.body);
        assert!(r.json()["message"].as_str().is_some_and(|m| m.contains("managed by its app, platform")), "with why: {}", r.body);
        // As a browser asks: the same refusal, compressed.
        let browser = as_browser(&boss);
        let mut h: Vec<(&str, &str)> = browser.iter().map(|(k, v)| (*k, v.as_str())).collect();
        h.push(("accept-encoding", "gzip, br"));
        let gz = call(&s, method.clone(), path, &h, body).await;
        assert!(gz.status == StatusCode::CONFLICT && gz.headers.contains_key(header::CONTENT_ENCODING), "{method} {path} as a browser: {}", gz.status);
    }
    assert_eq!(ask(&boss, Method::GET, "/api/admin/secrets", None).await.json()["owners"][0]["secrets"][0]["managed_by"], json!("platform"));
    a.vault.manage(&a.kernel, ws, "webhook-key", false).await.unwrap();
    let gone = ask(&boss, Method::DELETE, "/api/admin/secrets/platform/webhook-key", None).await;
    assert_eq!(gone.json()["removed"], json!(true));
    needles.extend([shown, shown_again]);

    // The operator: the installation's secrets by owner and the platform's own with their sources; set and removed.
    let token = OPERATOR.get().unwrap().clone();
    let op = |method: Method, path: &str, body: Option<Value>| {
        let (s, token, path) = (&s, token.clone(), path.to_string());
        async move { call(s, method, &path, &[("x-ferrum-operator-token", &token)], body).await }
    };
    let listed = op(Method::GET, "/api/operator/secrets", None).await;
    assert_eq!(listed.status, StatusCode::OK);
    let platform = listed.json()["platform"].clone();
    let of = |key: &str| platform.as_array().unwrap().iter().find(|p| p["key"] == key).cloned().unwrap();
    assert_eq!((of("mailer_password")["source"].clone(), of("mailer_password")["applies"].clone()), (json!("vault"), json!("start")));
    assert_eq!((of("operator_token")["source"].clone(), of("operator_token")["applies"].clone()), (json!("vault"), json!("live")));
    let set = op(Method::PUT, "/api/operator/secrets/platform/mailer_password", Some(json!({"value": "Mp-8642-from-the-operator"}))).await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.body);
    assert!(!set.body.contains("Mp-8642"), "a value goes in, and never comes back");
    let plugin =
        op(Method::PUT, "/api/operator/secrets/plugin:pay/key", Some(json!({"value": "Pk-9911-plugin-key", "rule": {"hosts": ["api.pay.example"]}}))).await;
    assert_eq!(plugin.status, StatusCode::OK, "{}", plugin.body);
    let listed = op(Method::GET, "/api/operator/secrets", None).await;
    for value in ["Mp-8642", "Pk-9911", "Mt-1234", "Ak-3344", "Sk-1122", LEGACY] {
        assert!(!listed.body.contains(value), "{value} in {}", listed.body);
    }
    assert_eq!(op(Method::DELETE, "/api/operator/secrets/plugin:pay/key", None).await.json()["removed"], json!(true));
    // An address moved with its secret, over HTTP.
    let moved = op(Method::PUT, "/api/operator/settings/mailer_host", Some(json!({"value": "smtp.fourth.example", "secret": "Mp-9753-fourth"}))).await;
    assert_eq!((moved.status, moved.json()["secrets"].clone()), (StatusCode::OK, json!([{"secret": "mailer_password", "now": "set"}])), "{}", moved.body);
    assert!(!moved.body.contains("Mp-9753"));
    let status = op(Method::GET, "/api/operator/vault", None).await;
    assert!(status.json()["slots"].as_array().is_some_and(|s| !s.is_empty()) && !status.body.contains("wrapped"), "{}", status.body);
    needles.extend(["Mp-8642-from-the-operator".to_string(), "Pk-9911-plugin-key".to_string(), "Mp-9753-fourth".to_string()]);
}

/// `the_records_door_digests_a_secret_field_keyed` (TP-S2): a create's receipt kept for 30 days holds no plain digest of
/// a secret value; the same body again is the same request, another value other input.
async fn records_door(a: &AppState, a_ctx: &AppContext) {
    let s = serve(a_ctx).await;
    let new = ferrum_kernel::tenant::NewTenant {
        slug: "safes".into(),
        name: "Safes".into(),
        admin_email: "admin@safes.test".into(),
        admin_password: PASSWORD.into(),
        admin_name: None,
        packages: vec!["keep".into()],
        settings: None,
    };
    let tenant = a.kernel.create_tenant(new).await.unwrap();
    let signed = call(&s, Method::POST, "/api/auth/login", &[], Some(json!({"tenant": "safes", "email": "admin@safes.test", "password": PASSWORD}))).await;
    let bearer = format!("Bearer {}", signed.json()["token"].as_str().unwrap());
    let make = |pin: &str| {
        let (s, bearer) = (&s, bearer.clone());
        let body = json!({"label": "Vault 1", "pin": pin});
        async move { call(s, Method::POST, "/api/v1/Safe", &[("authorization", &bearer), ("idempotency-key", "safe-key-0001")], Some(body)).await }
    };
    let made = make("4821").await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
    let mut plain = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut plain, b"Safe");
    sha2::Digest::update(&mut plain, ferrum_kernel::value::canonical_json(&json!({"label": "Vault 1", "pin": "4821"})));
    let plain = hex::encode(sha2::Digest::finalize(plain));
    let mut tx = a.kernel.db.tenant_tx(tenant).await.unwrap();
    let kept: String = sqlx::query_scalar("select request_hash from idempotency_key where key = 'safe-key-0001'").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_ne!(kept, plain, "the records door's digest of a secret field is keyed");
    let again = make("4821").await;
    assert!(again.status == StatusCode::OK && again.headers.contains_key("idempotent-replayed"), "the same pin again: {} {}", again.status, again.body);
    assert!(!make("4822").await.status.is_success(), "another pin under the key is another request");
}

/// The tables of `schema` that hold any of `needles` (each looked for as it is written in a row's text).
async fn holding(st: &AppState, schema: &str, needles: &[String]) -> Vec<String> {
    let like: Vec<String> = needles.iter().map(|s| format!("%{s}%")).collect();
    let mut tx = st.kernel.db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.system', 'on', true), set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
    let tables: Vec<String> =
        sqlx::query_scalar("select table_name::text from information_schema.tables where table_schema = $1 and table_type = 'BASE TABLE' order by 1")
            .bind(schema)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    let mut out = Vec::new();
    for table in tables {
        let hits: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from \"{schema}\".\"{table}\" x where x::text ilike any($1)")))
            .bind(&like)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        if hits > 0 {
            out.push(format!("{schema}.{table}: {hits}"));
        }
    }
    tx.commit().await.unwrap();
    out
}
