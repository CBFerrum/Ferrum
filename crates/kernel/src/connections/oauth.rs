//! A person authorises an account (BD-32): a sign-in broker for connections that act for a person, or for the
//! workspace's own account, at a provider. The provider is a description (its addresses, scopes, client, how it names
//! the account); the flow is the authorisation code flow with PKCE, its state bound to the person and their browser
//! session and used once, the redirect address the platform's own and exact. Tokens are sealed, refreshed before they
//! expire by a job, and never shown back; a refresh the provider refuses marks the connection "needs reconnecting" and
//! tells its owner once; disconnecting revokes at the provider when it offers that.

use super::{Auth, Loaded, Outcome, Reach, Request, is_name, pointer_problem};
use crate::ctx::Ctx;
use crate::db::Tx;
use crate::error::{Error, Result};
use crate::jobs::{Job, JobHandler, NewJob};
use crate::{Kernel, audit};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

pub const REFRESH_KIND: &str = "platform.connection.refresh";
/// The path the provider sends the browser back to, on the platform's public address.
pub const CALLBACK: &str = "/api/connections/callback";
/// How long a sign-in may take.
const SIGNIN_MINUTES: i64 = 10;
/// Tokens are refreshed this long before they expire, by the job; a call refreshes one expiring sooner.
const REFRESH_AHEAD_MINUTES: i64 = 10;
const CALL_AHEAD_SECS: i64 = 60;
/// Token answers larger than this are refused.
const TOKEN_BYTES: u64 = 64 << 10;
const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);
/// What the platform keeps among a connection's sealed secrets for it (names no administrator can give, since a
/// secret's name starts with a letter): the client a provider registered, and the DPoP key pair (PKCS#8, base64).
pub(crate) const REGISTERED_ID: &str = "_client_id";
pub(crate) const REGISTERED_SECRET: &str = "_client_secret";
pub(crate) const DPOP_KEY: &str = "_dpop_key";

/// The last DPoP nonce each server gave a connection (RFC 9449 §8, §9), by connection and the server's address.
static NONCES: std::sync::LazyLock<std::sync::Mutex<BTreeMap<(Uuid, String), String>>> = std::sync::LazyLock::new(Default::default);

fn origin_of(url: &reqwest::Url) -> String {
    format!("{}://{}", url.scheme(), url.authority())
}

/// Keeps the DPoP nonce an answer gives (its `DPoP-Nonce` header) for the next proof to that server; whether it gave
/// one.
pub(crate) fn remember_nonce(conn: Uuid, a: &super::Answer) -> bool {
    let Some(n) = a.header("dpop-nonce").filter(|n| !n.is_empty() && n.len() <= 500) else { return false };
    let mut nonces = NONCES.lock().unwrap_or_else(|p| p.into_inner());
    if nonces.len() > 10_000 {
        nonces.clear();
    }
    nonces.insert((conn, origin_of(&a.url)), n.to_string());
    true
}

/// Whose account a connection acts for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    /// The organisation's, connected by an administrator.
    #[default]
    Workspace,
    /// Each person's own (their mailbox): every call is made with the caller's.
    Person,
}

/// How the client proves itself at the token address.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAuth {
    #[default]
    Body,
    Basic,
}

/// A provider, described.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub authorize_url: String,
    pub token_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoke_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// Empty when the provider registers the client itself (`registration_url`).
    #[serde(default)]
    pub client_id: String,
    /// The provider's dynamic client registration address (RFC 7591), from its metadata's `registration_endpoint`:
    /// with no `client_id`, the first sign-in registers the platform's client there and keeps it sealed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_url: Option<String>,
    /// Tokens bound to a key pair of the connection's own (DPoP, RFC 9449): a proof on the token request and on every
    /// call, which the provider demands.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dpop: bool,
    /// Names the connection's secret holding the client secret; none for a public client (PKCE alone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub client_auth: ClientAuth,
    /// More parameters for the authorisation address (`prompt`, `access_type`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
    /// Where the token answer names the account: a pointer, `/id_token/<claim>` reading the id token's claims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_at: Option<String>,
    /// Fields of the token answer (and of `after`'s results) kept, sealed, with the tokens: `auth.<name>` (BD-61).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keep: Vec<String>,
    /// Operations made after the code is exchanged, in order, given `inputs.access_token`, `refresh_token` and
    /// `client_id`: each result's names (`access_token`, `refresh_token`, `expires_in`, and any to keep) replace the
    /// answer's (a long-lived token for a short one, the account's id).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// An operation that refreshes in place of the token address (a GET with the token itself), given the same
    /// inputs; its result is read as a token answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<String>,
    /// An operation that registers the platform's client at this server (its own request, not RFC 7591's), given
    /// `inputs.redirect_uri`, `scopes` and `client_name`; its result's `client_id` and `client_secret` are kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration: Option<String>,
    /// Its tokens never expire, whatever the token answer says: never refreshed ahead.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub never_expires: bool,
    /// What the provider calls the client's id on the authorisation address and the token requests (`client_key`);
    /// `client_id` unless said (BD-72).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id_param: Option<String>,
    /// What scopes are joined with: a space unless said (`,` for a provider that wants commas).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_separator: Option<String>,
    /// Kept fields that are not secret (a handle a permalink is made from): a result may show them (BD-77).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_secret: Vec<String>,
}

impl Provider {
    pub(crate) fn id_param(&self) -> &str {
        self.client_id_param.as_deref().unwrap_or("client_id")
    }

    pub(crate) fn scope(&self) -> String {
        self.scopes.join(self.scope_separator.as_deref().unwrap_or(" "))
    }
}

fn address_problem(name: &str, url: &str) -> Option<String> {
    match reqwest::Url::parse(url) {
        Ok(u) if matches!(u.scheme(), "https" | "http") && u.host_str().is_some() && u.username().is_empty() && u.fragment().is_none() => None,
        _ => Some(format!("auth.provider.{name} must be an https address")),
    }
}

impl Provider {
    pub(crate) fn problem(&self) -> Option<String> {
        if let Some(p) = address_problem("authorize_url", &self.authorize_url)
            .or_else(|| address_problem("token_url", &self.token_url))
            .or_else(|| self.revoke_url.as_deref().and_then(|u| address_problem("revoke_url", u)))
            .or_else(|| self.registration_url.as_deref().and_then(|u| address_problem("registration_url", u)))
        {
            return Some(p);
        }
        if self.client_id.trim().is_empty() && self.registration_url.is_none() && self.registration.is_none() {
            return Some("auth.provider.client_id is required, unless the provider registers clients (registration_url or registration)".into());
        }
        if self.keep.iter().any(|k| !is_name(k) || ["access_token", "refresh_token"].contains(&k.as_str())) {
            return Some("auth.provider.keep names fields of the token answer (not the tokens)".into());
        }
        if self.not_secret.iter().any(|n| !self.keep.contains(n)) {
            return Some("auth.provider.not_secret names fields it keeps".into());
        }
        if self.client_secret.as_deref().is_some_and(|s| !is_name(s)) {
            return Some("auth.provider.client_secret names one of the connection's secrets".into());
        }
        if self.scopes.iter().any(|s| s.is_empty() || s.contains(char::is_whitespace)) {
            return Some("auth.provider.scopes are words without spaces".into());
        }
        if self.client_id_param.as_deref().is_some_and(|p| !is_name(p)) {
            return Some("auth.provider.client_id_param is a parameter's name".into());
        }
        if self.scope_separator.as_deref().is_some_and(|s| ![" ", ",", "+", ";"].contains(&s)) {
            return Some("auth.provider.scope_separator is a space, a comma, a plus or a semicolon".into());
        }
        if ["response_type", "client_id", "redirect_uri", "state", "code_challenge", "code_challenge_method", "scope", self.id_param()]
            .iter()
            .any(|p| self.params.contains_key(*p))
        {
            return Some("auth.provider.params cannot set what the flow sets".into());
        }
        self.account_at.as_deref().and_then(pointer_problem)
    }
}

fn host_of(url: &str) -> String {
    reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.trim_matches(['[', ']']).to_string())).unwrap_or_default()
}

fn random() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn digest(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub(crate) fn token_label(tenant: Uuid, conn: Uuid, user: Uuid) -> String {
    format!("conn_token:{tenant}:{conn}:{user}")
}

fn signin_label(tenant: Uuid, state_digest: &str) -> String {
    format!("conn_signin:{tenant}:{state_digest}")
}

const STATE_LABEL: &str = "connection sign-in state";

/// A sign-in's `state` that names where it started: `{random}.{workspace|host, base64url}.{mac}`,
/// signed with the server key so nobody can change either.
fn state_from(key: &crate::secret::SecretKey, tenant: Uuid, host: &str) -> String {
    let said = URL_SAFE_NO_PAD.encode(format!("{tenant}|{host}"));
    let nonce = random();
    let mac = key.fingerprint(STATE_LABEL, &format!("{nonce}.{said}"));
    format!("{nonce}.{said}.{mac}")
}

/// The workspace and host a `state` of [`state_from`] names, when its signature holds.
fn state_names(key: &crate::secret::SecretKey, state: &str) -> Option<(Uuid, String)> {
    let mut parts = state.splitn(3, '.');
    let (nonce, said, mac) = (parts.next()?, parts.next()?, parts.next()?);
    let want = key.fingerprint(STATE_LABEL, &format!("{nonce}.{said}"));
    if want.len() != mac.len() || !subtle_eq(want.as_bytes(), mac.as_bytes()) {
        return None;
    }
    let said = String::from_utf8(URL_SAFE_NO_PAD.decode(said).ok()?).ok()?;
    let (tenant, host) = said.split_once('|')?;
    Some((tenant.parse().ok()?, host.to_string()))
}

/// Compares in time that does not depend on where the bytes differ.
fn subtle_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// What a token address said.
enum Granted {
    Tokens(Value),
    /// It refused (a code or a refresh token that is no good): no use trying again.
    Refused(String),
    /// It did not answer, or said "later".
    Later(String),
}

/// The provider and whose account, when the connection signs in.
fn oauth(conn: &Loaded) -> Result<(&Provider, Owner)> {
    match &conn.desc.auth {
        super::Auth::Oauth { provider, owner } => Ok((provider, *owner)),
        _ => Err(Error::invalid(format!("{} does not sign in to an account", conn.name))),
    }
}

/// The client the platform is at the provider: the described one, else the one it registered. Its id and secret.
fn client(conn: &Loaded, provider: &Provider) -> Result<(String, Option<String>)> {
    let registered = |n: &str| conn.secrets.get(n).filter(|v| !v.is_empty()).cloned();
    let id = match provider.client_id.trim() {
        "" => registered(REGISTERED_ID).ok_or_else(|| Error::invalid(format!("{} has no client at the provider yet: connect it first", conn.name)))?,
        id => id.to_string(),
    };
    let secret = match &provider.client_secret {
        Some(name) => Some(conn.secret(name)?.to_string()),
        None if provider.client_id.trim().is_empty() => registered(REGISTERED_SECRET),
        None => None,
    };
    Ok((id, secret))
}

/// When a token answer says it runs out: seconds from now, a Unix time (past 10^9), or a moment.
pub(crate) fn expiry(v: &Value) -> Option<DateTime<Utc>> {
    let n = match v {
        Value::Number(n) => n.as_i64()?,
        Value::String(s) => match s.trim().parse::<i64>() {
            Ok(n) => n,
            Err(_) => return DateTime::parse_from_rfc3339(s.trim()).ok().map(|t| t.to_utc()),
        },
        _ => return None,
    };
    if n >= 1_000_000_000 { DateTime::from_timestamp(n, 0) } else { Some(Utc::now() + chrono::Duration::seconds(n.clamp(0, 365 * 86400))) }
}

/// The claims of an id token, unverified: it came straight from the token address, over TLS (OpenID Connect 3.1.3.7).
fn claims(id_token: &str) -> Value {
    id_token
        .split('.')
        .nth(1)
        .and_then(|p| URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

impl Kernel {
    /// The exact address the provider sends the browser back to.
    pub fn connection_callback(&self) -> Result<String> {
        let base = self
            .settings
            .callback_base()
            .ok_or_else(|| Error::Unavailable("signing in to an account needs the server's public address (PUBLIC_URL)".into()))?;
        Ok(format!("{base}{CALLBACK}"))
    }

    /// Starts a sign-in to connection `id`'s account for the person `ctx` in browser session `session`: the address to
    /// send their browser to. The workspace's account is an administrator's to connect; a person connects their own
    /// where the connection lets them use it.
    pub async fn connect_start(&self, ctx: &Ctx, id: Uuid, session: Uuid) -> Result<String> {
        self.connect_start_on(ctx, id, session, None).await
    }

    /// [`Kernel::connect_start`] from a page on `host`: the `state` names the workspace and that
    /// host, signed, so the return address (on the public address or the hooks host, where the
    /// person may have no session) can pass the browser on to the host it started from
    /// ([`Kernel::connection_state_host`]), where the session and every check of
    /// [`Kernel::connect_finish`] are.
    pub async fn connect_start_on(&self, ctx: &Ctx, id: Uuid, session: Uuid, host: Option<&str>) -> Result<String> {
        let conn = self.load_connection(ctx.tenant, &id.to_string()).await?;
        let (provider, owner) = oauth(&conn)?;
        let owner_id = match owner {
            Owner::Workspace if ctx.is_admin => Uuid::nil(),
            Owner::Workspace => return Err(Error::Forbidden("the workspace's account is connected by an administrator".into())),
            Owner::Person if ctx.outside || ctx.user.is_nil() => return Err(Error::Forbidden("a person of the workspace connects their own account".into())),
            Owner::Person => {
                if !conn.enabled {
                    return Err(Error::invalid(format!("the connection {} is turned off", conn.name)));
                }
                super::may_use(ctx, None, &conn.desc)?;
                ctx.user
            }
        };
        let redirect = self.connection_callback()?;
        let key = self.settings.secret_key.as_ref().ok_or_else(|| Error::Unavailable("signing in to an account needs the server's secret key".into()))?;
        // A provider that registers its clients: registered once, the first time anyone connects.
        let conn = if provider.client_id.trim().is_empty() { self.register_client(&conn, &redirect).await? } else { conn };
        let (provider, _) = oauth(&conn)?;
        let (client_id, _) = client(&conn, provider)?;
        let host = host.and_then(crate::shell::normalize_host);
        let (state, verifier) = (host.map_or_else(random, |h| state_from(key, ctx.tenant, &h)), random());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state_digest = digest(&state);
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query(
            "insert into conn_signin (state_digest, tenant_id, conn_id, user_id, owner_id, session_id, verifier, expires_at) \
             values ($1, $2, $3, $4, $5, $6, $7, now() + make_interval(mins => $8::int))",
        )
        .bind(&state_digest)
        .bind(ctx.tenant)
        .bind(id)
        .bind(ctx.user)
        .bind(owner_id)
        .bind(session)
        .bind(key.seal(&signin_label(ctx.tenant, &state_digest), &verifier)?)
        .bind(SIGNIN_MINUTES)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut url = reqwest::Url::parse(&provider.authorize_url).map_err(|_| Error::invalid("the provider's authorize_url is not an address"))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("response_type", "code").append_pair(provider.id_param(), &client_id).append_pair("redirect_uri", &redirect);
            if !provider.scopes.is_empty() {
                q.append_pair("scope", &provider.scope());
            }
            q.append_pair("state", &state).append_pair("code_challenge", &challenge).append_pair("code_challenge_method", "S256");
            for (k, v) in &provider.params {
                q.append_pair(k, v);
            }
        }
        Ok(url.into())
    }

    /// The workspace and the host a sign-in started on, when its `state` names them under this
    /// server's key ([`Kernel::connect_start_on`]); nothing for any other state.
    pub fn connection_state_host(&self, state: &str) -> Option<(Uuid, String)> {
        state_names(self.settings.secret_key.as_ref()?, state)
    }

    /// The provider sent the browser back: the sign-in `state` started must be this person's, in this browser session,
    /// unused and recent, or nothing happens. The code is exchanged, the tokens sealed. The connection's id.
    pub async fn connect_finish(&self, ctx: &Ctx, session: Uuid, state: &str, code: Option<&str>, error: Option<&str>) -> Result<Uuid> {
        let not_here = || Error::Unauthorized("this sign-in was not started here, or has been used".into());
        let state_digest = digest(state);
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(Uuid, Uuid, Uuid, Uuid, String, DateTime<Utc>)> = sqlx::query_as(
            "delete from conn_signin where state_digest = $1 and tenant_id = $2 returning conn_id, user_id, owner_id, session_id, verifier, expires_at",
        )
        .bind(&state_digest)
        .bind(ctx.tenant)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let (conn_id, user, owner_id, started_in, sealed, expires) = row.ok_or_else(not_here)?;
        if user != ctx.user || started_in != session || expires < Utc::now() {
            return Err(not_here());
        }
        if let Some(e) = error {
            let said: String = e.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(60).collect();
            return Err(Error::invalid(format!("the provider did not connect the account: {said}")));
        }
        let code = code.filter(|c| !c.is_empty() && c.len() <= 2048).ok_or_else(|| Error::invalid("the provider sent no code"))?;
        let verifier = self.open_sealed(&signin_label(ctx.tenant, &state_digest), &sealed)?;
        let conn = self.load_connection(ctx.tenant, &conn_id.to_string()).await?;
        let (provider, _) = oauth(&conn)?;
        let form = [("grant_type", "authorization_code"), ("code", code), ("redirect_uri", &self.connection_callback()?), ("code_verifier", &verifier)];
        let mut answer = match self.token_request(&conn, provider, &provider.token_url, &form).await? {
            Granted::Tokens(t) => t,
            Granted::Refused(why) | Granted::Later(why) => return Err(Error::invalid(format!("the provider did not give tokens: {why}"))),
        };
        // What follows the sign-in (a long-lived token for a short one, the account's facts): each result over the answer.
        for op in &provider.after {
            match self.signing_op(&conn, op, self.token_inputs(&conn, &answer)?, &answer).await? {
                Granted::Tokens(Value::Object(more)) => answer.as_object_mut().into_iter().for_each(|a| a.extend(more.clone())),
                Granted::Tokens(_) => {}
                Granted::Refused(why) | Granted::Later(why) => return Err(Error::invalid(format!("{op} after the sign-in failed: {why}"))),
            }
        }
        let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
        let account = self.keep_tokens(&mut tx, &conn, owner_id, &answer, None, Some(ctx.user)).await?;
        // A person's own account: their pulls of it run (again), as them.
        if !owner_id.is_nil() {
            super::pull::person_connected(&mut tx, ctx.tenant, conn_id, owner_id, &conn.desc).await?;
        }
        audit::record_admin(
            &mut tx,
            ctx,
            "Connection",
            conn_id,
            "connect",
            None,
            json!({"owner": if owner_id.is_nil() { "workspace" } else { "person" }, "account": account}),
        )
        .await?;
        tx.commit().await?;
        Ok(conn_id)
    }

    /// A form to a token or revocation address, the client proving itself as the provider says; with a DPoP proof at
    /// the token address when the provider binds tokens, tried once more with the nonce it asks for.
    async fn token_request(&self, conn: &Loaded, provider: &Provider, url: &str, form: &[(&str, &str)]) -> Result<Granted> {
        let mut body = reqwest::Url::parse("http://form/").map_err(Error::internal)?;
        let mut headers = vec![("accept".to_string(), "application/json".to_string())];
        let (client_id, secret) = client(conn, provider)?;
        {
            let mut q = body.query_pairs_mut();
            q.extend_pairs(form.iter().copied());
            // A client the provider registered proves itself as it registered: with basic authentication.
            let basic = provider.client_auth == ClientAuth::Basic || (provider.client_id.trim().is_empty() && secret.is_some());
            match (basic, secret) {
                (true, Some(s)) => {
                    let pair = base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:{s}"));
                    headers.push(("authorization".into(), format!("Basic {pair}")));
                }
                (_, secret) => {
                    q.append_pair(provider.id_param(), &client_id);
                    if let Some(s) = secret {
                        q.append_pair("client_secret", &s);
                    }
                }
            }
        }
        let address = reqwest::Url::parse(url).map_err(|_| Error::invalid("the provider's address is not an address"))?;
        let bound = provider.dpop && url == provider.token_url;
        let reach = Reach { hosts: vec![host_of(url)], any: false, timeout: TOKEN_TIMEOUT, max_bytes: TOKEN_BYTES };
        let mask = conn.mask();
        let mut tries = 0;
        let got = loop {
            tries += 1;
            let mut headers = headers.clone();
            if bound {
                headers.push(("dpop".into(), self.dpop_proof(conn, &reqwest::Method::POST, &address, None).await?));
            }
            let req = Request {
                method: reqwest::Method::POST,
                url: address.clone(),
                headers,
                body: Some((body.query().unwrap_or_default().as_bytes().to_vec(), "application/x-www-form-urlencoded".to_string())),
            };
            let got = self.fetch(conn.tenant, req, &reach).await;
            if bound
                && let Ok(a) = &got
                && remember_nonce(conn.id, a)
                && tries == 1
                && a.status == 400
                && a.body.get("error").and_then(Value::as_str) == Some("use_dpop_nonce")
            {
                continue;
            }
            break got;
        };
        // A token answer written as a form (`access_token=…&expires_in=…`) is read as one.
        let got = got.map(|mut a| {
            if let Value::String(text) = &a.body
                && text.contains('=')
            {
                a.body = super::fns::form_value(text.as_bytes());
            }
            a
        });
        Ok(match got {
            Ok(a) if (200..300).contains(&a.status) && a.body.get("access_token").and_then(Value::as_str).is_some_and(|t| !t.is_empty()) => {
                Granted::Tokens(a.body)
            }
            Ok(a) if (200..300).contains(&a.status) => Granted::Tokens(a.body),
            Ok(a) if super::TEMPORARY.contains(&a.status) => Granted::Later(format!("{}", a.status)),
            Ok(a) => Granted::Refused(super::masked_text(&format!("{}: {}", a.status, a.body.get("error").and_then(Value::as_str).unwrap_or_default()), &mask)),
            Err(Error::Unavailable(m)) => Granted::Later(super::masked_text(&m, &mask)),
            Err(e) => Granted::Refused(super::masked_text(&e.to_string(), &mask)),
        })
    }

    /// Registers the platform's client at a provider offering dynamic registration (RFC 7591), once: the client it
    /// gives is sealed among the connection's secrets (a second sign-in, or two at once, uses it). The connection,
    /// read again.
    async fn register_client(&self, conn: &Loaded, redirect: &str) -> Result<Loaded> {
        let (provider, _) = oauth(conn)?;
        let mut tx = self.db.system_tenant_tx(conn.tenant).await?;
        let held = super::lock_secrets(&mut tx, self, conn.tenant, conn.id).await?;
        if let Some(op) = provider.registration.as_deref().filter(|_| held.get(REGISTERED_ID).is_none_or(|v| v.is_empty())) {
            // The server's own registration request, an operation of the connection.
            let inputs = json!({"redirect_uri": redirect, "scopes": provider.scopes.join(" "), "client_name": conn.name});
            let given = match self.signing_op(conn, op, inputs, &Value::Null).await? {
                Granted::Tokens(r) => r,
                Granted::Refused(why) | Granted::Later(why) => return Err(Error::invalid(format!("the server did not register a client: {why}"))),
            };
            let id =
                given["client_id"].as_str().filter(|i| !i.is_empty() && i.len() <= 500).ok_or_else(|| Error::invalid(format!("{op} gave no client_id")))?;
            let mut set = BTreeMap::from([(REGISTERED_ID.to_string(), id.to_string())]);
            if let Some(secret) = given["client_secret"].as_str().filter(|s| !s.is_empty()) {
                set.insert(REGISTERED_SECRET.into(), secret.to_string());
            }
            super::store_secrets(&mut tx, self, conn.tenant, conn.id, held, set, &[]).await?;
            let system = Ctx::system(conn.tenant, self.registry.schema(conn.tenant).await?);
            audit::record_admin(&mut tx, &system, "Connection", conn.id, "register_client", None, json!({"client_id": id})).await?;
            tx.commit().await?;
            return self.load_connection(conn.tenant, &conn.id.to_string()).await;
        }
        let url = provider.registration_url.clone().filter(|_| provider.registration.is_none());
        if held.get(REGISTERED_ID).is_none_or(|v| v.is_empty()) {
            let url = url.ok_or_else(|| Error::invalid("auth.provider.client_id is required"))?;
            let mut asked = json!({"client_name": conn.name, "redirect_uris": [redirect], "grant_types": ["authorization_code", "refresh_token"],
                                   "response_types": ["code"], "token_endpoint_auth_method": "client_secret_basic"});
            if !provider.scopes.is_empty() {
                asked["scope"] = json!(provider.scopes.join(" "));
            }
            if provider.dpop {
                asked["dpop_bound_access_tokens"] = json!(true);
            }
            let req = Request {
                method: reqwest::Method::POST,
                url: reqwest::Url::parse(&url).map_err(|_| Error::invalid("the provider's registration_url is not an address"))?,
                headers: vec![("accept".into(), "application/json".into())],
                body: Some((asked.to_string().into_bytes(), "application/json".to_string())),
            };
            let a = self.fetch(conn.tenant, req, &Reach { hosts: vec![host_of(&url)], any: false, timeout: TOKEN_TIMEOUT, max_bytes: TOKEN_BYTES }).await?;
            let id = a.body.get("client_id").and_then(Value::as_str).filter(|i| (200..300).contains(&a.status) && !i.is_empty() && i.len() <= 500);
            let Some(id) = id else {
                return Err(Error::invalid(format!("the provider did not register a client ({})", a.status)));
            };
            let mut set = BTreeMap::from([(REGISTERED_ID.to_string(), id.to_string())]);
            if let Some(secret) = a.body.get("client_secret").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                set.insert(REGISTERED_SECRET.into(), secret.to_string());
            }
            super::store_secrets(&mut tx, self, conn.tenant, conn.id, held, set, &[]).await?;
            let system = Ctx::system(conn.tenant, self.registry.schema(conn.tenant).await?);
            audit::record_admin(&mut tx, &system, "Connection", conn.id, "register_client", None, json!({"client_id": id})).await?;
        }
        tx.commit().await?;
        self.load_connection(conn.tenant, &conn.id.to_string()).await
    }

    /// The connection's DPoP key pair (PKCS#8), made and sealed among its secrets the first time.
    async fn dpop_key(&self, conn: &Loaded) -> Result<Vec<u8>> {
        use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
        let decode = |k: &str| base64::engine::general_purpose::STANDARD.decode(k).map_err(Error::internal);
        if let Some(k) = conn.secrets.get(DPOP_KEY) {
            return decode(k);
        }
        let mut tx = self.db.system_tenant_tx(conn.tenant).await?;
        let held = super::lock_secrets(&mut tx, self, conn.tenant, conn.id).await?;
        if let Some(k) = held.get(DPOP_KEY).cloned() {
            tx.commit().await?;
            return decode(&k);
        }
        let rng = ring::rand::SystemRandom::new();
        let made = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).map_err(|_| Error::internal("no key pair"))?;
        let pkcs8 = made.as_ref().to_vec();
        let set = BTreeMap::from([(DPOP_KEY.to_string(), base64::engine::general_purpose::STANDARD.encode(&pkcs8))]);
        super::store_secrets(&mut tx, self, conn.tenant, conn.id, held, set, &[]).await?;
        tx.commit().await?;
        Ok(pkcs8)
    }

    /// A DPoP proof (RFC 9449 §4.2) of a request: ES256 under the connection's key, its public key in the header, a
    /// fresh `jti`, the method, the address without query or fragment, the time, the access token's digest (`ath`)
    /// on a call, and the nonce that server last gave.
    pub(crate) async fn dpop_proof(&self, conn: &Loaded, method: &reqwest::Method, url: &reqwest::Url, token: Option<&str>) -> Result<String> {
        use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
        let rng = ring::rand::SystemRandom::new();
        let pair =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &self.dpop_key(conn).await?, &rng).map_err(|_| Error::internal("the DPoP key"))?;
        let point = pair.public_key().as_ref();
        let jwk = json!({"kty": "EC", "crv": "P-256", "x": URL_SAFE_NO_PAD.encode(&point[1..33]), "y": URL_SAFE_NO_PAD.encode(&point[33..65])});
        let mut htu = url.clone();
        htu.set_query(None);
        htu.set_fragment(None);
        let mut claims = json!({"jti": random(), "htm": method.as_str(), "htu": htu.as_str(), "iat": Utc::now().timestamp()});
        if let Some(t) = token {
            claims["ath"] = json!(URL_SAFE_NO_PAD.encode(Sha256::digest(t.as_bytes())));
        }
        if let Some(n) = NONCES.lock().unwrap_or_else(|p| p.into_inner()).get(&(conn.id, origin_of(url))) {
            claims["nonce"] = json!(n);
        }
        let header = json!({"typ": "dpop+jwt", "alg": "ES256", "jwk": jwk});
        let signing = format!("{}.{}", URL_SAFE_NO_PAD.encode(header.to_string()), URL_SAFE_NO_PAD.encode(claims.to_string()));
        let sig = pair.sign(&rng, signing.as_bytes()).map_err(|_| Error::internal("a DPoP proof"))?;
        Ok(format!("{signing}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref())))
    }

    /// Seals what a token address gave for `owner` (nil: the workspace's), keeping `refresh` when it gave none; marks
    /// it connected. The account it names.
    async fn keep_tokens(&self, tx: &mut Tx, conn: &Loaded, owner: Uuid, answer: &Value, before: Option<&Value>, by: Option<Uuid>) -> Result<Option<String>> {
        let refresh = before.and_then(|b| b["refresh_token"].as_str()).filter(|r| !r.is_empty()).map(str::to_string);
        let access =
            answer.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty()).ok_or_else(|| Error::invalid("the provider gave no access token"))?;
        let refresh = answer.get("refresh_token").and_then(Value::as_str).map(str::to_string).or(refresh);
        let never = matches!(&conn.desc.auth, Auth::Oauth { provider, .. } if provider.never_expires);
        let expires = answer
            .get("expires_in")
            .filter(|_| !never)
            .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
            .map(|s| Utc::now() + chrono::Duration::seconds(s.clamp(0, 365 * 86400)));
        let mut seen = answer.clone();
        if let Some(id) = answer.get("id_token").and_then(Value::as_str) {
            seen["id_token"] = claims(id);
        }
        let account_at = match &conn.desc.auth {
            Auth::Oauth { provider, .. } => provider.account_at.as_deref(),
            _ => None,
        };
        let account = super::at(&seen, account_at).map(|a| a.chars().take(200).collect::<String>());
        let key = self.settings.secret_key.as_ref().ok_or_else(|| Error::Unavailable("tokens are sealed with the server's secret key".into()))?;
        // What the sign-in keeps (`auth.<name>`): this answer's, else what was kept before.
        let names: &[String] = match &conn.desc.auth {
            Auth::Oauth { provider, .. } => &provider.keep,
            Auth::Session { keep, .. } => keep,
            _ => &[],
        };
        let mut kept = before.and_then(|b| b.get("kept")).cloned().unwrap_or_else(|| json!({}));
        for n in names {
            if let Some(v) = answer.get(n).filter(|v| !v.is_null()) {
                kept[n.as_str()] = v.clone();
            }
        }
        let sealed =
            key.seal(&token_label(conn.tenant, conn.id, owner), &json!({"access_token": access, "refresh_token": refresh, "kept": kept}).to_string())?;
        sqlx::query(
            "insert into conn_token (tenant_id, conn_id, user_id, sealed, expires_at, account, status, told, connected_by) values ($1, $2, $3, $4, $5, $6, 'connected', false, $7) \
             on conflict (tenant_id, conn_id, user_id) do update set sealed = excluded.sealed, expires_at = excluded.expires_at, \
             account = coalesce(excluded.account, conn_token.account), status = 'connected', told = false, \
             connected_by = coalesce(excluded.connected_by, conn_token.connected_by), updated_at = now()",
        )
        .bind(conn.tenant)
        .bind(conn.id)
        .bind(owner)
        .bind(sealed)
        .bind(expires)
        .bind(&account)
        .bind(by)
        .execute(&mut **tx)
        .await?;
        Ok(account)
    }

    /// Whose tokens a call by `ctx` uses: the workspace's, or the caller's own.
    pub(crate) fn token_owner(conn: &Loaded, ctx: &Ctx) -> Result<Uuid> {
        if matches!(conn.desc.auth, Auth::Session { .. }) {
            return Ok(Uuid::nil());
        }
        match oauth(conn)?.1 {
            Owner::Workspace => Ok(Uuid::nil()),
            Owner::Person if ctx.user.is_nil() => Err(Error::invalid(format!("{} is each person's own: it is used as a person", conn.name))),
            Owner::Person => Ok(ctx.user),
        }
    }

    /// What a call by `ctx` reads of its signed-in account (BD-60): the account the sign-in named, and what it kept
    /// (`auth.<name>`, BD-61). Nulls before anyone signed in.
    pub(crate) async fn account_vars(&self, conn: &Loaded, ctx: &Ctx) -> Result<(Value, Value)> {
        if !matches!(conn.desc.auth, Auth::Oauth { .. } | Auth::Session { .. }) {
            return Ok((Value::Null, json!({})));
        }
        let owner = Self::token_owner(conn, ctx)?;
        let mut tx = self.db.system_tenant_tx(conn.tenant).await?;
        let row: Option<(Option<String>, String)> =
            sqlx::query_as("select account, sealed from conn_token where tenant_id = $1 and conn_id = $2 and user_id = $3")
                .bind(conn.tenant)
                .bind(conn.id)
                .bind(owner)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        let Some((account, sealed)) = row else { return Ok((Value::Null, json!({}))) };
        let open: Value = serde_json::from_str(&self.open_sealed(&token_label(conn.tenant, conn.id, owner), &sealed)?).map_err(Error::internal)?;
        Ok((json!(account), open.get("kept").cloned().unwrap_or_else(|| json!({}))))
    }

    /// The access token a call by `ctx` sends, refreshed first when it is about to expire (`force`: whatever its time).
    pub(crate) async fn access_token(&self, conn: &Loaded, ctx: &Ctx, force: bool) -> Result<String> {
        let owner = Self::token_owner(conn, ctx)?;
        let mut tx = self.db.system_tenant_tx(conn.tenant).await?;
        let row: Option<(String, Option<DateTime<Utc>>, String)> =
            sqlx::query_as("select sealed, expires_at, status from conn_token where tenant_id = $1 and conn_id = $2 and user_id = $3")
                .bind(conn.tenant)
                .bind(conn.id)
                .bind(owner)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        let (sealed, expires, status) = match row {
            Some(r) => r,
            // A session signs itself in, the first time it is needed.
            None if matches!(conn.desc.auth, Auth::Session { .. }) => return self.session_sign_in(conn, ctx).await,
            None => return Err(Error::invalid(format!("{} is not connected to an account: connect it first", conn.name))),
        };
        if status != "connected" {
            return Err(Error::invalid(format!("{} needs reconnecting: the provider no longer accepts its sign-in", conn.name)));
        }
        if force || expires.is_some_and(|e| e < Utc::now() + chrono::Duration::seconds(CALL_AHEAD_SECS)) {
            return self.refresh_tokens(conn, owner, force.then_some(sealed.as_str())).await;
        }
        let open: Value = serde_json::from_str(&self.open_sealed(&token_label(conn.tenant, conn.id, owner), &sealed)?).map_err(Error::internal)?;
        Ok(open["access_token"].as_str().unwrap_or_default().to_string())
    }

    /// Refreshes `owner`'s tokens, the row locked so that two refreshes do not race a rotating refresh token: one that
    /// another refreshed meanwhile is used as it is (unless `stale` is the token a service just refused). A refusal
    /// marks it "needs reconnecting" and tells its owner once. The new access token.
    async fn refresh_tokens(&self, conn: &Loaded, owner: Uuid, stale: Option<&str>) -> Result<String> {
        let mut tx = self.db.system_tenant_tx(conn.tenant).await?;
        let row: Option<(String, Option<DateTime<Utc>>, String)> =
            sqlx::query_as("select sealed, expires_at, status from conn_token where tenant_id = $1 and conn_id = $2 and user_id = $3 for update")
                .bind(conn.tenant)
                .bind(conn.id)
                .bind(owner)
                .fetch_optional(&mut *tx)
                .await?;
        let (sealed, expires, status) = row.ok_or_else(|| Error::invalid(format!("{} is not connected to an account", conn.name)))?;
        if status != "connected" {
            return Err(Error::invalid(format!("{} needs reconnecting", conn.name)));
        }
        let open: Value = serde_json::from_str(&self.open_sealed(&token_label(conn.tenant, conn.id, owner), &sealed)?).map_err(Error::internal)?;
        let fresh = expires.is_none_or(|e| e > Utc::now() + chrono::Duration::seconds(CALL_AHEAD_SECS));
        if fresh && stale.is_none_or(|s| s != sealed) {
            return Ok(open["access_token"].as_str().unwrap_or_default().to_string());
        }
        let refresh = open["refresh_token"].as_str().filter(|r| !r.is_empty()).map(str::to_string);
        let granted = match (&conn.desc.auth, &refresh) {
            // A session refreshes with its refresh operation, else signs in again.
            (Auth::Session { .. }, _) => self.session_grant(conn, refresh.as_deref()).await?,
            // A provider that refreshes by an operation of its own (a GET with the token itself).
            (Auth::Oauth { provider, .. }, _) if provider.refresh.is_some() => {
                let op = provider.refresh.clone().unwrap_or_default();
                self.signing_op(conn, &op, self.token_inputs(conn, &open)?, &open).await?
            }
            (_, None) => {
                self.needs_reconnecting(&mut tx, conn, owner).await?;
                tx.commit().await?;
                return Err(Error::invalid(format!("{} needs reconnecting: its sign-in has run out", conn.name)));
            }
            (_, Some(r)) => {
                let (provider, _) = oauth(conn)?;
                self.token_request(conn, provider, &provider.token_url, &[("grant_type", "refresh_token"), ("refresh_token", r)]).await?
            }
        };
        match granted {
            Granted::Tokens(answer) => {
                self.keep_tokens(&mut tx, conn, owner, &answer, Some(&open), None).await?;
                tx.commit().await?;
                Ok(answer["access_token"].as_str().unwrap_or_default().to_string())
            }
            Granted::Later(why) => Err(Error::Unavailable(format!("the provider did not refresh the sign-in: {why}"))),
            Granted::Refused(why) => {
                self.needs_reconnecting(&mut tx, conn, owner).await?;
                tx.commit().await?;
                Err(Error::invalid(format!("{} needs reconnecting: the provider refused its sign-in ({why})", conn.name)))
            }
        }
    }

    /// Marks `owner`'s tokens as needing reconnecting, and tells the owner (a person's own; the workspace's: who
    /// connected it) the first time.
    async fn needs_reconnecting(&self, tx: &mut Tx, conn: &Loaded, owner: Uuid) -> Result<()> {
        let tell: Option<Option<Uuid>> = sqlx::query_scalar(
            "update conn_token set status = 'needs_reconnecting', told = true, updated_at = now() \
             where tenant_id = $1 and conn_id = $2 and user_id = $3 and not told returning connected_by",
        )
        .bind(conn.tenant)
        .bind(conn.id)
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?;
        let to: Vec<Uuid> = match tell {
            Some(_) if !owner.is_nil() => vec![owner],
            Some(Some(by)) => vec![by],
            // The workspace's, which nobody connected by hand (a session that signed itself in): its administrators.
            Some(None) => {
                sqlx::query_scalar("select id from app_user where tenant_id = $1 and is_admin and is_active and kind = 'member'")
                    .bind(conn.tenant)
                    .fetch_all(&mut **tx)
                    .await?
            }
            None => vec![],
        };
        for user in to {
            let title = format!("Reconnect {}", conn.name);
            let body = "The provider no longer accepts its sign-in. Connect the account again to carry on.";
            crate::notify::send_kind(tx, conn.tenant, user, "general", "inapp", &title, Some(body), Some("Connection"), Some(conn.id)).await?;
        }
        Ok(())
    }

    /// Disconnects: the workspace's account (an administrator), or the caller's own. Revoked at the provider where it
    /// offers that; the tokens are gone here whatever it says. Whether the provider confirmed.
    pub async fn connect_remove(&self, ctx: &Ctx, id: Uuid) -> Result<bool> {
        let conn = self.load_connection(ctx.tenant, &id.to_string()).await?;
        if matches!(conn.desc.auth, Auth::Session { .. }) {
            if !ctx.is_admin {
                return Err(Error::Forbidden("the workspace's account is disconnected by an administrator".into()));
            }
            self.forget_session(ctx, id).await?;
            return Ok(false);
        }
        let (provider, owner) = oauth(&conn)?;
        if owner == Owner::Workspace && !ctx.is_admin {
            return Err(Error::Forbidden("the workspace's account is disconnected by an administrator".into()));
        }
        let owner = Self::token_owner(&conn, ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let sealed: Option<String> = sqlx::query_scalar("delete from conn_token where tenant_id = $1 and conn_id = $2 and user_id = $3 returning sealed")
            .bind(ctx.tenant)
            .bind(id)
            .bind(owner)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(sealed) = sealed else { return Err(Error::NotFound("a connected account".into())) };
        audit::record_admin(&mut tx, ctx, "Connection", id, "disconnect", None, json!({"owner": if owner.is_nil() { "workspace" } else { "person" }})).await?;
        tx.commit().await?;
        self.revoke_tokens(&conn, provider, owner, &sealed).await
    }

    /// Revokes the tokens `sealed` (of `owner`'s sign-in) at the provider, where it gives a revoke address: the refresh
    /// token, else the access token. Whether it confirmed.
    async fn revoke_tokens(&self, conn: &Loaded, provider: &Provider, owner: Uuid, sealed: &str) -> Result<bool> {
        let Some(url) = &provider.revoke_url else { return Ok(false) };
        let open: Value = serde_json::from_str(&self.open_sealed(&token_label(conn.tenant, conn.id, owner), sealed)?).map_err(Error::internal)?;
        let (token, hint) = match open["refresh_token"].as_str() {
            Some(r) => (r.to_string(), "refresh_token"),
            None => (open["access_token"].as_str().unwrap_or_default().to_string(), "access_token"),
        };
        Ok(matches!(self.token_request(conn, provider, url, &[("token", &token), ("token_type_hint", hint)]).await?, Granted::Tokens(_)))
    }

    /// After an erasure took a person's own sign-ins (their tokens destroyed here, `taken`: connection and sealed
    /// tokens): each revoked at its provider where it gives a revoke address, best effort, and what came of it in the
    /// connection's history. What came of each: `{connection, revoked}` (`revoked` null without a revoke address).
    pub(crate) async fn revoke_erased(&self, ctx: &Ctx, person: Uuid, taken: Vec<(Uuid, String)>) -> Vec<Value> {
        let mut out = Vec::new();
        for (id, sealed) in taken {
            let tried = async {
                let conn = self.load_connection(ctx.tenant, &id.to_string()).await?;
                let (provider, _) = oauth(&conn)?;
                if provider.revoke_url.is_none() {
                    return Ok(None);
                }
                self.revoke_tokens(&conn, provider, person, &sealed).await.map(Some)
            }
            .await;
            let said = match &tried {
                Ok(r) => json!({"connection": id, "revoked": r}),
                Err(e) => json!({"connection": id, "revoked": false, "error": e.to_string()}),
            };
            let recorded = async {
                let mut tx = self.db.tenant_tx(ctx.tenant).await?;
                audit::record_admin(&mut tx, ctx, "Connection", id, "revoke_on_erasure", None, said.clone()).await?;
                tx.commit().await?;
                Ok::<(), Error>(())
            }
            .await;
            if let Err(e) = recorded {
                tracing::warn!(tenant = %ctx.tenant, connection = %id, "an erased person's sign-in: what its revocation did was not recorded: {e}");
            }
            out.push(said);
        }
        out
    }

    /// What a session's login or refresh operation gives (`refresh`: the refresh token, for its refresh operation), as
    /// a token address's answer would say it.
    async fn session_grant(&self, conn: &Loaded, refresh: Option<&str>) -> Result<Granted> {
        let Auth::Session { login, refresh: renew, token_at, refresh_at, expires_at, keep, .. } = &conn.desc.auth else {
            return Err(Error::invalid(format!("{} has no session", conn.name)));
        };
        let (op, inputs) = match (renew, refresh) {
            (Some(op), Some(r)) => (op, json!({"refresh_token": r})),
            _ => (login, json!({})),
        };
        // The refresh token is masked wherever the call's words are kept, as the secrets are.
        let mut quiet = conn.clone();
        if let Some(r) = refresh {
            quiet.secrets.insert("\u{0}refresh".into(), r.to_string());
        }
        let ctx = Ctx::system(conn.tenant, self.registry.schema(conn.tenant).await?);
        Ok(match Box::pin(self.perform(&ctx, &quiet, op, &inputs, None, super::Extra::default(), None, None)).await? {
            Outcome::Ok(_, a) => match super::at(&a.body, Some(token_at)).filter(|t| !t.is_empty()) {
                Some(token) => {
                    let mut out = json!({"access_token": token});
                    if let Some(r) = super::at(&a.body, refresh_at.as_deref()) {
                        out["refresh_token"] = json!(r);
                    }
                    if let Some(at) = expires_at.as_deref().and_then(|p| a.body.pointer(p)).and_then(expiry) {
                        out["expires_in"] = json!((at - Utc::now()).num_seconds().max(0));
                    }
                    for k in keep {
                        out[k.as_str()] = a.body.get(k).cloned().unwrap_or(Value::Null);
                    }
                    Granted::Tokens(out)
                }
                None => Granted::Refused(format!("{op} gave no token at {token_at}")),
            },
            Outcome::Later(why, _) => Granted::Later(why),
            Outcome::No(why) => Granted::Refused(why),
        })
    }

    /// The inputs a signing operation (`after`, `refresh`) is given: the tokens of `answer` and the client's id.
    fn token_inputs(&self, conn: &Loaded, answer: &Value) -> Result<Value> {
        let client_id = match &conn.desc.auth {
            Auth::Oauth { provider, .. } => client(conn, provider).map(|c| c.0).unwrap_or_default(),
            _ => String::new(),
        };
        Ok(json!({"access_token": answer["access_token"], "refresh_token": answer["refresh_token"], "client_id": client_id}))
    }

    /// A signing operation of the connection (`after`, `refresh`, `registration`), made without credentials of its own
    /// and never captured, the tokens it is given masked wherever its words are kept: its mapped result.
    async fn signing_op(&self, conn: &Loaded, op: &str, inputs: Value, tokens: &Value) -> Result<Granted> {
        let mut quiet = conn.clone();
        for (i, t) in ["access_token", "refresh_token"].iter().filter_map(|n| tokens.get(*n)?.as_str()).enumerate() {
            quiet.secrets.insert(format!("\u{0}token{i}"), t.to_string());
        }
        let ctx = Ctx::system(conn.tenant, self.registry.schema(conn.tenant).await?);
        Ok(match Box::pin(self.perform(&ctx, &quiet, op, &inputs, None, super::Extra::default(), None, None)).await? {
            Outcome::Ok(result, _) => Granted::Tokens(result),
            Outcome::Later(why, _) => Granted::Later(why),
            Outcome::No(why) => Granted::Refused(why),
        })
    }

    /// Signs a session in when it has no tokens yet, one sign-in at a time per connection: one that another made
    /// meanwhile is used. A login the service refuses marks it "needs reconnecting" and tells the administrators once,
    /// so that a wrong password is not tried on every call.
    async fn session_sign_in(&self, conn: &Loaded, ctx: &Ctx) -> Result<String> {
        let mut tx = self.db.system_tenant_tx(conn.tenant).await?;
        sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))").bind(format!("conn_session:{}", conn.id)).execute(&mut *tx).await?;
        let here: Option<(String, String)> = sqlx::query_as("select sealed, status from conn_token where tenant_id = $1 and conn_id = $2 and user_id = $3")
            .bind(conn.tenant)
            .bind(conn.id)
            .bind(Uuid::nil())
            .fetch_optional(&mut *tx)
            .await?;
        if let Some((sealed, status)) = here {
            tx.commit().await?;
            if status != "connected" {
                return Err(Error::invalid(format!("{} needs reconnecting: the service no longer accepts its sign-in", conn.name)));
            }
            let open: Value = serde_json::from_str(&self.open_sealed(&token_label(conn.tenant, conn.id, Uuid::nil()), &sealed)?).map_err(Error::internal)?;
            return Ok(open["access_token"].as_str().unwrap_or_default().to_string());
        }
        let by = (!ctx.user.is_nil()).then_some(ctx.user);
        match self.session_grant(conn, None).await? {
            Granted::Tokens(answer) => {
                self.keep_tokens(&mut tx, conn, Uuid::nil(), &answer, None, by).await?;
                tx.commit().await?;
                Ok(answer["access_token"].as_str().unwrap_or_default().to_string())
            }
            Granted::Later(why) => Err(Error::Unavailable(format!("{} did not sign in: {why}", conn.name))),
            Granted::Refused(why) => {
                let key = self.settings.secret_key.as_ref().ok_or_else(|| Error::Unavailable("tokens are sealed with the server's secret key".into()))?;
                sqlx::query(
                    "insert into conn_token (tenant_id, conn_id, user_id, sealed, status, told, connected_by) values ($1, $2, $3, $4, 'connected', false, $5)",
                )
                .bind(conn.tenant)
                .bind(conn.id)
                .bind(Uuid::nil())
                .bind(key.seal(&token_label(conn.tenant, conn.id, Uuid::nil()), "{}")?)
                .bind(by)
                .execute(&mut *tx)
                .await?;
                self.needs_reconnecting(&mut tx, conn, Uuid::nil()).await?;
                tx.commit().await?;
                Err(Error::invalid(format!("{} needs reconnecting: the service refused its sign-in ({why})", conn.name)))
            }
        }
    }

    /// Drops a session's tokens (its secrets changed, or an administrator disconnects it): the next call signs in
    /// afresh.
    pub(crate) async fn forget_session(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
        sqlx::query("delete from conn_token where tenant_id = $1 and conn_id = $2 and user_id = $3")
            .bind(ctx.tenant)
            .bind(id)
            .bind(Uuid::nil())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// An administrator signs a session in now (`POST /api/admin/connections/{id}/session`), afresh: `{status,
    /// expires_at}`, or what the service said.
    pub async fn connect_session(&self, ctx: &Ctx, id: Uuid) -> Result<Value> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("connections are configured by administrators".into()));
        }
        let conn = self.load_connection(ctx.tenant, &id.to_string()).await?;
        if !matches!(conn.desc.auth, Auth::Session { .. }) {
            return Err(Error::invalid(format!("{} does not sign in with a session", conn.name)));
        }
        self.forget_session(ctx, id).await?;
        self.session_sign_in(&conn, ctx).await?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        audit::record_admin(&mut tx, ctx, "Connection", id, "connect", None, json!({"owner": "workspace", "session": true})).await?;
        tx.commit().await?;
        self.workspace_account(ctx.tenant, id).await
    }

    /// The connections a person signs in to with their own account, and whether they have: `{id, name, label, status,
    /// account}` (`status` null when not connected).
    pub async fn personal_connections(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<(Uuid, String, String, Value, Option<String>, Option<String>)> = sqlx::query_as(
            "select c.id, c.name, c.label, c.description, t.status, t.account from conn c \
             left join conn_token t on t.tenant_id = c.tenant_id and t.conn_id = c.id and t.user_id = $2 \
             where c.tenant_id = $1 and c.enabled and c.description->'auth'->>'type' = 'oauth' and c.description->'auth'->>'owner' = 'person' order by c.label",
        )
        .bind(ctx.tenant)
        .bind(ctx.user)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .filter(|r| serde_json::from_value::<super::Description>(r.3.clone()).is_ok_and(|d| super::may_use(ctx, None, &d).is_ok()))
            .map(|(id, name, label, _, status, account)| json!({"id": id, "name": name, "label": label, "status": status, "account": account}))
            .collect())
    }

    /// The workspace's account of each connection that signs in, as administrators see it: `{status, account, expires_at}` or null.
    pub(crate) async fn workspace_account(&self, tenant: Uuid, id: Uuid) -> Result<Value> {
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let row: Option<Value> = sqlx::query_scalar(
            "select jsonb_build_object('status', status, 'account', account, 'expires_at', expires_at) from conn_token where tenant_id = $1 and conn_id = $2 and user_id = $3",
        )
        .bind(tenant)
        .bind(id)
        .bind(Uuid::nil())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row.unwrap_or(Value::Null))
    }

    /// Queues a refresh of every connected token about to expire (from the schedule scanner).
    pub async fn sweep_tokens_at(&self, now: DateTime<Utc>) -> Result<usize> {
        let mut tx = self.db.system_tx().await?;
        let due: Vec<(Uuid, Uuid, Uuid)> = sqlx::query_as(
            "select t.tenant_id, t.conn_id, t.user_id from conn_token t join conn c on c.tenant_id = t.tenant_id and c.id = t.conn_id \
             where t.status = 'connected' and t.expires_at is not null and t.expires_at < $1 + make_interval(mins => $2::int) \
               and not ferrum_workspace_held(t.tenant_id) limit 200",
        )
        .bind(now)
        .bind(REFRESH_AHEAD_MINUTES)
        .fetch_all(&mut *tx)
        .await?;
        for (tenant, conn, user) in &due {
            let mut job = NewJob::new(REFRESH_KIND, json!({"connection": conn, "user": user}));
            job.dedupe_key = Some(format!("{conn}:{user}"));
            job.max_attempts = 3;
            crate::jobs::enqueue(&mut tx, *tenant, None, job).await?;
        }
        tx.commit().await?;
        Ok(due.len())
    }
}

struct Refresh;

#[async_trait]
impl JobHandler for Refresh {
    fn kind(&self) -> &'static str {
        REFRESH_KIND
    }

    fn title(&self) -> &'static str {
        "Refreshing a connection's sign-in"
    }

    async fn run(&self, k: &Kernel, job: &Job) -> Result<Value> {
        let conn_id = job.payload["connection"].as_str().unwrap_or_default();
        let owner: Uuid = job.payload["user"].as_str().and_then(|s| s.parse().ok()).unwrap_or_default();
        let conn = k.load_connection(job.tenant_id, conn_id).await?;
        // Within the window the sweep queued it for: refreshed now, ahead of its calls.
        let mut tx = k.db.system_tenant_tx(job.tenant_id).await?;
        sqlx::query(
            "update conn_token set expires_at = least(expires_at, now()) where tenant_id = $1 and conn_id = $2 and user_id = $3 and status = 'connected'",
        )
        .bind(job.tenant_id)
        .bind(conn.id)
        .bind(owner)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        match k.refresh_tokens(&conn, owner, None).await {
            Ok(_) => Ok(json!({"refreshed": true})),
            Err(e @ Error::Unavailable(_)) => Err(e),
            Err(e) => Ok(json!({"refreshed": false, "why": e.to_string()})),
        }
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(60)
    }
}

pub(crate) fn handler(kind: &str) -> Option<Arc<dyn JobHandler>> {
    (kind == REFRESH_KIND).then(|| Arc::new(Refresh) as Arc<dyn JobHandler>)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_names_where_it_started_and_cannot_be_changed() {
        let key = crate::secret::SecretKey::new([7u8; 32]);
        let t = Uuid::now_v7();
        let state = state_from(&key, t, "erp.acme.com");
        assert_eq!(state_names(&key, &state), Some((t, "erp.acme.com".to_string())));
        assert!(state.len() < 300, "{}", state.len());
        assert_ne!(state, state_from(&key, t, "erp.acme.com"), "each one its own");
        assert_eq!(state_names(&crate::secret::SecretKey::new([8u8; 32]), &state), None, "another key");
        let (nonce, rest) = state.split_once('.').unwrap();
        let (_, mac) = rest.split_once('.').unwrap();
        let forged = format!("{nonce}.{}.{mac}", URL_SAFE_NO_PAD.encode(format!("{t}|evil.example")));
        assert_eq!(state_names(&key, &forged), None, "the host changed");
        assert_eq!(state_names(&key, &random()), None, "a state that names nothing");
        assert_eq!(state_names(&key, "a.b"), None);
    }

    #[test]
    fn expiries_and_providers_are_read() {
        let soon = expiry(&json!(3600)).unwrap() - Utc::now();
        assert!((3590..=3600).contains(&soon.num_seconds()), "seconds from now");
        assert_eq!(expiry(&json!("1900000000")), DateTime::from_timestamp(1_900_000_000, 0), "a Unix time");
        assert_eq!(expiry(&json!("2026-11-13T10:00:00Z")), Some(DateTime::parse_from_rfc3339("2026-11-13T10:00:00Z").unwrap().to_utc()));
        assert_eq!(expiry(&json!("soon")), None);
        let p = |v: Value| serde_json::from_value::<Provider>(v).unwrap().problem();
        let base = json!({"authorize_url": "https://id.test/a", "token_url": "https://id.test/t"});
        assert!(p(base.clone()).is_some_and(|e| e.contains("client_id")), "a client, or a provider that registers one");
        let mut registers = base.clone();
        registers["registration_url"] = json!("https://id.test/register");
        assert_eq!(p(registers.clone()), None);
        registers["registration_url"] = json!("javascript:alert(1)");
        assert!(p(registers).is_some());
    }
}
