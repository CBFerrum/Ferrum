//! Authentication (passwords, API keys, lockout) and the cached per-user request context.

use crate::Kernel;
use crate::ctx::{Ctx, PermSet};
use crate::db::Tx;
use crate::error::{Error, Result};
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use chrono::{DateTime, Utc};
use ferrum_meta::PermissionDef;
use rand::Rng;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

const MAX_FAILED_LOGINS: i32 = 5;
pub(crate) const LOCK_MINUTES: i32 = 15;
/// An emailed sign-in code works for this long, takes this many wrong tries, and a person is sent
/// at most this many in an hour.
const CODE_MINUTES: i32 = 10;
const CODE_TRIES: i32 = 5;
const CODES_PER_HOUR: i64 = 5;

/// What is kept of a sign-in code: salted with its row's id, so equal codes hash apart.
pub(crate) fn code_hash(id: Uuid, code: &str) -> String {
    hex::encode(Sha256::digest(format!("{id}:{code}").as_bytes()))
}

pub fn validate_password(p: &str) -> Result<()> {
    if p.chars().count() < 10 {
        return Err(Error::field("password", "must be at least 10 characters"));
    }
    if p.len() > 256 {
        return Err(Error::field("password", "is too long"));
    }
    let classes = [
        p.chars().any(|c| c.is_lowercase()),
        p.chars().any(|c| c.is_uppercase()),
        p.chars().any(|c| c.is_ascii_digit()),
        p.chars().any(|c| !c.is_alphanumeric()),
    ];
    if classes.iter().filter(|x| **x).count() < 3 {
        return Err(Error::field("password", "needs at least three of: lowercase, uppercase, digit, symbol"));
    }
    Ok(())
}

/// Argon2id hashing on the blocking pool (it is deliberately slow).
pub const USERS_CHANNEL: &str = "ferrum_users";

/// Tells every replica, when `tx` commits, to drop its cached copy of a user (or of all the
/// tenant's users when `user` is `None`).
/// Ends everything someone is signed in with, on every replica: sessions and refresh tokens. API
/// keys are not sign-ins, and stay.
pub(crate) async fn end_sessions(tx: &mut Tx, tenant: Uuid, user: Uuid) -> Result<()> {
    sqlx::query("update app_user set token_version = token_version + 1 where tenant_id = $1 and id = $2").bind(tenant).bind(user).execute(&mut **tx).await?;
    sqlx::query("update user_session set revoked_at = now() where tenant_id = $1 and user_id = $2 and revoked_at is null")
        .bind(tenant)
        .bind(user)
        .execute(&mut **tx)
        .await?;
    sqlx::query("update oauth_refresh set revoked_at = now() where tenant_id = $1 and user_id = $2 and revoked_at is null")
        .bind(tenant)
        .bind(user)
        .execute(&mut **tx)
        .await?;
    notify_users(tx, tenant, Some(user)).await
}

/// Why a role could not be given: one it may not be held with (the database's segregation of
/// duties, FERRUM-IDENTITY-014), or no such role.
fn refused_role(e: sqlx::Error, role: &str) -> Error {
    match &e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("P0001") => Error::field("roles", db.message().trim_start_matches("ferrum: ")),
        _ => Error::field("roles", format!("unknown role {role}")),
    }
}

pub(crate) async fn notify_users(tx: &mut Tx, tenant: Uuid, user: Option<Uuid>) -> Result<()> {
    let payload = match user {
        Some(u) => format!("{tenant}:{u}"),
        None => tenant.to_string(),
    };
    sqlx::query("select pg_notify($1, $2)").bind(USERS_CHANNEL).bind(payload).execute(&mut **tx).await?;
    Ok(())
}

pub async fn hash_password(p: String) -> Result<String> {
    tokio::task::spawn_blocking(move || {
        let mut bytes = [0u8; 16];
        rand::rng().fill(&mut bytes);
        let salt = SaltString::encode_b64(&bytes).map_err(Error::internal)?;
        Argon2::default().hash_password(p.as_bytes(), &salt).map(|h| h.to_string()).map_err(Error::internal)
    })
    .await
    .map_err(Error::internal)?
}

pub async fn verify_password(p: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || PasswordHash::new(&hash).map(|h| Argon2::default().verify_password(p.as_bytes(), &h).is_ok()).unwrap_or(false))
        .await
        .unwrap_or(false)
}

/// A fixed hash verified for unknown users so response time does not reveal which emails exist.
pub(crate) const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$ZmVycnVtLWR1bW15LWhhc2gtZm9yLXRpbWluZw";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub tenant: Uuid,
    pub user: Uuid,
    pub token_version: i32,
}

#[derive(Debug, Clone)]
pub struct CtxCore {
    pub read_only: bool,
    pub email: String,
    pub full_name: String,
    pub roles: Vec<String>,
    pub is_admin: bool,
    pub token_version: i32,
    pub scopes: BTreeMap<String, Vec<Uuid>>,
    pub default_company: Option<Uuid>,
    pub fy_start_month: u32,
    pub perms: Arc<PermSet>,
    /// When the first of the roles given until a moment ends: a cached context lapses then.
    pub until: Option<DateTime<Utc>>,
    /// A portal person or a guest.
    pub outside: bool,
}

/// A records key's scope (`records:Customer=read+write;SalesInvoice=read`): the entities it may
/// reach and what it may do with each, or `None` when the scope is not one. Within them the key
/// is its owner, narrowed as an app's screen is ([`PermSet::limited_to`]): it never has more than
/// the person who made it, never administrator power, and no field group of theirs.
pub fn records_scope(scope: &str) -> Option<BTreeMap<String, BTreeSet<String>>> {
    let mut out = BTreeMap::new();
    for part in scope.strip_prefix("records:")?.split(';').filter(|p| !p.trim().is_empty()) {
        let (entity, verbs) = part.split_once('=')?;
        let verbs: BTreeSet<String> = verbs.split('+').map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect();
        if entity.trim().is_empty() || verbs.is_empty() {
            return None;
        }
        out.entry(entity.trim().to_string()).or_insert_with(BTreeSet::new).extend(verbs);
    }
    (!out.is_empty()).then_some(out)
}

/// A records scope as it is kept: each entity of this workspace once, its verbs known and in order.
fn canonical_records_scope(ctx: &Ctx, scope: &str) -> Result<String> {
    let bad = |m: String| Error::field("scope", m);
    let grants = records_scope(scope).ok_or_else(|| bad("write a records key's scope as records:Entity=read+write;Other=read".into()))?;
    let known: BTreeSet<&str> = crate::ctx::action_verbs().collect();
    for (entity, verbs) in &grants {
        if ctx.schema.entity(entity).is_none() {
            return Err(bad(format!("{entity} is not an entity of this workspace")));
        }
        if let Some(v) = verbs.iter().find(|v| !known.contains(v.as_str())) {
            return Err(bad(format!("{v} is not something a key may do: {}", known.iter().copied().collect::<Vec<_>>().join(", "))));
        }
    }
    let parts: Vec<String> = grants.iter().map(|(entity, verbs)| format!("{entity}={}", verbs.iter().cloned().collect::<Vec<_>>().join("+"))).collect();
    Ok(format!("records:{}", parts.join(";")))
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Counts a failed sign-in attempt (a wrong password or second-step code); enough in a row lock
/// the account for a while.
pub(crate) async fn count_failure(tx: &mut Tx, tenant: Uuid, user: Uuid) -> Result<()> {
    sqlx::query(
        "update app_user set failed_logins = failed_logins + 1, \
         locked_until = case when failed_logins + 1 >= $3 then now() + make_interval(mins => $4) else locked_until end \
         where tenant_id = $1 and id = $2",
    )
    .bind(tenant)
    .bind(user)
    .bind(MAX_FAILED_LOGINS)
    .bind(LOCK_MINUTES)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Where the password step of signing in leads.
#[derive(Debug, Clone)]
pub enum SignIn {
    /// Signed in.
    Session(Session),
    /// The password was right; a code from the authenticator app or a recovery code completes the
    /// sign-in ([`Kernel::second_factor`]).
    SecondFactor(Session),
    /// Signed in, but the workspace requires two-factor sign-in, which this person has yet to set
    /// up: the session may do only that.
    SetupRequired(Session),
}

impl Kernel {
    /// Signs in with a password alone (tools, tests); an account with two-factor sign-in, or one
    /// that must set it up, signs in through [`Kernel::sign_in`].
    pub async fn login(&self, tenant_slug: &str, email: &str, password: &str) -> Result<Session> {
        match self.sign_in(tenant_slug, email, password).await? {
            SignIn::Session(s) => Ok(s),
            _ => Err(Error::Unauthorized("this account signs in with a second step; use the sign-in page".into())),
        }
    }

    /// The password step of signing in.
    pub async fn sign_in(&self, tenant_slug: &str, email: &str, password: &str) -> Result<SignIn> {
        let denied = || Error::Unauthorized("invalid tenant, email or password".into());
        let email = email.trim().to_lowercase();
        let mut tx = self.db.system_tx().await?;
        #[derive(sqlx::FromRow)]
        struct U {
            tenant_id: Uuid,
            id: Uuid,
            password_hash: Option<String>,
            is_active: bool,
            token_version: i32,
            locked: bool,
            tenant_active: bool,
            two_factor: bool,
            two_factor_required: bool,
            sso_required: bool,
            is_admin: bool,
        }
        let user: Option<U> = sqlx::query_as(
            "select u.tenant_id, u.id, u.password_hash, u.is_active, u.token_version, \
             coalesce(u.locked_until > now(), false) as locked, t.status = 'active' as tenant_active, \
             u.totp_enabled_at is not null as two_factor, coalesce(t.settings->'require_two_factor' = 'true'::jsonb, false) as two_factor_required, \
             coalesce(t.settings->'require_sso' = 'true'::jsonb, false) as sso_required, u.is_admin \
             from app_user u join tenant t on t.id = u.tenant_id where t.slug = $1 and u.email = $2",
        )
        .bind(tenant_slug.trim().to_lowercase())
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(u) = user else {
            tx.commit().await?;
            let _ = verify_password(password.to_string(), DUMMY_HASH.to_string()).await;
            return Err(denied());
        };
        if u.locked {
            tx.commit().await?;
            return Err(Error::TooManyRequests(format!("too many failed attempts; try again in {LOCK_MINUTES} minutes")));
        }
        let ok = match &u.password_hash {
            Some(h) => verify_password(password.to_string(), h.clone()).await,
            None => false,
        };
        if !ok || !u.is_active || !u.tenant_active {
            count_failure(&mut tx, u.tenant_id, u.id).await?;
            tx.commit().await?;
            return Err(denied());
        }
        if u.sso_required && !u.is_admin {
            // Administrators keep their passwords, to get in when the provider is down.
            tx.commit().await?;
            return Err(Error::Forbidden("this workspace signs in through single sign-on; use its button on the sign-in page".into()));
        }
        let session = Session { tenant: u.tenant_id, user: u.id, token_version: u.token_version };
        if u.two_factor {
            // The failure count stands until the second step succeeds, so wrong codes add up to a
            // lockout however often the password is entered again.
            tx.commit().await?;
            return Ok(SignIn::SecondFactor(session));
        }
        sqlx::query("update app_user set failed_logins = 0, locked_until = null, last_login_at = now() where tenant_id = $1 and id = $2")
            .bind(u.tenant_id)
            .bind(u.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(if u.two_factor_required { SignIn::SetupRequired(session) } else { SignIn::Session(session) })
    }

    /// Emails a portal person of the workspace a code to sign in with (docs/plan/apps-ui.md §9.1).
    /// The answer says nothing about the address: nobody by that address, someone who is not a
    /// portal person, a disabled one or one sent too many codes lately all get no mail and `Ok`.
    pub async fn send_sign_in_code(&self, tenant_slug: &str, email: &str) -> Result<()> {
        if self.mailer.is_none() {
            return Err(Error::Unavailable("this server sends no email, so it cannot send sign-in codes".into()));
        }
        let email = email.trim().to_lowercase();
        if email.len() > 254 || email.split('@').count() != 2 || email.starts_with('@') || email.ends_with('@') {
            return Err(Error::field("email", "is not an email address"));
        }
        let mut tx = self.db.system_tx().await?;
        // Codes past their day, of anyone: a few at a time, as people ask for new ones.
        sqlx::query("delete from sign_in_code where id in (select id from sign_in_code where expires_at < now() - interval '1 day' limit 100)")
            .execute(&mut *tx)
            .await?;
        let person: Option<(Uuid, Uuid, String)> = sqlx::query_as(
            "select u.tenant_id, u.id, t.name from app_user u join tenant t on t.id = u.tenant_id \
             where t.slug = $1 and u.email = $2 and u.kind = 'portal' and u.is_active and t.status = 'active'",
        )
        .bind(tenant_slug.trim().to_lowercase())
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((tenant, user, workspace)) = person else {
            tx.commit().await?;
            return Ok(());
        };
        let lately: i64 =
            sqlx::query_scalar("select count(*) from sign_in_code where tenant_id = $1 and user_id = $2 and created_at > now() - interval '1 hour'")
                .bind(tenant)
                .bind(user)
                .fetch_one(&mut *tx)
                .await?;
        if lately >= CODES_PER_HOUR {
            tx.commit().await?;
            return Ok(());
        }
        // Only the newest code works.
        sqlx::query("update sign_in_code set used_at = now() where tenant_id = $1 and user_id = $2 and used_at is null")
            .bind(tenant)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        let id = Uuid::new_v4();
        let code = format!("{:06}", rand::rng().random_range(0..1_000_000u32));
        sqlx::query("insert into sign_in_code (id, tenant_id, user_id, code_hash, expires_at) values ($1, $2, $3, $4, now() + make_interval(mins => $5))")
            .bind(id)
            .bind(tenant)
            .bind(user)
            .bind(code_hash(id, &code))
            .bind(CODE_MINUTES)
            .execute(&mut *tx)
            .await?;
        let mut name = String::new();
        crate::coedit::esc(&workspace, &mut name);
        let html = format!(
            "<p>Your code to sign in to {name}:</p><p style=\"font-size:28px;font-weight:700;letter-spacing:4px\">{code}</p>\
             <p>It works once, for {CODE_MINUTES} minutes. If you did not ask for it, you can ignore this email.</p>"
        );
        self.queue_mail(&mut tx, tenant, &email, &format!("{code} is your code for {workspace}"), &html).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Signs a portal person in with the code [`Kernel::send_sign_in_code`] sent last. A code takes
    /// five wrong tries; someone with an authenticator app still gives its code next.
    pub async fn sign_in_with_code(&self, tenant_slug: &str, email: &str, code: &str) -> Result<SignIn> {
        let denied = || Error::Unauthorized("that code is wrong or has expired; ask for a new one".into());
        let email = email.trim().to_lowercase();
        let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
            return Err(denied());
        }
        let mut tx = self.db.system_tx().await?;
        let person: Option<(Uuid, Uuid, i32, bool, bool)> = sqlx::query_as(
            "select u.tenant_id, u.id, u.token_version, coalesce(u.locked_until > now(), false), u.totp_enabled_at is not null \
             from app_user u join tenant t on t.id = u.tenant_id \
             where t.slug = $1 and u.email = $2 and u.kind = 'portal' and u.is_active and t.status = 'active'",
        )
        .bind(tenant_slug.trim().to_lowercase())
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((tenant, user, token_version, locked, two_factor)) = person else {
            tx.commit().await?;
            return Err(denied());
        };
        if locked {
            tx.commit().await?;
            return Err(Error::TooManyRequests(format!("too many failed attempts; try again in {LOCK_MINUTES} minutes")));
        }
        // Locked, so two tries at once cannot both use it.
        let live: Option<(Uuid, String, i32)> = sqlx::query_as(
            "select id, code_hash, attempts from sign_in_code where tenant_id = $1 and user_id = $2 and used_at is null and expires_at > now() \
             order by created_at desc limit 1 for update",
        )
        .bind(tenant)
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((id, hash, attempts)) = live else {
            tx.commit().await?;
            return Err(denied());
        };
        // Hashes compared: how long that takes says nothing about the code.
        if code_hash(id, &code) != hash {
            sqlx::query("update sign_in_code set attempts = attempts + 1, used_at = case when attempts + 1 >= $2 then now() end where id = $1")
                .bind(id)
                .bind(CODE_TRIES)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Err(if attempts + 1 >= CODE_TRIES {
                Error::Unauthorized("that code is wrong, and has now stopped working; ask for a new one".into())
            } else {
                denied()
            });
        }
        sqlx::query("update sign_in_code set used_at = now() where id = $1").bind(id).execute(&mut *tx).await?;
        let session = Session { tenant, user, token_version };
        if two_factor {
            tx.commit().await?;
            return Ok(SignIn::SecondFactor(session));
        }
        sqlx::query("update app_user set failed_logins = 0, locked_until = null, last_login_at = now() where tenant_id = $1 and id = $2")
            .bind(tenant)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(SignIn::Session(session))
    }

    /// API keys look like `fk_<prefix>_<secret>`; only a SHA-256 of the secret is stored. A key
    /// with a `scope` works only there: `dav`, the app password of calendar and contacts apps;
    /// `scim`, an administrator's key for an identity provider; or `records:…`, a key to the records
    /// API for the entities and actions it names ([`records_scope`]).
    pub async fn create_api_key(
        &self,
        ctx: &Ctx,
        user: Uuid,
        name: &str,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
        scope: Option<&str>,
    ) -> Result<(Uuid, String)> {
        if !ctx.is_admin && user != ctx.user {
            return Err(Error::Forbidden("you can only create API keys for yourself".into()));
        }
        let records = match scope {
            Some(s) if s.starts_with("records:") => Some(canonical_records_scope(ctx, s)?),
            _ => None,
        };
        if scope.is_some_and(|s| s != "dav" && s != "scim" && s != "support") && records.is_none() {
            return Err(Error::field("scope", "must be dav, scim, records:Entity=read+write;…, or none for a key to the whole API"));
        }
        let scope = records.as_deref().or(scope);
        if scope == Some("scim") && !ctx.is_admin {
            return Err(Error::Forbidden("only administrators can make a key for an identity provider".into()));
        }
        let (prefix, secret) = {
            let mut rng = rand::rng();
            let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
            let mut token = |n: usize| -> String { (0..n).map(|_| alphabet[rng.random_range(0..alphabet.len())] as char).collect() };
            (token(10), token(40))
        };
        let id = Uuid::now_v7();
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("insert into api_key (tenant_id, id, user_id, name, prefix, secret_hash, expires_at, scope) values ($1, $2, $3, $4, $5, $6, $7, $8)")
            .bind(ctx.tenant)
            .bind(id)
            .bind(user)
            .bind(name)
            .bind(&prefix)
            .bind(sha256_hex(&secret))
            .bind(expires_at)
            .bind(scope)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok((id, format!("fk_{prefix}_{secret}")))
    }

    /// The session an API key opens, and the key's scope (`None`: the whole API).
    pub async fn verify_api_key(&self, raw: &str) -> Result<(Session, Option<String>)> {
        let denied = || Error::Unauthorized("invalid API key".into());
        let rest = raw.strip_prefix("fk_").ok_or_else(denied)?;
        let (prefix, secret) = rest.split_once('_').ok_or_else(denied)?;
        let mut tx = self.db.system_tx().await?;
        let row: Option<(Uuid, Uuid, String, bool, i32, Option<String>)> = sqlx::query_as(
            "select k.tenant_id, k.user_id, k.secret_hash, \
             (k.revoked_at is null and (k.expires_at is null or k.expires_at > now()) and u.is_active and t.status = 'active'), u.token_version, k.scope \
             from api_key k join app_user u on u.tenant_id = k.tenant_id and u.id = k.user_id join tenant t on t.id = k.tenant_id \
             where k.prefix = $1",
        )
        .bind(prefix)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((tenant, user, hash, valid, tv, scope)) = row else {
            return Err(denied());
        };
        let computed = sha256_hex(secret);
        let equal = computed.len() == hash.len() && computed.bytes().zip(hash.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0;
        if !equal || !valid {
            return Err(denied());
        }
        sqlx::query("update api_key set last_used_at = now() where prefix = $1 and (last_used_at is null or last_used_at < now() - interval '1 minute')")
            .bind(prefix)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok((Session { tenant, user, token_version: tv }, scope))
    }

    pub async fn revoke_api_key(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("update api_key set revoked_at = now() where tenant_id = $1 and id = $2 and (user_id = $3 or $4)")
            .bind(ctx.tenant)
            .bind(id)
            .bind(ctx.user)
            .bind(ctx.is_admin)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        if n == 0 { Err(Error::NotFound("API key".into())) } else { Ok(()) }
    }

    async fn load_core(&self, tenant: Uuid, user: Uuid) -> Result<CtxCore> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let row: Option<(String, String, bool, bool, i32, serde_json::Value, String, bool)> = sqlx::query_as(
            "select u.email::text, u.full_name, u.is_admin, u.is_active and t.status = 'active', u.token_version, t.settings, u.kind, t.account_state in ('restricted', 'closed') \
             from app_user u join tenant t on t.id = u.tenant_id where u.tenant_id = $1 and u.id = $2",
        )
        .bind(tenant)
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((email, full_name, is_admin, active, token_version, settings, kind, read_only)) = row else {
            return Err(Error::Unauthorized("user no longer exists".into()));
        };
        if !active {
            return Err(Error::Unauthorized("user or tenant is disabled".into()));
        }
        // Portal people and guests reach the pages apps give them (docs/plan/apps-ui.md §9.1). Portal
        // people also hold the roles marked for the portal (docs/builder/decisions.md BD-02), which
        // reach only their own records; any other role given to them counts for nothing, as every
        // role does for a guest. Neither is ever an administrator.
        let outside = matches!(kind.as_str(), "portal" | "guest");
        let is_admin = is_admin && !outside;
        let roles: Vec<(String, Option<DateTime<Utc>>)> =
            match kind.as_str() {
                "guest" => vec![],
                "portal" => {
                    sqlx::query_as(
                        "select ur.role, ur.expires_at from user_role ur join role r on r.tenant_id = ur.tenant_id and r.name = ur.role \
                     where ur.tenant_id = $1 and ur.user_id = $2 and r.portal and (ur.expires_at is null or ur.expires_at > now()) order by ur.role",
                    )
                    .bind(tenant)
                    .bind(user)
                    .fetch_all(&mut *tx)
                    .await?
                }
                _ => sqlx::query_as(
                    "select role, expires_at from user_role where tenant_id = $1 and user_id = $2 and (expires_at is null or expires_at > now()) order by role",
                )
                .bind(tenant)
                .bind(user)
                .fetch_all(&mut *tx)
                .await?,
            };
        let until = roles.iter().filter_map(|r| r.1).min();
        let roles: Vec<String> = roles.into_iter().map(|r| r.0).collect();
        let scope_rows: Vec<(String, Uuid, bool)> = sqlx::query_as("select entity, value, is_default from user_scope where tenant_id = $1 and user_id = $2")
            .bind(tenant)
            .bind(user)
            .fetch_all(&mut *tx)
            .await?;
        let mut scopes: BTreeMap<String, Vec<Uuid>> = BTreeMap::new();
        let mut default_company = None;
        for (entity, value, is_default) in scope_rows {
            if entity == "Company" && (is_default || default_company.is_none()) {
                default_company = Some(value);
            }
            scopes.entry(entity).or_default().push(value);
        }
        if default_company.is_none() && !scopes.contains_key("Company") {
            // A tenant with a single company needs no choosing.
            let only: Vec<Uuid> = sqlx::query_scalar("select id from e_company where tenant_id = $1 limit 2").bind(tenant).fetch_all(&mut *tx).await?;
            if let [one] = only.as_slice() {
                default_company = Some(*one);
            }
        }
        let perms = PermSet::load(&mut tx, tenant, &roles, is_admin).await?;
        tx.commit().await?;
        let fy_start_month = settings.get("fiscal_year_start_month").and_then(|v| v.as_u64()).unwrap_or(1).clamp(1, 12) as u32;
        Ok(CtxCore {
            read_only,
            email,
            full_name,
            roles,
            is_admin,
            token_version,
            scopes,
            default_company,
            fy_start_month,
            perms: Arc::new(perms),
            until,
            outside,
        })
    }

    /// Builds the request context; `token_version` (from a session token) must match the user's
    /// current version, which is bumped on logout-all / password change / deactivation.
    pub async fn load_ctx(&self, tenant: Uuid, user: Uuid, token_version: Option<i32>, request_id: String) -> Result<Ctx> {
        let key = (tenant, user);
        let cached = self.users.get(&key).filter(|c| c.1.elapsed() < self.settings.ctx_ttl && c.0.until.is_none_or(|u| Utc::now() < u)).map(|c| c.0.clone());
        let core = match cached {
            Some(c) => c,
            None => {
                let c = Arc::new(self.load_core(tenant, user).await?);
                self.users.insert(key, (c.clone(), Instant::now()));
                c
            }
        };
        if let Some(tv) = token_version
            && tv != core.token_version
        {
            self.users.remove(&key);
            return Err(Error::Unauthorized("session expired; please sign in again".into()));
        }
        let schema = self.registry.schema(tenant).await?;
        Ok(Ctx {
            egress: None,
            read_only: core.read_only,
            tenant,
            user,
            email: core.email.clone(),
            full_name: core.full_name.clone(),
            roles: core.roles.clone(),
            is_admin: core.is_admin,
            is_system: false,
            outside: core.outside,
            request_id,
            scopes: core.scopes.clone(),
            default_company: core.default_company,
            fy_start_month: core.fy_start_month,
            schema,
            perms: core.perms.clone(),
            depth: 0,
        })
    }

    /// Every replica drops cached users when another replica changes them (roles, scopes,
    /// deactivation, passwords, sign-out everywhere, permissions), so changes apply at once.
    pub fn spawn_user_listener(self: &std::sync::Arc<Self>, mut shutdown: tokio::sync::watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        let k = self.clone();
        tokio::spawn(async move {
            loop {
                match sqlx::postgres::PgListener::connect_with(&k.db.pool).await {
                    Ok(mut listener) => match listener.listen(USERS_CHANNEL).await {
                        Ok(()) => {
                            // Anything may have changed while we were not listening.
                            k.users.clear();
                            loop {
                                tokio::select! {
                                    _ = shutdown.changed() => return,
                                    msg = listener.recv() => match msg {
                                        Ok(n) => match n.payload().split_once(':') {
                                            Some((t, u)) => {
                                                if let (Ok(t), Ok(u)) = (t.parse(), u.parse()) {
                                                    k.invalidate_user(t, u);
                                                }
                                            }
                                            None => match n.payload().parse() {
                                                Ok(t) => k.invalidate_tenant_users(t),
                                                Err(_) => k.users.clear(),
                                            },
                                        },
                                        Err(e) => {
                                            tracing::warn!("user listener lost its connection: {e}");
                                            break;
                                        }
                                    },
                                }
                            }
                        }
                        Err(e) => tracing::warn!("user listener failed to subscribe: {e}"),
                    },
                    Err(e) => tracing::warn!("user listener cannot connect: {e}"),
                }
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
            }
        })
    }

    /// The pairs of roles one person may not hold together (FERRUM-IDENTITY-014), each with whoever
    /// holds both, having held them before the pair was declared.
    pub async fn role_conflicts(&self, ctx: &Ctx) -> Result<Vec<serde_json::Value>> {
        Self::require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let holds = |role: &str| {
            format!(
                "exists (select 1 from user_role r where r.tenant_id = u.tenant_id and r.user_id = u.id and r.role = c.{role} \
                 and (r.expires_at is null or r.expires_at > now()))"
            )
        };
        let rows = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select jsonb_build_object('role_a', c.role_a, 'role_b', c.role_b, 'reason', c.reason, 'created_at', c.created_at, \
             'holders', coalesce((select jsonb_agg(jsonb_build_object('id', u.id, 'email', u.email, 'full_name', u.full_name) order by u.email) \
                                  from app_user u where u.tenant_id = c.tenant_id and {} and {}), '[]'::jsonb)) \
             from role_conflict c where c.tenant_id = $1 order by c.role_a, c.role_b",
            holds("role_a"),
            holds("role_b")
        )))
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Declares that one person may not hold both roles, and why. Grants already joining them stay,
    /// listed beside the pair ([`Self::role_conflicts`]); from now the database refuses any new one.
    pub async fn set_role_conflict(&self, ctx: &Ctx, a: &str, b: &str, reason: &str) -> Result<()> {
        Self::require_admin(ctx)?;
        let (a, b, reason) = (a.trim(), b.trim(), reason.trim());
        if a == b {
            return Err(Error::field("role_b", "a role is never in conflict with itself"));
        }
        if reason.is_empty() || reason.chars().count() > 500 {
            return Err(Error::field("reason", "say why one person may not hold both (up to 500 characters)"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        // The database orders the pair, as its check does.
        sqlx::query(
            "insert into role_conflict (tenant_id, role_a, role_b, reason, created_by) values ($1, least($2::text, $3::text), greatest($2::text, $3::text), $4, $5) \
             on conflict (tenant_id, role_a, role_b) do update set reason = excluded.reason",
        )
        .bind(ctx.tenant)
        .bind(a)
        .bind(b)
        .bind(reason)
        .bind(ctx.user)
        .execute(&mut *tx)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.code().as_deref() == Some("23503") => Error::field("role_a", "both roles must exist"),
            _ => Error::from(e),
        })?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_role_conflict(&self, ctx: &Ctx, a: &str, b: &str) -> Result<()> {
        Self::require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("delete from role_conflict where tenant_id = $1 and role_a = least($2::text, $3::text) and role_b = greatest($2::text, $3::text)")
            .bind(ctx.tenant)
            .bind(a.trim())
            .bind(b.trim())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        if n == 0 { Err(Error::NotFound("role conflict".into())) } else { Ok(()) }
    }

    /// Hourly: grants whose end has passed go, so whatever reads who holds a role (apps' notices
    /// among them) stops finding the person. Their access ended at that moment ([`Self::load_ctx`]).
    pub(crate) async fn expire_roles(&self) -> Result<u64> {
        let mut tx = self.db.system_tx().await?;
        let ended: Vec<(Uuid, Uuid)> =
            sqlx::query_as("delete from user_role where expires_at <= now() returning tenant_id, user_id").fetch_all(&mut *tx).await?;
        for (tenant, user) in &ended {
            notify_users(&mut tx, *tenant, Some(*user)).await?;
        }
        tx.commit().await?;
        Ok(ended.len() as u64)
    }

    pub fn invalidate_user(&self, tenant: Uuid, user: Uuid) {
        self.users.remove(&(tenant, user));
    }

    pub fn invalidate_tenant_users(&self, tenant: Uuid) {
        self.users.retain(|k, _| k.0 != tenant);
    }

    fn require_admin(ctx: &Ctx) -> Result<()> {
        if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("administrator access required".into())) }
    }

    pub async fn create_user(&self, ctx: &Ctx, email: &str, full_name: &str, password: Option<&str>, roles: &[String], is_admin: bool) -> Result<Uuid> {
        Self::require_admin(ctx)?;
        let hash = match password {
            Some(p) => {
                validate_password(p)?;
                Some(hash_password(p.to_string()).await?)
            }
            None => None,
        };
        let id = Uuid::now_v7();
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("insert into app_user (tenant_id, id, email, full_name, password_hash, is_admin) values ($1, $2, $3, $4, $5, $6)")
            .bind(ctx.tenant)
            .bind(id)
            .bind(email.trim().to_lowercase())
            .bind(full_name.trim())
            .bind(hash)
            .bind(is_admin)
            .execute(&mut *tx)
            .await
            .map_err(|e| match Error::from(e) {
                Error::Conflict(_) => Error::field("email", "a user with this email already exists"),
                other => other,
            })?;
        for r in roles {
            sqlx::query("insert into user_role (tenant_id, user_id, role) values ($1, $2, $3)")
                .bind(ctx.tenant)
                .bind(id)
                .bind(r)
                .execute(&mut *tx)
                .await
                .map_err(|e| refused_role(e, r))?;
        }
        tx.commit().await?;
        Ok(id)
    }

    pub async fn set_user_roles(&self, ctx: &Ctx, user: Uuid, roles: &[String]) -> Result<()> {
        self.set_user_roles_until(ctx, user, roles, &BTreeMap::new()).await
    }

    /// Sets someone's roles, each held for good or, when `until` names it, until that moment.
    pub async fn set_user_roles_until(&self, ctx: &Ctx, user: Uuid, roles: &[String], until: &BTreeMap<String, DateTime<Utc>>) -> Result<()> {
        Self::require_admin(ctx)?;
        for (role, end) in until {
            if !roles.contains(role) {
                return Err(Error::field("until", format!("{role} is not among the roles given")));
            }
            if *end <= Utc::now() {
                return Err(Error::field("until", format!("{role} would end before it starts: give a moment still to come")));
            }
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        // A guest holds no role, a portal person only portal roles (BD-02): refused here, not left
        // to count for nothing.
        let kind: Option<String> =
            sqlx::query_scalar("select kind from app_user where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(user).fetch_optional(&mut *tx).await?;
        match kind.as_deref() {
            Some("guest") if !roles.is_empty() => return Err(Error::field("roles", "guests hold no roles")),
            Some("portal") => {
                let portal: Vec<String> = sqlx::query_scalar("select name from role where tenant_id = $1 and portal and name = any($2)")
                    .bind(ctx.tenant)
                    .bind(roles)
                    .fetch_all(&mut *tx)
                    .await?;
                if let Some(other) = roles.iter().find(|r| !portal.contains(r)) {
                    return Err(Error::field("roles", format!("portal people hold only portal roles, and {other} is not one")));
                }
            }
            _ => {}
        }
        sqlx::query("delete from user_role where tenant_id = $1 and user_id = $2").bind(ctx.tenant).bind(user).execute(&mut *tx).await?;
        for r in roles {
            sqlx::query("insert into user_role (tenant_id, user_id, role, expires_at) values ($1, $2, $3, $4)")
                .bind(ctx.tenant)
                .bind(user)
                .bind(r)
                .bind(until.get(r))
                .execute(&mut *tx)
                .await
                .map_err(|e| refused_role(e, r))?;
        }
        notify_users(&mut tx, ctx.tenant, Some(user)).await?;
        tx.commit().await?;
        self.invalidate_user(ctx.tenant, user);
        Ok(())
    }

    pub async fn set_user_scopes(&self, ctx: &Ctx, user: Uuid, entity: &str, ids: &[Uuid], default: Option<Uuid>) -> Result<()> {
        Self::require_admin(ctx)?;
        if ctx.schema.entity(entity).is_none() {
            return Err(Error::field("entity", "unknown entity"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("delete from user_scope where tenant_id = $1 and user_id = $2 and entity = $3")
            .bind(ctx.tenant)
            .bind(user)
            .bind(entity)
            .execute(&mut *tx)
            .await?;
        for id in ids {
            sqlx::query("insert into user_scope (tenant_id, user_id, entity, value, is_default) values ($1, $2, $3, $4, $5)")
                .bind(ctx.tenant)
                .bind(user)
                .bind(entity)
                .bind(id)
                .bind(Some(*id) == default)
                .execute(&mut *tx)
                .await?;
        }
        notify_users(&mut tx, ctx.tenant, Some(user)).await?;
        tx.commit().await?;
        self.invalidate_user(ctx.tenant, user);
        Ok(())
    }

    pub async fn set_user_active(&self, ctx: &Ctx, user: Uuid, active: bool) -> Result<()> {
        Self::require_admin(ctx)?;
        if user == ctx.user && !active {
            return Err(Error::invalid("you cannot deactivate yourself"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("update app_user set is_active = $3, token_version = token_version + 1, updated_at = now() where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(user)
            .bind(active)
            .execute(&mut *tx)
            .await?;
        // Journalled outside the database, so that a restore from before does not undo it (FEO-014).
        crate::outbox::enqueue(
            &mut tx,
            ctx.tenant,
            crate::lifecycle::ACCESS_TOPIC,
            Some("User"),
            Some(user),
            serde_json::json!({"user": user, "active": active}),
        )
        .await?;
        notify_users(&mut tx, ctx.tenant, Some(user)).await?;
        tx.commit().await?;
        self.invalidate_user(ctx.tenant, user);
        Ok(())
    }

    /// Changes a password and signs the user out everywhere.
    pub async fn set_password(&self, ctx: &Ctx, user: Uuid, current: Option<&str>, new_password: &str) -> Result<()> {
        if user != ctx.user {
            Self::require_admin(ctx)?;
        }
        validate_password(new_password)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        if user == ctx.user {
            let hash: Option<String> = sqlx::query_scalar("select password_hash from app_user where tenant_id = $1 and id = $2")
                .bind(ctx.tenant)
                .bind(user)
                .fetch_one(&mut *tx)
                .await?;
            let ok = match (hash, current) {
                (Some(h), Some(c)) => verify_password(c.to_string(), h).await,
                (None, _) => true,
                _ => false,
            };
            if !ok {
                return Err(Error::field("current_password", "is incorrect"));
            }
        }
        let new_hash = hash_password(new_password.to_string()).await?;
        sqlx::query(
            "update app_user set password_hash = $3, token_version = token_version + 1, failed_logins = 0, locked_until = null, updated_at = now() \
             where tenant_id = $1 and id = $2",
        )
        .bind(ctx.tenant)
        .bind(user)
        .bind(new_hash)
        .execute(&mut *tx)
        .await?;
        notify_users(&mut tx, ctx.tenant, Some(user)).await?;
        tx.commit().await?;
        self.invalidate_user(ctx.tenant, user);
        Ok(())
    }

    /// Invalidates every session of a user.
    pub async fn logout_everywhere(&self, ctx: &Ctx) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("update app_user set token_version = token_version + 1 where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(ctx.user)
            .execute(&mut *tx)
            .await?;
        notify_users(&mut tx, ctx.tenant, Some(ctx.user)).await?;
        tx.commit().await?;
        self.invalidate_user(ctx.tenant, ctx.user);
        Ok(())
    }

    pub async fn set_role_permission(&self, ctx: &Ctx, role: &str, p: &PermissionDef) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        self.set_role_permission_in(ctx, &mut tx, role, p).await?;
        tx.commit().await?;
        self.invalidate_tenant_users(ctx.tenant);
        Ok(())
    }

    /// [`Kernel::set_role_permission`] in the caller's transaction (a pack's install): the caller drops the
    /// users' caches once it commits.
    pub(crate) async fn set_role_permission_in(&self, ctx: &Ctx, tx: &mut Tx, role: &str, p: &PermissionDef) -> Result<()> {
        Self::require_admin(ctx)?;
        if p.entity != "*" && ctx.schema.entity(&p.entity).is_none() {
            return Err(Error::field("entity", "unknown entity"));
        }
        let ent = ctx.schema.entity(&p.entity).cloned();
        p.check_condition(|f| ent.as_deref().and_then(|e| e.field(f))).map_err(|m| Error::field("where", m))?;
        // A path is checked against the entities it crosses now, and makes the rule an own-record rule.
        let via = p.via.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
        if let Some(path) = &via {
            if p.entity == "*" {
                return Err(Error::field("via", "a path starts at one entity, not every entity"));
            }
            ferrum_meta::via::resolve(&p.entity, path, |n| ctx.schema.entity(n).map(|e| &e.def)).map_err(|m| Error::field("via", m))?;
        }
        if let Some(why) = p.unknown_action(ent.as_ref().map(|e| &e.def)) {
            return Err(Error::field("actions", why));
        }
        let p = &PermissionDef { if_owner: p.if_owner || via.is_some(), via, ..p.clone() };
        let condition = (!p.condition.is_empty()).then(|| serde_json::to_value(&p.condition).unwrap_or_default());
        sqlx::query("insert into role (tenant_id, name) values ($1, $2) on conflict do nothing").bind(ctx.tenant).bind(role).execute(&mut **tx).await?;
        let portal: bool =
            sqlx::query_scalar("select portal from role where tenant_id = $1 and name = $2").bind(ctx.tenant).bind(role).fetch_one(&mut **tx).await?;
        if let Some(why) = portal.then(|| p.portal_problem(holds_personal(ctx, &p.entity))).flatten() {
            return Err(Error::field("entity", why));
        }
        sqlx::query(
            "insert into role_permission (tenant_id, role, entity, can_read, can_write, can_create, can_delete, can_submit, can_cancel, \
             can_export, can_import, if_owner, read_groups, write_groups, match_rule, rule_key, via, actions) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18) \
             on conflict (tenant_id, role, entity, if_owner, rule_key) do update set can_read = excluded.can_read, can_write = excluded.can_write, \
             can_create = excluded.can_create, can_delete = excluded.can_delete, can_submit = excluded.can_submit, can_cancel = excluded.can_cancel, \
             can_export = excluded.can_export, can_import = excluded.can_import, read_groups = excluded.read_groups, write_groups = excluded.write_groups, \
             via = excluded.via, actions = excluded.actions",
        )
        .bind(ctx.tenant)
        .bind(role)
        .bind(&p.entity)
        .bind(p.read)
        .bind(p.write)
        .bind(p.create)
        .bind(p.delete)
        .bind(p.submit)
        .bind(p.cancel)
        .bind(p.export)
        .bind(p.import)
        .bind(p.if_owner)
        .bind(&p.read_groups)
        .bind(&p.write_groups)
        .bind(condition)
        .bind(p.condition_key())
        .bind(&p.via)
        .bind(&p.actions)
        .execute(&mut **tx)
        .await?;
        notify_users(tx, ctx.tenant, None).await
    }

    /// Marks a role for the portal or not (BD-02). Marking is refused while any of its rules could
    /// not belong to a portal role, naming them; unmarked, portal people holding it lose it at once.
    pub async fn set_role_portal(&self, ctx: &Ctx, role: &str, portal: bool) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        self.set_role_portal_in(ctx, &mut tx, role, portal).await?;
        tx.commit().await?;
        self.invalidate_tenant_users(ctx.tenant);
        Ok(())
    }

    /// [`Kernel::set_role_portal`] in the caller's transaction (a pack's install).
    pub(crate) async fn set_role_portal_in(&self, ctx: &Ctx, tx: &mut Tx, role: &str, portal: bool) -> Result<()> {
        Self::require_admin(ctx)?;
        let exists: bool = sqlx::query_scalar("select exists (select 1 from role where tenant_id = $1 and name = $2)")
            .bind(ctx.tenant)
            .bind(role)
            .fetch_one(&mut **tx)
            .await?;
        if !exists {
            return Err(Error::NotFound(format!("role {role}")));
        }
        if portal {
            #[allow(clippy::type_complexity)]
            let rules: Vec<(String, bool, bool, bool, bool, bool, Option<String>, bool, bool, bool, Vec<String>, Vec<String>)> = sqlx::query_as(
                "select entity, if_owner, can_delete, can_cancel, can_export, can_import, via, can_write, can_create, can_submit, actions, write_groups \
                 from role_permission where tenant_id = $1 and role = $2 order by entity",
            )
            .bind(ctx.tenant)
            .bind(role)
            .fetch_all(&mut **tx)
            .await?;
            let problems: Vec<String> = rules
                .into_iter()
                .filter_map(|(entity, if_owner, delete, cancel, export, import, via, write, create, submit, actions, write_groups)| {
                    let personal = holds_personal(ctx, &entity);
                    PermissionDef { entity, if_owner, delete, cancel, export, import, via, write, create, submit, actions, write_groups, ..Default::default() }
                        .portal_problem(personal)
                })
                .collect();
            if !problems.is_empty() {
                return Err(Error::field("portal", format!("its rules must change first: {}", problems.join("; "))));
            }
        }
        sqlx::query("update role set portal = $3 where tenant_id = $1 and name = $2").bind(ctx.tenant).bind(role).bind(portal).execute(&mut **tx).await?;
        notify_users(tx, ctx.tenant, None).await
    }
}

/// Whether records of `entity` hold personal data in this workspace (see
/// [`ferrum_meta::holds_personal_data`]); an entity not installed counts as holding it.
fn holds_personal(ctx: &Ctx, entity: &str) -> bool {
    ctx.schema.entity(entity).is_none_or(|e| ferrum_meta::holds_personal_data(&e.def, |n| ctx.schema.personal_data.iter().any(|d| d.entity == n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_scopes() {
        let g = records_scope("records:Customer=read+write;SalesInvoice=read;Customer=export").unwrap();
        assert_eq!(g["Customer"].iter().map(String::as_str).collect::<Vec<_>>(), ["export", "read", "write"]);
        assert_eq!(g["SalesInvoice"].len(), 1);
        for bad in ["records:", "records:Customer", "records:=read", "records:Customer=", "dav", "records:Customer=read;Item"] {
            assert!(records_scope(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn password_policy() {
        assert!(validate_password("short1!").is_err());
        assert!(validate_password("alllowercaseletters").is_err());
        assert!(validate_password("Correct-Horse-9").is_ok());
    }

    #[tokio::test]
    async fn hashes_verify() {
        let h = hash_password("Correct-Horse-9".into()).await.unwrap();
        assert!(verify_password("Correct-Horse-9".into(), h.clone()).await);
        assert!(!verify_password("wrong".into(), h).await);
        assert!(!verify_password("x".into(), DUMMY_HASH.into()).await);
    }
}
