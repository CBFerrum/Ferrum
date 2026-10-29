//! Two-factor sign-in with an authenticator app: TOTP (RFC 6238; HMAC-SHA1, 30-second steps,
//! six digits), the codes every authenticator app shows. The secret is sealed with the server key;
//! a code is accepted within one step either side of now and only once (its step is remembered);
//! ten single-use recovery codes, kept as hashes, stand in for a lost device. Failed codes count
//! against the same lockout as failed passwords. A workspace can require it of everyone
//! (`require_two_factor`); an administrator resets it for someone who lost their device.

use crate::auth::Session;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::{Kernel, audit};
use data_encoding::BASE32_NOPAD;
use hmac::{Hmac, Mac};
use rand::Rng;
use serde_json::{Value, json};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const STEP_SECONDS: u64 = 30;
const RECOVERY_CODES: usize = 10;

/// The six-digit code for a time step.
fn code_at(secret: &[u8], step: u64) -> u32 {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC takes any key length");
    mac.update(&step.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let at = (h[19] & 0x0f) as usize;
    u32::from_be_bytes([h[at] & 0x7f, h[at + 1], h[at + 2], h[at + 3]]) % 1_000_000
}

/// The time step (now, or one either side) whose code this is, if any.
fn matching_step(secret: &[u8], code: &str, now: u64) -> Option<u64> {
    let digits: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let wanted: u32 = digits.parse().ok()?;
    let t = now / STEP_SECONDS;
    [t.saturating_sub(1), t, t + 1].into_iter().find(|s| code_at(secret, *s) == wanted)
}

/// The code an authenticator app shows at `unix_time` for a secret as [`Kernel::begin_two_factor`]
/// gives it (tools and tests).
pub fn code(secret_b32: &str, unix_time: u64) -> Option<String> {
    let secret = BASE32_NOPAD.decode(secret_b32.as_bytes()).ok()?;
    Some(format!("{:06}", code_at(&secret, unix_time / STEP_SECONDS)))
}

fn now_seconds() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn sealed_label(tenant: Uuid, user: Uuid) -> String {
    format!("totp|{tenant}|{user}")
}

/// A recovery code as stored: its normalized text's SHA-256.
fn recovery_hash(code: &str) -> String {
    let norm: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase();
    hex::encode(Sha256::digest(norm.as_bytes()))
}

/// Ten fresh recovery codes (`abcde-fghij`, 50 random bits each).
fn new_recovery_codes() -> Vec<String> {
    let alphabet = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::rng();
    (0..RECOVERY_CODES)
        .map(|_| {
            let mut s: String = (0..10).map(|_| alphabet[rng.random_range(0..alphabet.len())] as char).collect();
            s.insert(5, '-');
            s
        })
        .collect()
}

/// An `otpauth://` link (what the QR code holds) for an authenticator app.
fn otpauth_uri(issuer: &str, account: &str, secret_b32: &str) -> String {
    let enc = |s: &str| {
        s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>()
    };
    format!("otpauth://totp/{}:{}?secret={secret_b32}&issuer={}&algorithm=SHA1&digits=6&period=30", enc(issuer), enc(account), enc(issuer))
}

fn qr_svg(data: &str) -> Result<String> {
    let code = qrcode::QrCode::new(data.as_bytes()).map_err(|e| Error::Internal(format!("QR code: {e}")))?;
    Ok(code.render::<qrcode::render::svg::Color>().min_dimensions(200, 200).quiet_zone(true).build())
}

impl Kernel {
    fn totp_key(&self) -> Result<&crate::secret::SecretKey> {
        self.settings.secret_key.as_ref().ok_or_else(|| Error::Unavailable("two-factor sign-in needs the server's secret key (FERRUM_SECRET_KEY)".into()))
    }

    fn open_secret(&self, tenant: Uuid, user: Uuid, sealed: &str) -> Result<Vec<u8>> {
        let b32 = self.totp_key()?.open(&sealed_label(tenant, user), sealed)?;
        BASE32_NOPAD.decode(b32.as_bytes()).map_err(|_| Error::Internal("corrupt two-factor secret".into()))
    }

    /// Whether the caller has two-factor sign-in, is setting it up, and how many recovery codes
    /// are left; and whether the workspace requires it.
    pub async fn two_factor_status(&self, ctx: &Ctx) -> Result<Value> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let (enabled, pending): (bool, bool) = sqlx::query_as(
            "select totp_enabled_at is not null, totp_secret is not null and totp_enabled_at is null from app_user where tenant_id = $1 and id = $2",
        )
        .bind(ctx.tenant)
        .bind(ctx.user)
        .fetch_one(&mut *tx)
        .await?;
        let left: i64 = sqlx::query_scalar("select count(*) from user_recovery_code where tenant_id = $1 and user_id = $2 and used_at is null")
            .bind(ctx.tenant)
            .bind(ctx.user)
            .fetch_one(&mut *tx)
            .await?;
        let required: bool = sqlx::query_scalar("select coalesce(settings->'require_two_factor' = 'true'::jsonb, false) from tenant where id = $1")
            .bind(ctx.tenant)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(json!({"enabled": enabled, "pending": pending, "recovery_codes_left": left, "required": required}))
    }

    /// Starts setting up two-factor sign-in: a secret for the caller's authenticator app, as text,
    /// an `otpauth://` link and a QR code (SVG). It takes effect once a code confirms it; until then
    /// asking again gives the same secret (a page shown twice must not show two).
    pub async fn begin_two_factor(&self, ctx: &Ctx) -> Result<Value> {
        let key = self.totp_key()?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let (email, enabled, pending, workspace): (String, bool, Option<String>, String) = sqlx::query_as(
            "select u.email::text, u.totp_enabled_at is not null, u.totp_secret, t.name from app_user u join tenant t on t.id = u.tenant_id \
             where u.tenant_id = $1 and u.id = $2 for update of u",
        )
        .bind(ctx.tenant)
        .bind(ctx.user)
        .fetch_one(&mut *tx)
        .await?;
        if enabled {
            return Err(Error::invalid("two-factor sign-in is already on; turn it off first to change the device"));
        }
        let label = sealed_label(ctx.tenant, ctx.user);
        let secret = match pending.map(|sealed| key.open(&label, &sealed)) {
            Some(Ok(secret)) => secret,
            _ => {
                let mut raw = [0u8; 20];
                rand::rng().fill(&mut raw);
                let secret = BASE32_NOPAD.encode(&raw);
                sqlx::query("update app_user set totp_secret = $3, totp_last_step = null, updated_at = now() where tenant_id = $1 and id = $2")
                    .bind(ctx.tenant)
                    .bind(ctx.user)
                    .bind(key.seal(&label, &secret)?)
                    .execute(&mut *tx)
                    .await?;
                secret
            }
        };
        tx.commit().await?;
        let uri = otpauth_uri(&format!("Ferrum {workspace}"), &email, &secret);
        Ok(json!({"secret": secret, "uri": uri, "qr_svg": qr_svg(&uri)?}))
    }

    /// Turns two-factor sign-in on once `code` comes from the new secret; returns the recovery codes
    /// (shown once).
    pub async fn enable_two_factor(&self, ctx: &Ctx, code: &str) -> Result<Vec<String>> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let (sealed, enabled): (Option<String>, bool) =
            sqlx::query_as("select totp_secret, totp_enabled_at is not null from app_user where tenant_id = $1 and id = $2 for update")
                .bind(ctx.tenant)
                .bind(ctx.user)
                .fetch_one(&mut *tx)
                .await?;
        if enabled {
            return Err(Error::invalid("two-factor sign-in is already on"));
        }
        let sealed = sealed.ok_or_else(|| Error::invalid("start the setup first"))?;
        let secret = self.open_secret(ctx.tenant, ctx.user, &sealed)?;
        let step = matching_step(&secret, code, now_seconds()).ok_or_else(|| Error::field("code", "not the code your app shows now"))?;
        sqlx::query("update app_user set totp_enabled_at = now(), totp_last_step = $3, updated_at = now() where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(ctx.user)
            .bind(step as i64)
            .execute(&mut *tx)
            .await?;
        let codes = self.replace_recovery_codes(&mut tx, ctx.tenant, ctx.user).await?;
        audit::record(&mut tx, ctx, "User", ctx.user, "enable_two_factor", None, json!({})).await?;
        tx.commit().await?;
        Ok(codes)
    }

    async fn replace_recovery_codes(&self, tx: &mut crate::db::Tx, tenant: Uuid, user: Uuid) -> Result<Vec<String>> {
        sqlx::query("delete from user_recovery_code where tenant_id = $1 and user_id = $2").bind(tenant).bind(user).execute(&mut **tx).await?;
        let codes = new_recovery_codes();
        let hashes: Vec<String> = codes.iter().map(|c| recovery_hash(c)).collect();
        sqlx::query("insert into user_recovery_code (tenant_id, user_id, code_hash) select $1, $2, unnest($3::text[])")
            .bind(tenant)
            .bind(user)
            .bind(&hashes)
            .execute(&mut **tx)
            .await?;
        Ok(codes)
    }

    /// New recovery codes in place of the old ones, confirmed with a code from the app.
    pub async fn new_recovery_codes(&self, ctx: &Ctx, code: &str) -> Result<Vec<String>> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let (sealed, last): (Option<String>, Option<i64>) =
            sqlx::query_as("select totp_secret, totp_last_step from app_user where tenant_id = $1 and id = $2 and totp_enabled_at is not null for update")
                .bind(ctx.tenant)
                .bind(ctx.user)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| Error::invalid("two-factor sign-in is off"))?;
        let secret = self.open_secret(ctx.tenant, ctx.user, &sealed.unwrap_or_default())?;
        let step = matching_step(&secret, code, now_seconds())
            .filter(|s| last.is_none_or(|l| *s as i64 > l))
            .ok_or_else(|| Error::field("code", "not the code your app shows now"))?;
        sqlx::query("update app_user set totp_last_step = $3 where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(ctx.user)
            .bind(step as i64)
            .execute(&mut *tx)
            .await?;
        let codes = self.replace_recovery_codes(&mut tx, ctx.tenant, ctx.user).await?;
        tx.commit().await?;
        Ok(codes)
    }

    /// Turns the caller's two-factor sign-in off, confirmed with their password or a code from the
    /// app (someone who signs in through single sign-on has no password).
    pub async fn disable_two_factor(&self, ctx: &Ctx, password: Option<&str>, code: Option<&str>) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let (hash, sealed, last): (Option<String>, Option<String>, Option<i64>) = sqlx::query_as(
            "select password_hash, totp_secret, totp_last_step from app_user where tenant_id = $1 and id = $2 and totp_enabled_at is not null for update",
        )
        .bind(ctx.tenant)
        .bind(ctx.user)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::invalid("two-factor sign-in is off"))?;
        let confirmed = match (password, code) {
            (Some(p), _) if !p.is_empty() => crate::auth::verify_password(p.to_string(), hash.unwrap_or_default()).await,
            (_, Some(c)) => {
                let secret = self.open_secret(ctx.tenant, ctx.user, &sealed.unwrap_or_default())?;
                matching_step(&secret, c, now_seconds()).is_some_and(|s| last.is_none_or(|l| s as i64 > l))
            }
            _ => false,
        };
        if !confirmed {
            return Err(match password {
                Some(p) if !p.is_empty() => Error::field("password", "is not your current password"),
                _ => Error::field("code", "not the code your app shows now"),
            });
        }
        self.clear_two_factor(&mut tx, ctx.tenant, ctx.user).await?;
        audit::record(&mut tx, ctx, "User", ctx.user, "disable_two_factor", None, json!({})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// An administrator turns someone's two-factor sign-in off (a lost device), lifts a lockout and
    /// signs them out everywhere; they set it up again at their next sign-in if the workspace
    /// requires it.
    pub async fn reset_two_factor(&self, ctx: &Ctx, user: Uuid) -> Result<()> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("only administrators can reset someone's two-factor sign-in".into()));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        self.clear_two_factor(&mut tx, ctx.tenant, user).await?;
        // The lost device may have gone with a signed-in session: end them all (and lift a lockout
        // from wrong codes).
        sqlx::query("update app_user set token_version = token_version + 1, failed_logins = 0, locked_until = null where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        crate::auth::notify_users(&mut tx, ctx.tenant, Some(user)).await?;
        audit::record(&mut tx, ctx, "User", user, "reset_two_factor", None, json!({})).await?;
        tx.commit().await?;
        self.invalidate_user(ctx.tenant, user);
        Ok(())
    }

    async fn clear_two_factor(&self, tx: &mut crate::db::Tx, tenant: Uuid, user: Uuid) -> Result<()> {
        let done = sqlx::query(
            "update app_user set totp_secret = null, totp_enabled_at = null, totp_last_step = null, updated_at = now() where tenant_id = $1 and id = $2",
        )
        .bind(tenant)
        .bind(user)
        .execute(&mut **tx)
        .await?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound("user".into()));
        }
        sqlx::query("delete from user_recovery_code where tenant_id = $1 and user_id = $2").bind(tenant).bind(user).execute(&mut **tx).await?;
        Ok(())
    }

    /// The second step of signing in: a code from the app (each accepted once) or an unused recovery
    /// code. A wrong one counts against the lockout like a wrong password; a right one completes the
    /// sign-in. `token_version` is the one the first step saw (a password change since voids it).
    pub async fn second_factor(&self, tenant: Uuid, user: Uuid, token_version: i32, code: &str) -> Result<Session> {
        let again = || Error::Unauthorized("sign in again".into());
        let mut tx = self.db.system_tx().await?;
        #[derive(sqlx::FromRow)]
        struct U {
            totp_secret: Option<String>,
            enabled: bool,
            last_step: Option<i64>,
            token_version: i32,
            active: bool,
            locked: bool,
        }
        let u: U = sqlx::query_as(
            "select u.totp_secret, u.totp_enabled_at is not null as enabled, u.totp_last_step as last_step, u.token_version, \
               u.is_active and t.status = 'active' as active, coalesce(u.locked_until > now(), false) as locked \
             from app_user u join tenant t on t.id = u.tenant_id where u.tenant_id = $1 and u.id = $2 for update of u",
        )
        .bind(tenant)
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(again)?;
        if !u.active || !u.enabled || u.token_version != token_version {
            return Err(again());
        }
        if u.locked {
            return Err(Error::TooManyRequests(format!("too many failed attempts; try again in {} minutes", crate::auth::LOCK_MINUTES)));
        }
        let secret = self.open_secret(tenant, user, u.totp_secret.as_deref().unwrap_or_default())?;
        let step = matching_step(&secret, code, now_seconds()).filter(|s| u.last_step.is_none_or(|l| *s as i64 > l));
        let ok = match step {
            Some(s) => {
                sqlx::query("update app_user set totp_last_step = $3 where tenant_id = $1 and id = $2")
                    .bind(tenant)
                    .bind(user)
                    .bind(s as i64)
                    .execute(&mut *tx)
                    .await?;
                true
            }
            None => {
                sqlx::query("update user_recovery_code set used_at = now() where tenant_id = $1 and user_id = $2 and code_hash = $3 and used_at is null")
                    .bind(tenant)
                    .bind(user)
                    .bind(recovery_hash(code))
                    .execute(&mut *tx)
                    .await?
                    .rows_affected()
                    == 1
            }
        };
        if !ok {
            crate::auth::count_failure(&mut tx, tenant, user).await?;
            tx.commit().await?;
            return Err(Error::Unauthorized("that code is not right; use the one your app shows now, or a recovery code".into()));
        }
        sqlx::query("update app_user set failed_logins = 0, locked_until = null, last_login_at = now() where tenant_id = $1 and id = $2")
            .bind(tenant)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Session { tenant, user, token_version })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_follow_rfc_6238() {
        // RFC 6238 appendix B, SHA-1 seed "12345678901234567890", eight digits → the last six.
        let seed = b"12345678901234567890";
        assert_eq!(code_at(seed, 59 / 30), 287_082);
        assert_eq!(code_at(seed, 1_111_111_109 / 30), 81_804);
        assert_eq!(code_at(seed, 1_234_567_890 / 30), 5_924);
        assert_eq!(matching_step(seed, "287 082", 59), Some(1));
        assert_eq!(matching_step(seed, "287082", 59 + 30), Some(1), "one step late still counts");
        assert_eq!(matching_step(seed, "287082", 59 + 90), None, "three steps late does not");
        assert_eq!(matching_step(seed, "28708", 59), None);
    }

    #[test]
    fn recovery_codes_are_compared_loosely_and_stored_hashed() {
        let codes = new_recovery_codes();
        assert_eq!(codes.len(), 10);
        assert!(codes.iter().all(|c| c.len() == 11 && c.as_bytes()[5] == b'-'));
        assert_eq!(recovery_hash("ABCDE-fghij"), recovery_hash(" abcde fghij "));
        assert!(
            otpauth_uri("Ferrum Acme Co", "pat@acme.test", "JBSWY3DPEHPK3PXP")
                .starts_with("otpauth://totp/Ferrum%20Acme%20Co:pat%40acme.test?secret=JBSWY3DPEHPK3PXP")
        );
    }
}
