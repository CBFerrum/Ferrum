//! Ferrum as an OpenID Connect provider for the programs that run beside it (crates/kernel/src/oidc.rs), against a
//! real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database is reset first), with two replicas on it.
//!
//! Each workspace's signing key is made once however many replicas ask, signs ID tokens that a JWT library checks with
//! the published key, and is rotated with an overlap. A companion's client is declared only by the app that registered
//! it, once, and its secret is that app's own. A person signs in only through a live browser session of theirs and only
//! with the app's right; `auth_time` and `amr` are their session's real proof, and `max_age` and `prompt=login` wait for
//! them to confirm it is them. Every refusal the brief names is refused: a code used twice (and what it gave stops), one
//! redeemed by another client, a return address one character off, a wrong verifier, a mixed-up issuer, a wrong secret,
//! a nonce used again, a person without the right. Nothing outlives the browser session: its code, its access token and
//! its user-info stop with it, and the app is told once, however many replicas look, whichever way it ended. API
//! clients' refresh tokens are as they were: they end with the person's sign-out everywhere, not with one device's.

use async_trait::async_trait;
use ferrum_kernel::gate::Arrival;
use ferrum_kernel::oidc::{AuthRequest, Authorized, Companion, SIGNED_OUT_TOPIC};
use ferrum_kernel::shell::StepUp;
use ferrum_kernel::vault::{self, Kdf, Opener, Scope, Vault};
use ferrum_kernel::{Action, Ctx, Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use jsonwebtoken::jwk::Jwk;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const PASSWORD: &str = "Correct-Horse-9";
const BACK: &str = "https://forge.example/user/oauth2/acme/callback";
const ISSUER: &str = "https://acme.example";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

fn url() -> String {
    std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into())
}

/// The forge's account name as its app keeps it (the Cloud app's `forge_login`).
fn login_of(tenant: Uuid, user: Uuid) -> String {
    format!("u-{}", &hex::encode(Sha256::digest(format!("{tenant}|{user}")))[..20])
}

struct Forge;

#[async_trait]
impl Companion for Forge {
    fn right(&self) -> (&str, Action) {
        ("LetterHead", Action::Read)
    }

    async fn claims(&self, _k: &Kernel, tenant: Uuid, user: Uuid) -> ferrum_kernel::Result<Map<String, Value>> {
        let login = login_of(tenant, user);
        Ok(json!({"preferred_username": login, "email": format!("{login}@people.ferrum.invalid")}).as_object().cloned().unwrap_or_default())
    }
}

/// An app that tries to name a claim only the platform sets.
struct Greedy;

#[async_trait]
impl Companion for Greedy {
    fn right(&self) -> (&str, Action) {
        ("LetterHead", Action::Read)
    }

    async fn claims(&self, _k: &Kernel, _tenant: Uuid, _user: Uuid) -> ferrum_kernel::Result<Map<String, Value>> {
        Ok(json!({"sub": "someone else"}).as_object().cloned().unwrap_or_default())
    }
}

fn core() -> Catalog {
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    Catalog::from_packages(vec![core]).unwrap()
}

/// A replica with the vault opened by the key file, and the apps `forgeapp` (with companions `forge` and `greedy`) and
/// `other` (with none), and their handles.
async fn replica(file: &Path) -> (Arc<Kernel>, Vault, Vault) {
    let db = Db::connect(DbConfig { url: url(), max_connections: 6, ..Default::default() }).await.expect("connect");
    let opener = Opener {
        database_url: "postgres://ferrum@localhost:5432/ferrum".into(),
        file: file.to_path_buf(),
        transit: None,
        command: None,
        kdf: Kdf { m_kib: 64, t: 1, p: 1 },
    };
    vault::open(&db, opener, false).await.expect("the vault opens");
    let mut hooks = HookRegistry::default();
    let (mut forge, mut other) = (None, None);
    hooks.app("forgeapp", &[], |h| {
        forge = Some(h.vault());
        h.companion("forge", Arc::new(Forge));
        h.companion("greedy", Arc::new(Greedy));
    });
    hooks.app("other", &[], |h| other = Some(h.vault()));
    let k = Kernel::boot(db, core(), hooks, KernelSettings::default(), None).await.expect("boot");
    (k, forge.unwrap(), other.unwrap())
}

async fn workspace(k: &Kernel, slug: &str) -> Uuid {
    k.create_tenant(ferrum_kernel::tenant::NewTenant {
        slug: slug.into(),
        name: slug.into(),
        admin_email: format!("admin@{slug}.test"),
        admin_password: PASSWORD.into(),
        admin_name: None,
        packages: vec![],
        settings: None,
    })
    .await
    .unwrap()
}

/// A person signed in with a password in a browser session: their context and the session.
async fn signed_in(k: &Kernel, slug: &str, email: &str) -> (Ctx, Uuid) {
    let s = k.login(slug, email, PASSWORD).await.unwrap();
    let sid = k.open_session_proved(&s, Duration::from_secs(3600), "it", "192.0.2.1", None, None, None, &["password"]).await.unwrap();
    (k.load_ctx(s.tenant, s.user, Some(s.token_version), "it".into()).await.unwrap(), sid)
}

fn request(client: &str, nonce: Option<&str>) -> AuthRequest {
    AuthRequest {
        response_type: "code".into(),
        client_id: client.into(),
        redirect_uri: BACK.into(),
        scope: "openid profile".into(),
        state: "s1".into(),
        nonce: nonce.map(str::to_string),
        code_challenge: CHALLENGE.into(),
        code_challenge_method: "S256".into(),
        ..Default::default()
    }
}

async fn code(k: &Kernel, ctx: &Ctx, sid: Uuid, client: &ferrum_kernel::oidc::Client, r: &AuthRequest) -> String {
    match k.openid_authorize(ctx, sid, client, r, ISSUER).await.unwrap() {
        Ok(Authorized::Code(c)) => c,
        other => panic!("expected a code, got {other:?}"),
    }
}

fn refused(r: ferrum_kernel::Result<std::result::Result<Authorized, ferrum_kernel::oidc::Refusal>>) -> &'static str {
    match r.unwrap() {
        Err(no) => no.error,
        Ok(a) => panic!("expected a refusal, got {a:?}"),
    }
}

fn grant_refused(r: ferrum_kernel::Result<ferrum_kernel::oidc::Tokens>) -> String {
    match r {
        Err(Error::Invalid(m)) | Err(Error::Unauthorized(m)) => m,
        Err(e) => panic!("expected the specification's refusal, got {e}"),
        Ok(_) => panic!("expected a refusal, got tokens"),
    }
}

/// The ID token's claims, checked as a relying party checks them, with the key the workspace publishes.
async fn verified(k: &Kernel, tenant: Uuid, token: &str, client: &str) -> Value {
    let header = jsonwebtoken::decode_header(token).unwrap();
    assert_eq!(header.alg, Algorithm::EdDSA);
    let kid = header.kid.expect("names its key");
    let keys = k.openid_keys(tenant).await.unwrap();
    let key = keys.iter().find(|p| p.kid == kid).expect("a published key signed it");
    let jwk: Jwk = serde_json::from_value(key.jwk()).unwrap();
    let mut v = Validation::new(Algorithm::EdDSA);
    v.set_audience(&[client]);
    v.set_issuer(&[ISSUER]);
    jsonwebtoken::decode::<Value>(token, &DecodingKey::from_jwk(&jwk).unwrap(), &v).expect("it verifies").claims
}

async fn signed_out_events(k: &Kernel) -> Vec<Value> {
    let mut tx = k.db.system_tx().await.unwrap();
    let rows: Vec<Value> =
        sqlx::query_scalar("select payload from outbox where topic = $1 order by id").bind(SIGNED_OUT_TOPIC).fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    rows
}

async fn trail(k: &Kernel, tenant: Uuid, action: &str) -> Vec<Value> {
    let mut tx = k.db.system_tx().await.unwrap();
    let rows: Vec<Value> = sqlx::query_scalar("select diff from audit_event where tenant_id = $1 and action = $2 order by id")
        .bind(tenant)
        .bind(action)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    rows
}

#[test]
fn companions_sign_in_through_ferrum() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    {
        let pool = sqlx::PgPool::connect(&url()).await.unwrap();
        sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&pool).await.unwrap();
    }
    let dir = std::env::temp_dir().join(format!("ferrum-openid-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("root.key");
    std::fs::write(&file, format!("{}\n", ferrum_kernel::kms::new_key())).unwrap();
    let (a, forge, other) = replica(&file).await;
    let (b, forge_b, _) = replica(&file).await;
    let acme = workspace(&a, "acme").await;
    let beta = workspace(&a, "beta").await;
    let admin = a.load_ctx(acme, a.login("acme", "admin@acme.test", PASSWORD).await.unwrap().user, None, "it".into()).await.unwrap();
    a.set_role_permission(&admin, "Developer", &serde_json::from_value(json!({"entity": "LetterHead", "read": true})).unwrap()).await.unwrap();
    let dev = a.create_user(&admin, "dev@acme.test", "Dev", Some(PASSWORD), &["Developer".to_string()], false).await.unwrap();
    let plain = a.create_user(&admin, "plain@acme.test", "Plain", Some(PASSWORD), &[], false).await.unwrap();
    let outside = a.create_user(&admin, "portal@acme.test", "Portal", Some(PASSWORD), &["Developer".to_string()], false).await.unwrap();
    a.set_user_kind(&admin, outside, "portal").await.unwrap();

    // ── the workspace's signing key: made once by two replicas asking at once, published as a JWK ──
    let (ka, kb) = tokio::join!(a.openid_keys(acme), b.openid_keys(acme));
    let (ka, kb) = (ka.unwrap(), kb.unwrap());
    assert_eq!((ka.len(), ka[0].kid.clone()), (1, kb[0].kid.clone()), "one key, whichever replica asked");
    let jwk = ka[0].jwk();
    assert_eq!((jwk["kty"].as_str(), jwk["crv"].as_str(), jwk["alg"].as_str()), (Some("OKP"), Some("Ed25519"), Some("EdDSA")));
    assert!(jwk.get("d").is_none(), "no private part is ever published");
    assert_ne!(a.openid_keys(beta).await.unwrap()[0].kid, ka[0].kid, "each workspace has a key of its own");

    // ── a companion's client: only the app that registered it declares it, once, and its secret is the app's ──
    let e = other.companion(&a, acme, "forge", &[BACK.into()]).await.unwrap_err();
    assert!(e.to_string().contains("registered no companion"), "{e}");
    for bad in ["http://forge.example/cb", "https://forge.example/cb#x", "forge://cb"] {
        assert!(forge.companion(&a, acme, "forge", &[bad.into()]).await.is_err(), "{bad}");
    }
    let back = [BACK.to_string()];
    let (c1, c2) = tokio::join!(forge.companion(&a, acme, "forge", &back), forge_b.companion(&b, acme, "forge", &back));
    let client = c1.unwrap();
    assert_eq!(client.client_id, c2.unwrap().client_id, "two replicas declaring at once make one client");
    assert_eq!(forge.companion(&a, acme, "forge", &[BACK.into()]).await.unwrap().client_id, client.client_id, "declaring again changes nothing");
    assert_eq!(trail(&a, acme, "companion_declared").await.len(), 1, "and is recorded once");
    let secret = String::from_utf8(forge.reveal(&a, Scope::Workspace(acme), &client.secret).await.unwrap().to_vec()).unwrap();
    assert!(matches!(other.reveal(&a, Scope::Workspace(acme), &client.secret).await, Err(Error::NotFound(_))), "another app's handle has no such secret");
    let beta_client = forge.companion(&a, beta, "forge", &[BACK.into()]).await.unwrap();
    let beta_secret = String::from_utf8(forge.reveal(&a, Scope::Workspace(beta), &beta_client.secret).await.unwrap().to_vec()).unwrap();
    assert_ne!(beta_client.client_id, client.client_id);

    // ── a member with the right signs in: the ID token verifies, and says who and how ──
    let (dev_ctx, dev_sid) = signed_in(&a, "acme", "dev@acme.test").await;
    let c = code(&a, &dev_ctx, dev_sid, &client, &request(&client.client_id, Some("n-1"))).await;
    let t = b.openid_token(&client.client_id, &secret, &c, BACK, VERIFIER, ISSUER).await.unwrap();
    let claims = verified(&a, acme, &t.id_token, &client.client_id).await;
    assert_eq!(claims["azp"], json!(client.client_id));
    assert_eq!(claims["nonce"], json!("n-1"));
    assert_eq!(claims["amr"], json!(["password"]), "how the session's person proved who they are");
    assert_eq!(claims["preferred_username"], json!(login_of(acme, dev)), "what the app places them by");
    assert_eq!(claims["name"], json!("Dev"));
    assert_eq!(claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(), 300);
    assert!((claims["iat"].as_i64().unwrap() - claims["auth_time"].as_i64().unwrap()).abs() < 60, "the session's sign-in, a moment ago");
    let sub = claims["sub"].as_str().unwrap().to_string();
    assert!(sub.len() >= 43 && !sub.contains(&dev.to_string()), "a subject that says nothing else about them");
    let info = a.openid_userinfo(&t.access_token, ISSUER).await.unwrap();
    assert_eq!((info["sub"].as_str(), info["preferred_username"].as_str()), (Some(sub.as_str()), Some(login_of(acme, dev).as_str())));
    assert_eq!(trail(&a, acme, "companion_signed_in").await.len(), 1, "on the person's trail");
    assert!(!trail(&a, acme, "companion_signed_in").await[0].to_string().contains(&c), "never a code or a token");
    // The same person again: the same subject.
    let c = code(&a, &dev_ctx, dev_sid, &client, &request(&client.client_id, None)).await;
    let again = a.openid_token(&client.client_id, &secret, &c, BACK, VERIFIER, ISSUER).await.unwrap();
    assert_eq!(verified(&a, acme, &again.id_token, &client.client_id).await["sub"], json!(sub));

    // ── refused: each the specification's way, saying nothing more ──
    let r = request(&client.client_id, Some("n-1"));
    assert_eq!(refused(a.openid_authorize(&dev_ctx, dev_sid, &client, &r, ISSUER).await), "invalid_request", "a nonce used again");
    let (plain_ctx, plain_sid) = signed_in(&a, "acme", "plain@acme.test").await;
    assert_eq!(refused(a.openid_authorize(&plain_ctx, plain_sid, &client, &request(&client.client_id, None), ISSUER).await), "access_denied", "no right");
    let (out_ctx, out_sid) = signed_in(&a, "acme", "portal@acme.test").await;
    assert_eq!(refused(a.openid_authorize(&out_ctx, out_sid, &client, &request(&client.client_id, None), ISSUER).await), "access_denied", "from outside");
    let _ = plain;
    let c = code(&a, &dev_ctx, dev_sid, &client, &request(&client.client_id, None)).await;
    let first = a.openid_token(&client.client_id, &secret, &c, BACK, VERIFIER, ISSUER).await.unwrap();
    assert_eq!(grant_refused(a.openid_token(&client.client_id, &secret, &c, BACK, VERIFIER, ISSUER).await), "invalid_grant", "a code used twice");
    assert!(a.openid_userinfo(&first.access_token, ISSUER).await.is_err(), "and what it gave stops");
    assert_eq!(trail(&a, acme, "companion_code_replayed").await.len(), 1);
    assert_eq!(grant_refused(b.openid_token(&beta_client.client_id, &beta_secret, "made-up", BACK, VERIFIER, ISSUER).await), "invalid_grant", "no such code");
    let c = code(&a, &dev_ctx, dev_sid, &client, &request(&client.client_id, None)).await;
    assert_eq!(
        grant_refused(b.openid_token(&beta_client.client_id, &beta_secret, &c, BACK, VERIFIER, ISSUER).await),
        "invalid_grant",
        "redeemed by another client"
    );
    assert_eq!(
        grant_refused(a.openid_token(&client.client_id, &secret, &c, &format!("{BACK}x"), VERIFIER, ISSUER).await),
        "invalid_grant",
        "one character more"
    );
    assert_eq!(grant_refused(a.openid_token(&client.client_id, &secret, &c, &BACK[..BACK.len() - 1], VERIFIER, ISSUER).await), "invalid_grant", "one less");
    assert_eq!(
        grant_refused(a.openid_token(&client.client_id, &secret, &c, BACK, "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXX", ISSUER).await),
        "invalid_grant",
        "a wrong verifier"
    );
    assert_eq!(
        grant_refused(a.openid_token(&client.client_id, &secret, &c, BACK, VERIFIER, "https://sales--acme.example").await),
        "invalid_grant",
        "another issuer"
    );
    assert_eq!(grant_refused(a.openid_token(&client.client_id, "wrong", &c, BACK, VERIFIER, ISSUER).await), "invalid_client", "a wrong secret");
    assert_eq!(grant_refused(a.openid_token("fo_nobody", &secret, &c, BACK, VERIFIER, ISSUER).await), "invalid_client", "nobody's: the same answer");
    assert!(a.openid_token(&client.client_id, &secret, &c, BACK, VERIFIER, ISSUER).await.is_ok(), "none of those used the code up");
    assert!(a.openid_userinfo("fa_nothing", ISSUER).await.is_err());
    assert!(a.openid_userinfo(&again.access_token, "https://sales--acme.example").await.is_err(), "user-info at another issuer");
    // An app's claim that is the platform's never reaches a token.
    let greedy = forge.companion(&a, acme, "greedy", &[BACK.into()]).await.unwrap();
    let greedy_secret = String::from_utf8(forge.reveal(&a, Scope::Workspace(acme), &greedy.secret).await.unwrap().to_vec()).unwrap();
    let c = code(&a, &dev_ctx, dev_sid, &greedy, &request(&greedy.client_id, None)).await;
    assert!(a.openid_token(&greedy.client_id, &greedy_secret, &c, BACK, VERIFIER, ISSUER).await.is_err(), "sub is the platform's");

    // ── max_age and prompt=login: the person confirms it is them, in one of their ways, never assumed ──
    let mut login = request(&client.client_id, None);
    login.prompt = "login".into();
    let waiting = match a.openid_authorize(&dev_ctx, dev_sid, &client, &login, ISSUER).await.unwrap() {
        Ok(Authorized::Confirm(w)) => w,
        other => panic!("expected to wait for a confirmation, got {other:?}"),
    };
    assert!(matches!(a.openid_confirm(&dev_ctx, dev_sid, &waiting, ISSUER).await, Err(Error::Refused { code: "step_up", .. })), "not yet");
    assert!(matches!(a.openid_confirm(&plain_ctx, plain_sid, &waiting, ISSUER).await, Err(Error::NotFound(_))), "someone else's");
    assert_eq!(
        grant_refused(a.openid_token(&client.client_id, &secret, &waiting, BACK, VERIFIER, ISSUER).await),
        "invalid_grant",
        "a waiting request is no code"
    );
    let asked = chrono::Utc::now().timestamp();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let who = StepUp { tenant: acme, user: dev, session: dev_sid, keys: (acme, dev) };
    a.stepped_up(&who, "passkey", &Arrival::default()).await.unwrap();
    let (back, c, state) = a.openid_confirm(&dev_ctx, dev_sid, &waiting, ISSUER).await.unwrap();
    assert_eq!((back.as_str(), state.as_str()), (BACK, "s1"));
    let t = a.openid_token(&client.client_id, &secret, &c, BACK, VERIFIER, ISSUER).await.unwrap();
    let claims = verified(&a, acme, &t.id_token, &client.client_id).await;
    assert!(claims["auth_time"].as_i64().unwrap() >= asked, "proved after the request");
    assert_eq!(claims["amr"], json!(["passkey"]), "the way they confirmed it");
    let mut aged = request(&client.client_id, None);
    aged.max_age = Some("3600".into());
    assert!(matches!(a.openid_authorize(&dev_ctx, dev_sid, &client, &aged, ISSUER).await.unwrap(), Ok(Authorized::Code(_))), "recent enough");
    aged.max_age = Some("0".into());
    assert!(matches!(a.openid_authorize(&dev_ctx, dev_sid, &client, &aged, ISSUER).await.unwrap(), Ok(Authorized::Confirm(_))), "too old for it");
    aged.prompt = "none".into();
    assert_eq!(refused(a.openid_authorize(&dev_ctx, dev_sid, &client, &aged, ISSUER).await), "login_required", "and no page may be shown");

    // ── leaving: nothing outlives the browser session, and the app hears it once ──
    assert_eq!(a.openid_sweep().await.unwrap(), 0, "nothing has ended");
    let c = code(&a, &dev_ctx, dev_sid, &client, &request(&client.client_id, None)).await;
    a.end_session(acme, dev_sid).await.unwrap();
    assert_eq!(
        grant_refused(a.openid_token(&client.client_id, &secret, &c, BACK, VERIFIER, ISSUER).await),
        "invalid_grant",
        "a code from a session signed out"
    );
    assert!(a.openid_userinfo(&again.access_token, ISSUER).await.is_err(), "user-info for it");
    assert!(a.openid_authorize(&dev_ctx, dev_sid, &client, &request(&client.client_id, None), ISSUER).await.is_err(), "no new code through it");
    let (one, two) = tokio::join!(a.openid_sweep(), b.openid_sweep());
    assert_eq!(one.unwrap() + two.unwrap(), 1, "told once, by whichever replica looked");
    let told = signed_out_events(&a).await;
    assert_eq!(told.len(), 1);
    assert_eq!(told[0]["reason"], json!("signed_out"));
    assert_eq!((told[0]["user"].as_str(), told[0]["subject"].as_str()), (Some(dev.to_string().as_str()), Some(sub.as_str())));
    assert_eq!(
        (told[0]["app"].as_str(), told[0]["companion"].as_str(), told[0]["client_id"].as_str()),
        (Some("forgeapp"), Some("forge"), Some(client.client_id.as_str()))
    );
    // Signing out everywhere, and being deactivated.
    let (dev_ctx, dev_sid) = signed_in(&a, "acme", "dev@acme.test").await;
    let c = code(&a, &dev_ctx, dev_sid, &client, &request(&client.client_id, None)).await;
    a.openid_token(&client.client_id, &secret, &c, BACK, VERIFIER, ISSUER).await.unwrap();
    a.logout_everywhere(&dev_ctx).await.unwrap();
    let admin2 = a.create_user(&admin, "second@acme.test", "Second", Some(PASSWORD), &[], true).await.unwrap();
    let (second_ctx, second_sid) = signed_in(&a, "acme", "second@acme.test").await;
    let c = code(&a, &second_ctx, second_sid, &client, &request(&client.client_id, None)).await;
    let t = a.openid_token(&client.client_id, &secret, &c, BACK, VERIFIER, ISSUER).await.unwrap();
    a.set_user_active(&admin, admin2, false).await.unwrap();
    assert!(a.openid_userinfo(&t.access_token, ISSUER).await.is_err(), "deactivated: user-info refused at once");
    assert_eq!(a.openid_sweep().await.unwrap(), 2);
    let reasons: Vec<Value> = signed_out_events(&a).await.into_iter().map(|e| e["reason"].clone()).collect();
    assert_eq!(reasons, [json!("signed_out"), json!("signed_out_everywhere"), json!("access_ended")]);
    assert_eq!(a.openid_sweep().await.unwrap(), 0, "each once");

    // ── API clients' refresh tokens, as they were: one device signing out leaves them; signing out everywhere ends them ──
    let api = a.oauth_register(Some("it".into()), vec!["http://127.0.0.1:9/cb".into()]).await.unwrap();
    let (api_ctx, api_sid) = signed_in(&a, "acme", "dev@acme.test").await;
    let c = a.oauth_issue_code(&api_ctx, &api.id, "http://127.0.0.1:9/cb", CHALLENGE, "S256").await.unwrap();
    let g = a.oauth_redeem_code(&api.id, &c, "http://127.0.0.1:9/cb", VERIFIER).await.unwrap();
    a.end_session(acme, api_sid).await.unwrap();
    let g = a.oauth_refresh(&api.id, &g.refresh_token).await.expect("an API client's grant is not the device's session");
    a.logout_everywhere(&a.load_ctx(acme, dev, None, "it".into()).await.unwrap()).await.unwrap();
    assert!(a.oauth_refresh(&api.id, &g.refresh_token).await.is_err(), "signing out everywhere ends it");

    // ── rotation: the new key signs, the one before is still published; and an app removes its client ──
    let before = a.openid_keys(acme).await.unwrap()[0].kid.clone();
    let rotated = a.openid_rotate(acme).await.unwrap();
    let published: Vec<String> = a.openid_keys(acme).await.unwrap().into_iter().map(|k| k.kid).collect();
    assert_eq!(published, [rotated.kid.clone(), before.clone()]);
    let (ctx, sid) = signed_in(&a, "acme", "admin@acme.test").await;
    let c = code(&a, &ctx, sid, &client, &request(&client.client_id, None)).await;
    let t = a.openid_token(&client.client_id, &secret, &c, BACK, VERIFIER, ISSUER).await.unwrap();
    assert_eq!(jsonwebtoken::decode_header(&t.id_token).unwrap().kid.as_deref(), Some(rotated.kid.as_str()));
    a.openid_rotate(acme).await.unwrap();
    assert!(!a.openid_keys(acme).await.unwrap().iter().any(|k| k.kid == before), "two rotations on, the first is gone");
    let c = code(&a, &ctx, sid, &client, &request(&client.client_id, None)).await;
    assert!(forge.remove_companion(&a, acme, "forge").await.unwrap());
    assert!(a.companion_client(&client.client_id).await.unwrap().is_none());
    assert_eq!(grant_refused(a.openid_token(&client.client_id, &secret, &c, BACK, VERIFIER, ISSUER).await), "invalid_client", "its codes went with it");
    assert!(matches!(forge.describe(&a, Scope::Workspace(acme), &client.secret).await, Ok(None)), "and its secret");
    assert_eq!(a.openid_purge().await.unwrap(), 0, "nothing is a day old");
    let _ = std::fs::remove_dir_all(&dir);
}
