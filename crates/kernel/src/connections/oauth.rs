//! A person authorises an account (BD-31): a sign-in broker for connections that act for a person, or for the
//! workspace's own account, at a provider. The provider is a description (its addresses, scopes, client, how it names
//! the account); the flow is the authorisation code flow with PKCE, its state bound to the person and their browser
//! session and used once, the redirect address the platform's own and exact. Tokens are sealed, refreshed before they
//! expire by a job, and never shown back; a refresh the provider refuses marks the connection "needs reconnecting" and
//! tells its owner once; disconnecting revokes at the provider when it offers that.

use super::{Loaded, Reach, Request, is_name, pointer_problem};
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
    pub client_id: String,
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
        {
            return Some(p);
        }
        if self.client_id.trim().is_empty() {
            return Some("auth.provider.client_id is required".into());
        }
        if self.client_secret.as_deref().is_some_and(|s| !is_name(s)) {
            return Some("auth.provider.client_secret names one of the connection's secrets".into());
        }
        if self.scopes.iter().any(|s| s.is_empty() || s.contains(char::is_whitespace)) {
            return Some("auth.provider.scopes are words without spaces".into());
        }
        if ["response_type", "client_id", "redirect_uri", "state", "code_challenge", "code_challenge_method", "scope"]
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
            .public_url
            .as_deref()
            .ok_or_else(|| Error::Unavailable("signing in to an account needs the server's public address (PUBLIC_URL)".into()))?;
        Ok(format!("{}{CALLBACK}", base.trim_end_matches('/')))
    }

    /// Starts a sign-in to connection `id`'s account for the person `ctx` in browser session `session`: the address to
    /// send their browser to. The workspace's account is an administrator's to connect; a person connects their own
    /// where the connection lets them use it.
    pub async fn connect_start(&self, ctx: &Ctx, id: Uuid, session: Uuid) -> Result<String> {
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
        let (state, verifier) = (random(), random());
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
            q.append_pair("response_type", "code").append_pair("client_id", &provider.client_id).append_pair("redirect_uri", &redirect);
            if !provider.scopes.is_empty() {
                q.append_pair("scope", &provider.scopes.join(" "));
            }
            q.append_pair("state", &state).append_pair("code_challenge", &challenge).append_pair("code_challenge_method", "S256");
            for (k, v) in &provider.params {
                q.append_pair(k, v);
            }
        }
        Ok(url.into())
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
        let answer = match self.token_request(&conn, provider, &provider.token_url, &form).await? {
            Granted::Tokens(t) => t,
            Granted::Refused(why) | Granted::Later(why) => return Err(Error::invalid(format!("the provider did not give tokens: {why}"))),
        };
        let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
        let account = self.keep_tokens(&mut tx, &conn, owner_id, &answer, None, Some(ctx.user)).await?;
        audit::record(
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

    /// A form to a token or revocation address, the client proving itself as the provider says.
    async fn token_request(&self, conn: &Loaded, provider: &Provider, url: &str, form: &[(&str, &str)]) -> Result<Granted> {
        let mut body = reqwest::Url::parse("http://form/").map_err(Error::internal)?;
        let mut headers = vec![("accept".to_string(), "application/json".to_string())];
        {
            let mut q = body.query_pairs_mut();
            q.extend_pairs(form.iter().copied());
            let secret = provider.client_secret.as_deref().map(|s| conn.secret(s)).transpose()?;
            match (provider.client_auth, secret) {
                (ClientAuth::Basic, Some(s)) => {
                    let pair = base64::engine::general_purpose::STANDARD.encode(format!("{}:{s}", provider.client_id));
                    headers.push(("authorization".into(), format!("Basic {pair}")));
                }
                (_, secret) => {
                    q.append_pair("client_id", &provider.client_id);
                    if let Some(s) = secret {
                        q.append_pair("client_secret", s);
                    }
                }
            }
        }
        let req = Request {
            method: reqwest::Method::POST,
            url: reqwest::Url::parse(url).map_err(|_| Error::invalid("the provider's address is not an address"))?,
            headers,
            body: Some((body.query().unwrap_or_default().as_bytes().to_vec(), "application/x-www-form-urlencoded")),
        };
        let reach = Reach { hosts: vec![host_of(url)], timeout: TOKEN_TIMEOUT, max_bytes: TOKEN_BYTES };
        let mask = conn.mask();
        Ok(match self.fetch(req, &reach).await {
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

    /// Seals what a token address gave for `owner` (nil: the workspace's), keeping `refresh` when it gave none; marks
    /// it connected. The account it names.
    async fn keep_tokens(&self, tx: &mut Tx, conn: &Loaded, owner: Uuid, answer: &Value, refresh: Option<String>, by: Option<Uuid>) -> Result<Option<String>> {
        let access =
            answer.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty()).ok_or_else(|| Error::invalid("the provider gave no access token"))?;
        let refresh = answer.get("refresh_token").and_then(Value::as_str).map(str::to_string).or(refresh);
        let expires = answer
            .get("expires_in")
            .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
            .map(|s| Utc::now() + chrono::Duration::seconds(s.clamp(0, 365 * 86400)));
        let (provider, _) = oauth(conn)?;
        let mut seen = answer.clone();
        if let Some(id) = answer.get("id_token").and_then(Value::as_str) {
            seen["id_token"] = claims(id);
        }
        let account = super::at(&seen, provider.account_at.as_deref()).map(|a| a.chars().take(200).collect::<String>());
        let key = self.settings.secret_key.as_ref().ok_or_else(|| Error::Unavailable("tokens are sealed with the server's secret key".into()))?;
        let sealed = key.seal(&token_label(conn.tenant, conn.id, owner), &json!({"access_token": access, "refresh_token": refresh}).to_string())?;
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
    fn token_owner(conn: &Loaded, ctx: &Ctx) -> Result<Uuid> {
        match oauth(conn)?.1 {
            Owner::Workspace => Ok(Uuid::nil()),
            Owner::Person if ctx.user.is_nil() => Err(Error::invalid(format!("{} is each person's own: it is used as a person", conn.name))),
            Owner::Person => Ok(ctx.user),
        }
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
        let (sealed, expires, status) = row.ok_or_else(|| Error::invalid(format!("{} is not connected to an account: connect it first", conn.name)))?;
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
        let (provider, _) = oauth(conn)?;
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
        let Some(refresh) = open["refresh_token"].as_str().filter(|r| !r.is_empty()).map(str::to_string) else {
            self.needs_reconnecting(&mut tx, conn, owner).await?;
            tx.commit().await?;
            return Err(Error::invalid(format!("{} needs reconnecting: its sign-in has run out", conn.name)));
        };
        match self.token_request(conn, provider, &provider.token_url, &[("grant_type", "refresh_token"), ("refresh_token", &refresh)]).await? {
            Granted::Tokens(answer) => {
                self.keep_tokens(&mut tx, conn, owner, &answer, Some(refresh), None).await?;
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
        let to = match tell {
            Some(_) if !owner.is_nil() => Some(owner),
            Some(by) => by,
            None => None,
        };
        if let Some(user) = to {
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
        audit::record(&mut tx, ctx, "Connection", id, "disconnect", None, json!({"owner": if owner.is_nil() { "workspace" } else { "person" }})).await?;
        tx.commit().await?;
        let Some(url) = &provider.revoke_url else { return Ok(false) };
        let open: Value = serde_json::from_str(&self.open_sealed(&token_label(ctx.tenant, id, owner), &sealed)?).map_err(Error::internal)?;
        let (token, hint) = match open["refresh_token"].as_str() {
            Some(r) => (r.to_string(), "refresh_token"),
            None => (open["access_token"].as_str().unwrap_or_default().to_string(), "access_token"),
        };
        Ok(matches!(self.token_request(&conn, provider, url, &[("token", &token), ("token_type_hint", hint)]).await?, Granted::Tokens(_)))
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
             where t.status = 'connected' and t.expires_at is not null and t.expires_at < $1 + make_interval(mins => $2::int) limit 200",
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
