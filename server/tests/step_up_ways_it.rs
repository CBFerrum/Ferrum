//! The ways to confirm it is you (docs/operations.md, "Asking again before sensitive work"), through the whole server,
//! booted as the binary boots it, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_test`; the database is reset): the ways a step-up refusal and
//! `GET /api/auth/step-up/ways` list for people with only a passkey, only single sign-on, only a password, all three, and
//! none; a marked change refused before each way and let through after it; what the audit trail says; and the routes not
//! served where the zone admits no staff. A software authenticator and a stand-in OpenID Connect provider play the device
//! and the provider; a browser and a real provider are not here.

mod common;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::zones::Zones;
use ferrum_kernel::{Kernel, sso::SsoProviderInput};
use ferrum_server::app::App;
use ferrum_server::state::AppState;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use loco_rs::app::{AppContext, Hooks};
use loco_rs::environment::Environment;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "Correct-Horse-9";
const OFFICE: &str = "erp.acme.test";
const PUBLIC: &str = "www.acme.test";
const ORIGIN: &str = "http://erp.acme.test";
const ZONES: &str = r#"{"office": {}, "public": {"staff": "none"}}"#;
const CLIENT_ID: &str = "ferrum-app";

#[test]
fn step_up_ways() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

// ── the stand-in provider: discovery, its key, and codes for ID tokens ──

struct Idp {
    issuer: String,
    key: Vec<u8>,
    jwk: Value,
    codes: HashMap<String, (Value, String, String)>,
}

type Shared = Arc<Mutex<Idp>>;

async fn well_known(State(s): State<Shared>) -> Json<Value> {
    let issuer = s.lock().unwrap().issuer.clone();
    Json(json!({
        "issuer": issuer, "authorization_endpoint": format!("{issuer}/authorize"), "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks"), "token_endpoint_auth_methods_supported": ["client_secret_basic"],
    }))
}

async fn jwks(State(s): State<Shared>) -> Json<Value> {
    Json(json!({"keys": [s.lock().unwrap().jwk.clone()]}))
}

/// The code for an ID token, once, to the return address and PKCE verifier it was asked with.
async fn token(State(s): State<Shared>, Form(f): Form<HashMap<String, String>>) -> (StatusCode, Json<Value>) {
    let mut idp = s.lock().unwrap();
    let Some((claims, challenge, redirect)) = f.get("code").and_then(|c| idp.codes.remove(c)) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"})));
    };
    let verifier = f.get("code_verifier").cloned().unwrap_or_default();
    if f.get("redirect_uri") != Some(&redirect) || B64.encode(Sha256::digest(verifier.as_bytes())) != challenge {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"})));
    }
    let mut h = Header::new(Algorithm::ES256);
    h.kid = Some("k1".into());
    let id_token = jsonwebtoken::encode(&h, &claims, &EncodingKey::from_ec_der(&idp.key)).unwrap();
    (StatusCode::OK, Json(json!({"access_token": "at", "token_type": "Bearer", "id_token": id_token})))
}

async fn start_idp() -> Shared {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let rng = SystemRandom::new();
    let key = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap().as_ref().to_vec();
    let public = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &key, &rng).unwrap().public_key().as_ref().to_vec();
    let jwk =
        json!({"kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": "k1", "x": B64.encode(&public[1..33]), "y": B64.encode(&public[33..65])});
    let shared: Shared = Arc::new(Mutex::new(Idp { issuer, key, jwk, codes: HashMap::new() }));
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(well_known))
        .route("/jwks", get(jwks))
        .route("/token", post(token))
        .with_state(shared.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    shared
}

/// What the provider does once the person signs in there: a code for an ID token with `claims` → the state and the code.
fn authorize(idp: &Shared, url: &str, claims: Value) -> (String, String) {
    let q: HashMap<String, String> = reqwest::Url::parse(url).unwrap().query_pairs().into_owned().collect();
    let mut idp = idp.lock().unwrap();
    let now = chrono::Utc::now().timestamp();
    let mut c = json!({"iss": idp.issuer, "aud": CLIENT_ID, "nonce": q["nonce"], "iat": now, "exp": now + 300});
    for (k, v) in claims.as_object().unwrap() {
        c[k] = v.clone();
    }
    let code = format!("code-{}", Uuid::now_v7());
    idp.codes.insert(code.clone(), (c, q["code_challenge"].clone(), q["redirect_uri"].clone()));
    (q["state"].clone(), code)
}

// ── a device keeping one passkey ──

struct Device {
    rng: SystemRandom,
    pair: EcdsaKeyPair,
    id: Vec<u8>,
    count: u32,
}

impl Device {
    /// A new passkey of `user`'s for this host, kept as registering would keep it.
    async fn kept_for(k: &Kernel, tenant: Uuid, user: Uuid) -> Device {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        let id = [*Uuid::new_v4().as_bytes(), *Uuid::new_v4().as_bytes()].concat();
        let mut tx = k.db.system_tx().await.unwrap();
        sqlx::query(
            "insert into passkey (tenant_id, id, user_id, credential_id, rp_id, public_key, alg, sign_count, name, transports) \
             values ($1, $2, $3, $4, $5, $6, -7, 0, 'it', '{internal}')",
        )
        .bind(tenant)
        .bind(Uuid::now_v7())
        .bind(user)
        .bind(&id)
        .bind(OFFICE)
        .bind(pair.public_key().as_ref())
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        Device { rng, pair, id, count: 0 }
    }

    /// The browser's answer to `navigator.credentials.get` with `options`, the person verified.
    fn get(&mut self, options: &Value, user: Uuid) -> Value {
        self.count += 1;
        let client = json!({"type": "webauthn.get", "challenge": options["publicKey"]["challenge"], "origin": ORIGIN}).to_string();
        let mut auth = Sha256::digest(OFFICE.as_bytes()).to_vec();
        auth.push(0x01 | 0x04);
        auth.extend(self.count.to_be_bytes());
        let sig = self.pair.sign(&self.rng, &[&auth[..], &Sha256::digest(client.as_bytes())].concat()).unwrap();
        json!({
            "id": B64.encode(&self.id), "rawId": B64.encode(&self.id), "type": "public-key",
            "response": {"clientDataJSON": B64.encode(client), "authenticatorData": B64.encode(auth), "signature": B64.encode(sig.as_ref()), "userHandle": B64.encode(user.as_bytes())},
        })
    }
}

// ── the server ──

struct Server {
    router: axum::Router,
}

#[derive(Clone, Copy)]
enum Cred<'a> {
    None,
    Bearer(&'a str),
    /// The browser's session cookie, from a page of this site.
    Cookie(&'a str),
    /// No session yet, from a page of this site (signing in).
    Page,
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

    fn cookie(&self) -> Option<String> {
        let name = format!("{}=", ferrum_server::shell::cookie_name(false));
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|c| c.strip_prefix(&name).map(|rest| rest.split(';').next().unwrap_or_default().to_string()).filter(|v| !v.is_empty()))
    }
}

async fn call(s: &Server, method: Method, host: &str, path: &str, cred: Cred<'_>, body: Option<Value>) -> Answer {
    let mut req = Request::builder().method(method).uri(path).header(header::HOST, host).header("x-forwarded-for", "10.1.2.3");
    match cred {
        Cred::None => {}
        Cred::Bearer(t) => req = req.header(header::AUTHORIZATION, format!("Bearer {t}")),
        Cred::Cookie(t) => {
            req = req
                .header(header::COOKIE, format!("{}={t}", ferrum_server::shell::cookie_name(false)))
                .header("sec-fetch-site", "same-origin")
                .header(header::ORIGIN, format!("http://{host}"))
        }
        Cred::Page => req = req.header("sec-fetch-site", "same-origin").header(header::ORIGIN, format!("http://{host}")),
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

async fn exec(k: &Kernel, sql: &str) {
    let mut tx = k.db.system_tx().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string())).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

/// Ages every live session's step-up by an hour, so that marked work asks again (and drops what this replica holds of
/// them, as a notice from another replica would).
async fn age(k: &Kernel) {
    let mut tx = k.db.system_tx().await.unwrap();
    let aged: Vec<(Uuid, Uuid)> =
        sqlx::query_as("update user_session set step_up_at = now() - interval '1 hour' where revoked_at is null returning tenant_id, user_id")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    for (tenant, user) in aged {
        k.invalidate_user(tenant, user);
    }
}

fn asked(a: &Answer) -> bool {
    a.status == StatusCode::FORBIDDEN && a.json()["error"] == "step_up"
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
    let zones = Arc::new(ferrum_server::zone::resolve(Zones::parse(ZONES, "", "", "").unwrap(), &k).await.expect("zones"));
    k.set_zones(zones.clone());
    let s = {
        let st = AppState { zones, ..st.clone() };
        let ctx = AppContext::builder(Environment::Test, Default::default(), base.config.clone()).build();
        ctx.shared_store.insert(st);
        let router = App::routes(&ctx).to_router::<App>(ctx.clone(), axum::Router::new()).expect("router");
        Server { router: App::after_routes(router, &ctx).await.expect("layers") }
    };

    let acme = k
        .create_tenant(NewTenant {
            slug: "acme".into(),
            name: "Acme".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: PASSWORD.into(),
            admin_name: None,
            packages: vec!["contacts".into()],
            settings: None,
        })
        .await
        .unwrap();
    for (host, zone) in [(OFFICE, "office"), (PUBLIC, "public")] {
        k.add_operator_host("acme", host, "shell", Some(zone)).await.unwrap();
    }
    let root = k.login("acme", "admin@acme.test", PASSWORD).await.unwrap();
    let admin = k.load_ctx(acme, root.user, None, "it".into()).await.unwrap();
    let entity = json!({"name": "XSecret", "label": "XSecret", "kind": "master", "title_field": "title", "fields": [{"name": "title", "type": "data", "in_list": true}]});
    let ops: Vec<ferrum_kernel::customize::NewCustomization> =
        serde_json::from_value(json!([{"entity": "XSecret", "op": {"op": "define_entity", "entity": entity}}])).unwrap();
    k.batch_customizations(&admin, ops, false).await.map_err(|(_, e)| e.to_string()).unwrap();
    let admin = k.load_ctx(acme, root.user, None, "it".into()).await.unwrap();
    let rule = ferrum_meta::PermissionDef { entity: "XSecret".into(), read: true, write: true, create: true, ..Default::default() };
    k.set_role_permission(&admin, "Desk", &rule).await.unwrap();
    let mut patch = serde_json::Map::new();
    patch.insert("step_up".into(), json!({"entities": {"XSecret": {"work": ["write"]}}}));
    k.update_tenant_settings(&admin, &patch).await.unwrap();
    let record = k.insert(&admin, "XSecret", json!({"title": "one"}).as_object().unwrap().clone()).await.unwrap()["id"].as_str().unwrap().to_string();
    let path = format!("/api/v1/XSecret/{record}");
    let change = |cookie: &str| {
        let (s, path, cookie) = (&s, path.clone(), cookie.to_string());
        async move { call(s, Method::PATCH, OFFICE, &path, Cred::Cookie(&cookie), Some(json!({"title": "changed"}))).await }
    };

    let idp = start_idp().await;
    let issuer = idp.lock().unwrap().issuer.clone();
    let input: SsoProviderInput =
        serde_json::from_value(json!({"label": "Okta", "issuer": issuer, "client_id": CLIENT_ID, "client_secret": "s3cret"})).unwrap();
    let provider = k.save_sso_provider(&admin, None, input).await.unwrap();
    let person = |email: &'static str, password: Option<&'static str>| {
        let (k, admin) = (&k, &admin);
        async move { k.create_user(admin, email, email, password, &["Desk".into()], false).await.unwrap() }
    };
    // Signing in on the office's sign-in page, each way → the browser's session cookie.
    let by_password = |email: &'static str| {
        let s = &s;
        async move {
            let a = call(
                s,
                Method::POST,
                OFFICE,
                "/api/auth/login",
                Cred::Page,
                Some(json!({"tenant": "acme", "email": email, "password": PASSWORD, "session": "cookie"})),
            )
            .await;
            a.cookie().unwrap_or_else(|| panic!("{email} signs in: {} {}", a.status, a.body))
        }
    };
    let by_provider = |sub: &'static str, email: &'static str| {
        let (s, idp) = (&s, &idp);
        async move {
            let binding = "browser-binding-0123456789";
            let start = json!({"tenant": "acme", "provider": provider, "redirect_uri": format!("{ORIGIN}/app/login"), "binding": binding});
            let url = call(s, Method::POST, OFFICE, "/api/auth/sso/start", Cred::Page, Some(start)).await.json()["url"].as_str().unwrap().to_string();
            let (state, code) = authorize(idp, &url, json!({"sub": sub, "email": email, "email_verified": true}));
            let finish = json!({"state": state, "code": code, "binding": binding, "session": "cookie"});
            let a = call(s, Method::POST, OFFICE, "/api/auth/sso/finish", Cred::Page, Some(finish)).await;
            a.cookie().unwrap_or_else(|| panic!("{email} signs in through the provider: {} {}", a.status, a.body))
        }
    };

    // ── Five people: a passkey only, single sign-on only, a password only, all three, none ──
    let pam = person("pam@acme.test", None).await;
    let mut pams = Device::kept_for(&k, acme, pam).await;
    let o = call(&s, Method::POST, OFFICE, "/api/auth/passkey/options", Cred::Page, None).await.json();
    let a = call(
        &s,
        Method::POST,
        OFFICE,
        "/api/auth/passkey",
        Cred::Page,
        Some(json!({"request": o["request"], "credential": pams.get(&o, pam), "session": "cookie"})),
    )
    .await;
    let pam_cookie = a.cookie().unwrap_or_else(|| panic!("Pam signs in with her passkey: {} {}", a.status, a.body));
    let sol = person("sol@acme.test", None).await;
    let sol_cookie = by_provider("okta-sol", "sol@acme.test").await;
    let pat = person("pat@acme.test", Some(PASSWORD)).await;
    let pat_cookie = by_password("pat@acme.test").await;
    let ali = person("ali@acme.test", Some(PASSWORD)).await;
    let mut alis = Device::kept_for(&k, acme, ali).await;
    by_provider("okta-ali", "ali@acme.test").await;
    let ali_cookie = by_password("ali@acme.test").await;
    let nia = person("nia@acme.test", Some(PASSWORD)).await;
    let nia_cookie = by_password("nia@acme.test").await;
    exec(&k, &format!("update app_user set password_hash = null where id = '{nia}'")).await;
    age(&k).await;

    let passkey = json!({"way": "passkey"});
    let password = json!({"way": "password", "code": false});
    let okta = json!({"way": "provider", "name": "Okta"});
    for (who, cookie, ways) in [
        ("only a passkey", &pam_cookie, vec![passkey.clone()]),
        ("only single sign-on", &sol_cookie, vec![okta.clone()]),
        ("only a password", &pat_cookie, vec![password.clone()]),
        ("all three, a passkey first", &ali_cookie, vec![passkey.clone(), password.clone(), okta.clone()]),
        ("none", &nia_cookie, vec![]),
    ] {
        let refused = change(cookie).await;
        assert!(asked(&refused), "{who}: {} {}", refused.status, refused.body);
        assert_eq!(refused.json()["ways"], json!(ways), "{who}: {}", refused.body);
        let listed = call(&s, Method::GET, OFFICE, "/api/auth/step-up/ways", Cred::Cookie(cookie), None).await;
        assert_eq!((listed.status, listed.json()["ways"].clone()), (StatusCode::OK, json!(ways)), "{who}: {}", listed.body);
        if ways.is_empty() {
            for said in [refused.json()["message"].clone(), listed.json()["message"].clone()] {
                assert!(said.as_str().unwrap().contains("ask an administrator"), "{who}: said plainly: {said}");
            }
        } else {
            assert_eq!(refused.json()["message"], json!("confirm it is you to do this"), "{who}");
        }
    }
    // A bearer token with no browser session behind it cannot step up, and is told so.
    let token = call(&s, Method::POST, OFFICE, "/api/auth/login", Cred::None, Some(json!({"tenant": "acme", "email": "pat@acme.test", "password": PASSWORD})))
        .await
        .json()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let a = call(&s, Method::PATCH, OFFICE, &path, Cred::Bearer(&token), Some(json!({"title": "x"}))).await;
    assert!(asked(&a) && a.json()["ways"] == json!([]), "{}", a.body);
    assert!(a.json()["message"].as_str().unwrap().contains("from a page of this site"), "{}", a.body);

    // ── A passkey: refused before, let through after; a wrong one is a failed step-up ──
    let pk_options = call(&s, Method::POST, OFFICE, "/api/auth/step-up/passkey/options", Cred::Cookie(&pam_cookie), None).await.json();
    assert_eq!(
        (pk_options["publicKey"]["userVerification"].clone(), pk_options["publicKey"]["allowCredentials"][0]["id"].clone()),
        (json!("required"), json!(B64.encode(&pams.id)))
    );
    let mut forged = pams.get(&pk_options, pam);
    forged["response"]["signature"] = json!(B64.encode([7u8; 64]));
    let a = call(
        &s,
        Method::POST,
        OFFICE,
        "/api/auth/step-up/passkey",
        Cred::Cookie(&pam_cookie),
        Some(json!({"request": pk_options["request"], "credential": forged})),
    )
    .await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED, "{}", a.body);
    assert!(asked(&change(&pam_cookie).await), "still asked");
    let pk_options = call(&s, Method::POST, OFFICE, "/api/auth/step-up/passkey/options", Cred::Cookie(&pam_cookie), None).await.json();
    let a = call(
        &s,
        Method::POST,
        OFFICE,
        "/api/auth/step-up/passkey",
        Cred::Cookie(&pam_cookie),
        Some(json!({"request": pk_options["request"], "credential": pams.get(&pk_options, pam)})),
    )
    .await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.body);
    let a = change(&pam_cookie).await;
    assert!(a.status.is_success(), "after a passkey: {} {}", a.status, a.body);
    // Ali's passkey proves nothing for Pam's session.
    age(&k).await;
    let pk_options = call(&s, Method::POST, OFFICE, "/api/auth/step-up/passkey/options", Cred::Cookie(&pam_cookie), None).await.json();
    let a = call(
        &s,
        Method::POST,
        OFFICE,
        "/api/auth/step-up/passkey",
        Cred::Cookie(&pam_cookie),
        Some(json!({"request": pk_options["request"], "credential": alis.get(&pk_options, ali)})),
    )
    .await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED, "another person's passkey: {}", a.body);

    // ── Signing in again at the provider, in the small window: its return address, the done page, then the dialog's finish ──
    let dialog = "dlg-0123456789abcdefXY";
    assert!(asked(&change(&sol_cookie).await));
    let started = call(&s, Method::POST, OFFICE, "/api/auth/step-up/provider", Cred::Cookie(&sol_cookie), Some(json!({"dialog": dialog}))).await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    let to_idp = started.json()["url"].as_str().unwrap().to_string();
    let q: HashMap<String, String> = reqwest::Url::parse(&to_idp).unwrap().query_pairs().into_owned().collect();
    assert_eq!((q["prompt"].as_str(), q["max_age"].as_str(), q["redirect_uri"].as_str()), ("login", "0", &*format!("{ORIGIN}/app/login")));
    assert!(!to_idp.contains(dialog), "the dialog's value never goes to the provider");
    let bad = call(&s, Method::POST, OFFICE, "/api/auth/step-up/provider", Cred::Cookie(&sol_cookie), Some(json!({"dialog": "short"}))).await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", bad.body);
    let now = chrono::Utc::now().timestamp();
    let (state, code) = authorize(&idp, &to_idp, json!({"sub": "okta-sol", "auth_time": now}));
    // The provider sends the window to the return address registered with it, without the session's cookie.
    let landed = call(&s, Method::GET, OFFICE, &format!("/app/login?state={state}&code={code}"), Cred::None, None).await;
    assert_eq!(landed.status, StatusCode::SEE_OTHER, "{}", landed.body);
    assert!(landed.location().starts_with("/api/auth/step-up/provider/back?"), "{}", landed.location());
    let back = call(&s, Method::GET, OFFICE, &landed.location(), Cred::None, None).await;
    assert_eq!(back.status, StatusCode::SEE_OTHER, "{}", back.body);
    let done = back.location();
    assert!(done.starts_with(&format!("{ORIGIN}/api/auth/step-up/provider/done?dialog={dialog}&")), "{done}");
    assert_eq!(back.headers.get(header::REFERRER_POLICY).unwrap(), "no-referrer");
    let handed: HashMap<String, String> = reqwest::Url::parse(&done).unwrap().query_pairs().into_owned().collect();
    let page = call(&s, Method::GET, OFFICE, done.trim_start_matches(ORIGIN), Cred::None, None).await;
    assert_eq!(page.status, StatusCode::OK);
    let csp = page.headers.get("content-security-policy").unwrap().to_str().unwrap().to_string();
    assert!(csp.contains("default-src 'none'") && csp.contains("script-src 'sha256-") && csp.contains("frame-ancestors 'none'"), "{csp}");
    assert!(page.body.contains("BroadcastChannel(\"ferrum-step-up\")") && !page.body.contains(&handed["code"]), "the page holds no proof of its own");
    // Another person's session cannot take Sol's proof; Sol's can, once.
    let finish = json!({"state": handed["state"], "code": handed["code"]});
    let a = call(&s, Method::POST, OFFICE, "/api/auth/step-up/provider/finish", Cred::Cookie(&pat_cookie), Some(finish.clone())).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED, "{}", a.body);
    let a = call(&s, Method::POST, OFFICE, "/api/auth/step-up/provider/finish", Cred::Cookie(&sol_cookie), Some(finish.clone())).await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.body);
    let a = change(&sol_cookie).await;
    assert!(a.status.is_success(), "after signing in again: {} {}", a.status, a.body);
    let a = call(&s, Method::POST, OFFICE, "/api/auth/step-up/provider/finish", Cred::Cookie(&sol_cookie), Some(finish)).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED, "once: {}", a.body);
    // A provider that answered from a session it had: refused in the window, plainly, and nothing changes.
    age(&k).await;
    let to_idp = call(&s, Method::POST, OFFICE, "/api/auth/step-up/provider", Cred::Cookie(&sol_cookie), Some(json!({"dialog": dialog}))).await.json()["url"]
        .as_str()
        .unwrap()
        .to_string();
    let (state, code) = authorize(&idp, &to_idp, json!({"sub": "okta-sol", "auth_time": now - 3600}));
    let page = call(&s, Method::GET, OFFICE, &format!("/api/auth/step-up/provider/back?state={state}&code={code}"), Cred::None, None).await;
    assert!(page.status == StatusCode::UNAUTHORIZED && page.body.contains("did not ask you to sign in again"), "{} {}", page.status, page.body);
    assert!(asked(&change(&sol_cookie).await));

    // ── A password, as before; under the workspace's staff networks too, from inside them ──
    exec(
        &k,
        &format!(
            "update tenant set settings = settings || '{{\"staff_networks\": {{\"networks\": [\"10.0.0.0/8\"], \"mode\": \"enforce\"}}}}' where id = '{acme}'"
        ),
    )
    .await;
    let a = call(&s, Method::POST, OFFICE, "/api/auth/step-up", Cred::Cookie(&pat_cookie), Some(json!({"password": "Wrong-Horse-9"}))).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED, "{}", a.body);
    let a = call(&s, Method::POST, OFFICE, "/api/auth/step-up", Cred::Cookie(&pat_cookie), Some(json!({"password": PASSWORD}))).await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.body);
    assert!(change(&pat_cookie).await.status.is_success());
    exec(&k, &format!("update tenant set settings = settings - 'staff_networks' where id = '{acme}'")).await;
    // Single sign-on required of Sol: no password or passkey proves it, and Ali keeps only the provider too.
    k.update_tenant_settings(&admin, json!({"require_sso": true}).as_object().unwrap()).await.unwrap();
    age(&k).await;
    let refused = change(&ali_cookie).await;
    assert_eq!(refused.json()["ways"], json!([okta.clone()]), "{}", refused.body);
    let a = call(&s, Method::POST, OFFICE, "/api/auth/step-up", Cred::Cookie(&ali_cookie), Some(json!({"password": PASSWORD}))).await;
    assert_eq!(a.status, StatusCode::FORBIDDEN, "a password proves nothing here now: {}", a.body);
    let a = call(&s, Method::POST, OFFICE, "/api/auth/step-up/passkey/options", Cred::Cookie(&ali_cookie), None).await;
    assert_eq!(a.status, StatusCode::FORBIDDEN, "nor a passkey: {}", a.body);
    k.update_tenant_settings(&admin, json!({"require_sso": false}).as_object().unwrap()).await.unwrap();
    // Staff networks that leave this address out refuse every way, as any staff request, before anything is tried.
    exec(&k, &format!("update tenant set settings = settings || '{{\"staff_networks\": {{\"networks\": [\"192.168.0.0/16\"]}}}}' where id = '{acme}'")).await;
    k.invalidate_tenant_users(acme);
    for (method, route, cookie, body) in [
        (Method::GET, "/api/auth/step-up/ways", &pam_cookie, None),
        (Method::POST, "/api/auth/step-up/passkey/options", &pam_cookie, None),
        (Method::POST, "/api/auth/step-up/provider", &sol_cookie, Some(json!({"dialog": dialog}))),
        (Method::POST, "/api/auth/step-up", &pat_cookie, Some(json!({"password": PASSWORD}))),
    ] {
        let a = call(&s, method.clone(), OFFICE, route, Cred::Cookie(cookie), body).await;
        assert!(a.status == StatusCode::FORBIDDEN && a.json()["error"] == "network", "{method} {route}: {} {}", a.status, a.body);
    }
    exec(&k, &format!("update tenant set settings = settings - 'staff_networks' where id = '{acme}'")).await;
    k.invalidate_tenant_users(acme);

    // ── On the trail: each step-up with its way, address and zone; failed ones too ──
    let mut tx = k.db.system_tx().await.unwrap();
    let events: Vec<(Uuid, String, Value)> =
        sqlx::query_as("select record_id, action, diff from audit_event where category = 'sign_in' and action in ('stepped_up', 'step_up_failed') order by id")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    for (who, way) in [(pam, "passkey"), (sol, "provider"), (pat, "password")] {
        let e = events.iter().find(|e| e.0 == who && e.1 == "stepped_up").unwrap_or_else(|| panic!("{way}: {events:?}"));
        assert_eq!((e.2["method"].clone(), e.2["address"].clone(), e.2["zone"].clone()), (json!(way), json!("10.1.2.3"), json!("office")), "{way}");
    }
    for (who, way) in [(pam, "passkey"), (sol, "provider"), (pat, "password")] {
        assert!(events.iter().any(|e| e.0 == who && e.1 == "step_up_failed" && e.2["method"] == way), "a failed {way} step-up: {events:?}");
    }
    let wrong = events.iter().find(|e| e.0 == pat && e.1 == "step_up_failed").unwrap();
    assert_eq!((wrong.2["address"].clone(), wrong.2["zone"].clone()), (json!("10.1.2.3"), json!("office")), "where it came from");
    let text = format!("{events:?}");
    assert!(
        !text.contains(&pams.id.iter().map(|b| format!("{b:02x}")).collect::<String>()) && !text.contains("okta-sol"),
        "no credential or subject on the trail"
    );

    // ── Where the zone admits no staff, none of it is served; the provider's return address is, as for signing in ──
    for (method, route, body) in [
        (Method::GET, "/api/auth/step-up/ways", None),
        (Method::POST, "/api/auth/step-up/passkey/options", None),
        (Method::POST, "/api/auth/step-up/passkey", Some(json!({"request": Uuid::now_v7(), "credential": {}}))),
        (Method::POST, "/api/auth/step-up/provider", Some(json!({"dialog": dialog}))),
        (Method::POST, "/api/auth/step-up/provider/finish", Some(json!({"state": "su.x", "code": "y"}))),
        (Method::GET, "/api/auth/step-up/provider/done?dialog=x&state=y&code=z", None),
    ] {
        for cred in [Cred::Cookie(&pat_cookie), Cred::None] {
            let a = call(&s, method.clone(), PUBLIC, route, cred, body.clone()).await;
            assert_eq!(a.status, StatusCode::NOT_FOUND, "{method} {route}: {}", a.body);
        }
    }
    let a = call(&s, Method::GET, PUBLIC, "/api/auth/step-up/provider/back?state=su.not-one-of-ours&code=x", Cred::None, None).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED, "reachable, and refused as no answer of ours: {}", a.body);
    let _ = (sol, nia);
}
