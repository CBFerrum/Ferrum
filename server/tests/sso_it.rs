//! Single sign-on against a real PostgreSQL (`FERRUM_SSO_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_sso_test`; the database is reset) and a fake
//! OpenID Connect provider on a local port: setup checks, the sign-in round trip with PKCE and
//! client authentication, single use and the browser binding, every ID token check, linking and
//! unlinking, account creation, email rules, and requiring single sign-on.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ferrum_kernel::auth::{Session, SignIn};
use ferrum_kernel::secret::SecretKey;
use ferrum_kernel::sso::SsoProviderInput;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const CLIENT_ID: &str = "ferrum-app";
const CLIENT_SECRET: &str = "s3cret:with/odd chars";
const REDIRECT: &str = "https://erp.example.test/app/login";
const BINDING: &str = "browser-binding-0123456789";

struct Grant {
    claims: Value,
    challenge: String,
    redirect_uri: String,
    rogue: bool,
    kid: String,
}

/// The fake provider: one published signing key (and a rogue one it never publishes).
struct Idp {
    issuer: String,
    key: Vec<u8>,
    rogue: Vec<u8>,
    jwk: Value,
    codes: HashMap<String, Grant>,
}

type Shared = Arc<Mutex<Idp>>;

fn ec_key() -> (Vec<u8>, Vec<u8>) {
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap().as_ref().to_vec();
    let public = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8, &rng).unwrap().public_key().as_ref().to_vec();
    (pkcs8, public)
}

fn discovery(issuer: &str) -> Value {
    json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks"),
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
    })
}

async fn well_known(State(s): State<Shared>) -> Json<Value> {
    Json(discovery(&s.lock().unwrap().issuer))
}

/// Served under `/other`: a document naming another issuer than the one it is found under.
async fn other_well_known(State(s): State<Shared>) -> Json<Value> {
    Json(discovery(&s.lock().unwrap().issuer))
}

async fn jwks(State(s): State<Shared>) -> Json<Value> {
    // A key of a kind nobody reads sits beside the real one.
    Json(json!({"keys": [{"kty": "unknown-kind", "kid": "junk"}, s.lock().unwrap().jwk.clone()]}))
}

async fn token(State(s): State<Shared>, headers: HeaderMap, Form(f): Form<HashMap<String, String>>) -> (StatusCode, Json<Value>) {
    let refuse = |e: &str| (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant", "error_description": e})));
    let basic = format!("Basic {}", STANDARD.encode(format!("{CLIENT_ID}:{}", "s3cret%3Awith%2Fodd%20chars")));
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(basic.as_str()) {
        return refuse("client authentication failed");
    }
    if f.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return refuse("grant type");
    }
    let mut idp = s.lock().unwrap();
    let Some(g) = f.get("code").and_then(|c| idp.codes.remove(c)) else { return refuse("unknown code") };
    if f.get("redirect_uri") != Some(&g.redirect_uri) {
        return refuse("redirect_uri differs");
    }
    let verifier = f.get("code_verifier").cloned().unwrap_or_default();
    if URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) != g.challenge {
        return refuse("PKCE verifier does not match");
    }
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(g.kid.clone());
    let key = EncodingKey::from_ec_der(if g.rogue { &idp.rogue } else { &idp.key });
    let id_token = jsonwebtoken::encode(&header, &g.claims, &key).unwrap();
    (StatusCode::OK, Json(json!({"access_token": "at", "token_type": "Bearer", "id_token": id_token})))
}

async fn start_idp() -> Shared {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let (key, public) = ec_key();
    let (rogue, _) = ec_key();
    let jwk = json!({
        "kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": "k1",
        "x": URL_SAFE_NO_PAD.encode(&public[1..33]), "y": URL_SAFE_NO_PAD.encode(&public[33..65]),
    });
    let shared: Shared = Arc::new(Mutex::new(Idp { issuer, key, rogue, jwk, codes: HashMap::new() }));
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(well_known))
        .route("/other/.well-known/openid-configuration", get(other_well_known))
        .route("/jwks", get(jwks))
        .route("/token", post(token))
        .with_state(shared.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    shared
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// What the provider would do after the person signs in there: a code for an ID token with these
/// claims (over the defaults), signed with the rogue key or under another key id if asked.
fn authorize(idp: &Shared, url: &str, claims: Value, rogue: bool, kid: &str) -> (String, String) {
    let q: HashMap<String, String> = reqwest::Url::parse(url).unwrap().query_pairs().into_owned().collect();
    assert_eq!((q["response_type"].as_str(), q["client_id"].as_str(), q["code_challenge_method"].as_str()), ("code", CLIENT_ID, "S256"));
    assert!(q["scope"].split(' ').any(|s| s == "openid"));
    let mut idp = idp.lock().unwrap();
    let mut c = json!({
        "iss": idp.issuer, "aud": CLIENT_ID, "sub": "okta-pat", "email": "pat@ssoco.test", "email_verified": true,
        "name": "Pat Doe", "nonce": q["nonce"], "iat": now(), "exp": now() + 300,
    });
    for (k, v) in claims.as_object().unwrap() {
        if v.is_null() {
            c.as_object_mut().unwrap().remove(k);
        } else {
            c[k] = v.clone();
        }
    }
    let code = format!("code-{}", Uuid::now_v7());
    idp.codes
        .insert(code.clone(), Grant { claims: c, challenge: q["code_challenge"].clone(), redirect_uri: q["redirect_uri"].clone(), rogue, kid: kid.into() });
    (q["state"].clone(), code)
}

async fn sign_in(k: &Kernel, idp: &Shared, provider: Uuid, claims: Value) -> ferrum_kernel::Result<Session> {
    let url = k.sso_begin("ssoco", provider, REDIRECT, BINDING).await?;
    let (state, code) = authorize(idp, &url, claims, false, "k1");
    k.sso_finish(&state, &code, BINDING).await
}

/// Rows read inside the workspace (row-level security holds for the test's database role too).
async fn read<T>(k: &Kernel, tenant: Uuid, sql: String) -> Vec<T>
where
    T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin,
{
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let rows = sqlx::query_scalar(sqlx::AssertSqlSafe(sql)).bind(tenant).fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    rows
}

fn input(issuer: &str) -> SsoProviderInput {
    serde_json::from_value(json!({"label": "Okta", "issuer": issuer, "client_id": CLIENT_ID, "client_secret": CLIENT_SECRET})).unwrap()
}

#[tokio::test]
async fn single_sign_on() {
    let url = std::env::var("FERRUM_SSO_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_sso_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let catalog = Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages")).expect("catalog");
    let settings = KernelSettings { secret_key: Some(SecretKey::new([9u8; 32])), sso_allow_private: true, ..Default::default() };
    let k = Kernel::boot(db, catalog, HookRegistry::default(), settings, None).await.expect("boot");
    let idp = start_idp().await;
    let issuer = idp.lock().unwrap().issuer.clone();

    let tenant = k
        .create_tenant(NewTenant {
            slug: "ssoco".into(),
            name: "SSO Co".into(),
            admin_email: "admin@ssoco.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let s = k.login("ssoco", "admin@ssoco.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, s.user, Some(s.token_version), "it".into()).await.unwrap();
    let pat = k.create_user(&admin, "pat@ssoco.test", "Pat", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let pat_ctx = k.load_ctx(tenant, pat, None, "it".into()).await.unwrap();

    // Setting up: the provider must answer for itself, as itself; only administrators set it up.
    assert!(matches!(k.save_sso_provider(&admin, None, input("http://127.0.0.1:1")).await, Err(Error::Validation(_))), "unreachable");
    let Err(Error::Validation(f)) = k.save_sso_provider(&admin, None, input(&format!("{issuer}/other"))).await else { panic!("another issuer") };
    assert!(f[0].message.contains("calls itself"), "{f:?}");
    let mut bad_role = input(&issuer);
    bad_role.default_roles = vec!["No Such Role".into()];
    assert!(matches!(k.save_sso_provider(&admin, None, bad_role).await, Err(Error::Validation(_))));
    assert!(matches!(k.save_sso_provider(&pat_ctx, None, input(&issuer)).await, Err(Error::Forbidden(_))));
    let provider = k.save_sso_provider(&admin, None, input(&format!("{issuer}/"))).await.unwrap();
    let listed = k.sso_providers(&admin).await.unwrap();
    assert_eq!((listed[0]["issuer"].clone(), listed[0]["has_secret"].clone()), (json!(issuer), json!(true)));
    assert!(!listed[0].to_string().contains("s3cret"), "the secret is never shown");
    assert_eq!(k.sso_sign_in_options("SSOCO").await.unwrap(), vec![json!({"id": provider, "label": "Okta"})]);
    assert!(k.sso_sign_in_options("nobody").await.unwrap().is_empty());

    // The round trip signs Pat in and links the identity; the sign-in works once.
    let url = k.sso_begin("ssoco", provider, REDIRECT, BINDING).await.unwrap();
    let (state, code) = authorize(&idp, &url, json!({}), false, "k1");
    let session = k.sso_finish(&state, &code, BINDING).await.unwrap();
    assert_eq!(session.user, pat);
    assert!(k.sso_finish(&state, &code, BINDING).await.is_err(), "used up");
    let linked: Vec<Option<String>> = read(&k, tenant, format!("select sso_subject from app_user where tenant_id = $1 and id = '{pat}'")).await;
    assert_eq!(linked, vec![Some(format!("{provider}|okta-pat"))]);

    // Started in another browser: refused, and used up all the same.
    let url = k.sso_begin("ssoco", provider, REDIRECT, BINDING).await.unwrap();
    let (state, code) = authorize(&idp, &url, json!({}), false, "k1");
    assert!(matches!(k.sso_finish(&state, &code, "another-browser-binding").await, Err(Error::Unauthorized(_))));
    assert!(k.sso_finish(&state, &code, BINDING).await.is_err());

    // Every check on the ID token.
    for (why, claims) in [
        ("another sign-in's nonce", json!({"nonce": "not-this-one"})),
        ("another audience", json!({"aud": "someone-else"})),
        ("expired", json!({"exp": now() - 3600, "iat": now() - 4000})),
        ("another issuer", json!({"iss": "https://evil.example"})),
        ("another application", json!({"aud": [CLIENT_ID, "other"], "azp": "other"})),
        ("several audiences, no azp", json!({"aud": [CLIENT_ID, "other"]})),
    ] {
        assert!(matches!(sign_in(&k, &idp, provider, claims).await, Err(Error::Unauthorized(_))), "{why}");
    }
    for (why, rogue, kid) in [("forged signature", true, "k1"), ("unpublished key", false, "k9")] {
        let url = k.sso_begin("ssoco", provider, REDIRECT, BINDING).await.unwrap();
        let (state, code) = authorize(&idp, &url, json!({}), rogue, kid);
        assert!(matches!(k.sso_finish(&state, &code, BINDING).await, Err(Error::Unauthorized(_))), "{why}");
    }

    // Email rules: verified, known, within the listed domains.
    assert!(matches!(sign_in(&k, &idp, provider, json!({"sub": "x1", "email_verified": false})).await, Err(Error::Forbidden(_))));
    assert!(matches!(sign_in(&k, &idp, provider, json!({"sub": "x2", "email": "new@ssoco.test"})).await, Err(Error::Forbidden(_))), "no account");
    assert!(matches!(sign_in(&k, &idp, provider, json!({"email_verified": null})).await, Err(Error::Forbidden(_))), "unsaid, no domains listed");
    let role: String = read(&k, tenant, "select name from role where tenant_id = $1 order by name limit 1".into()).await.remove(0);
    let mut creating = input(&issuer);
    creating.client_secret = None;
    creating.domains = vec!["SSOCO.test".into()];
    creating.auto_create = true;
    creating.default_roles = vec![role.clone()];
    k.save_sso_provider(&admin, Some(provider), creating).await.unwrap();
    assert_eq!(k.sso_providers(&admin).await.unwrap()[0]["has_secret"], json!(true), "kept");
    assert_eq!(sign_in(&k, &idp, provider, json!({"email_verified": null})).await.unwrap().user, pat, "Entra ID style, domain listed");
    let created = sign_in(&k, &idp, provider, json!({"sub": "okta-new", "email": "new@ssoco.test", "name": "New Person"})).await.unwrap();
    let roles: Vec<String> = read(&k, tenant, format!("select role from user_role where tenant_id = $1 and user_id = '{}'", created.user)).await;
    assert_eq!(roles, vec![role]);
    assert!(k.login("ssoco", "new@ssoco.test", "anything-at-all").await.is_err(), "no password");
    assert!(matches!(sign_in(&k, &idp, provider, json!({"sub": "x3", "email": "pat@other.test"})).await, Err(Error::Forbidden(_))), "domain");

    // The same address from another identity at the provider: refused until unlinked.
    assert!(matches!(sign_in(&k, &idp, provider, json!({"sub": "okta-imposter"})).await, Err(Error::Forbidden(_))));
    k.unlink_sso(&admin, pat).await.unwrap();
    assert_eq!(sign_in(&k, &idp, provider, json!({"sub": "okta-pat-2"})).await.unwrap().user, pat);

    // A deactivated account stays out.
    k.set_user_active(&admin, created.user, false).await.unwrap();
    assert!(matches!(sign_in(&k, &idp, provider, json!({"sub": "okta-new", "email": "new@ssoco.test"})).await, Err(Error::Forbidden(_))));

    // Requiring it: passwords only for administrators, and a provider must stay enabled.
    k.update_tenant_settings(&admin, json!({"require_sso": true}).as_object().unwrap()).await.unwrap();
    assert!(matches!(k.sign_in("ssoco", "pat@ssoco.test", "Correct-Horse-9").await, Err(Error::Forbidden(_))));
    assert!(matches!(k.sign_in("ssoco", "pat@ssoco.test", "wrong-password").await, Err(Error::Unauthorized(_))), "says nothing more to a wrong password");
    assert!(matches!(k.sign_in("ssoco", "admin@ssoco.test", "Correct-Horse-9").await.unwrap(), SignIn::Session(_)));
    let mut off = input(&issuer);
    off.enabled = false;
    assert!(matches!(k.save_sso_provider(&admin, Some(provider), off.clone()).await, Err(Error::Invalid(_))));
    assert!(matches!(k.delete_sso_provider(&admin, provider).await, Err(Error::Invalid(_))));
    k.update_tenant_settings(&admin, json!({"require_sso": false}).as_object().unwrap()).await.unwrap();
    k.save_sso_provider(&admin, Some(provider), off).await.unwrap();
    assert!(k.sso_sign_in_options("ssoco").await.unwrap().is_empty(), "disabled");
    assert!(matches!(k.sso_begin("ssoco", provider, REDIRECT, BINDING).await, Err(Error::NotFound(_))));
    assert!(
        matches!(k.update_tenant_settings(&admin, json!({"require_sso": true}).as_object().unwrap()).await, Err(Error::Validation(_))),
        "no enabled provider"
    );
    k.delete_sso_provider(&admin, provider).await.unwrap();
    let links: Vec<i64> = read(&k, tenant, "select count(*) from app_user where tenant_id = $1 and sso_subject is not null".into()).await;
    assert_eq!(links, vec![0], "unlinked with the provider");
    assert!(matches!(k.save_sso_provider(&admin, Some(provider), input(&issuer)).await, Err(Error::NotFound(_))), "no such provider to change");
}
