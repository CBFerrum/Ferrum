//! Single sign-on through OpenID Connect providers, set up per workspace.
//!
//! A sign-in is the authorization code flow with PKCE, a nonce, and a state bound to the browser
//! that started it. The ID token's signature (against the keys the provider publishes), issuer,
//! audience, expiry and nonce are checked. The person is matched by an email address the provider
//! vouches for (within the provider's email domains, when the administrator lists them) and is
//! linked to their identity at that provider from then on, so the same address held later by
//! someone else at the provider does not open the account. People without an account get one when
//! the provider is set to create them (with its default roles). A workspace can require single
//! sign-on of everyone but its administrators. The provider's own sign-in rules apply (its
//! two-factor step included); Ferrum's two-factor step does not.

use crate::auth::Session;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::oauth::{hash, random_secret};
use crate::{Kernel, audit};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::jwk::Jwk;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::time::Duration;
use uuid::Uuid;

/// A sign-in must come back from the provider within ten minutes.
const FLOW_MINUTES: i32 = 10;
/// The most a provider's answer may hold.
const MAX_ANSWER: usize = 1 << 20;
/// Signature algorithms accepted on ID tokens (no shared-secret or unsigned ones).
const ALGORITHMS: [Algorithm; 9] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

/// A provider as an administrator sets it up.
#[derive(Debug, Clone, Deserialize)]
pub struct SsoProviderInput {
    pub label: String,
    /// The provider's issuer URL (its discovery document lives under it).
    pub issuer: String,
    pub client_id: String,
    /// `None` keeps the stored secret; empty clears it (a public client, PKCE only).
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Email domains accepted from this provider; empty accepts any it has verified.
    #[serde(default)]
    pub domains: Vec<String>,
    /// Creates an account for someone signing in who has none.
    #[serde(default)]
    pub auto_create: bool,
    /// The roles of accounts it creates.
    #[serde(default)]
    pub default_roles: Vec<String>,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

/// The parts of a provider's discovery document used here.
#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Vec<String>,
}

#[derive(Deserialize)]
struct IdClaims {
    sub: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_verified: Option<Value>,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    upn: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    azp: Option<String>,
    #[serde(default)]
    aud: Value,
}

/// A provider ready for signing in (its secret opened).
struct Provider {
    id: Uuid,
    label: String,
    issuer: String,
    client_id: String,
    client_secret: Option<String>,
    domains: Vec<String>,
    auto_create: bool,
    default_roles: Vec<String>,
}

fn admin_only(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("single sign-on is set up by administrators".into())) }
}

fn sealed_label(tenant: Uuid, id: Uuid) -> String {
    format!("sso|{tenant}|{id}")
}

/// An email domain as listed: lower case, no `@`.
fn clean_domain(d: &str) -> Result<String> {
    let d = d.trim().trim_start_matches('@').to_lowercase();
    let ok =
        d.len() <= 253 && d.contains('.') && !d.starts_with('.') && !d.ends_with('.') && d.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    if ok { Ok(d) } else { Err(Error::field("domains", format!("{d:?} is not an email domain (like example.com)"))) }
}

/// The email address the provider vouches for: its `email` claim (or the user principal name
/// Microsoft Entra ID sends), verified by the provider; a provider that does not say whether it
/// verified addresses is trusted for the domains the administrator listed, and only for them.
fn verified_email(c: &IdClaims, p: &Provider) -> Result<String> {
    let email = c
        .email
        .iter()
        .chain(c.upn.iter())
        .chain(c.preferred_username.iter())
        .map(|e| e.trim().to_lowercase())
        .find(|e| e.len() <= 254 && e.split_once('@').is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty()))
        .ok_or_else(|| Error::Forbidden(format!("{} did not say which email address you use", p.label)))?;
    let domain = email.rsplit_once('@').map(|(_, d)| d).unwrap_or_default();
    if !p.domains.is_empty() && !p.domains.iter().any(|d| d == domain) {
        return Err(Error::Forbidden(format!("this workspace does not accept {domain} addresses from {}", p.label)));
    }
    let verified = match &c.email_verified {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
        None | Some(Value::Null) => !p.domains.is_empty(),
        Some(_) => false,
    };
    if !verified {
        let hint = if c.email_verified.is_none() { " (an administrator can accept its addresses by listing the email domains)" } else { "" };
        return Err(Error::Forbidden(format!("{} has not confirmed that {email} is yours{hint}", p.label)));
    }
    Ok(email)
}

/// `application/x-www-form-urlencoded` for HTTP Basic client authentication (RFC 6749 2.3.1).
fn form_encode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

impl Kernel {
    /// A request to an identity provider: https only (plain http too when private addresses are
    /// allowed: `sso_allow_private`), its host resolved once, vetted like a webhook target and pinned,
    /// no redirects, ten seconds, a megabyte at most. → the status and the JSON answer.
    async fn idp_request(&self, url: &str, form: Option<&[(&str, &str)]>, basic: Option<(&str, &str)>) -> Result<(u16, Value)> {
        let unreachable = |e: String| Error::Unavailable(format!("the identity provider cannot be reached: {e}"));
        let allow_private = self.settings.sso_allow_private;
        let parsed = reqwest::Url::parse(url).map_err(|_| Error::invalid(format!("{url} is not a URL")))?;
        if parsed.scheme() != "https" && !(allow_private && parsed.scheme() == "http") {
            return Err(Error::invalid(format!("{url} must use https")));
        }
        let host = parsed.host_str().ok_or_else(|| Error::invalid(format!("{url} has no host")))?.to_string();
        let port = parsed.port_or_known_default().unwrap_or(443);
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port)).await.map_err(|e| unreachable(e.to_string()))?.collect();
        if addrs.is_empty() {
            return Err(unreachable(format!("{host} does not resolve")));
        }
        if !allow_private && addrs.iter().any(|a| crate::outbox::is_private(a.ip())) {
            return Err(Error::invalid(format!("{host} is a private address")));
        }
        let client = reqwest::Client::builder()
            .resolve_to_addrs(&host, &addrs)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .user_agent("Ferrum-SSO/1")
            .build()
            .map_err(|e| unreachable(e.to_string()))?;
        let mut req = match form {
            Some(f) => client.post(parsed).form(f),
            None => client.get(parsed),
        };
        if let Some((user, password)) = basic {
            req = req.basic_auth(form_encode(user), Some(form_encode(password)));
        }
        let mut resp = req.header("accept", "application/json").send().await.map_err(|e| unreachable(e.to_string()))?;
        let status = resp.status().as_u16();
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| unreachable(e.to_string()))? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_ANSWER {
                return Err(unreachable("its answer is too large".into()));
            }
        }
        let v = serde_json::from_slice(&body).map_err(|_| unreachable(format!("HTTP {status} without a JSON answer from {host}")))?;
        Ok((status, v))
    }

    /// The provider's discovery document, which must name the issuer it was found under.
    async fn discover(&self, issuer: &str) -> Result<Discovery> {
        let url = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
        let (status, v) = self.idp_request(&url, None, None).await?;
        if status != 200 {
            return Err(Error::Unavailable(format!("the provider's discovery document ({url}) answered HTTP {status}")));
        }
        let d: Discovery = serde_json::from_value(v).map_err(|e| Error::invalid(format!("the provider's discovery document is incomplete: {e}")))?;
        if d.issuer.trim_end_matches('/') != issuer.trim_end_matches('/') {
            return Err(Error::invalid(format!("the provider calls itself {}; use that as the issuer", d.issuer)));
        }
        Ok(d)
    }

    /// The workspace's providers (administrators; secrets are never shown).
    pub async fn sso_providers(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', id, 'label', label, 'issuer', issuer, 'client_id', client_id, 'has_secret', client_secret is not null, \
               'domains', domains, 'auto_create', auto_create, 'default_roles', default_roles, 'enabled', enabled, \
               'linked_users', (select count(*) from app_user u where u.tenant_id = p.tenant_id and u.sso_subject like p.id::text || '|%'), \
               'created_at', created_at, 'updated_at', updated_at) \
             from sso_provider p where tenant_id = $1 order by label",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Adds (`id` none) or changes a provider; its discovery document is fetched to check it.
    pub async fn save_sso_provider(&self, ctx: &Ctx, id: Option<Uuid>, input: SsoProviderInput) -> Result<Uuid> {
        admin_only(ctx)?;
        let label = input.label.trim();
        if label.is_empty() || label.chars().count() > 60 {
            return Err(Error::field("label", "name it, in at most 60 characters (it shows on the sign-in button)"));
        }
        let client_id = input.client_id.trim();
        if client_id.is_empty() || client_id.len() > 255 {
            return Err(Error::field("client_id", "give the client ID the provider issued"));
        }
        let mut domains = input.domains.iter().filter(|d| !d.trim().is_empty()).map(|d| clean_domain(d)).collect::<Result<Vec<_>>>()?;
        domains.sort();
        domains.dedup();
        if domains.len() > 50 {
            return Err(Error::field("domains", "at most 50"));
        }
        let mut roles: Vec<String> = input.default_roles.iter().map(|r| r.trim().to_string()).filter(|r| !r.is_empty()).collect();
        roles.sort();
        roles.dedup();
        let issuer = input.issuer.trim();
        if issuer.len() > 500 {
            return Err(Error::field("issuer", "is too long"));
        }
        // The issuer as the provider spells it (a trailing slash matters to its tokens).
        let issuer = self.discover(issuer).await.map_err(|e| Error::field("issuer", e.to_string()))?.issuer;
        let key =
            self.settings.secret_key.as_ref().ok_or_else(|| Error::Unavailable("single sign-on needs the server's secret key (FERRUM_SECRET_KEY)".into()))?;
        let (id, new) = match id {
            Some(id) => (id, false),
            None => (Uuid::now_v7(), true),
        };
        let secret = match input.client_secret.as_deref().map(str::trim) {
            Some("") => Some(None),
            Some(s) if s.len() > 1000 => return Err(Error::field("client_secret", "is too long")),
            Some(s) => Some(Some(key.seal(&sealed_label(ctx.tenant, id), s)?)),
            None => None,
        };
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        if !new
            && sqlx::query_scalar::<_, i64>("select count(*) from sso_provider where tenant_id = $1 and id = $2")
                .bind(ctx.tenant)
                .bind(id)
                .fetch_one(&mut *tx)
                .await?
                == 0
        {
            return Err(Error::NotFound("single sign-on provider".into()));
        }
        let known: i64 =
            sqlx::query_scalar("select count(*) from role where tenant_id = $1 and name = any($2)").bind(ctx.tenant).bind(&roles).fetch_one(&mut *tx).await?;
        if known != roles.len() as i64 {
            return Err(Error::field("default_roles", "names a role this workspace does not have"));
        }
        if !input.enabled {
            self.keep_one_for_required(&mut tx, ctx.tenant, id).await?;
        }
        sqlx::query(
            "insert into sso_provider (tenant_id, id, label, issuer, client_id, client_secret, domains, auto_create, default_roles, enabled) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             on conflict (tenant_id, id) do update set label = excluded.label, issuer = excluded.issuer, client_id = excluded.client_id, \
               client_secret = case when $11 then excluded.client_secret else sso_provider.client_secret end, \
               domains = excluded.domains, auto_create = excluded.auto_create, default_roles = excluded.default_roles, enabled = excluded.enabled, updated_at = now()",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(label)
        .bind(&issuer)
        .bind(client_id)
        .bind(secret.clone().flatten())
        .bind(&domains)
        .bind(input.auto_create)
        .bind(&roles)
        .bind(input.enabled)
        .bind(secret.is_some())
        .execute(&mut *tx)
        .await?;
        let what = json!({"label": label, "issuer": issuer, "client_id": client_id, "secret_changed": secret.is_some(), "domains": domains, "auto_create": input.auto_create, "default_roles": roles, "enabled": input.enabled});
        audit::record(&mut tx, ctx, "SsoProvider", id, "save", None, what).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Refuses to leave a workspace that requires single sign-on without an enabled provider
    /// (other than `id`).
    async fn keep_one_for_required(&self, tx: &mut crate::db::Tx, tenant: Uuid, id: Uuid) -> Result<()> {
        let stuck: bool = sqlx::query_scalar(
            "select coalesce(t.settings->'require_sso' = 'true'::jsonb, false) \
               and not exists (select 1 from sso_provider p where p.tenant_id = t.id and p.enabled and p.id <> $2) from tenant t where t.id = $1",
        )
        .bind(tenant)
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
        if stuck { Err(Error::invalid("the workspace requires single sign-on: keep another provider enabled, or stop requiring it first")) } else { Ok(()) }
    }

    /// Removes a provider; the people linked through it are unlinked.
    pub async fn delete_sso_provider(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        self.keep_one_for_required(&mut tx, ctx.tenant, id).await?;
        sqlx::query("update app_user set sso_subject = null where tenant_id = $1 and sso_subject like $2 || '|%'")
            .bind(ctx.tenant)
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        let done = sqlx::query("delete from sso_provider where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).execute(&mut *tx).await?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound("single sign-on provider".into()));
        }
        audit::record(&mut tx, ctx, "SsoProvider", id, "delete", None, json!({})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Unlinks someone from their identity at a provider (it changed there, or the account was
    /// handed to someone else); their next single sign-on links them again.
    pub async fn unlink_sso(&self, ctx: &Ctx, user: Uuid) -> Result<()> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let done = sqlx::query("update app_user set sso_subject = null, updated_at = now() where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound("user".into()));
        }
        audit::record(&mut tx, ctx, "User", user, "unlink_sso", None, json!({})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The enabled providers of a workspace for its sign-in page (none for an unknown or closed
    /// workspace).
    pub async fn sso_sign_in_options(&self, tenant_slug: &str) -> Result<Vec<Value>> {
        let mut tx = self.db.system_tx().await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', p.id, 'label', p.label) from sso_provider p join tenant t on t.id = p.tenant_id \
             where t.slug = $1 and t.status = 'active' and p.enabled order by p.label",
        )
        .bind(tenant_slug.trim().to_lowercase())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    async fn sign_in_provider(&self, tx: &mut crate::db::Tx, tenant: Option<Uuid>, slug: Option<&str>, id: Uuid) -> Result<(Uuid, Provider)> {
        #[derive(sqlx::FromRow)]
        struct P {
            tenant_id: Uuid,
            label: String,
            issuer: String,
            client_id: String,
            client_secret: Option<String>,
            domains: Vec<String>,
            auto_create: bool,
            default_roles: Vec<String>,
        }
        let p: P = sqlx::query_as(
            "select p.tenant_id, p.label, p.issuer, p.client_id, p.client_secret, p.domains, p.auto_create, p.default_roles \
             from sso_provider p join tenant t on t.id = p.tenant_id \
             where p.id = $1 and p.enabled and t.status = 'active' and (t.id = $2 or t.slug = $3)",
        )
        .bind(id)
        .bind(tenant)
        .bind(slug.map(|s| s.trim().to_lowercase()))
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| Error::NotFound("single sign-on provider".into()))?;
        let client_secret = match p.client_secret {
            Some(sealed) => {
                let key = self
                    .settings
                    .secret_key
                    .as_ref()
                    .ok_or_else(|| Error::Unavailable("single sign-on needs the server's secret key (FERRUM_SECRET_KEY)".into()))?;
                Some(key.open(&sealed_label(p.tenant_id, id), &sealed)?)
            }
            None => None,
        };
        let provider = Provider {
            id,
            label: p.label,
            issuer: p.issuer,
            client_id: p.client_id,
            client_secret,
            domains: p.domains,
            auto_create: p.auto_create,
            default_roles: p.default_roles,
        };
        Ok((p.tenant_id, provider))
    }

    /// Starts signing in through a provider → the address to send the browser to. The provider
    /// sends it back to `redirect_uri` (registered with the provider); `binding` is a secret the
    /// browser keeps and shows again to finish, so a sign-in started elsewhere cannot be finished
    /// in it.
    pub async fn sso_begin(&self, tenant_slug: &str, provider: Uuid, redirect_uri: &str, binding: &str) -> Result<String> {
        if !(16..=200).contains(&binding.len()) {
            return Err(Error::field("binding", "16 to 200 characters"));
        }
        let back = reqwest::Url::parse(redirect_uri).map_err(|_| Error::field("redirect_uri", "not a URL"))?;
        if !matches!(back.scheme(), "https" | "http") || redirect_uri.len() > 500 {
            return Err(Error::field("redirect_uri", "an http(s) address"));
        }
        let mut tx = self.db.system_tx().await?;
        let (tenant, p) = self.sign_in_provider(&mut tx, None, Some(tenant_slug), provider).await?;
        tx.commit().await?;
        let d = self.discover(&p.issuer).await?;
        let mut url = reqwest::Url::parse(&d.authorization_endpoint).map_err(|_| Error::Unavailable("the provider's sign-in address is not a URL".into()))?;
        if url.scheme() != "https" && !self.settings.sso_allow_private {
            return Err(Error::Unavailable("the provider's sign-in address must use https".into()));
        }
        let (state, nonce, verifier) = (random_secret(""), random_secret(""), random_secret(""));
        let mut tx = self.db.system_tx().await?;
        sqlx::query("delete from sso_flow where created_at < now() - make_interval(mins => $1)").bind(FLOW_MINUTES).execute(&mut *tx).await?;
        sqlx::query(
            "insert into sso_flow (state_hash, tenant_id, provider_id, nonce, verifier, redirect_uri, binding_hash) values ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(hash(&state))
        .bind(tenant)
        .bind(provider)
        .bind(&nonce)
        .bind(&verifier)
        .bind(redirect_uri)
        .bind(hash(binding))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &p.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", "openid email profile")
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())))
            .append_pair("code_challenge_method", "S256");
        Ok(url.to_string())
    }

    /// Finishes signing in through a provider with the `state` and `code` it sent the browser back
    /// with, and the browser's `binding`.
    pub async fn sso_finish(&self, state: &str, code: &str, binding: &str) -> Result<Session> {
        let again = |why: &str| Error::Unauthorized(format!("{why}; sign in again"));
        // The sign-in is used up whatever happens next.
        let mut tx = self.db.system_tx().await?;
        let flow: Option<(Uuid, Uuid, String, String, String, String)> = sqlx::query_as(
            "delete from sso_flow where state_hash = $1 and created_at > now() - make_interval(mins => $2) \
             returning tenant_id, provider_id, nonce, verifier, redirect_uri, binding_hash",
        )
        .bind(hash(state))
        .bind(FLOW_MINUTES)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((tenant, provider, nonce, verifier, redirect_uri, binding_hash)) = flow else {
            tx.commit().await?;
            return Err(again("this sign-in has expired or was already used"));
        };
        if hash(binding) != binding_hash {
            tx.commit().await?;
            return Err(again("this sign-in was started in another browser"));
        }
        let (_, p) = self.sign_in_provider(&mut tx, Some(tenant), None, provider).await?;
        tx.commit().await?;
        let d = self.discover(&p.issuer).await?;
        let mut form =
            vec![("grant_type", "authorization_code"), ("code", code), ("redirect_uri", redirect_uri.as_str()), ("code_verifier", verifier.as_str())];
        // client_secret_basic is the default the standard gives; post where the provider prefers it.
        let post = d.token_endpoint_auth_methods_supported.iter().any(|m| m == "client_secret_post")
            && !d.token_endpoint_auth_methods_supported.iter().any(|m| m == "client_secret_basic");
        let basic = match &p.client_secret {
            Some(s) if post => {
                form.push(("client_id", p.client_id.as_str()));
                form.push(("client_secret", s.as_str()));
                None
            }
            Some(s) => Some((p.client_id.as_str(), s.as_str())),
            None => {
                form.push(("client_id", p.client_id.as_str()));
                None
            }
        };
        let (status, answer) = self.idp_request(&d.token_endpoint, Some(&form), basic).await?;
        if status != 200 {
            let why = answer.get("error_description").or_else(|| answer.get("error")).and_then(Value::as_str).unwrap_or("no reason given");
            return Err(Error::Unauthorized(format!("{} refused the sign-in: {why}", p.label)));
        }
        let token = answer.get("id_token").and_then(Value::as_str).ok_or_else(|| Error::Unauthorized(format!("{} sent no ID token", p.label)))?;
        let claims = self.check_id_token(&d, &p, token, &nonce).await?;
        let email = verified_email(&claims, &p)?;
        self.sso_account(tenant, &p, &claims, &email).await
    }

    async fn check_id_token(&self, d: &Discovery, p: &Provider, token: &str, nonce: &str) -> Result<IdClaims> {
        let bad = |why: String| Error::Unauthorized(format!("{}'s ID token {why}", p.label));
        let header = jsonwebtoken::decode_header(token).map_err(|e| bad(format!("is malformed: {e}")))?;
        if !ALGORITHMS.contains(&header.alg) {
            return Err(bad(format!("is signed with {:?}, which is not accepted", header.alg)));
        }
        let (status, set) = self.idp_request(&d.jwks_uri, None, None).await?;
        if status != 200 {
            return Err(Error::Unavailable(format!("{}'s signing keys answered HTTP {status}", p.label)));
        }
        // Keys of kinds this server does not read are skipped, not fatal.
        let keys: Vec<Jwk> = set.get("keys").and_then(Value::as_array).into_iter().flatten().filter_map(|k| serde_json::from_value(k.clone()).ok()).collect();
        let jwk = match &header.kid {
            Some(kid) => keys.iter().find(|k| k.common.key_id.as_deref() == Some(kid.as_str())),
            None if keys.len() == 1 => keys.first(),
            None => None,
        }
        .ok_or_else(|| bad("is signed with a key the provider does not publish".into()))?;
        let key = DecodingKey::from_jwk(jwk).map_err(|e| bad(format!("cannot be checked: {e}")))?;
        let mut v = Validation::new(header.alg);
        v.set_issuer(&[&d.issuer]);
        v.set_audience(&[&p.client_id]);
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        v.leeway = 120;
        let c = jsonwebtoken::decode::<IdClaims>(token, &key, &v).map_err(|e| bad(format!("is not valid: {e}")))?.claims;
        if c.nonce.as_deref() != Some(nonce) {
            return Err(bad("belongs to another sign-in".into()));
        }
        let audiences = c.aud.as_array().map_or(1, Vec::len);
        if c.azp.as_deref().is_some_and(|azp| azp != p.client_id) || (audiences > 1 && c.azp.is_none()) {
            return Err(bad("was issued to another application".into()));
        }
        if c.sub.is_empty() || c.sub.len() > 255 {
            return Err(bad("has no usable subject".into()));
        }
        Ok(c)
    }

    /// The account signing in: linked to this identity at the provider, or found by email (and
    /// linked), or created when the provider creates accounts.
    async fn sso_account(&self, tenant: Uuid, p: &Provider, c: &IdClaims, email: &str) -> Result<Session> {
        let subject = format!("{}|{}", p.id, c.sub);
        #[derive(sqlx::FromRow)]
        struct U {
            id: Uuid,
            is_active: bool,
            token_version: i32,
            sso_subject: Option<String>,
        }
        let find = "select id, is_active, token_version, sso_subject from app_user where tenant_id = $1 and (sso_subject = $2 or email = $3) \
                    order by sso_subject = $2 desc nulls last limit 1 for update";
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let mut found: Option<U> = sqlx::query_as(find).bind(tenant).bind(&subject).bind(email).fetch_optional(&mut *tx).await?;
        if found.is_none() {
            if !p.auto_create {
                return Err(Error::Forbidden(format!("there is no account for {email} in this workspace; ask an administrator to add you")));
            }
            tx.commit().await?;
            let name: String = c
                .name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| email.split('@').next().unwrap_or(email))
                .chars()
                .take(100)
                .collect();
            let system = Ctx::system(tenant, self.registry.schema(tenant).await?);
            match self.create_user(&system, email, &name, None, &p.default_roles, false).await {
                Ok(_) => {}
                // Signing in twice at once: the other one created it.
                Err(Error::Validation(f)) if f.iter().all(|e| e.field == "email") => {}
                Err(Error::Validation(_)) => {
                    return Err(Error::invalid(format!("{}'s default roles include one this workspace no longer has; an administrator can fix them", p.label)));
                }
                Err(e) => return Err(e),
            }
            tx = self.db.system_tenant_tx(tenant).await?;
            found = sqlx::query_as(find).bind(tenant).bind(&subject).bind(email).fetch_optional(&mut *tx).await?;
        }
        let u = found.ok_or_else(|| Error::Retryable("the account is being created".into()))?;
        match u.sso_subject.as_deref() {
            Some(s) if s == subject => {}
            Some(s) if s.starts_with(&format!("{}|", p.id)) => {
                return Err(Error::Forbidden(format!("this account is linked to another {} identity; an administrator can unlink it", p.label)));
            }
            // Not linked yet, or linked through another provider: linked to this one from now on.
            _ => {
                sqlx::query("update app_user set sso_subject = $3 where tenant_id = $1 and id = $2")
                    .bind(tenant)
                    .bind(u.id)
                    .bind(&subject)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        if !u.is_active {
            return Err(Error::Forbidden("this account is deactivated".into()));
        }
        sqlx::query("update app_user set last_login_at = now() where tenant_id = $1 and id = $2").bind(tenant).bind(u.id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Session { tenant, user: u.id, token_version: u.token_version })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(domains: &[&str]) -> Provider {
        Provider {
            id: Uuid::nil(),
            label: "Okta".into(),
            issuer: "https://example.okta.com".into(),
            client_id: "ferrum".into(),
            client_secret: None,
            domains: domains.iter().map(|d| d.to_string()).collect(),
            auto_create: false,
            default_roles: vec![],
        }
    }

    fn claims(v: Value) -> IdClaims {
        let mut m = v.as_object().unwrap().clone();
        m.insert("sub".into(), json!("abc"));
        serde_json::from_value(Value::Object(m)).unwrap()
    }

    #[test]
    fn email_must_be_vouched_for() {
        let any = provider(&[]);
        let listed = provider(&["acme.test"]);
        assert_eq!(verified_email(&claims(json!({"email": " Pat@Acme.TEST ", "email_verified": true})), &any).unwrap(), "pat@acme.test");
        assert_eq!(verified_email(&claims(json!({"email": "pat@acme.test", "email_verified": "true"})), &any).unwrap(), "pat@acme.test");
        assert!(verified_email(&claims(json!({"email": "pat@acme.test", "email_verified": false})), &listed).is_err(), "unverified");
        assert!(verified_email(&claims(json!({"email": "pat@acme.test"})), &any).is_err(), "unsaid, no domains listed");
        assert_eq!(verified_email(&claims(json!({"upn": "pat@acme.test"})), &listed).unwrap(), "pat@acme.test", "Entra ID");
        assert!(verified_email(&claims(json!({"email": "pat@other.test", "email_verified": true})), &listed).is_err(), "domain not listed");
        assert!(verified_email(&claims(json!({"preferred_username": "pat"})), &any).is_err(), "no address");
        assert_eq!(clean_domain(" @Acme.Test ").unwrap(), "acme.test");
        assert!(clean_domain("acme").is_err() && clean_domain("a b.test").is_err());
        assert_eq!(form_encode("id:with space"), "id%3Awith%20space");
    }
}
