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
//!
//! A provider that supports OpenID Connect Back-Channel Logout can end a person's sessions here when
//! they sign out there, or when it disables them: it posts a signed logout token to
//! `/api/auth/sso/{provider}/backchannel-logout`. That ends all their sessions at Ferrum, on every
//! replica, not only the one the token names: refusing too much is the safe way to be wrong.
//!
//! A provider can speak SAML 2.0 instead (crates/saml). The browser is sent to it with a request, and
//! it posts its signed answer to the workspace's assertion consumer service
//! (`/api/auth/saml/{workspace}/acs`). The server checks the answer, keeps who it names on the
//! sign-in, and sends the browser back to where it started with a one-time code, which finishes the
//! sign-in as with OpenID Connect: binding, linking and account rules are the same. A SAML provider
//! never says whether it checked an address, so it is trusted for the email domains listed, and only
//! for them. The return address must be one of this server's own: the code travels in it.

use crate::auth::Session;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::oauth::{hash, random_secret};
use crate::{Kernel, audit};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use jsonwebtoken::jwk::Jwk;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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

/// What marks a token as a logout token (OpenID Connect Back-Channel Logout 1.0).
const LOGOUT_EVENT: &str = "http://schemas.openid.net/event/backchannel-logout";
/// How old a logout token may be. Seen tokens are remembered for longer than this.
const LOGOUT_TOKEN_SECS: i64 = 600;
/// How far ahead of our clock a provider's may be.
const CLOCK_SKEW_SECS: i64 = 120;

#[derive(Debug, Deserialize)]
struct LogoutClaims {
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    sid: Option<String>,
    iat: i64,
    jti: String,
    events: Value,
    #[serde(default)]
    nonce: Option<String>,
}

impl LogoutClaims {
    /// What makes it a logout token and a recent one, beyond its signature, issuer and audience.
    fn check(&self, now: i64) -> std::result::Result<(), &'static str> {
        if !self.events.get(LOGOUT_EVENT).is_some_and(Value::is_object) {
            return Err("is not a logout event");
        }
        // Forbidden by the standard: it is how an ID token passed off as a logout token is told apart.
        if self.nonce.is_some() {
            return Err("carries a nonce");
        }
        if self.sub.as_deref().unwrap_or_default().is_empty() && self.sid.as_deref().unwrap_or_default().is_empty() {
            return Err("names neither a person nor a session");
        }
        if self.jti.is_empty() || self.jti.len() > 255 {
            return Err("has no usable id");
        }
        if self.iat > now + CLOCK_SKEW_SECS || self.iat < now - LOGOUT_TOKEN_SECS {
            return Err("is too old, or from the future");
        }
        Ok(())
    }
}

/// How a provider signs people in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Oidc,
    Saml,
}

impl Protocol {
    fn name(self) -> &'static str {
        match self {
            Protocol::Oidc => "oidc",
            Protocol::Saml => "saml",
        }
    }
}

/// Whether this server checks SAML signatures: `None` when it was built without SAML, else whether
/// xmlsec1 signs and verifies here (crates/saml, `self_check`).
pub fn saml_self_check() -> Option<std::result::Result<(), String>> {
    ferrum_saml::AVAILABLE.then(ferrum_saml::self_check)
}

/// A provider as an administrator sets it up.
#[derive(Debug, Clone, Deserialize)]
pub struct SsoProviderInput {
    pub label: String,
    #[serde(default)]
    pub protocol: Protocol,
    /// OpenID Connect: the provider's issuer URL (its discovery document lives under it).
    #[serde(default)]
    pub issuer: String,
    /// OpenID Connect: the client ID the provider issued.
    #[serde(default)]
    pub client_id: String,
    /// SAML: the identity provider's metadata, as XML or the https address it is published at
    /// (fetched again at each sign-in, so a new signing certificate is picked up).
    #[serde(default)]
    pub saml_metadata: Option<String>,
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

#[derive(Deserialize, Default)]
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
    /// The provider's session, for its logout tokens to name.
    #[serde(default)]
    sid: Option<String>,
    #[serde(default)]
    aud: Value,
}

/// A provider ready for signing in (its secret opened).
struct Provider {
    id: Uuid,
    label: String,
    protocol: Protocol,
    issuer: String,
    client_id: String,
    client_secret: Option<String>,
    saml_metadata: Option<String>,
    domains: Vec<String>,
    auto_create: bool,
    default_roles: Vec<String>,
}

fn admin_only(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("single sign-on is set up by administrators".into())) }
}

pub(crate) fn sealed_label(tenant: Uuid, id: Uuid) -> String {
    format!("sso|{tenant}|{id}")
}

pub(crate) fn saml_key_label(tenant: Uuid) -> String {
    format!("saml-sp|{tenant}")
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
        let (status, body, host) = self.idp_fetch(url, form, basic, "application/json").await?;
        let v = serde_json::from_slice(&body)
            .map_err(|_| Error::Unavailable(format!("the identity provider cannot be reached: HTTP {status} without a JSON answer from {host}")))?;
        Ok((status, v))
    }

    /// [`Kernel::idp_request`] without reading the answer: the status, the body and the host.
    async fn idp_fetch(&self, url: &str, form: Option<&[(&str, &str)]>, basic: Option<(&str, &str)>, accept: &str) -> Result<(u16, Vec<u8>, String)> {
        let unreachable = |e: String| Error::Unavailable(format!("the identity provider cannot be reached: {e}"));
        let (client, parsed) = self.vetted_client(url, self.settings.sso_allow_private, Duration::from_secs(10), "Ferrum-SSO/1").await?;
        let host = parsed.host_str().unwrap_or_default().to_string();
        let mut req = match form {
            Some(f) => client.post(parsed).form(f),
            None => client.get(parsed),
        };
        if let Some((user, password)) = basic {
            req = req.basic_auth(form_encode(user), Some(form_encode(password)));
        }
        let mut resp = req.header("accept", accept).send().await.map_err(|e| unreachable(e.to_string()))?;
        let status = resp.status().as_u16();
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| unreachable(e.to_string()))? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_ANSWER {
                return Err(unreachable("its answer is too large".into()));
            }
        }
        Ok((status, body, host))
    }

    /// A SAML identity provider from its metadata as stored: the XML itself, or the address it is
    /// published at, fetched now.
    async fn saml_idp(&self, source: &str) -> Result<ferrum_saml::Idp> {
        let xml = if source.starts_with("https://") || source.starts_with("http://") {
            let (status, body, host) = self.idp_fetch(source, None, None, "application/samlmetadata+xml, application/xml, text/xml").await?;
            if status != 200 {
                return Err(Error::Unavailable(format!("the provider's metadata ({source}) answered HTTP {status}")));
            }
            String::from_utf8(body).map_err(|_| Error::Unavailable(format!("the metadata {host} sent is not text")))?
        } else {
            source.to_string()
        };
        ferrum_saml::Idp::read(&xml).map_err(Error::invalid)
    }

    /// This workspace as SAML identity providers know it: its entity ID (where its metadata is) and
    /// its assertion consumer service. One per workspace, so it can be given to a provider before the
    /// provider is added here.
    fn saml_sp(&self, tenant: Uuid) -> Result<(String, String)> {
        let base = self
            .settings
            .public_url
            .as_deref()
            .map(|u| u.trim().trim_end_matches('/'))
            .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
            .ok_or_else(|| Error::Unavailable("SAML needs the server's public address (PUBLIC_URL)".into()))?;
        let entity_id = format!("{base}/api/auth/saml/{tenant}");
        Ok((entity_id.clone(), format!("{entity_id}/acs")))
    }

    /// The workspace's own SAML key, made the first time it is wanted (by whichever replica gets
    /// there first), its private half sealed by the server key.
    async fn saml_key(&self, tenant: Uuid) -> Result<ferrum_saml::SpKey> {
        let label = saml_key_label(tenant);
        let read = || async {
            let mut tx = self.db.system_tx().await?;
            let row: Option<(String, String)> =
                sqlx::query_as("select private_key, certificate from saml_sp_key where tenant_id = $1").bind(tenant).fetch_optional(&mut *tx).await?;
            tx.commit().await?;
            Ok::<_, Error>(row)
        };
        let row = match read().await? {
            Some(row) => row,
            None => {
                let key =
                    self.settings.secret_key.as_ref().ok_or_else(|| Error::Unavailable("SAML needs the server's secret key (FERRUM_SECRET_KEY)".into()))?;
                let made = tokio::task::spawn_blocking(move || ferrum_saml::new_sp_key(&format!("Ferrum {tenant}")))
                    .await
                    .map_err(|e| Error::Unavailable(format!("making the workspace's SAML key stopped: {e}")))?
                    .map_err(Error::Unavailable)?;
                let sealed = key.seal(&label, &STANDARD.encode(&made.private_der))?;
                let mut tx = self.db.system_tx().await?;
                sqlx::query("insert into saml_sp_key (tenant_id, private_key, certificate) values ($1, $2, $3) on conflict (tenant_id) do nothing")
                    .bind(tenant)
                    .bind(sealed)
                    .bind(STANDARD.encode(&made.certificate_der))
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                read().await?.ok_or_else(|| Error::Retryable("the workspace's SAML key is being made".into()))?
            }
        };
        let unreadable = |_| Error::Internal("the workspace's SAML key is not base64".into());
        let private_der = STANDARD.decode(self.open_sealed(&label, &row.0)?).map_err(unreadable)?;
        Ok(ferrum_saml::SpKey { private_der, certificate_der: STANDARD.decode(&row.1).map_err(unreadable)? })
    }

    /// What a SAML identity provider's administrator needs about this workspace; `None` when this
    /// server cannot do SAML.
    pub fn saml_service_provider(&self, tenant: Uuid) -> Option<Value> {
        let (entity_id, acs_url) = self.saml_sp(tenant).ok().filter(|_| ferrum_saml::AVAILABLE)?;
        Some(json!({"entity_id": entity_id, "acs_url": acs_url, "metadata_url": entity_id}))
    }

    /// This workspace's SAML metadata, for identity providers to load (it is public: it says only
    /// where answers go). Not found for a workspace that is not active.
    pub async fn saml_metadata(&self, tenant: Uuid) -> Result<String> {
        let mut tx = self.db.system_tx().await?;
        let active: bool =
            sqlx::query_scalar("select exists (select 1 from tenant where id = $1 and status = 'active')").bind(tenant).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        if !active {
            return Err(Error::NotFound("workspace".into()));
        }
        let (entity_id, acs_url) = self.saml_sp(tenant)?;
        let key = self.saml_key(tenant).await?;
        ferrum_saml::sp_metadata(&ferrum_saml::Sp { entity_id: &entity_id, acs_url: &acs_url, key: Some(&key) }).map_err(Error::Unavailable)
    }

    /// Whether a sign-in may come back to `url`, which will carry its one-time code: the server's
    /// public address, a host of this workspace, or the accounts host; over https when the server is.
    async fn own_return(&self, tenant: Uuid, url: &reqwest::Url) -> Result<bool> {
        let public = self.settings.public_url.as_deref().and_then(|u| reqwest::Url::parse(u).ok());
        if public.as_ref().is_some_and(|p| p.origin() == url.origin()) {
            return Ok(true);
        }
        if public.as_ref().is_some_and(|p| p.scheme() == "https") && url.scheme() != "https" {
            return Ok(false);
        }
        let Some(host) = url.host_str() else { return Ok(false) };
        if self.settings.accounts_host.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(host)) {
            return Ok(true);
        }
        Ok(matches!(self.route_host(host).await?, crate::shell::HostRoute::Serve { tenant: t, .. } if t == tenant))
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
            "select jsonb_build_object('id', id, 'label', label, 'protocol', protocol, 'issuer', issuer, 'client_id', client_id, 'has_secret', client_secret is not null, \
               'saml_metadata', saml_metadata, 'domains', domains, 'auto_create', auto_create, 'default_roles', default_roles, 'enabled', enabled, \
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
        let mut domains = input.domains.iter().filter(|d| !d.trim().is_empty()).map(|d| clean_domain(d)).collect::<Result<Vec<_>>>()?;
        domains.sort();
        domains.dedup();
        if domains.len() > 50 {
            return Err(Error::field("domains", "at most 50"));
        }
        let mut roles: Vec<String> = input.default_roles.iter().map(|r| r.trim().to_string()).filter(|r| !r.is_empty()).collect();
        roles.sort();
        roles.dedup();
        let (id, new) = match id {
            Some(id) => (id, false),
            None => (Uuid::now_v7(), true),
        };
        if !new {
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            let was: Option<String> = sqlx::query_scalar("select protocol from sso_provider where tenant_id = $1 and id = $2")
                .bind(ctx.tenant)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
            tx.commit().await?;
            match was {
                None => return Err(Error::NotFound("single sign-on provider".into())),
                // People are linked to their identity at the provider, which another protocol names otherwise.
                Some(p) if p != input.protocol.name() => return Err(Error::field("protocol", "a provider keeps its protocol; add another provider instead")),
                Some(_) => {}
            }
        }
        let (issuer, client_id, saml_metadata) = match input.protocol {
            Protocol::Oidc => {
                let client_id = input.client_id.trim();
                if client_id.is_empty() || client_id.len() > 255 {
                    return Err(Error::field("client_id", "give the client ID the provider issued"));
                }
                let issuer = input.issuer.trim();
                if issuer.len() > 500 {
                    return Err(Error::field("issuer", "is too long"));
                }
                // The issuer as the provider spells it (a trailing slash matters to its tokens).
                (self.discover(issuer).await.map_err(|e| Error::field("issuer", e.to_string()))?.issuer, client_id.to_string(), None)
            }
            Protocol::Saml => {
                if domains.is_empty() {
                    return Err(Error::field("domains", "list the email domains this provider vouches for: SAML never says whether an address was checked"));
                }
                let source = input.saml_metadata.as_deref().map(str::trim).filter(|m| !m.is_empty());
                let source = source.ok_or_else(|| Error::field("saml_metadata", "paste the provider's metadata, or the https address it is published at"))?;
                if source.len() > MAX_ANSWER {
                    return Err(Error::field("saml_metadata", "is larger than a megabyte"));
                }
                self.saml_sp(ctx.tenant).map_err(|e| Error::field("saml_metadata", e.to_string()))?;
                let idp = self.saml_idp(source).await.map_err(|e| Error::field("saml_metadata", e.to_string()))?;
                (idp.entity_id, String::new(), Some(source.to_string()))
            }
        };
        let key =
            self.settings.secret_key.as_ref().ok_or_else(|| Error::Unavailable("single sign-on needs the server's secret key (FERRUM_SECRET_KEY)".into()))?;
        let secret = match input.client_secret.as_deref().map(str::trim) {
            _ if input.protocol == Protocol::Saml => Some(None),
            Some("") => Some(None),
            Some(s) if s.len() > 1000 => return Err(Error::field("client_secret", "is too long")),
            Some(s) => Some(Some(key.seal(&sealed_label(ctx.tenant, id), s)?)),
            None => None,
        };
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        if !new
            && sqlx::query_scalar::<_, i64>("select count(*) from sso_provider where tenant_id = $1 and id = $2 and protocol = $3")
                .bind(ctx.tenant)
                .bind(id)
                .bind(input.protocol.name())
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
            "insert into sso_provider (tenant_id, id, label, issuer, client_id, client_secret, domains, auto_create, default_roles, enabled, protocol, saml_metadata) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $12, $13) \
             on conflict (tenant_id, id) do update set label = excluded.label, issuer = excluded.issuer, client_id = excluded.client_id, \
               client_secret = case when $11 then excluded.client_secret else sso_provider.client_secret end, \
               domains = excluded.domains, auto_create = excluded.auto_create, default_roles = excluded.default_roles, enabled = excluded.enabled, \
               saml_metadata = excluded.saml_metadata, updated_at = now()",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(label)
        .bind(&issuer)
        .bind(&client_id)
        .bind(secret.clone().flatten())
        .bind(&domains)
        .bind(input.auto_create)
        .bind(&roles)
        .bind(input.enabled)
        .bind(secret.is_some())
        .bind(input.protocol.name())
        .bind(&saml_metadata)
        .execute(&mut *tx)
        .await?;
        let what = json!({"label": label, "protocol": input.protocol.name(), "issuer": issuer, "client_id": client_id, "secret_changed": secret.is_some(), "metadata_changed": saml_metadata.is_some(), "domains": domains, "auto_create": input.auto_create, "default_roles": roles, "enabled": input.enabled});
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
            protocol: String,
            issuer: String,
            client_id: String,
            client_secret: Option<String>,
            saml_metadata: Option<String>,
            domains: Vec<String>,
            auto_create: bool,
            default_roles: Vec<String>,
        }
        let p: P = sqlx::query_as(
            "select p.tenant_id, p.label, p.protocol, p.issuer, p.client_id, p.client_secret, p.saml_metadata, p.domains, p.auto_create, p.default_roles \
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
            Some(sealed) => Some(self.open_sealed(&sealed_label(p.tenant_id, id), &sealed)?),
            None => None,
        };
        let provider = Provider {
            id,
            label: p.label,
            protocol: if p.protocol == "saml" { Protocol::Saml } else { Protocol::Oidc },
            issuer: p.issuer,
            client_id: p.client_id,
            client_secret,
            saml_metadata: p.saml_metadata,
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
        if p.protocol == Protocol::Saml {
            if !self.own_return(tenant, &back).await? {
                return Err(Error::field("redirect_uri", "a SAML sign-in comes back only to this server's own addresses"));
            }
            let idp = self.saml_idp(p.saml_metadata.as_deref().unwrap_or_default()).await?;
            let (entity_id, acs_url) = self.saml_sp(tenant)?;
            // An XML ID: a letter or `_` first.
            let (state, request) = (random_secret(""), format!("_{}", hex::encode(rand::random::<[u8; 16]>())));
            self.store_flow(&state, tenant, provider, &request, "", redirect_uri, binding).await?;
            let key = self.saml_key(tenant).await?;
            let sp = ferrum_saml::Sp { entity_id: &entity_id, acs_url: &acs_url, key: Some(&key) };
            return ferrum_saml::request_url(&sp, &idp, &request, &state).map_err(Error::Unavailable);
        }
        let d = self.discover(&p.issuer).await?;
        let mut url = reqwest::Url::parse(&d.authorization_endpoint).map_err(|_| Error::Unavailable("the provider's sign-in address is not a URL".into()))?;
        if url.scheme() != "https" && !self.settings.sso_allow_private {
            return Err(Error::Unavailable("the provider's sign-in address must use https".into()));
        }
        let (state, nonce, verifier) = (random_secret(""), random_secret(""), random_secret(""));
        self.store_flow(&state, tenant, provider, &nonce, &verifier, redirect_uri, binding).await?;
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

    /// A sign-in on its way through a provider: `nonce` is the OpenID Connect nonce, or the SAML
    /// request's ID.
    async fn store_flow(&self, state: &str, tenant: Uuid, provider: Uuid, nonce: &str, verifier: &str, redirect_uri: &str, binding: &str) -> Result<()> {
        let mut tx = self.db.system_tx().await?;
        sqlx::query("delete from sso_flow where created_at < now() - make_interval(mins => $1)").bind(FLOW_MINUTES).execute(&mut *tx).await?;
        sqlx::query(
            "insert into sso_flow (state_hash, tenant_id, provider_id, nonce, verifier, redirect_uri, binding_hash) values ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(hash(state))
        .bind(tenant)
        .bind(provider)
        .bind(nonce)
        .bind(verifier)
        .bind(redirect_uri)
        .bind(hash(binding))
        .execute(&mut *tx)
        .await?;
        Ok(tx.commit().await?)
    }

    /// A SAML provider posted its answer (`SAMLResponse`) to a sign-in (`RelayState`) of this
    /// workspace's → where to send the browser: back where the sign-in started, with a one-time code
    /// to finish with ([`Kernel::sso_finish`]). The sign-in is used up when the answer is refused, and
    /// an answer counts once.
    pub async fn saml_acs(&self, tenant: Uuid, relay_state: &str, response: &str) -> Result<String> {
        let again = |why: &str| Error::Unauthorized(format!("{why}; sign in again"));
        let mut tx = self.db.system_tx().await?;
        let flow: Option<(Uuid, String, String)> = sqlx::query_as(
            "select provider_id, nonce, redirect_uri from sso_flow \
             where state_hash = $1 and tenant_id = $2 and code_hash is null and created_at > now() - make_interval(mins => $3)",
        )
        .bind(hash(relay_state))
        .bind(tenant)
        .bind(FLOW_MINUTES)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((provider, request, redirect_uri)) = flow else {
            tx.commit().await?;
            return Err(again("this sign-in has expired or was already used"));
        };
        let (_, p) = self.sign_in_provider(&mut tx, Some(tenant), None, provider).await?;
        tx.commit().await?;
        let checked = match p.protocol {
            Protocol::Saml => self.saml_check(tenant, &p, response, request).await,
            Protocol::Oidc => Err(again("this provider does not speak SAML")),
        };
        let mut tx = self.db.system_tx().await?;
        let person = match checked {
            Ok(person) => person,
            Err(e) => {
                sqlx::query("delete from sso_flow where state_hash = $1").bind(hash(relay_state)).execute(&mut *tx).await?;
                tx.commit().await?;
                return Err(e);
            }
        };
        let code = random_secret("");
        let done = sqlx::query("update sso_flow set code_hash = $2, answer = $3 where state_hash = $1 and code_hash is null")
            .bind(hash(relay_state))
            .bind(hash(&code))
            .bind(serde_json::to_value(&person).map_err(|e| Error::Unavailable(e.to_string()))?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        if done.rows_affected() == 0 {
            return Err(again("this sign-in was already used"));
        }
        let mut back = reqwest::Url::parse(&redirect_uri).map_err(|_| again("this sign-in's return address is not a URL"))?;
        back.query_pairs_mut().append_pair("state", relay_state).append_pair("code", &code);
        Ok(back.to_string())
    }

    /// A SAML provider's answer checked against its metadata, for this workspace and request.
    async fn saml_check(&self, tenant: Uuid, p: &Provider, response: &str, request: String) -> Result<ferrum_saml::Person> {
        let idp = self.saml_idp(p.saml_metadata.as_deref().unwrap_or_default()).await?;
        let (entity_id, acs_url) = self.saml_sp(tenant)?;
        let key = self.saml_key(tenant).await?;
        let response = response.to_string();
        // xmlsec1 is synchronous work, kept off the request threads.
        let checked = tokio::task::spawn_blocking(move || {
            ferrum_saml::verify(&ferrum_saml::Sp { entity_id: &entity_id, acs_url: &acs_url, key: Some(&key) }, &idp, &response, &request)
        })
        .await
        .map_err(|e| Error::Unavailable(format!("checking the answer stopped: {e}")))?;
        checked.map_err(|why| Error::Unauthorized(format!("{}'s answer was refused: {why}; sign in again", p.label)))
    }

    /// Finishes signing in through a provider with the `state` and `code` it sent the browser back
    /// with, and the browser's `binding`.
    pub async fn sso_finish(&self, state: &str, code: &str, binding: &str) -> Result<Session> {
        let again = |why: &str| Error::Unauthorized(format!("{why}; sign in again"));
        // The sign-in is used up whatever happens next.
        let mut tx = self.db.system_tx().await?;
        type Flow = (Uuid, Uuid, String, String, String, String, Option<String>, Option<Value>);
        let flow: Option<Flow> = sqlx::query_as(
            "delete from sso_flow where state_hash = $1 and created_at > now() - make_interval(mins => $2) \
             returning tenant_id, provider_id, nonce, verifier, redirect_uri, binding_hash, code_hash, answer",
        )
        .bind(hash(state))
        .bind(FLOW_MINUTES)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((tenant, provider, nonce, verifier, redirect_uri, binding_hash, code_hash, answer)) = flow else {
            tx.commit().await?;
            return Err(again("this sign-in has expired or was already used"));
        };
        if hash(binding) != binding_hash {
            tx.commit().await?;
            return Err(again("this sign-in was started in another browser"));
        }
        let (_, p) = self.sign_in_provider(&mut tx, Some(tenant), None, provider).await?;
        tx.commit().await?;
        if p.protocol == Protocol::Saml {
            // The code is the one the server sent the browser on with, once the provider's answer
            // checked out (saml_acs); the answer is what it named.
            let person: Option<ferrum_saml::Person> = answer.and_then(|a| serde_json::from_value(a).ok());
            let (Some(person), true) = (person, code_hash.as_deref() == Some(hash(code).as_str())) else {
                return Err(again("this sign-in was not finished at the provider"));
            };
            let claims = IdClaims { sub: person.subject, email: person.email, name: person.name, ..IdClaims::default() };
            let email = verified_email(&claims, &p)?;
            return self.sso_account(tenant, &p, &claims, &email).await;
        }
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
        let session = self.sso_account(tenant, &p, &claims, &email).await?;
        if let Some(sid) = claims.sid.as_deref().filter(|s| !s.is_empty() && s.len() <= 255) {
            let mut tx = self.db.system_tenant_tx(tenant).await?;
            sqlx::query("insert into sso_sid (tenant_id, provider_id, sid, user_id) values ($1, $2, $3, $4) on conflict (tenant_id, provider_id, sid) do update set user_id = excluded.user_id")
                .bind(tenant)
                .bind(p.id)
                .bind(sid)
                .bind(session.user)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        Ok(session)
    }

    /// A provider signed someone out: ends everything they are signed in with here. A token already
    /// seen, or for nobody we know, does nothing and is not an error: the provider need not retry.
    pub async fn sso_backchannel_logout(&self, provider: Uuid, token: &str) -> Result<()> {
        let mut tx = self.db.system_tx().await?;
        let tenant: Option<Uuid> = sqlx::query_scalar("select tenant_id from sso_provider where id = $1").bind(provider).fetch_optional(&mut *tx).await?;
        let tenant = tenant.ok_or_else(|| Error::NotFound("single sign-on provider".into()))?;
        let (_, p) = self.sign_in_provider(&mut tx, Some(tenant), None, provider).await?;
        tx.commit().await?;
        if p.protocol != Protocol::Oidc {
            return Err(Error::Unauthorized(format!("{} does not sign people in with OpenID Connect", p.label)));
        }
        let d = self.discover(&p.issuer).await?;
        let (key, mut v) = self.token_key(&d, &p, token, "logout token").await?;
        v.set_required_spec_claims(&["iss", "aud"]);
        let c = jsonwebtoken::decode::<LogoutClaims>(token, &key, &v)
            .map_err(|e| Error::Unauthorized(format!("{}'s logout token is not valid: {e}", p.label)))?
            .claims;
        c.check(chrono::Utc::now().timestamp()).map_err(|why| Error::Unauthorized(format!("{}'s logout token {why}", p.label)))?;

        let schema = self.registry.schema(tenant).await?;
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let fresh = sqlx::query("insert into sso_logout_seen (tenant_id, provider_id, jti) values ($1, $2, $3) on conflict do nothing")
            .bind(tenant)
            .bind(provider)
            .bind(&c.jti)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
        if !fresh {
            return Ok(tx.commit().await?);
        }
        let subject = c.sub.as_ref().map(|sub| format!("{provider}|{sub}"));
        let mut user: Option<Uuid> = sqlx::query_scalar("select id from app_user where tenant_id = $1 and sso_subject = $2")
            .bind(tenant)
            .bind(&subject)
            .fetch_optional(&mut *tx)
            .await?;
        if let (None, Some(sid)) = (user, &c.sid) {
            user = sqlx::query_scalar("select user_id from sso_sid where tenant_id = $1 and provider_id = $2 and sid = $3")
                .bind(tenant)
                .bind(provider)
                .bind(sid)
                .fetch_optional(&mut *tx)
                .await?;
        }
        let Some(user) = user else {
            return Ok(tx.commit().await?);
        };
        sqlx::query("delete from sso_sid where tenant_id = $1 and provider_id = $2 and user_id = $3")
            .bind(tenant)
            .bind(provider)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        crate::auth::end_sessions(&mut tx, tenant, user).await?;
        let by = Ctx::system(tenant, schema);
        audit::record(&mut tx, &by, "User", user, "sso_logout", None, json!({"provider": p.label})).await?;
        tx.commit().await?;
        self.invalidate_user(tenant, user);
        Ok(())
    }

    /// The key a token of this provider's is signed with, and a validation of its issuer and this
    /// client to go with it.
    async fn token_key(&self, d: &Discovery, p: &Provider, token: &str, what: &str) -> Result<(DecodingKey, Validation)> {
        let bad = |why: String| Error::Unauthorized(format!("{}'s {what} {why}", p.label));
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
        v.leeway = CLOCK_SKEW_SECS as u64;
        Ok((key, v))
    }

    async fn check_id_token(&self, d: &Discovery, p: &Provider, token: &str, nonce: &str) -> Result<IdClaims> {
        let bad = |why: String| Error::Unauthorized(format!("{}'s ID token {why}", p.label));
        let (key, mut v) = self.token_key(d, p, token, "ID token").await?;
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
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

    #[test]
    fn logout_tokens_are_told_from_everything_else() {
        let token = |v: Value| serde_json::from_value::<LogoutClaims>(v).unwrap();
        let event = json!({LOGOUT_EVENT: {}});
        let now = 1_800_000_000;
        assert!(token(json!({"sub": "a", "iat": now, "jti": "1", "events": event})).check(now).is_ok());
        assert!(token(json!({"sid": "a", "iat": now, "jti": "1", "events": event})).check(now).is_ok());
        assert!(token(json!({"sub": "a", "iat": now, "jti": "1", "events": {}})).check(now).is_err(), "another event");
        assert!(token(json!({"sub": "a", "iat": now, "jti": "1", "events": {LOGOUT_EVENT: "yes"}})).check(now).is_err(), "must be an object");
        assert!(token(json!({"sub": "a", "iat": now, "jti": "1", "events": event, "nonce": "n"})).check(now).is_err(), "an ID token");
        assert!(token(json!({"iat": now, "jti": "1", "events": event})).check(now).is_err(), "nobody named");
        assert!(token(json!({"sub": "a", "iat": now, "jti": "", "events": event})).check(now).is_err());
        assert!(token(json!({"sub": "a", "iat": now - LOGOUT_TOKEN_SECS - 1, "jti": "1", "events": event})).check(now).is_err(), "replayed later");
        assert!(token(json!({"sub": "a", "iat": now + CLOCK_SKEW_SECS + 1, "jti": "1", "events": event})).check(now).is_err());
    }

    fn provider(domains: &[&str]) -> Provider {
        Provider {
            id: Uuid::nil(),
            label: "Okta".into(),
            protocol: Protocol::Oidc,
            issuer: "https://example.okta.com".into(),
            client_id: "ferrum".into(),
            client_secret: None,
            saml_metadata: None,
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
