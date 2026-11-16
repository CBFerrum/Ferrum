//! Ferrum as an OpenID Connect provider for the programs that run beside it (docs/api.md, "Signing in to programs
//! beside Ferrum"; docs/plan/stateless.md, D8), first of all the forge.
//!
//! A program an installed app runs (its *companion*) is a confidential client its app declares, in code, for a
//! workspace ([`Vault::companion`]): never one anybody registers over the internet. Its secret is the app's own in the
//! vault; only its digest is here. The app registers, as it boots, what the companion needs ([`Companion`]): the right
//! that lets a person in, and the claims that place the person there. There is no consent page (the workspace's
//! administrator installed the app); a code goes only to a member who holds that right, through a browser session of
//! theirs, bound to PKCE, its client, its redirect address and the address (issuer) it was asked at.
//!
//! The issuer is the address of the workspace the request came to; the server decides which addresses are one
//! (crate::shell). Each workspace signs its ID tokens with a key pair of its own, which the platform's vault makes and
//! holds (`oidc.signing`, Ed25519: the private half never leaves the vault). A rotation makes a new version, which
//! signs from then on; the version before still verifies, and is still published, for a day (the secret's overlap). A
//! version is named by its RFC 7638 thumbprint, and each signature by the version that made it (`sign_at`).
//!
//! `auth_time` is when the browser session's person last proved who they are (signing in, or the step-up), never the
//! time of the request; `max_age` and `prompt=login` are met by asking the person to confirm it is them in one of their
//! usual ways ([`Kernel::openid_confirm`]). Codes, waiting requests and access tokens are random 256-bit values stored
//! as SHA-256 digests. Access tokens are good for the user-info only: they are opaque, and no other route takes them.
//! Nothing outlives the browser session it came from: its code, its access token and its user-info stop with it, and
//! the app hears that it ended ([`SIGNED_OUT_TOPIC`]) so that it can end the companion's own session.

use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::oauth::{hash, pkce_matches, random_secret};
use crate::vault::{New, Rule, Scope, Vault};
use crate::{Access, Action, Kernel, gate, outbox};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// How long a code lives.
pub const CODE_SECS: i64 = 60;
/// How long a request waits for its person to confirm it is them.
pub const WAIT_SECS: i64 = 600;
/// How long an ID token and an access token live.
pub const TOKEN_SECS: i64 = 300;
/// How long the version of the signing key before the newest still verifies, and is published, after a rotation.
pub const KEY_OVERLAP_SECS: i64 = 86_400;
/// Published (the outbox) once a browser session that signed in to a companion has ended, whichever way it ended:
/// `{user, subject, client_id, app, companion, session, reason}`, `reason` one of `signed_out` (that device signed out,
/// or its session was revoked or ended idle), `signed_out_everywhere` (every session of the person ended: signing out
/// everywhere, a new password), `access_ended` (deactivated, deprovisioned, the workspace suspended), `expired`, or
/// `client_removed` (its app or an operator took the companion's client away).
pub const SIGNED_OUT_TOPIC: &str = "ferrum.companion.signed_out";
/// The scopes a companion may ask for; others are left out.
pub const SCOPES: [&str; 3] = ["openid", "profile", "email"];
/// Claims only the platform sets: an app's claims never name them.
const PLATFORM_CLAIMS: [&str; 17] =
    ["iss", "sub", "aud", "azp", "exp", "iat", "nbf", "jti", "auth_time", "nonce", "acr", "amr", "at_hash", "c_hash", "sid", "client_id", "scope"];
/// The vault's name of a workspace's signing key, a secret of the platform's in that workspace.
const KEY: &str = "oidc.signing";
/// The app's own secret that holds its companion's client secret: `oidc-client.<companion>`.
pub const CLIENT_SECRET: &str = "oidc-client.";
/// How often a replica that runs background work looks for sign-ins whose session ended.
const SWEEP_EVERY: Duration = Duration::from_secs(15);

/// What an app gives, as it registers, for a program it runs beside Ferrum ([`crate::HookRegistry::companion`]).
#[async_trait]
pub trait Companion: Send + Sync {
    /// The right that lets a person in: an entity of the app's and what they may do with it (some records are enough).
    /// Anyone else is refused; and once the app is gone from a workspace, its entity is too, and so is everyone.
    fn right(&self) -> (&str, Action);

    /// What the companion needs to place `user` of `tenant` (the forge: the name of their account there), put in the ID
    /// token and the user-info beside the platform's own claims. Never one of the platform's (`iss`, `sub`, `aud`, …).
    /// An error refuses the sign-in.
    async fn claims(&self, kernel: &Kernel, tenant: Uuid, user: Uuid) -> Result<Map<String, Value>>;
}

/// A companion's client, as its app declared it. Its secret's digest is never shown (not even by `Debug`).
#[derive(Clone, PartialEq, Eq, serde::Serialize)]
pub struct Client {
    pub client_id: String,
    pub tenant: Uuid,
    pub app: String,
    /// The companion's name, its app's.
    pub name: String,
    pub redirect_uris: Vec<String>,
    /// The app's own secret in the vault whose value is the client's secret (its code reveals it to the companion).
    pub secret: String,
    #[serde(skip)]
    secret_hash: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("client_id", &self.client_id).field("tenant", &self.tenant).field("app", &self.app).field("name", &self.name).finish()
    }
}

/// What a companion's name may be: lowercase letters, digits and dashes, up to 40.
pub(crate) fn name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=40).contains(&b.len()) && b[0].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// A companion's return address: `https://…`, or `http://` on this machine's loopback (a development forge); never a
/// fragment, spaces or another scheme. Matched exactly.
pub fn redirect_ok(uri: &str) -> bool {
    crate::oauth::redirect_uri_allowed(uri) && (uri.starts_with("https://") || uri.starts_with("http://"))
}

impl Vault {
    /// Declares, or brings up to date, this app's client for its companion `name` in workspace `tenant`: the addresses
    /// it returns people to (each matched exactly), and its secret, which the vault makes once as this app's own
    /// (`oidc-client.<name>`, revealed to this app's code to hand to the companion; a sandbox has none). Refused for a
    /// companion this app did not register ([`crate::HookRegistry::companion`]). Doing it again changes nothing that
    /// is the same, whichever replica does it.
    pub async fn companion(&self, k: &Kernel, tenant: Uuid, name: &str, redirect_uris: &[String]) -> Result<Client> {
        let scope = Scope::Workspace(tenant);
        let secret = format!("{CLIENT_SECRET}{name}");
        // Its first use of the handle: refused unless this kernel's registry gave it out.
        let made = self.describe(k, scope, &secret).await?;
        let app = self.owner().to_string();
        if !name_ok(name) || k.hooks.companion_of(&app, name).is_none() {
            return Err(Error::invalid(format!("app {app} registered no companion named {name:?} (HookRegistry::companion, as it registers)")));
        }
        if redirect_uris.is_empty() || redirect_uris.len() > 10 {
            return Err(Error::field("redirect_uris", "between 1 and 10"));
        }
        if let Some(bad) = redirect_uris.iter().find(|u| !redirect_ok(u)) {
            return Err(Error::field("redirect_uris", format!("{bad} is not one: https, or http on this machine's loopback, with no fragment")));
        }
        if made.is_none() {
            let rule = Rule { reveal: true, ..Default::default() };
            self.adopt(k, scope, &secret, New::Random(32), rule, "companion").await?;
        }
        // The app's to manage: the administrator's and operator's routes refuse to change it (the companion holds it).
        if made.as_ref().is_none_or(|d| d.managed_by.is_none()) {
            self.manage(k, scope, &secret, true).await?;
        }
        let value = self.reveal(k, scope, &secret).await?;
        let secret_hash = hex::encode(Sha256::digest(&*value));
        let mut tx = k.db.system_tenant_tx(tenant).await?;
        sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))").bind(format!("ferrum.companion {tenant}/{app}/{name}")).execute(&mut *tx).await?;
        let before: Option<(String, Vec<String>, String)> =
            sqlx::query_as("select client_id, redirect_uris, secret_hash from oauth_companion where tenant_id = $1 and app = $2 and name = $3")
                .bind(tenant)
                .bind(&app)
                .bind(name)
                .fetch_optional(&mut *tx)
                .await?;
        let client_id = match &before {
            Some((id, uris, digest)) if uris.as_slice() == redirect_uris && *digest == secret_hash => id.clone(),
            _ => {
                let id = before.as_ref().map_or_else(|| random_secret("fo_")[..27].to_string(), |b| b.0.clone());
                sqlx::query(
                    "insert into oauth_companion (client_id, tenant_id, app, name, redirect_uris, secret_hash) values ($1, $2, $3, $4, $5, $6) \
                     on conflict (client_id) do update set redirect_uris = excluded.redirect_uris, secret_hash = excluded.secret_hash, updated_at = now()",
                )
                .bind(&id)
                .bind(tenant)
                .bind(&app)
                .bind(name)
                .bind(redirect_uris)
                .bind(&secret_hash)
                .execute(&mut *tx)
                .await?;
                let action = if before.is_some() { "companion_changed" } else { "companion_declared" };
                let diff = json!({"client_id": id, "app": app, "companion": name, "redirect_uris": redirect_uris});
                crate::audit::record_as(&mut tx, tenant, None, "", crate::audit::ADMIN, "Companion", Uuid::now_v7(), action, None, diff).await?;
                id
            }
        };
        tx.commit().await?;
        Ok(Client { client_id, tenant, app, name: name.to_string(), redirect_uris: redirect_uris.to_vec(), secret, secret_hash })
    }

    /// Removes this app's client for its companion `name` in `tenant`, and its secret: every sign-in through it ends
    /// at once (the companion's own sessions are the app's to end). Whether there was one.
    pub async fn remove_companion(&self, k: &Kernel, tenant: Uuid, name: &str) -> Result<bool> {
        let scope = Scope::Workspace(tenant);
        self.remove(k, scope, &format!("{CLIENT_SECRET}{name}")).await?;
        let app = self.owner().to_string();
        let mut tx = k.db.system_tenant_tx(tenant).await?;
        let id: Option<String> = sqlx::query_scalar("select client_id from oauth_companion where tenant_id = $1 and app = $2 and name = $3")
            .bind(tenant)
            .bind(&app)
            .bind(name)
            .fetch_optional(&mut *tx)
            .await?;
        let gone = match id {
            Some(id) => take_away(&mut tx, tenant, &id, None).await?.is_some(),
            None => false,
        };
        tx.commit().await?;
        Ok(gone)
    }
}

/// Takes the client `client_id` of workspace `tenant` away in `tx`: each live sign-in through it ends, and its app is
/// told (`client_removed`), then the client goes, with its codes and access tokens. Recorded, `by` the operator or (none)
/// its app. Its app and companion, when there was one.
async fn take_away(tx: &mut crate::Tx, tenant: Uuid, client_id: &str, by: Option<&str>) -> Result<Option<(String, String)>> {
    let found: Option<(String, String)> = sqlx::query_as("select app, name from oauth_companion where tenant_id = $1 and client_id = $2 for update")
        .bind(tenant)
        .bind(client_id)
        .fetch_optional(&mut **tx)
        .await?;
    let Some((app, name)) = found else { return Ok(None) };
    let ended: Vec<(Uuid, Uuid, Option<String>)> = sqlx::query_as(
        "update oidc_sign_in i set ended_at = now() where i.client_id = $1 and i.ended_at is null \
         returning i.session_id, i.user_id, (select subject from oidc_subject x where x.tenant_id = i.tenant_id and x.user_id = i.user_id)",
    )
    .bind(client_id)
    .fetch_all(&mut **tx)
    .await?;
    for (session, user, subject) in ended {
        let payload =
            json!({"user": user, "subject": subject, "client_id": client_id, "app": app, "companion": name, "session": session, "reason": "client_removed"});
        outbox::enqueue(tx, tenant, SIGNED_OUT_TOPIC, Some("User"), Some(user), payload).await?;
    }
    sqlx::query("delete from oauth_companion where client_id = $1").bind(client_id).execute(&mut **tx).await?;
    let mut diff = json!({"client_id": client_id, "app": app, "companion": name});
    if let Some(by) = by {
        diff["by"] = json!(by);
    }
    crate::audit::record_as(tx, tenant, None, "", crate::audit::ADMIN, "Companion", Uuid::now_v7(), "companion_removed", None, diff).await?;
    Ok(Some((app, name)))
}

// ── signing keys ──

/// A version of a workspace's signing key, as relying parties see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    /// Its RFC 7638 thumbprint, which tokens name it by.
    pub kid: String,
    /// The public half, base64url.
    pub x: String,
    /// The vault's version of `oidc.signing` it is.
    pub version: i32,
    /// None: it signs. Else until when it still verifies, and is published.
    pub until: Option<DateTime<Utc>>,
}

impl PublicKey {
    /// As a JSON Web Key.
    pub fn jwk(&self) -> Value {
        json!({"kty": "OKP", "crv": "Ed25519", "x": self.x, "kid": self.kid, "alg": "EdDSA", "use": "sig"})
    }
}

/// The RFC 7638 thumbprint of an Ed25519 public key given as base64url.
pub fn thumbprint(x: &str) -> String {
    B64URL.encode(Sha256::digest(format!(r#"{{"crv":"Ed25519","kty":"OKP","x":"{x}"}}"#).as_bytes()))
}

impl Kernel {
    /// A workspace's published signing keys: the version that signs, then the one before it while it still verifies.
    /// The first is made now, once however many replicas ask at once. Refused, saying why, while the vault is closed.
    pub async fn openid_keys(&self, tenant: Uuid) -> Result<Vec<PublicKey>> {
        let platform = Vault::kernel("platform");
        let scope = Scope::Workspace(tenant);
        let current = match platform.describe(self, scope, KEY).await? {
            Some(d) => d,
            None => {
                let rule = Rule { overlap_secs: KEY_OVERLAP_SECS, ..Default::default() };
                platform.adopt(self, scope, KEY, New::Ed25519, rule, "openid").await?;
                platform.describe(self, scope, KEY).await?.ok_or_else(|| Error::Internal("a signing key just made is not there".into()))?
            }
        };
        let x = current.public_key.ok_or_else(|| Error::Internal(format!("{KEY} holds no key pair")))?;
        let mut keys = vec![PublicKey { kid: thumbprint(&x), x, version: current.version, until: None }];
        if let Some(until) = current.previous_until.filter(|t| *t > Utc::now()) {
            let before = current.version - 1;
            match platform.public_key(self, scope, KEY, Some(before)).await {
                Ok(Some(x)) => keys.push(PublicKey { kid: thumbprint(&x), x, version: before, until: Some(until) }),
                Ok(None) | Err(Error::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(keys)
    }

    /// A new version of workspace `tenant`'s signing key: it signs from now on; the one it replaces still verifies, and
    /// is published, for a day, and any older one goes. The new one.
    pub async fn openid_rotate(&self, tenant: Uuid) -> Result<PublicKey> {
        let before = self.openid_keys(tenant).await?;
        Vault::kernel("platform").rotate(self, Scope::Workspace(tenant), KEY, New::Ed25519).await?;
        self.openid_keys(tenant)
            .await?
            .into_iter()
            .next()
            .filter(|k| before.first().is_none_or(|b| b.version < k.version))
            .ok_or_else(|| Error::Internal("the signing key did not turn".into()))
    }

    /// `claims` as a JWT signed with the workspace's key: the version that signs now, named in the header, and the
    /// signature made with that very version (`sign_at`, recorded as a use), even if a rotation came in between.
    async fn openid_sign(&self, tenant: Uuid, claims: &Value) -> Result<String> {
        let keys = self.openid_keys(tenant).await?;
        let key = keys.first().ok_or_else(|| Error::Internal("no signing key".into()))?;
        let header = json!({"alg": "EdDSA", "typ": "JWT", "kid": key.kid});
        let input = format!("{}.{}", B64URL.encode(header.to_string()), B64URL.encode(claims.to_string()));
        let (_, signature) = Vault::kernel("platform").sign_at(self, Scope::Workspace(tenant), KEY, input.as_bytes(), Some(key.version)).await?;
        Ok(format!("{input}.{}", B64URL.encode(signature)))
    }
}

// ── sign-ins ──

/// An authorization request for a companion, as its parameters came (`/oauth/authorize`).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct AuthRequest {
    #[serde(default)]
    pub response_type: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub redirect_uri: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub nonce: Option<String>,
    #[serde(default)]
    pub code_challenge: String,
    #[serde(default)]
    pub code_challenge_method: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub max_age: Option<String>,
    #[serde(default)]
    pub request: Option<String>,
    #[serde(default)]
    pub request_uri: Option<String>,
}

impl AuthRequest {
    /// Whether it asks for no page at all (`prompt=none`).
    pub fn silent(&self) -> bool {
        self.prompt.split_whitespace().any(|p| p == "none")
    }
}

/// What an authorization request comes to. Neither value is ever shown (not even by `Debug`).
#[derive(Clone, PartialEq, Eq)]
pub enum Authorized {
    /// Back to the client with this code.
    Code(String),
    /// Its person confirms it is them first ([`Kernel::openid_confirm`]), for the request this names.
    Confirm(String),
}

impl std::fmt::Debug for Authorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Authorized::Code(_) => "Authorized::Code(..)",
            Authorized::Confirm(_) => "Authorized::Confirm(..)",
        })
    }
}

/// A refusal the client is told at its own address: the specification's error and what it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub error: &'static str,
    pub description: String,
}

fn refusal(error: &'static str, description: &str) -> Refusal {
    Refusal { error, description: description.to_string() }
}

/// What the tokens endpoint gives a companion. Never shown (not even by `Debug`).
#[derive(Clone)]
pub struct Tokens {
    pub id_token: String,
    pub access_token: String,
    pub scope: String,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens").field("scope", &self.scope).finish_non_exhaustive()
    }
}

/// The parts of a request a companion's sign-in checks, read once.
struct Asked {
    scope: String,
    nonce: Option<String>,
    max_age: Option<i64>,
    prompt_login: bool,
}

/// The request's own checks (beyond its client and return address, which the caller checked): the specification's
/// error for the first that fails.
fn asked(r: &AuthRequest) -> std::result::Result<Asked, Refusal> {
    if r.request.is_some() {
        return Err(refusal("request_not_supported", "request objects are not supported"));
    }
    if r.request_uri.is_some() {
        return Err(refusal("request_uri_not_supported", "request_uri is not supported"));
    }
    if r.response_type != "code" {
        return Err(refusal("unsupported_response_type", "response_type must be code"));
    }
    let wanted: Vec<&str> = r.scope.split_whitespace().collect();
    if !wanted.contains(&"openid") {
        return Err(refusal("invalid_scope", "the scope must hold openid"));
    }
    let scope = SCOPES.iter().filter(|s| wanted.contains(s)).copied().collect::<Vec<_>>().join(" ");
    if r.code_challenge_method != "S256"
        || !(43..=128).contains(&r.code_challenge.len())
        || !r.code_challenge.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(refusal("invalid_request", "PKCE is required: code_challenge, a base64url SHA-256 digest, with code_challenge_method S256"));
    }
    if r.nonce.as_ref().is_some_and(|n| n.is_empty() || n.len() > 255 || n.chars().any(char::is_control)) {
        return Err(refusal("invalid_request", "the nonce must be 1 to 255 characters"));
    }
    if r.state.len() > 1000 {
        return Err(refusal("invalid_request", "the state is longer than 1000 characters"));
    }
    let prompts: Vec<&str> = r.prompt.split_whitespace().collect();
    if prompts.contains(&"none") && prompts.len() > 1 {
        return Err(refusal("invalid_request", "prompt=none goes with no other prompt"));
    }
    let max_age = match &r.max_age {
        None => None,
        Some(m) => Some(m.parse::<i64>().ok().filter(|m| *m >= 0).ok_or_else(|| refusal("invalid_request", "max_age is a number of seconds"))?),
    };
    Ok(Asked { scope, nonce: r.nonce.clone(), max_age, prompt_login: prompts.contains(&"login") })
}

/// Whether a proof of who the person is, made at `at`, does for a request asked at `asked` with `max_age` and
/// `prompt_login`, checked at `now`: never a proof that is not there.
fn fresh_enough(at: Option<DateTime<Utc>>, asked: DateTime<Utc>, max_age: Option<i64>, prompt_login: bool, now: DateTime<Utc>) -> bool {
    let Some(at) = at else { return max_age.is_none() && !prompt_login };
    (!prompt_login || at > asked) && max_age.is_none_or(|m| now - at <= chrono::Duration::seconds(m))
}

/// SQL: the browser session `s` of person `u` of workspace `t` is still one they work in, for what was given at token
/// version `{tv}`: not signed out, revoked, expired or idle past the workspace's limit, the person active and still
/// at that version (no sign-out everywhere, no new password), the workspace active.
fn live(tv: &str) -> String {
    format!(
        "(s.id is not null and s.revoked_at is null and s.expires_at > now() and u.is_active and u.token_version = {tv} and t.status = 'active' \
         and (case when t.settings->>'idle_minutes' ~ '^[0-9]{{1,6}}$' then (t.settings->>'idle_minutes')::int else 0 end = 0 \
              or s.last_seen_at > now() - make_interval(mins => (t.settings->>'idle_minutes')::int)))"
    )
}

/// The browser session `session` of `ctx`'s person, while it is live: when they last proved who they are, how, and
/// their token version.
async fn session_proof(k: &Kernel, ctx: &Ctx, session: Uuid) -> Result<(Option<DateTime<Utc>>, Option<Vec<String>>, i32)> {
    let mut tx = k.db.system_tx().await?;
    let row: Option<(Option<DateTime<Utc>>, Option<Vec<String>>, i32)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select s.step_up_at, s.proof, u.token_version from user_session s join app_user u on u.tenant_id = s.tenant_id and u.id = s.user_id \
         join tenant t on t.id = s.tenant_id where s.tenant_id = $1 and s.id = $2 and s.user_id = $3 and {}",
        live("u.token_version")
    )))
    .bind(ctx.tenant)
    .bind(session)
    .bind(ctx.user)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    row.ok_or_else(|| Error::Unauthorized("this device was signed out; please sign in again".into()))
}

impl Kernel {
    /// The companion's client `client_id`, when it is one (`None`: an API client's, or nobody's).
    pub async fn companion_client(&self, client_id: &str) -> Result<Option<Client>> {
        if !client_id.starts_with("fo_") {
            return Ok(None);
        }
        let mut tx = self.db.system_tx().await?;
        let row: Option<(String, Uuid, String, String, Vec<String>, String)> =
            sqlx::query_as("select client_id, tenant_id, app, name, redirect_uris, secret_hash from oauth_companion where client_id = $1")
                .bind(client_id)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(row.map(|(client_id, tenant, app, name, redirect_uris, secret_hash)| Client {
            secret: format!("{CLIENT_SECRET}{name}"),
            client_id,
            tenant,
            app,
            name,
            redirect_uris,
            secret_hash,
        }))
    }

    /// Whether `ctx`'s person may sign in to `client`'s companion: a member (not someone from outside), holding the right
    /// its app names. `Err`: why not, for the client.
    fn let_in(&self, ctx: &Ctx, client: &Client) -> std::result::Result<(), Refusal> {
        let denied = || refusal("access_denied", "you may not sign in to this program");
        let companion = self.hooks.companion_of(&client.app, &client.name).ok_or_else(denied)?;
        let (entity, action) = companion.right();
        if ctx.outside || ctx.perms.access(entity, action) == Access::None {
            return Err(denied());
        }
        Ok(())
    }

    /// An authorization request for `client` (its return address already matched exactly), from `ctx`'s person in their
    /// browser session `session`, at `issuer` (the workspace's address it came to): a code; or, when the request asks for
    /// a proof of who they are newer than the session has, a request waiting for them to confirm it is them; or what the
    /// client is told. Nothing is shown to anyone but the person.
    pub async fn openid_authorize(
        &self,
        ctx: &Ctx,
        session: Uuid,
        client: &Client,
        r: &AuthRequest,
        issuer: &str,
    ) -> Result<std::result::Result<Authorized, Refusal>> {
        let a = match asked(r) {
            Ok(a) => a,
            Err(no) => return Ok(Err(no)),
        };
        if let Err(no) = self.let_in(ctx, client) {
            // On the person's trail, as a refused sign-in is (the authorization endpoint's limit bounds how often).
            let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
            let detail = json!({"client_id": client.client_id, "app": client.app, "companion": client.name, "issuer": issuer, "session": session});
            gate::event(&mut tx, ctx.tenant, ctx.user, "companion_refused", detail).await?;
            tx.commit().await?;
            return Ok(Err(no));
        }
        let (at, proof, tv) = session_proof(self, ctx, session).await?;
        let waiting = !fresh_enough(at, Utc::now(), a.max_age, a.prompt_login, Utc::now());
        if waiting && r.silent() {
            return Ok(Err(refusal("login_required", "you need to confirm it is you at Ferrum first")));
        }
        let secret = random_secret("");
        let mut tx = self.db.system_tx().await?;
        let inserted = sqlx::query(
            "insert into oidc_code (code_hash, client_id, tenant_id, user_id, session_id, token_version, issuer, redirect_uri, code_challenge, scope, nonce, \
             state, max_age, prompt_login, waiting, auth_time, amr, expires_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, now() + make_interval(secs => $18))",
        )
        .bind(hash(&secret))
        .bind(&client.client_id)
        .bind(ctx.tenant)
        .bind(ctx.user)
        .bind(session)
        .bind(tv)
        .bind(issuer)
        .bind(&r.redirect_uri)
        .bind(&r.code_challenge)
        .bind(&a.scope)
        .bind(&a.nonce)
        .bind(&r.state)
        .bind(a.max_age.map(|m| m.min(i64::from(i32::MAX)) as i32))
        .bind(a.prompt_login)
        .bind(waiting)
        .bind(if waiting { None } else { at })
        .bind(if waiting { None } else { proof })
        .bind(if waiting { WAIT_SECS } else { CODE_SECS } as f64)
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {}
            Err(sqlx::Error::Database(d)) if d.constraint() == Some("oidc_code_nonce") => {
                return Ok(Err(refusal("invalid_request", "this nonce was used already: send a new one with each request")));
            }
            Err(e) => return Err(e.into()),
        }
        tx.commit().await?;
        Ok(Ok(if waiting { Authorized::Confirm(secret) } else { Authorized::Code(secret) }))
    }

    /// The request `waiting` (what [`Kernel::openid_authorize`] gave its page) once `ctx`'s person, in the same browser
    /// session, has confirmed it is them as recently as it asks: its client's return address, the code and the client's
    /// state. Until then the step-up refusal (`step_up`), which the page answers by asking them in one of their ways and
    /// trying again; a request that is not theirs, not there or too old, as not found.
    pub async fn openid_confirm(&self, ctx: &Ctx, session: Uuid, waiting: &str, issuer: &str) -> Result<(String, String, String)> {
        let gone = || Error::NotFound("this sign-in has expired: start it again from the program".into());
        let mut tx = self.db.system_tx().await?;
        let row: Option<(String, String, Option<i32>, bool, DateTime<Utc>, String)> = sqlx::query_as(
            "select client_id, redirect_uri, max_age, prompt_login, created_at, state from oidc_code \
             where code_hash = $1 and waiting and tenant_id = $2 and user_id = $3 and session_id = $4 and issuer = $5 and expires_at > now() for update",
        )
        .bind(hash(waiting))
        .bind(ctx.tenant)
        .bind(ctx.user)
        .bind(session)
        .bind(issuer)
        .fetch_optional(&mut *tx)
        .await?;
        let (client_id, redirect_uri, max_age, prompt_login, asked_at, state) = row.ok_or_else(gone)?;
        let client = self.companion_client(&client_id).await?.ok_or_else(gone)?;
        if self.let_in(ctx, &client).is_err() {
            return Err(Error::Forbidden("you may not sign in to this program".into()));
        }
        let (at, proof, _) = session_proof(self, ctx, session).await?;
        if !fresh_enough(at, asked_at, max_age.map(i64::from), prompt_login, Utc::now()) {
            return Err(crate::controls::needed());
        }
        let code = random_secret("");
        sqlx::query(
            "update oidc_code set code_hash = $2, waiting = false, auth_time = $3, amr = $4, expires_at = now() + make_interval(secs => $5) where code_hash = $1",
        )
        .bind(hash(waiting))
        .bind(hash(&code))
        .bind(at)
        .bind(proof)
        .bind(CODE_SECS as f64)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((redirect_uri, code, state))
    }

    /// The tokens endpoint, for a companion: `client_id` authenticated by `secret`, then the code (once), checked against
    /// that client, `redirect_uri` exactly, PKCE's `verifier`, `issuer` (the address it is asked at) and the browser
    /// session it came from. A client that is not one, or a wrong secret: `Unauthorized("invalid_client")`; anything wrong
    /// with the code, the same `Invalid("invalid_grant")` whatever it was. The sign-in is on the person's trail.
    pub async fn openid_token(&self, client_id: &str, secret: &str, code: &str, redirect_uri: &str, verifier: &str, issuer: &str) -> Result<Tokens> {
        let bad_client = || Error::Unauthorized("invalid_client".into());
        let bad_grant = || Error::Invalid("invalid_grant".into());
        let client = self.companion_client(client_id).await?.ok_or_else(bad_client)?;
        if !crate::messages::subtle_eq(hex::encode(Sha256::digest(secret.as_bytes())).as_bytes(), client.secret_hash.as_bytes()) {
            return Err(bad_client());
        }
        let mut tx = self.db.system_tenant_tx(client.tenant).await?;
        type Row =
            (String, Uuid, Uuid, Uuid, i32, String, String, String, String, Option<String>, Option<DateTime<Utc>>, Option<Vec<String>>, bool, bool, bool);
        let row: Option<Row> = sqlx::query_as(
            "select client_id, tenant_id, user_id, session_id, token_version, issuer, redirect_uri, code_challenge, scope, nonce, auth_time, amr, \
                    waiting, used_at is not null, expires_at <= now() \
               from oidc_code where code_hash = $1 for update",
        )
        .bind(hash(code))
        .fetch_optional(&mut *tx)
        .await?;
        let (cid, tenant, user, session, tv, iss, redirect, challenge, scope, nonce, auth_time, amr, waiting, used, expired) = row.ok_or_else(bad_grant)?;
        if used && cid == client.client_id {
            // A code used twice leaked: what it gave stops answering, and the person's trail says so.
            sqlx::query("update oidc_code set access_expires_at = now() where code_hash = $1").bind(hash(code)).execute(&mut *tx).await?;
            gate::event(&mut tx, tenant, user, "companion_code_replayed", json!({"client_id": cid, "app": client.app, "companion": client.name})).await?;
            tx.commit().await?;
            return Err(bad_grant());
        }
        if cid != client.client_id || waiting || used || expired || redirect != redirect_uri || iss != issuer || !pkce_matches(verifier, &challenge) {
            return Err(bad_grant());
        }
        sqlx::query("update oidc_code set used_at = now() where code_hash = $1").bind(hash(code)).execute(&mut *tx).await?;
        if !self.session_live(&mut tx, tenant, session, user, tv).await? {
            return Err(bad_grant());
        }
        let ctx = self.load_ctx(tenant, user, Some(tv), String::new()).await.map_err(|_| bad_grant())?;
        if self.let_in(&ctx, &client).is_err() {
            return Err(bad_grant());
        }
        let subject = self.subject(tenant, user).await?;
        let now = Utc::now().timestamp();
        let mut claims = self.openid_claims(tenant, user, &subject, &scope, &client).await?;
        claims.insert("iss".into(), json!(issuer));
        claims.insert("aud".into(), json!(client.client_id));
        claims.insert("azp".into(), json!(client.client_id));
        claims.insert("iat".into(), json!(now));
        claims.insert("exp".into(), json!(now + TOKEN_SECS));
        if let Some(t) = auth_time {
            claims.insert("auth_time".into(), json!(t.timestamp()));
        }
        if let Some(n) = nonce {
            claims.insert("nonce".into(), json!(n));
        }
        if let Some(amr) = amr.filter(|a| !a.is_empty()) {
            claims.insert("amr".into(), json!(amr));
        }
        let id_token = self.openid_sign(tenant, &Value::Object(claims)).await?;
        let access_token = random_secret("fa_");
        sqlx::query("update oidc_code set access_hash = $2, access_expires_at = now() + make_interval(secs => $3) where code_hash = $1")
            .bind(hash(code))
            .bind(hash(&access_token))
            .bind(TOKEN_SECS as f64)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "insert into oidc_sign_in (tenant_id, session_id, client_id, user_id, token_version) values ($1, $2, $3, $4, $5) \
             on conflict (tenant_id, session_id, client_id) do update set token_version = excluded.token_version, signed_in_at = now(), ended_at = null",
        )
        .bind(tenant)
        .bind(session)
        .bind(&client.client_id)
        .bind(user)
        .bind(tv)
        .execute(&mut *tx)
        .await?;
        let detail = json!({"client_id": client.client_id, "app": client.app, "companion": client.name, "issuer": issuer, "session": session});
        gate::event(&mut tx, tenant, user, "companion_signed_in", detail).await?;
        tx.commit().await?;
        Ok(Tokens { id_token, access_token, scope })
    }

    /// What the user-info endpoint answers for `access_token`, asked at `issuer`: while its browser session is live.
    /// Anything else, the same `Unauthorized("invalid_token")`.
    pub async fn openid_userinfo(&self, access_token: &str, issuer: &str) -> Result<Map<String, Value>> {
        let bad = || Error::Unauthorized("invalid_token".into());
        let mut tx = self.db.system_tx().await?;
        let row: Option<(String, Uuid, Uuid, Uuid, i32, String)> = sqlx::query_as(
            "select client_id, tenant_id, user_id, session_id, token_version, scope from oidc_code \
             where access_hash = $1 and access_expires_at > now() and issuer = $2",
        )
        .bind(hash(access_token))
        .bind(issuer)
        .fetch_optional(&mut *tx)
        .await?;
        let (client_id, tenant, user, session, tv, scope) = row.ok_or_else(bad)?;
        let live = self.session_live(&mut tx, tenant, session, user, tv).await?;
        tx.commit().await?;
        let client = self.companion_client(&client_id).await?.filter(|_| live).ok_or_else(bad)?;
        let subject = self.subject(tenant, user).await?;
        self.openid_claims(tenant, user, &subject, &scope, &client).await
    }

    /// Whether browser session `session` of `user` is still live for what was given at token version `tv`.
    async fn session_live(&self, tx: &mut crate::Tx, tenant: Uuid, session: Uuid, user: Uuid, tv: i32) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, bool>(sqlx::AssertSqlSafe(format!(
            "select exists (select 1 from user_session s join app_user u on u.tenant_id = s.tenant_id and u.id = s.user_id \
             join tenant t on t.id = s.tenant_id where s.tenant_id = $1 and s.id = $2 and s.user_id = $3 and {})",
            live("$4")
        )))
        .bind(tenant)
        .bind(session)
        .bind(user)
        .bind(tv)
        .fetch_one(&mut **tx)
        .await?)
    }

    /// The subject workspace `tenant`'s tokens name `user` by: made once, at random, and kept.
    async fn subject(&self, tenant: Uuid, user: Uuid) -> Result<String> {
        let mut tx = self.db.system_tx().await?;
        sqlx::query("insert into oidc_subject (tenant_id, user_id, subject) values ($1, $2, $3) on conflict do nothing")
            .bind(tenant)
            .bind(user)
            .bind(random_secret(""))
            .execute(&mut *tx)
            .await?;
        let subject: String =
            sqlx::query_scalar("select subject from oidc_subject where tenant_id = $1 and user_id = $2").bind(tenant).bind(user).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(subject)
    }

    /// The claims about `user` that `scope` and the companion's app give: `sub`; `name` (profile); `email` and
    /// `email_verified` (email); then the app's own, which never name one of the platform's.
    async fn openid_claims(&self, tenant: Uuid, user: Uuid, subject: &str, scope: &str, client: &Client) -> Result<Map<String, Value>> {
        let mut claims = Map::new();
        claims.insert("sub".into(), json!(subject));
        let scopes: Vec<&str> = scope.split_whitespace().collect();
        if scopes.contains(&"profile") || scopes.contains(&"email") {
            let mut tx = self.db.system_tx().await?;
            let (email, name, verified): (String, String, bool) =
                sqlx::query_as("select email::text, coalesce(nullif(full_name, ''), email::text), email_verified_at is not null from app_user where tenant_id = $1 and id = $2")
                    .bind(tenant)
                    .bind(user)
                    .fetch_one(&mut *tx)
                    .await?;
            tx.commit().await?;
            if scopes.contains(&"profile") {
                claims.insert("name".into(), json!(name));
            }
            if scopes.contains(&"email") {
                claims.insert("email".into(), json!(email));
                claims.insert("email_verified".into(), json!(verified));
            }
        }
        let companion = self.hooks.companion_of(&client.app, &client.name).ok_or_else(|| Error::Forbidden("this program is no longer offered here".into()))?;
        for (key, value) in companion.claims(self, tenant, user).await? {
            if PLATFORM_CLAIMS.contains(&key.as_str()) {
                return Err(Error::Internal(format!("app {} gave its companion the claim {key}, which only the platform sets", client.app)));
            }
            claims.insert(key, value);
        }
        Ok(claims)
    }
}

// ── leaving ──

impl Kernel {
    /// Every sign-in to a companion whose browser session has ended, whichever way it ended (by this build or another,
    /// one device or all of them, the person or the workspace gone): marked ended, and its app told
    /// ([`SIGNED_OUT_TOPIC`]) in the same transaction, once however many replicas look at once. How many.
    pub async fn openid_sweep(&self) -> Result<u64> {
        let mut tx = self.db.system_tx().await?;
        let ended: Vec<(Uuid, Uuid, String, Uuid, String, String, String, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "with gone as ( \
               select i.tenant_id, i.session_id, i.client_id, \
                      case when u.id is null or not u.is_active or t.status is distinct from 'active' then 'access_ended' \
                           when u.token_version <> i.token_version then 'signed_out_everywhere' \
                           when s.id is null or s.revoked_at is not null then 'signed_out' \
                           else 'expired' end as reason \
                 from oidc_sign_in i \
                 left join user_session s on s.tenant_id = i.tenant_id and s.id = i.session_id \
                 left join app_user u on u.tenant_id = i.tenant_id and u.id = i.user_id \
                 left join tenant t on t.id = i.tenant_id \
                where i.ended_at is null and not coalesce({}, false) \
                order by i.signed_in_at limit 500 for update of i skip locked) \
             update oidc_sign_in i set ended_at = now() from gone g join oauth_companion c on c.client_id = g.client_id \
              where i.tenant_id = g.tenant_id and i.session_id = g.session_id and i.client_id = g.client_id \
             returning i.tenant_id, i.session_id, i.client_id, i.user_id, g.reason, c.app, c.name, \
                       (select subject from oidc_subject x where x.tenant_id = i.tenant_id and x.user_id = i.user_id)",
            live("i.token_version")
        )))
        .fetch_all(&mut *tx)
        .await?;
        for (tenant, session, client_id, user, reason, app, name, subject) in &ended {
            let payload =
                json!({"user": user, "subject": subject, "client_id": client_id, "app": app, "companion": name, "session": session, "reason": reason});
            outbox::enqueue(&mut tx, *tenant, SIGNED_OUT_TOPIC, Some("User"), Some(*user), payload).await?;
        }
        tx.commit().await?;
        Ok(ended.len() as u64)
    }

    /// Looks for ended sign-ins every few seconds ([`Kernel::openid_sweep`]), on a replica that runs background work.
    pub fn spawn_openid_sweep(self: &Arc<Self>, mut shutdown: tokio::sync::watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        let k = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = k.openid_sweep().await {
                    tracing::warn!("telling apps that sign-ins to their companions ended: {e}");
                }
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(SWEEP_EVERY) => {}
                }
            }
        })
    }

    /// Drops what is a day past its end (requests, codes and access tokens: a client's nonces are kept that long), and
    /// sign-ins a week after their app was told they ended.
    pub async fn openid_purge(&self) -> Result<u64> {
        let mut tx = self.db.system_tx().await?;
        let n = sqlx::query(
            "delete from oidc_code where expires_at < now() - interval '1 day' and (access_expires_at is null or access_expires_at < now() - interval '1 day')",
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
            + sqlx::query("delete from oidc_sign_in where ended_at < now() - interval '7 days'").execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        Ok(n)
    }
}

// ── what an operator sees ──

/// A workspace's companions and keys, as `ferrum task openid` shows them.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Overview {
    pub companions: Vec<Value>,
    pub keys: Vec<Value>,
}

impl Kernel {
    /// Workspace `tenant`'s companions (their apps, clients, return addresses and last sign-in) and its published keys.
    pub async fn openid_overview(&self, tenant: Uuid) -> Result<Overview> {
        let mut tx = self.db.system_tx().await?;
        let companions: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('client_id', c.client_id, 'app', c.app, 'companion', c.name, 'redirect_uris', c.redirect_uris, \
               'declared_at', c.created_at, 'changed_at', c.updated_at, \
               'last_sign_in', (select max(signed_in_at) from oidc_sign_in i where i.client_id = c.client_id), \
               'signed_in_now', (select count(*) from oidc_sign_in i where i.client_id = c.client_id and i.ended_at is null)) \
             from oauth_companion c where c.tenant_id = $1 order by c.app, c.name",
        )
        .bind(tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let keys = self.openid_keys(tenant).await?.iter().map(|k| json!({"kid": k.kid, "version": k.version, "until": k.until})).collect();
        Ok(Overview { companions, keys })
    }

    /// An operator takes a companion's client away (`ferrum task openid tenant:… remove:<client_id>`): every sign-in
    /// through it ends at once, and the app's next declaration makes it again. Recorded. Whether there was one.
    pub async fn openid_remove(&self, tenant: Uuid, client_id: &str) -> Result<bool> {
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let gone = take_away(&mut tx, tenant, client_id, Some("operator")).await?;
        tx.commit().await?;
        Ok(gone.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_named_by_its_thumbprint() {
        // RFC 8037 appendix A.3: the thumbprint of its Ed25519 example key.
        assert_eq!(thumbprint("11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"), "kPrK_qmxVWaYVA9wwBF6Iuo3vVzz7TxHCTwXBygrS4k");
    }

    #[test]
    fn requests_are_checked_as_the_specification_says() {
        let good = AuthRequest {
            response_type: "code".into(),
            client_id: "fo_x".into(),
            redirect_uri: "https://forge.example/cb".into(),
            scope: "openid profile email offline_access".into(),
            code_challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".into(),
            code_challenge_method: "S256".into(),
            ..Default::default()
        };
        let a = asked(&good).unwrap();
        assert_eq!(a.scope, "openid profile email", "only what it may have");
        let refused = |change: fn(&mut AuthRequest)| {
            let mut r = good.clone();
            change(&mut r);
            asked(&r).err().map(|e| e.error)
        };
        assert_eq!(refused(|r| r.response_type = "token".into()), Some("unsupported_response_type"));
        assert_eq!(refused(|r| r.scope = "profile".into()), Some("invalid_scope"));
        assert_eq!(refused(|r| r.code_challenge_method = "plain".into()), Some("invalid_request"), "PKCE S256 only");
        assert_eq!(refused(|r| r.code_challenge = String::new()), Some("invalid_request"));
        assert_eq!(refused(|r| r.nonce = Some(String::new())), Some("invalid_request"));
        assert_eq!(refused(|r| r.prompt = "none login".into()), Some("invalid_request"));
        assert_eq!(refused(|r| r.max_age = Some("-1".into())), Some("invalid_request"));
        assert_eq!(refused(|r| r.request = Some("x".into())), Some("request_not_supported"));
        assert_eq!(refused(|r| r.request_uri = Some("x".into())), Some("request_uri_not_supported"));
        assert!(refused(|r| r.prompt = "login consent".into()).is_none());
    }

    #[test]
    fn a_proof_does_only_when_it_is_recent_enough() {
        let now = Utc::now();
        let asked = now - chrono::Duration::seconds(30);
        let before = Some(asked - chrono::Duration::seconds(10));
        let after = Some(asked + chrono::Duration::seconds(10));
        assert!(fresh_enough(before, asked, None, false, now), "nothing asked: the session's proof does");
        assert!(!fresh_enough(None, asked, None, true, now), "never one that is not there");
        assert!(!fresh_enough(before, asked, None, true, now), "prompt=login: one made after the request");
        assert!(fresh_enough(after, asked, None, true, now));
        assert!(fresh_enough(before, asked, Some(60), false, now), "40 seconds old, max_age 60");
        assert!(!fresh_enough(before, asked, Some(30), false, now), "40 seconds old, max_age 30");
        assert!(!fresh_enough(None, asked, Some(3600), false, now));
    }

    #[test]
    fn codes_and_tokens_never_show_themselves() {
        assert_eq!(format!("{:?}", Authorized::Code("c0de".into())), "Authorized::Code(..)");
        assert_eq!(format!("{:?}", Authorized::Confirm("w4it".into())), "Authorized::Confirm(..)");
        let t = Tokens { id_token: "id.tok.en".into(), access_token: "fa_secret".into(), scope: "openid".into() };
        assert!(!format!("{t:?}").contains("fa_secret") && !format!("{t:?}").contains("id.tok.en"));
    }

    #[test]
    fn names_and_addresses() {
        assert!(name_ok("forge") && name_ok("ci-runner") && !name_ok("Forge") && !name_ok("") && !name_ok("a b") && !name_ok("-x"));
        assert!(redirect_ok("https://forge.example/user/oauth2/acme/callback") && redirect_ok("http://127.0.0.1:3000/cb"));
        assert!(!redirect_ok("http://forge.example/cb") && !redirect_ok("com.example.app:/cb") && !redirect_ok("https://forge.example/cb#x"));
    }
}
