//! OAuth 2.1 state for API and MCP clients: self-registered public clients (RFC 7591), one-time
//! authorization codes bound to PKCE S256 challenges, and rotating refresh tokens with reuse
//! detection. Codes and tokens are random 256-bit values stored only as SHA-256 hashes.

use crate::error::{Error, Result};
use crate::{Ctx, Kernel};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const SCOPE: &str = "ferrum";
const CODE_TTL_SECS: i64 = 600;
const REFRESH_TTL_DAYS: i64 = 30;
/// Clients that never obtained a token are removed after this many days.
const IDLE_CLIENT_DAYS: i32 = 90;

#[derive(Debug, Clone, serde::Serialize)]
pub struct OAuthClient {
    pub id: String,
    pub name: String,
    pub redirect_uris: Vec<String>,
}

/// Who a code or refresh token speaks for, plus the new refresh token to hand out.
#[derive(Debug, Clone)]
pub struct Grant {
    pub tenant: Uuid,
    pub user: Uuid,
    pub token_version: i32,
    pub scope: String,
    pub refresh_token: String,
}

fn random_secret(prefix: &str) -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes))
}

fn hash(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// PKCE S256: `BASE64URL(SHA256(verifier)) == challenge`, with RFC 7636 verifier rules.
pub fn pkce_matches(verifier: &str, challenge: &str) -> bool {
    let valid = (43..=128).contains(&verifier.len()) && verifier.bytes().all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c));
    valid && URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) == challenge
}

/// A redirect URI a client may register: https anywhere, http only on loopback (RFC 8252), or a
/// private-use scheme for native apps. Never fragments or script-capable schemes.
pub fn redirect_uri_allowed(uri: &str) -> bool {
    let Some((scheme, rest)) = uri.split_once(':') else { return false };
    let scheme = scheme.to_ascii_lowercase();
    if uri.contains('#') || rest.is_empty() || uri.len() > 2000 || uri.chars().any(char::is_whitespace) {
        return false;
    }
    match scheme.as_str() {
        "https" => rest.starts_with("//") && rest.len() > 2,
        "http" => loopback_host(rest).is_some(),
        "javascript" | "data" | "file" | "vbscript" | "about" | "blob" | "ftp" | "ws" | "wss" => false,
        s => s.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && s.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)),
    }
}

/// `//host[:port]/path` → host when it is a loopback address.
fn loopback_host(rest: &str) -> Option<&str> {
    let authority = rest.strip_prefix("//")?.split(['/', '?']).next()?;
    let host = if authority.starts_with('[') { authority.split(']').next().map(|h| &h[1..])? } else { authority.split(':').next()? };
    matches!(host, "localhost" | "127.0.0.1" | "::1").then_some(host)
}

/// Exact match, except that loopback redirects may use any port (RFC 8252 §7.3).
pub fn redirect_matches(registered: &[String], given: &str) -> bool {
    let strip_port = |u: &str| -> Option<String> {
        let (scheme, rest) = u.split_once("://")?;
        let (authority, path) = rest.split_at(rest.find(['/', '?']).unwrap_or(rest.len()));
        let host = if authority.starts_with('[') { authority.split_inclusive(']').next()? } else { authority.split(':').next()? };
        Some(format!("{scheme}://{host}{path}"))
    };
    registered.iter().any(|r| {
        r == given
            || (r.starts_with("http://")
                && loopback_host(&r[5..]).is_some()
                && given.starts_with("http://")
                && strip_port(r).is_some()
                && strip_port(r) == strip_port(given))
    })
}

impl Kernel {
    pub async fn oauth_register(&self, name: Option<String>, redirect_uris: Vec<String>) -> Result<OAuthClient> {
        if redirect_uris.is_empty() || redirect_uris.len() > 10 {
            return Err(Error::invalid("give between 1 and 10 redirect_uris"));
        }
        if let Some(bad) = redirect_uris.iter().find(|u| !redirect_uri_allowed(u)) {
            return Err(Error::invalid(format!("redirect URI not allowed: {bad} (use https, a loopback http URL or an app scheme)")));
        }
        let name = name.map(|n| n.trim().chars().take(100).collect::<String>()).filter(|n| !n.is_empty()).unwrap_or_else(|| "MCP client".into());
        let client = OAuthClient { id: random_secret("fc_")[..27].to_string(), name, redirect_uris };
        let mut tx = self.db.system_tx().await?;
        sqlx::query("insert into oauth_client (id, name, redirect_uris) values ($1, $2, $3)")
            .bind(&client.id)
            .bind(&client.name)
            .bind(&client.redirect_uris)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(client)
    }

    pub async fn oauth_client(&self, id: &str) -> Result<OAuthClient> {
        let mut tx = self.db.system_tx().await?;
        let row: Option<(String, String, Vec<String>)> =
            sqlx::query_as("select id, name, redirect_uris from oauth_client where id = $1").bind(id).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        let (id, name, redirect_uris) = row.ok_or_else(|| Error::NotFound("OAuth client".into()))?;
        Ok(OAuthClient { id, name, redirect_uris })
    }

    /// The signed-in user approved `client_id`: a one-time code for the token endpoint.
    pub async fn oauth_issue_code(&self, ctx: &Ctx, client_id: &str, redirect_uri: &str, challenge: &str, method: &str) -> Result<String> {
        if method != "S256" {
            return Err(Error::invalid("code_challenge_method must be S256"));
        }
        if !(43..=128).contains(&challenge.len()) || !challenge.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
            return Err(Error::invalid("code_challenge must be a base64url SHA-256 digest"));
        }
        let client = self.oauth_client(client_id).await?;
        if !redirect_matches(&client.redirect_uris, redirect_uri) {
            return Err(Error::invalid("redirect_uri is not registered for this client"));
        }
        let code = random_secret("");
        let mut tx = self.db.system_tx().await?;
        sqlx::query(
            "insert into oauth_code (code_hash, client_id, tenant_id, user_id, redirect_uri, code_challenge, scope, expires_at) \
             values ($1, $2, $3, $4, $5, $6, $7, now() + make_interval(secs => $8))",
        )
        .bind(hash(&code))
        .bind(client_id)
        .bind(ctx.tenant)
        .bind(ctx.user)
        .bind(redirect_uri)
        .bind(challenge)
        .bind(SCOPE)
        .bind(CODE_TTL_SECS as f64)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(code)
    }

    /// Exchanges a code (once) for a grant, checking client, redirect URI, expiry and PKCE.
    pub async fn oauth_redeem_code(&self, client_id: &str, code: &str, redirect_uri: &str, verifier: &str) -> Result<Grant> {
        let mut tx = self.db.system_tx().await?;
        let row: Option<(String, Uuid, Uuid, String, String, String, bool, bool)> = sqlx::query_as(
            "select client_id, tenant_id, user_id, redirect_uri, code_challenge, scope, expires_at < now(), used_at is not null \
             from oauth_code where code_hash = $1 for update",
        )
        .bind(hash(code))
        .fetch_optional(&mut *tx)
        .await?;
        let invalid = || Error::Invalid("invalid_grant".into());
        let (cid, tenant, user, redirect, challenge, scope, expired, used) = row.ok_or_else(invalid)?;
        if used {
            // A replayed code means it leaked: revoke what it produced.
            sqlx::query("update oauth_refresh set revoked_at = now() where client_id = $1 and tenant_id = $2 and user_id = $3 and revoked_at is null")
                .bind(&cid)
                .bind(tenant)
                .bind(user)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Err(invalid());
        }
        if cid != client_id || redirect != redirect_uri || expired || !pkce_matches(verifier, &challenge) {
            return Err(invalid());
        }
        sqlx::query("update oauth_code set used_at = now() where code_hash = $1").bind(hash(code)).execute(&mut *tx).await?;
        let grant = self.new_refresh(&mut tx, client_id, tenant, user, &scope).await?;
        tx.commit().await?;
        Ok(grant)
    }

    /// Rotates a refresh token. Presenting an already rotated token revokes the whole family.
    pub async fn oauth_refresh(&self, client_id: &str, refresh_token: &str) -> Result<Grant> {
        let mut tx = self.db.system_tx().await?;
        let row: Option<(String, Uuid, Uuid, String, bool, bool)> = sqlx::query_as(
            "select client_id, tenant_id, user_id, scope, expires_at < now(), revoked_at is not null from oauth_refresh where token_hash = $1 for update",
        )
        .bind(hash(refresh_token))
        .fetch_optional(&mut *tx)
        .await?;
        let invalid = || Error::Invalid("invalid_grant".into());
        let (cid, tenant, user, scope, expired, revoked) = row.ok_or_else(invalid)?;
        if cid != client_id {
            return Err(invalid());
        }
        if revoked {
            sqlx::query("update oauth_refresh set revoked_at = now() where client_id = $1 and tenant_id = $2 and user_id = $3 and revoked_at is null")
                .bind(&cid)
                .bind(tenant)
                .bind(user)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Err(invalid());
        }
        if expired {
            return Err(invalid());
        }
        sqlx::query("update oauth_refresh set revoked_at = now() where token_hash = $1").bind(hash(refresh_token)).execute(&mut *tx).await?;
        let grant = self.new_refresh(&mut tx, client_id, tenant, user, &scope).await?;
        tx.commit().await?;
        Ok(grant)
    }

    async fn new_refresh(&self, tx: &mut crate::Tx, client_id: &str, tenant: Uuid, user: Uuid, scope: &str) -> Result<Grant> {
        let token_version: Option<i32> = sqlx::query_scalar(
            "select u.token_version from app_user u join tenant t on t.id = u.tenant_id \
             where u.tenant_id = $1 and u.id = $2 and u.is_active and t.status = 'active'",
        )
        .bind(tenant)
        .bind(user)
        .fetch_optional(&mut **tx)
        .await?;
        let token_version = token_version.ok_or_else(|| Error::Invalid("invalid_grant".into()))?;
        let refresh_token = random_secret("fr_");
        sqlx::query(
            "insert into oauth_refresh (token_hash, client_id, tenant_id, user_id, scope, expires_at) \
             values ($1, $2, $3, $4, $5, now() + make_interval(days => $6))",
        )
        .bind(hash(&refresh_token))
        .bind(client_id)
        .bind(tenant)
        .bind(user)
        .bind(scope)
        .bind(REFRESH_TTL_DAYS as i32)
        .execute(&mut **tx)
        .await?;
        sqlx::query("update oauth_client set last_used_at = now() where id = $1").bind(client_id).execute(&mut **tx).await?;
        Ok(Grant { tenant, user, token_version, scope: scope.to_string(), refresh_token })
    }

    /// RFC 7009: revoking an unknown token is not an error.
    pub async fn oauth_revoke(&self, token: &str) -> Result<()> {
        let mut tx = self.db.system_tx().await?;
        sqlx::query("update oauth_refresh set revoked_at = now() where token_hash = $1 and revoked_at is null").bind(hash(token)).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Drops expired codes and tokens and clients that never got a token. Part of maintenance.
    pub async fn oauth_purge(&self) -> Result<u64> {
        let mut tx = self.db.system_tx().await?;
        let mut n = sqlx::query("delete from oauth_code where expires_at < now() - interval '1 day'").execute(&mut *tx).await?.rows_affected();
        n += sqlx::query("delete from oauth_refresh where expires_at < now() or revoked_at < now() - interval '30 days'")
            .execute(&mut *tx)
            .await?
            .rows_affected();
        n += sqlx::query("delete from oauth_client where coalesce(last_used_at, created_at) < now() - make_interval(days => $1)")
            .bind(IDLE_CLIENT_DAYS)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256() {
        // RFC 7636 appendix B.
        assert!(pkce_matches("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk", "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
        assert!(!pkce_matches("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXX", "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
        assert!(!pkce_matches("short", "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
    }

    #[test]
    fn redirect_rules() {
        assert!(redirect_uri_allowed("https://claude.ai/api/mcp/auth_callback"));
        assert!(redirect_uri_allowed("http://localhost:33418/callback"));
        assert!(redirect_uri_allowed("http://127.0.0.1/cb"));
        assert!(redirect_uri_allowed("http://[::1]:8080/cb"));
        assert!(redirect_uri_allowed("com.example.app:/oauth"));
        assert!(!redirect_uri_allowed("http://evil.example/cb"));
        assert!(!redirect_uri_allowed("javascript:alert(1)"));
        assert!(!redirect_uri_allowed("https://ok.example/cb#frag"));
        assert!(!redirect_uri_allowed("https:"));
        let reg = vec!["http://localhost:3000/callback".to_string(), "https://app.example/cb".to_string()];
        assert!(redirect_matches(&reg, "http://localhost:51234/callback"), "loopback ports may vary");
        assert!(!redirect_matches(&reg, "http://localhost:51234/other"));
        assert!(redirect_matches(&reg, "https://app.example/cb"));
        assert!(!redirect_matches(&reg, "https://app.example/cb2"));
    }
}
