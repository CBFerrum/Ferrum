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
//!
//! A SAML provider may encrypt its assertions to the workspace's encryption key (its metadata
//! publishes it; a task replaces it), and sign people out both ways (saml_logout.rs). Each provider
//! keeps the addresses it was saved with: a provider saved on a server with an accounts host has them
//! there, every other under PUBLIC_URL.
//!
//! Signing in again confirms it is the person of a browser session (a step-up, at the end of this
//! file): the provider is asked to have them sign in again, and the answer counts only when it is the
//! identity they are linked to, signed in just now. Its flow is bound to the session and the person,
//! never finishes a sign-in, and a sign-in's flow never confirms anyone.

use crate::auth::Session;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::oauth::{hash, random_secret};
use crate::shell::StepUp;
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
/// How long before its answer is checked a person may have signed in at the provider for that to confirm it is them (a
/// step-up): the two minutes a provider's clock may be behind ours (as for every token, [`CLOCK_SKEW_SECS`]) and a
/// minute for the browser's way back and the token's fetch. A provider that answers from a session it already had states
/// a sign-in from before the step-up asked, which is refused too (allowing for the same clock).
pub const STEP_UP_FRESH_SECS: i64 = 180;
/// The proof of signing in again is taken by the page the person works on within a minute, as a sign-in carried from
/// the accounts host is (crate::identity, `HOST_CODE_SECONDS`).
const STEP_UP_PROOF_SECS: i64 = 60;
/// What a step-up's state starts with, so the return address providers have registered (`/app/login`) can tell it from
/// a sign-in's. Which kind a flow is, is kept with it: each finish refuses the other kind whatever its state says.
pub const STEP_UP_STATE: &str = "su.";

/// Whether a sign-in at the provider at `at` (seconds since 1970) shows the person signed in again for a step-up asked at
/// `asked`, checked at `now`: within [`STEP_UP_FRESH_SECS`] of now and not before it was asked, either way allowing for
/// the provider's clock. None (the provider did not say) is not.
pub fn fresh_sign_in(at: Option<i64>, asked: i64, now: i64) -> bool {
    at.is_some_and(|t| t <= now + CLOCK_SKEW_SECS && t >= now - STEP_UP_FRESH_SECS && t >= asked - CLOCK_SKEW_SECS)
}

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

/// Whether this server opens encrypted SAML assertions: `None` when it was built without SAML, else
/// whether the linked xmlsec1 has AES-GCM and RSA-OAEP (crates/saml, xmlenc.rs). When it has not,
/// says so once in the log, and nothing about encryption changes from before it was added.
pub fn saml_encryption() -> Option<std::result::Result<(), String>> {
    static SAID: std::sync::Once = std::sync::Once::new();
    let can = ferrum_saml::AVAILABLE.then(|| ferrum_saml::encryption().map(|_| ()))?;
    if let Err(e) = &can {
        SAID.call_once(|| tracing::warn!("{e}"));
    }
    Some(can)
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
    /// SAML: signing out here signs the person out at the provider too, when it offers single logout.
    /// On unless set off.
    #[serde(default)]
    pub single_logout_out: Option<bool>,
    /// SAML: the provider may end people's sessions here when they sign out there. On unless set off.
    #[serde(default)]
    pub single_logout_in: Option<bool>,
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
    /// When the person signed in at the provider (a step-up asks how recently). Read loosely: an odd value here never
    /// stops a sign-in, which does not look at it.
    #[serde(default)]
    auth_time: Option<Value>,
    #[serde(default)]
    aud: Value,
}

/// A provider ready for signing in (its secret opened).
pub(crate) struct Provider {
    pub(crate) id: Uuid,
    label: String,
    protocol: Protocol,
    issuer: String,
    client_id: String,
    client_secret: Option<String>,
    pub(crate) saml_metadata: Option<String>,
    domains: Vec<String>,
    auto_create: bool,
    default_roles: Vec<String>,
    /// SAML: the origin its addresses are under, when kept (see the migration 20261118002401).
    pub(crate) saml_sp_base: Option<String>,
    /// SAML: whether the provider may end sessions here (logout requests it sends).
    pub(crate) slo_in: Option<bool>,
}

/// This workspace as a SAML provider knows it, under one origin: its entity ID (where its metadata
/// is), its assertion consumer service and its logout service.
pub(crate) struct SpNames {
    pub(crate) entity_id: String,
    pub(crate) acs_url: String,
    pub(crate) slo_url: String,
}

impl SpNames {
    fn under(base: &str, tenant: Uuid) -> SpNames {
        let entity_id = format!("{}/api/auth/saml/{tenant}", base.trim_end_matches('/'));
        SpNames { acs_url: format!("{entity_id}/acs"), slo_url: format!("{entity_id}/slo"), entity_id }
    }
}

pub(crate) fn saml_enc_label(tenant: Uuid, id: Uuid) -> String {
    format!("saml-enc|{tenant}|{id}")
}

/// How long a replaced encryption key is still published and accepted.
const ENC_KEY_OVERLAP_DAYS: i32 = 30;

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
    pub(crate) async fn saml_idp(&self, source: &str) -> Result<ferrum_saml::Idp> {
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

    /// PUBLIC_URL, under which SAML providers saved before their addresses were kept have them.
    fn saml_public_base(&self) -> Result<String> {
        self.settings
            .public_url
            .as_deref()
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
            .ok_or_else(|| Error::Unavailable("SAML needs the server's public address (PUBLIC_URL)".into()))
    }

    /// Where a provider added now has its addresses: on the accounts host when the server has one (where
    /// every sign-in can happen, whatever host a workspace has or comes to have), else under PUBLIC_URL.
    fn saml_new_base(&self) -> Result<String> {
        let public = self.saml_public_base()?;
        Ok(match self.settings.accounts_host.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
            Some(host) => format!("{}://{host}", if public.starts_with("http://") { "http" } else { "https" }),
            None => public,
        })
    }

    /// A provider's addresses here: those it was saved with, or PUBLIC_URL's.
    pub(crate) fn saml_names(&self, tenant: Uuid, p: &Provider) -> Result<SpNames> {
        Ok(SpNames::under(&p.saml_sp_base.clone().map_or_else(|| self.saml_public_base(), Ok)?, tenant))
    }

    /// The workspace's keys assertions may be encrypted to, the current one first, made the first time
    /// wanted (one current key at most, whichever replica makes it), each sealed by the server key.
    pub(crate) async fn saml_enc_keys(&self, tenant: Uuid) -> Result<Vec<ferrum_saml::SpKey>> {
        type Row = (Uuid, String, String, bool);
        let read = || async {
            let mut tx = self.db.system_tx().await?;
            let rows: Vec<Row> = sqlx::query_as(
                "select id, private_key, certificate, retires_at is null from saml_enc_key where tenant_id = $1 and (retires_at is null or retires_at > now()) \
                 order by retires_at is null desc, created_at desc",
            )
            .bind(tenant)
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, Error>(rows)
        };
        let mut rows = read().await?;
        if !rows.iter().any(|r| r.3) {
            self.add_saml_enc_key(tenant, false).await?;
            rows = read().await?;
        }
        let unreadable = |_| Error::Internal("the workspace's SAML encryption key is not base64".into());
        rows.into_iter()
            .map(|(id, private, cert, _)| {
                let private_der = STANDARD.decode(self.open_sealed(&saml_enc_label(tenant, id), &private)?).map_err(unreadable)?;
                Ok(ferrum_saml::SpKey { private_der, certificate_der: STANDARD.decode(cert).map_err(unreadable)? })
            })
            .collect()
    }

    /// Makes the workspace a new encryption key; `replace`: the current one stays published and
    /// accepted for 30 days more, else none is replaced (and a key made at the same time elsewhere
    /// wins). Two replacements at once: one fails, on the one current key the database allows.
    async fn add_saml_enc_key(&self, tenant: Uuid, replace: bool) -> Result<Uuid> {
        let key = self.settings.secret_key.as_ref().ok_or_else(|| Error::Unavailable("SAML needs the server's secret key (FERRUM_SECRET_KEY)".into()))?;
        let made = tokio::task::spawn_blocking(move || ferrum_saml::new_sp_key(&format!("Ferrum {tenant} encryption")))
            .await
            .map_err(|e| Error::Unavailable(format!("making the workspace's SAML key stopped: {e}")))?
            .map_err(Error::Unavailable)?;
        let id = Uuid::now_v7();
        let sealed = key.seal(&saml_enc_label(tenant, id), &STANDARD.encode(&made.private_der))?;
        let mut tx = self.db.system_tx().await?;
        if replace {
            sqlx::query("delete from saml_enc_key where tenant_id = $1 and retires_at < now()").bind(tenant).execute(&mut *tx).await?;
            sqlx::query("update saml_enc_key set retires_at = now() + make_interval(days => $2) where tenant_id = $1 and retires_at is null")
                .bind(tenant)
                .bind(ENC_KEY_OVERLAP_DAYS)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(
            "insert into saml_enc_key (tenant_id, id, private_key, certificate) values ($1, $2, $3, $4) \
             on conflict (tenant_id) where retires_at is null do nothing",
        )
        .bind(tenant)
        .bind(id)
        .bind(sealed)
        .bind(STANDARD.encode(&made.certificate_der))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Replaces a workspace's SAML encryption key (the task `saml_encryption_key`): the new one is
    /// published first, and the old one is still published and accepted for 30 days, while providers
    /// read the metadata again. → the new key's id.
    pub async fn rotate_saml_encryption_key(&self, tenant: Uuid) -> Result<Uuid> {
        let made = self.add_saml_enc_key(tenant, true).await?;
        let mut tx = self.db.system_tx().await?;
        let current: bool = sqlx::query_scalar("select exists (select 1 from saml_enc_key where tenant_id = $1 and id = $2 and retires_at is null)")
            .bind(tenant)
            .bind(made)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        if current { Ok(made) } else { Err(Error::Conflict("another replacement of this key ran at the same time; run it again".into())) }
    }

    /// The workspace's own SAML key, made the first time it is wanted (by whichever replica gets
    /// there first), its private half sealed by the server key.
    pub(crate) async fn saml_key(&self, tenant: Uuid) -> Result<ferrum_saml::SpKey> {
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

    /// What a SAML identity provider's administrator needs about this workspace for a provider added
    /// now; `None` when this server cannot do SAML.
    pub fn saml_service_provider(&self, tenant: Uuid) -> Option<Value> {
        let n = self.saml_new_base().ok().filter(|_| ferrum_saml::AVAILABLE).map(|b| SpNames::under(&b, tenant))?;
        Some(json!({"entity_id": n.entity_id, "acs_url": n.acs_url, "slo_url": n.slo_url, "metadata_url": n.entity_id}))
    }

    /// This workspace's SAML metadata, for identity providers to load (it is public: it says only
    /// where answers go). Not found for a workspace that is not active.
    pub async fn saml_metadata(&self, tenant: Uuid) -> Result<String> {
        self.saml_metadata_at(tenant, None).await
    }

    /// [`Kernel::saml_metadata`] as served on `host`: on the accounts host, the addresses there; on
    /// any other, PUBLIC_URL's. While a provider saved before keys and logout were published still
    /// uses PUBLIC_URL's (not saved since), those say only what they said then: a provider that reads
    /// the metadata again (AD FS, Shibboleth) would otherwise start encrypting or sending logout
    /// messages unasked.
    pub async fn saml_metadata_at(&self, tenant: Uuid, host: Option<&str>) -> Result<String> {
        let mut tx = self.db.system_tx().await?;
        let (active, earlier): (bool, bool) = sqlx::query_as(
            "select exists (select 1 from tenant where id = $1 and status = 'active'), \
             exists (select 1 from sso_provider where tenant_id = $1 and protocol = 'saml' and saml_sp_base is null)",
        )
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        if !active {
            return Err(Error::NotFound("workspace".into()));
        }
        let accounts = self.settings.accounts_host.as_deref().and_then(crate::shell::normalize_host);
        let on_accounts = accounts.is_some() && host.and_then(crate::shell::normalize_host) == accounts;
        let base = if on_accounts { self.saml_new_base()? } else { self.saml_public_base()? };
        let n = SpNames::under(&base, tenant);
        let key = self.saml_key(tenant).await?;
        let as_then = earlier && !on_accounts;
        let can = saml_encryption().is_some_and(|c| c.is_ok());
        let decrypt = if as_then || !can { Vec::new() } else { self.saml_enc_keys(tenant).await? };
        let sp = ferrum_saml::Sp {
            entity_id: &n.entity_id,
            acs_url: &n.acs_url,
            key: Some(&key),
            decrypt: &decrypt,
            slo_url: (!as_then).then_some(n.slo_url.as_str()),
        };
        ferrum_saml::sp_metadata(&sp).map_err(Error::Unavailable)
    }

    /// Whether a sign-in may come back to `url`, which will carry its one-time code: the server's
    /// public address, a host of this workspace, or the accounts host; over https when the server is.
    pub(crate) async fn own_return(&self, tenant: Uuid, url: &reqwest::Url) -> Result<bool> {
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
               'single_logout_out', coalesce(slo_out, false), 'single_logout_in', coalesce(slo_in, false), 'saml_sp_base', saml_sp_base, \
               'linked_users', (select count(*) from app_user u where u.tenant_id = p.tenant_id and u.sso_subject like p.id::text || '|%'), \
               'created_at', created_at, 'updated_at', updated_at) \
             from sso_provider p where tenant_id = $1 order by label",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        // A SAML provider's own addresses, for its administrator.
        Ok(rows
            .into_iter()
            .map(|mut r| {
                let base = r["saml_sp_base"].as_str().map(str::to_string).map_or_else(|| self.saml_public_base(), Ok);
                if let (Some("saml"), Ok(base)) = (r["protocol"].as_str(), base) {
                    let n = SpNames::under(&base, ctx.tenant);
                    r["service_provider"] = json!({"entity_id": n.entity_id, "acs_url": n.acs_url, "slo_url": n.slo_url, "metadata_url": n.entity_id});
                }
                if let Some(o) = r.as_object_mut() {
                    o.remove("saml_sp_base");
                }
                r
            })
            .collect())
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
                self.outward(ctx.tenant).await?;
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
                self.saml_new_base().map_err(|e| Error::field("saml_metadata", e.to_string()))?;
                self.outward(ctx.tenant).await?;
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
        // SAML: a new provider's addresses are where new ones go; one saved before keeps PUBLIC_URL's,
        // as then, from now on whatever PUBLIC_URL becomes. Never changed once kept.
        let (sp_base, slo_out, slo_in) = match input.protocol {
            Protocol::Saml => (
                Some(if new { self.saml_new_base()? } else { self.saml_public_base()? }),
                Some(input.single_logout_out.unwrap_or(true)),
                Some(input.single_logout_in.unwrap_or(true)),
            ),
            Protocol::Oidc => (None, None, None),
        };
        sqlx::query(
            "insert into sso_provider (tenant_id, id, label, issuer, client_id, client_secret, domains, auto_create, default_roles, enabled, protocol, saml_metadata, \
               saml_sp_base, slo_out, slo_in) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $12, $13, $14, $15, $16) \
             on conflict (tenant_id, id) do update set label = excluded.label, issuer = excluded.issuer, client_id = excluded.client_id, \
               client_secret = case when $11 then excluded.client_secret else sso_provider.client_secret end, \
               domains = excluded.domains, auto_create = excluded.auto_create, default_roles = excluded.default_roles, enabled = excluded.enabled, \
               saml_metadata = excluded.saml_metadata, saml_sp_base = coalesce(sso_provider.saml_sp_base, excluded.saml_sp_base), \
               slo_out = excluded.slo_out, slo_in = excluded.slo_in, updated_at = now()",
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
        .bind(&sp_base)
        .bind(slo_out)
        .bind(slo_in)
        .execute(&mut *tx)
        .await?;
        let what = json!({"label": label, "protocol": input.protocol.name(), "issuer": issuer, "client_id": client_id, "secret_changed": secret.is_some(), "metadata_changed": saml_metadata.is_some(), "domains": domains, "auto_create": input.auto_create, "default_roles": roles, "enabled": input.enabled, "single_logout_out": slo_out, "single_logout_in": slo_in});
        audit::record_admin(&mut tx, ctx, "SsoProvider", id, "save", None, what).await?;
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
        audit::record_admin(&mut tx, ctx, "SsoProvider", id, "delete", None, json!({})).await?;
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
        audit::record_admin(&mut tx, ctx, "User", user, "unlink_sso", None, json!({})).await?;
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

    pub(crate) async fn sign_in_provider(&self, tx: &mut crate::db::Tx, tenant: Option<Uuid>, slug: Option<&str>, id: Uuid) -> Result<(Uuid, Provider)> {
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
            saml_sp_base: Option<String>,
            slo_in: Option<bool>,
        }
        let p: P = sqlx::query_as(
            "select p.tenant_id, p.label, p.protocol, p.issuer, p.client_id, p.client_secret, p.saml_metadata, p.domains, p.auto_create, p.default_roles, \
               p.saml_sp_base, p.slo_in \
             from sso_provider p join tenant t on t.id = p.tenant_id \
             where p.id = $1 and p.enabled and t.status = 'active' and (t.id = $2 or t.slug = $3)",
        )
        .bind(id)
        .bind(tenant)
        .bind(slug.map(|s| s.trim().to_lowercase()))
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| Error::NotFound("single sign-on provider".into()))?;
        // Every sign-in through a provider reaches it: never from a sandbox (sandbox.rs).
        self.outward(p.tenant_id).await?;
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
            saml_sp_base: p.saml_sp_base,
            slo_in: p.slo_in,
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
            let n = self.saml_names(tenant, &p)?;
            // An XML ID: a letter or `_` first.
            let (state, request) = (random_secret(""), format!("_{}", hex::encode(rand::random::<[u8; 16]>())));
            self.store_flow(&state, tenant, provider, &request, "", redirect_uri, binding, None).await?;
            let key = self.saml_key(tenant).await?;
            let sp = ferrum_saml::Sp { entity_id: &n.entity_id, acs_url: &n.acs_url, key: Some(&key), decrypt: &[], slo_url: None };
            return ferrum_saml::request_url(&sp, &idp, &request, &state, false).map_err(Error::Unavailable);
        }
        let d = self.discover(&p.issuer).await?;
        let mut url = reqwest::Url::parse(&d.authorization_endpoint).map_err(|_| Error::Unavailable("the provider's sign-in address is not a URL".into()))?;
        if url.scheme() != "https" && !self.settings.sso_allow_private {
            return Err(Error::Unavailable("the provider's sign-in address must use https".into()));
        }
        let (state, nonce, verifier) = (random_secret(""), random_secret(""), random_secret(""));
        self.store_flow(&state, tenant, provider, &nonce, &verifier, redirect_uri, binding, None).await?;
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
    /// request's ID. `step_up`: a sign-in again that only confirms it is the person of this browser session (the
    /// session, the person, the page that hands the proof on), which never signs anyone in.
    #[allow(clippy::too_many_arguments)]
    async fn store_flow(
        &self,
        state: &str,
        tenant: Uuid,
        provider: Uuid,
        nonce: &str,
        verifier: &str,
        redirect_uri: &str,
        binding: &str,
        step_up: Option<(Uuid, Uuid, &str)>,
    ) -> Result<()> {
        let mut tx = self.db.system_tx().await?;
        sqlx::query("delete from sso_flow where created_at < now() - make_interval(mins => $1)").bind(FLOW_MINUTES).execute(&mut *tx).await?;
        sqlx::query(
            "insert into sso_flow (state_hash, tenant_id, provider_id, nonce, verifier, redirect_uri, binding_hash, step_up_session, step_up_user, step_up_back) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(hash(state))
        .bind(tenant)
        .bind(provider)
        .bind(nonce)
        .bind(verifier)
        .bind(redirect_uri)
        .bind(hash(binding))
        .bind(step_up.map(|s| s.0))
        .bind(step_up.map(|s| s.1))
        .bind(step_up.map(|s| s.2))
        .execute(&mut *tx)
        .await?;
        Ok(tx.commit().await?)
    }

    /// A SAML provider posted its answer (`SAMLResponse`) to a sign-in (`RelayState`) of this
    /// workspace's → where to send the browser: back where the sign-in started, with a one-time code
    /// to finish with ([`Kernel::sso_finish`]). The sign-in is used up when the answer is refused, and
    /// an answer counts once.
    pub async fn saml_acs(&self, tenant: Uuid, relay_state: &str, response: &str) -> Result<String> {
        self.saml_acs_from(tenant, relay_state, response, &crate::gate::Arrival::default()).await
    }

    /// [`Kernel::saml_acs`] for a browser at `from`: the answer to a step-up's signing in again
    /// ([`Kernel::sso_step_up_begin`]) is checked as one, and a refused one is a failed step-up on the person's trail.
    pub async fn saml_acs_from(&self, tenant: Uuid, relay_state: &str, response: &str, from: &crate::gate::Arrival) -> Result<String> {
        let again = |why: &str| Error::Unauthorized(format!("{why}; sign in again"));
        let mut tx = self.db.system_tx().await?;
        let flow: Option<(Uuid, String, String, Option<Uuid>, i64)> = sqlx::query_as(
            "select provider_id, nonce, redirect_uri, step_up_user, extract(epoch from created_at)::bigint from sso_flow \
             where state_hash = $1 and tenant_id = $2 and code_hash is null and created_at > now() - make_interval(mins => $3)",
        )
        .bind(hash(relay_state))
        .bind(tenant)
        .bind(FLOW_MINUTES)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((provider, request, redirect_uri, step_up, asked)) = flow else {
            tx.commit().await?;
            return Err(again("this sign-in has expired or was already used"));
        };
        let (_, p) = self.sign_in_provider(&mut tx, Some(tenant), None, provider).await?;
        tx.commit().await?;
        let checked = match p.protocol {
            Protocol::Saml => self.saml_check(tenant, &p, response, request).await,
            Protocol::Oidc => Err(again("this provider does not speak SAML")),
        };
        // Signing in again to confirm it is the person: the same identity, signed in just now. Only that it was is kept;
        // nothing here signs anyone in, links or makes an account.
        if let Some(user) = step_up {
            let checked = match checked {
                Ok(person) => self.same_and_fresh(tenant, user, &p, &person.subject, person.authn_instant, asked).await,
                Err(e) => Err(e),
            };
            return self.step_up_answered(relay_state, tenant, user, &redirect_uri, checked, from).await;
        }
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
        let n = self.saml_names(tenant, p)?;
        let decrypt = if saml_encryption().is_some_and(|c| c.is_ok()) { self.saml_enc_keys(tenant).await? } else { Vec::new() };
        let response = response.to_string();
        // xmlsec1 is synchronous work, kept off the request threads.
        let checked = tokio::task::spawn_blocking(move || {
            let sp = ferrum_saml::Sp { entity_id: &n.entity_id, acs_url: &n.acs_url, key: None, decrypt: &decrypt, slo_url: None };
            ferrum_saml::verify(&sp, &idp, &response, &request)
        })
        .await
        .map_err(|e| Error::Unavailable(format!("checking the answer stopped: {e}")))?;
        checked.map_err(|why| Error::Unauthorized(format!("{}'s answer was refused: {why}; sign in again", p.label)))
    }

    /// Finishes signing in through a provider with the `state` and `code` it sent the browser back
    /// with, and the browser's `binding`.
    pub async fn sso_finish(&self, state: &str, code: &str, binding: &str) -> Result<Session> {
        Ok(self.sso_finish_full(state, code, binding).await?.0)
    }

    /// [`Kernel::sso_finish`], with what a SAML sign-in's logout needs, for the session it opens to
    /// keep ([`Kernel::open_session_with`]).
    pub async fn sso_finish_full(&self, state: &str, code: &str, binding: &str) -> Result<(Session, Option<Value>)> {
        let again = |why: &str| Error::Unauthorized(format!("{why}; sign in again"));
        // The sign-in is used up whatever happens next.
        let mut tx = self.db.system_tx().await?;
        type Flow = (Uuid, Uuid, String, String, String, String, Option<String>, Option<Value>);
        let flow: Option<Flow> = sqlx::query_as(
            "delete from sso_flow where state_hash = $1 and created_at > now() - make_interval(mins => $2) and step_up_session is null \
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
        if let Err(e) = self.refuse_if_moving(tenant).await {
            tx.commit().await?;
            return Err(e);
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
            let logout =
                person.name_id.as_ref().map(|n| json!({"provider": p.id, "name_id": n.value, "format": n.format, "session_index": person.session_index}));
            let claims = IdClaims { sub: person.subject, email: person.email, name: person.name, ..IdClaims::default() };
            let email = verified_email(&claims, &p)?;
            return Ok((self.sso_account(tenant, &p, &claims, &email).await?, logout));
        }
        let d = self.discover(&p.issuer).await?;
        let claims = self.id_token_for(&d, &p, code, &redirect_uri, &verifier, &nonce).await?;
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
        Ok((session, None))
    }

    /// The provider's ID token for the authorization `code` it sent back, checked ([`Kernel::check_id_token`]).
    async fn id_token_for(&self, d: &Discovery, p: &Provider, code: &str, redirect_uri: &str, verifier: &str, nonce: &str) -> Result<IdClaims> {
        let mut form = vec![("grant_type", "authorization_code"), ("code", code), ("redirect_uri", redirect_uri), ("code_verifier", verifier)];
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
        self.check_id_token(d, p, token, nonce).await
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
        audit::record_admin(&mut tx, &by, "User", user, "sso_logout", None, json!({"provider": p.label})).await?;
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

// ── signing in again to confirm it is you (a step-up; docs/operations.md, "Asking again before sensitive work") ──

/// When an ID token says the person signed in at the provider (`auth_time`, seconds since 1970), read loosely: a value
/// that is not a number is none.
fn auth_time(c: &IdClaims) -> Option<i64> {
    c.auth_time.as_ref().and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
}

impl Kernel {
    /// The provider a person is linked to (its id and name): enabled, of an active workspace that is not a sandbox (a
    /// sandbox reaches no provider), and, for SAML, only where this server checks SAML. None: no such way.
    pub async fn linked_provider(&self, tenant: Uuid, user: Uuid) -> Result<Option<(Uuid, String)>> {
        let mut tx = self.db.system_tx().await?;
        let row: Option<(Uuid, String, String)> = sqlx::query_as(
            "select p.id, p.label, p.protocol from app_user u join tenant t on t.id = u.tenant_id \
             join sso_provider p on p.tenant_id = u.tenant_id and p.id::text = split_part(u.sso_subject, '|', 1) \
             where u.tenant_id = $1 and u.id = $2 and u.is_active and p.enabled and t.status = 'active' and t.sandbox_of is null",
        )
        .bind(tenant)
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row.filter(|r| r.2 != "saml" || ferrum_saml::AVAILABLE).map(|(id, label, _)| (id, label)))
    }

    /// Starts signing in again at the provider the person of `who` is linked to, to confirm it is them for that browser
    /// session → the address to open (in a small window). The provider is asked to have the person sign in again rather
    /// than answer from a session it already has: OpenID Connect `prompt=login` and `max_age=0`, SAML `ForceAuthn`. It
    /// comes back to `redirect_uri` (OpenID Connect: the return address registered with it, `/app/login`) or to the
    /// assertion consumer service (SAML), and the proof goes on to `back`, a page on the host the session is on
    /// ([`Kernel::sso_step_up_finish`]). The flow is bound to the session and the person; its state starts with
    /// [`STEP_UP_STATE`], and its browser binding is a secret nobody is given, so it never finishes a sign-in.
    pub async fn sso_step_up_begin(&self, who: &StepUp, redirect_uri: &str, back: &str) -> Result<String> {
        let (provider, _) = self.linked_provider(who.tenant, who.user).await?.ok_or_else(|| Error::invalid("you have no single sign-on identity here"))?;
        let back_url = reqwest::Url::parse(back).map_err(|_| Error::invalid("the page to come back to is not an address"))?;
        if back.len() > 500 || !self.own_return(who.tenant, &back_url).await? {
            return Err(Error::invalid("the page to come back to is not one of this workspace's"));
        }
        let mut tx = self.db.system_tx().await?;
        let (_, p) = self.sign_in_provider(&mut tx, Some(who.tenant), None, provider).await?;
        tx.commit().await?;
        let (state, binding) = (random_secret(STEP_UP_STATE), random_secret(""));
        let bound = Some((who.session, who.user, back));
        if p.protocol == Protocol::Saml {
            let idp = self.saml_idp(p.saml_metadata.as_deref().unwrap_or_default()).await?;
            let n = self.saml_names(who.tenant, &p)?;
            let request = format!("_{}", hex::encode(rand::random::<[u8; 16]>()));
            self.store_flow(&state, who.tenant, provider, &request, "", back, &binding, bound).await?;
            let key = self.saml_key(who.tenant).await?;
            let sp = ferrum_saml::Sp { entity_id: &n.entity_id, acs_url: &n.acs_url, key: Some(&key), decrypt: &[], slo_url: None };
            return ferrum_saml::request_url(&sp, &idp, &request, &state, true).map_err(Error::Unavailable);
        }
        let d = self.discover(&p.issuer).await?;
        let mut url = reqwest::Url::parse(&d.authorization_endpoint).map_err(|_| Error::Unavailable("the provider's sign-in address is not a URL".into()))?;
        if url.scheme() != "https" && !self.settings.sso_allow_private {
            return Err(Error::Unavailable("the provider's sign-in address must use https".into()));
        }
        let (nonce, verifier) = (random_secret(""), random_secret(""));
        self.store_flow(&state, who.tenant, provider, &nonce, &verifier, redirect_uri, &binding, bound).await?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &p.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", "openid")
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("prompt", "login")
            .append_pair("max_age", "0")
            .append_pair("code_challenge", &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())))
            .append_pair("code_challenge_method", "S256");
        Ok(url.to_string())
    }

    /// An OpenID Connect provider sent the browser back from signing in again (`code`; none when it did not sign them in)
    /// to a step-up's state → where to send it: the page that hands the proof to the person's dialog, with the state and
    /// a one-time code. Accepted only for the identity the person is linked to, signed in again just now
    /// ([`fresh_sign_in`]), after every check a sign-in's ID token gets; a sign-in's state is not found here. The flow is
    /// used up when anything is refused, and a refusal is a failed step-up on the person's trail.
    pub async fn sso_step_up_back(&self, state: &str, code: Option<&str>, from: &crate::gate::Arrival) -> Result<String> {
        type Flow = (Uuid, Uuid, String, String, String, Uuid, String, i64);
        let mut tx = self.db.system_tx().await?;
        let flow: Option<Flow> = sqlx::query_as(
            "select tenant_id, provider_id, nonce, verifier, redirect_uri, step_up_user, step_up_back, extract(epoch from created_at)::bigint from sso_flow \
             where state_hash = $1 and step_up_session is not null and code_hash is null and created_at > now() - make_interval(mins => $2)",
        )
        .bind(hash(state))
        .bind(FLOW_MINUTES)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let Some((tenant, provider, nonce, verifier, redirect_uri, user, back, asked)) = flow else {
            return Err(Error::Unauthorized("this request to sign in again has expired or was already used; try again".into()));
        };
        let checked = async {
            let code = code.filter(|c| !c.is_empty()).ok_or_else(|| Error::Unauthorized("the identity provider did not sign you in again".into()))?;
            let mut tx = self.db.system_tx().await?;
            let (_, p) = self.sign_in_provider(&mut tx, Some(tenant), None, provider).await?;
            tx.commit().await?;
            if p.protocol != Protocol::Oidc {
                return Err(Error::Unauthorized(format!("{} does not sign people in with OpenID Connect", p.label)));
            }
            let d = self.discover(&p.issuer).await?;
            let claims = self.id_token_for(&d, &p, code, &redirect_uri, &verifier, &nonce).await?;
            self.same_and_fresh(tenant, user, &p, &claims.sub, auth_time(&claims), asked).await
        }
        .await;
        self.step_up_answered(state, tenant, user, &back, checked, from).await
    }

    /// The provider's answer to a step-up's signing in again, `checked`: refused, the flow goes and the person's trail has
    /// a failed step-up; accepted, the flow keeps a one-time code (as a hash) and when it was made, and the browser goes on
    /// to `back` with the state and that code. Once: a flow already answered is not answered again.
    async fn step_up_answered(&self, state: &str, tenant: Uuid, user: Uuid, back: &str, checked: Result<()>, from: &crate::gate::Arrival) -> Result<String> {
        if let Err(e) = checked {
            let mut tx = self.db.system_tx().await?;
            sqlx::query("delete from sso_flow where state_hash = $1 and code_hash is null").bind(hash(state)).execute(&mut *tx).await?;
            tx.commit().await?;
            if matches!(e, Error::Unauthorized(_) | Error::Forbidden(_)) {
                self.record_sign_in_failure(tenant, user, "step_up_failed", "provider", from).await?;
            }
            return Err(e);
        }
        let proof = random_secret("");
        let mut tx = self.db.system_tx().await?;
        let done = sqlx::query("update sso_flow set code_hash = $2, answer = $3 where state_hash = $1 and code_hash is null")
            .bind(hash(state))
            .bind(hash(&proof))
            .bind(json!({"step_up": true, "at": chrono::Utc::now().timestamp()}))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        if done.rows_affected() == 0 {
            return Err(Error::Unauthorized("this request to sign in again was already used; try again".into()));
        }
        let mut next = reqwest::Url::parse(back).map_err(|_| Error::Unauthorized("this request to sign in again cannot come back; try again".into()))?;
        next.query_pairs_mut().append_pair("state", state).append_pair("code", &proof);
        Ok(next.to_string())
    }

    /// What signing in again must show: a sign-in at `p` at `at` fresh for a step-up asked at `asked`
    /// ([`fresh_sign_in`]), as the identity `user` is linked to there (`sub`). Each refusal says only what the person
    /// needs to know to put it right.
    async fn same_and_fresh(&self, tenant: Uuid, user: Uuid, p: &Provider, sub: &str, at: Option<i64>, asked: i64) -> Result<()> {
        if !fresh_sign_in(at, asked, chrono::Utc::now().timestamp()) {
            return Err(Error::Unauthorized(format!(
                "{} did not ask you to sign in again, so this cannot confirm it is you; an administrator can have the provider ask again when told to",
                p.label
            )));
        }
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let linked: Option<String> = sqlx::query_scalar("select sso_subject from app_user where tenant_id = $1 and id = $2 and is_active")
            .bind(tenant)
            .bind(user)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
        tx.commit().await?;
        if linked.as_deref() != Some(format!("{}|{sub}", p.id).as_str()) {
            return Err(Error::Unauthorized(format!("you signed in at {} as someone else: the account does not match, so nothing changed", p.label)));
        }
        Ok(())
    }

    /// Takes the proof that the person of `who` signed in again at their provider (the `state` and one-time `code` the page
    /// was handed): once, within a minute of being made, only for that session and person, and only on the origin it was
    /// handed to (`here`, this request's). Then the session counts as stepped up ([`Kernel::stepped_up`]). Anything else is
    /// a failed step-up on the person's trail, and changes nothing.
    pub async fn sso_step_up_finish(&self, who: &StepUp, here: &str, state: &str, code: &str, from: &crate::gate::Arrival) -> Result<()> {
        let unfinished = || Error::Unauthorized("signing in again did not finish; try again".into());
        let proved = async {
            let mut tx = self.db.system_tx().await?;
            let taken: Option<(Option<String>, Option<Value>)> = sqlx::query_as(
                "delete from sso_flow where state_hash = $1 and tenant_id = $2 and step_up_session = $3 and step_up_user = $4 and code_hash = $5 \
                 and created_at > now() - make_interval(mins => $6) returning step_up_back, answer",
            )
            .bind(hash(state))
            .bind(who.tenant)
            .bind(who.session)
            .bind(who.user)
            .bind(hash(code))
            .bind(FLOW_MINUTES)
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            let (back, answer) = taken.ok_or_else(unfinished)?;
            let made = answer.filter(|a| a["step_up"] == json!(true)).and_then(|a| a["at"].as_i64()).ok_or_else(unfinished)?;
            let origin = |u: &str| reqwest::Url::parse(u).ok().map(|u| u.origin().ascii_serialization());
            let same_origin = origin(here).is_some() && origin(here) == back.as_deref().and_then(origin);
            if !same_origin || made < chrono::Utc::now().timestamp() - STEP_UP_PROOF_SECS {
                return Err(unfinished());
            }
            Ok(())
        }
        .await;
        match proved {
            Ok(()) => self.stepped_up(who, "provider", from).await,
            Err(e @ Error::Unauthorized(_)) => {
                self.record_sign_in_failure(who.tenant, who.user, "step_up_failed", "provider", from).await?;
                Err(e)
            }
            Err(e) => Err(e),
        }
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
            saml_sp_base: None,
            slo_in: None,
        }
    }

    fn claims(v: Value) -> IdClaims {
        let mut m = v.as_object().unwrap().clone();
        m.insert("sub".into(), json!("abc"));
        serde_json::from_value(Value::Object(m)).unwrap()
    }

    #[test]
    fn a_step_up_takes_only_a_sign_in_made_after_it_asked() {
        let now = 1_800_000_000;
        let asked = now - 30;
        assert!(fresh_sign_in(Some(now - 5), asked, now));
        assert!(fresh_sign_in(Some(now - STEP_UP_FRESH_SECS), now - 600, now), "the oldest taken, for a step-up asked long ago");
        assert!(!fresh_sign_in(Some(now - STEP_UP_FRESH_SECS - 1), now - 600, now), "older than that");
        assert!(fresh_sign_in(Some(asked - CLOCK_SKEW_SECS), asked, now), "a provider's clock behind ours");
        assert!(!fresh_sign_in(Some(asked - CLOCK_SKEW_SECS - 1), asked, now), "a sign-in from before it asked: answered from a session it had");
        assert!(fresh_sign_in(Some(now + CLOCK_SKEW_SECS), asked, now), "a provider's clock ahead");
        assert!(!fresh_sign_in(Some(now + CLOCK_SKEW_SECS + 1), asked, now), "from the future");
        assert!(!fresh_sign_in(None, asked, now), "the provider did not say");
        assert_eq!(auth_time(&claims(json!({"auth_time": 1_800_000_000}))), Some(1_800_000_000));
        assert_eq!(auth_time(&claims(json!({"auth_time": 1_800_000_000.5}))), Some(1_800_000_000));
        assert_eq!(auth_time(&claims(json!({"auth_time": "1800000000"}))), None, "not a number");
        assert_eq!(auth_time(&claims(json!({}))), None);
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
