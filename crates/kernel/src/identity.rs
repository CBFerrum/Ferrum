//! One person, many workspaces (docs/plan/apps-ui.md §9.3, §12.3; U6). In shapes B and C people
//! sign in once, at the accounts host, as an *identity*: a person of the cluster's directory, the
//! reserved workspace [`DIRECTORY`], so an identity's password, two-step sign-in, passkeys and
//! browser sessions are those any person has, kept by the same code. A member of a workspace (a
//! *membership*) joins an identity once the person proves the membership is theirs: its password,
//! a code sent to its address, or the workspace's single sign-on, never the address alone. From
//! then on the person enters that workspace through the identity, under the workspace's own rules
//! (single sign-on, two-step sign-in). The accounts host hands a sign-in to a workspace's host as a
//! one-use code ([`Kernel::mint_host_code`]), which that host turns into a session of its own.
//!
//! The first time someone signs in at accounts with a workspace's password, the identity is made
//! from that membership: it takes the password, the authenticator app and the unused recovery
//! codes, once a code sent to the address proves the address too (when the server sends mail).

use crate::auth::{DUMMY_HASH, Session, SignIn, code_hash, count_failure, hash_password, notify_users, validate_password, verify_password};
use crate::db::Tx;
use crate::error::{Error, Result};
use crate::shell::normalize_host;
use crate::{Ctx, Kernel};
use base64::Engine;
use rand::Rng;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// The directory's slug: the workspace whose people are the cluster's identities, and the accounts
/// host's first label (`accounts.{base}`).
pub const DIRECTORY: &str = "accounts";

/// Names no workspace may take: the cluster's own hosts under its base domain (shape B).
pub const RESERVED_SLUGS: &[&str] = &[DIRECTORY, "hooks", "www", "api"];

/// A code sent to an address works for this long, takes this many wrong tries, and an address is
/// sent at most this many in an hour.
const CODE_MINUTES: i32 = 15;
const CODE_TRIES: i32 = 5;
const CODES_PER_HOUR: i64 = 5;
/// A sign-in carried to a workspace's host must arrive within this many seconds.
const HOST_CODE_SECONDS: f64 = 60.0;
/// Memberships of one address tried when an identity is made from a password.
const FOUND_MAX: i64 = 20;

/// What signing in at the accounts host leads to.
#[derive(Debug, Clone)]
pub enum AccountSignIn {
    /// Signed in as the identity.
    Signed(Session),
    /// A code from the authenticator app comes next: the identity's, or (`found`) that of the
    /// membership whose password is making an identity.
    SecondFactor { session: Session, found: bool },
    /// A membership's password was right and no identity has the address yet: the code just sent
    /// to the address finishes making one ([`Kernel::account_verify`]).
    Verify(Session),
}

/// Where an identity entering a workspace stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// Its membership there, ready to sign in as.
    Ready(Session),
    /// A membership with the identity's address has yet to join it; one proof adds it. `click`:
    /// the identity's address is proven and the membership has no authenticator app of its own,
    /// so confirming is proof enough. `password`: the membership has a password of its own.
    /// `two_factor`: its authenticator app's code is needed as well.
    Join { click: bool, password: bool, two_factor: bool },
    /// The workspace signs its people in through its single sign-on (all but its administrators).
    Sso,
    /// The workspace requires two-step sign-in, which the identity has yet to turn on.
    TwoFactor,
    /// No membership there, or one that was turned off.
    NoAccess,
}

/// What proves a membership is the person's (with [`Kernel::join`]): its password, or a code sent
/// to the address, or neither when confirming is enough ([`Entry::Join`]'s `click`); plus the
/// membership's own authenticator code when it has one.
#[derive(Debug, Clone, Copy, Default)]
pub struct Proof<'a> {
    pub password: Option<&'a str>,
    pub email_code: Option<&'a str>,
    pub totp: Option<&'a str>,
}

/// The identity behind a request, as [`Kernel::identity`] gives it.
#[derive(Debug, Clone)]
pub struct Identity {
    pub id: Uuid,
    pub email: String,
    pub name: String,
    /// The address was proven with a code sent to it.
    pub verified: bool,
    pub two_factor: bool,
}

/// A code's text as people type it: six digits, spaces dropped.
fn six_digits(code: &str) -> Option<String> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    (code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit())).then_some(code)
}

fn normalized_email(email: &str) -> Result<String> {
    let email = email.trim().to_lowercase();
    if email.len() > 254 || email.split('@').count() != 2 || email.starts_with('@') || email.ends_with('@') || email.contains(char::is_whitespace) {
        return Err(Error::field("email", "is not an email address"));
    }
    Ok(email)
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

impl Kernel {
    /// The directory, made when missing: a server that signs people in through accounts makes it
    /// at start. Refused when a workspace already has its name.
    pub async fn ensure_directory(&self) -> Result<Uuid> {
        if let Some(id) = self.directory.get() {
            return Ok(*id);
        }
        let mut tx = self.db.system_tx().await?;
        sqlx::query("insert into tenant (id, slug, name, settings) values ($1, $2, 'Accounts', '{\"directory\": true}'::jsonb) on conflict (slug) do nothing")
            .bind(Uuid::now_v7())
            .bind(DIRECTORY)
            .execute(&mut *tx)
            .await?;
        let (id, settings): (Uuid, Value) = sqlx::query_as("select id, settings from tenant where slug = $1").bind(DIRECTORY).fetch_one(&mut *tx).await?;
        let core: bool =
            sqlx::query_scalar("select exists(select 1 from installed_package where tenant_id = $1 and name = 'core')").bind(id).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        if settings.get("directory") != Some(&json!(true)) {
            return Err(Error::Conflict(format!(
                "a workspace is named `{DIRECTORY}`, the name of the accounts host; rename it before the cluster signs people in through accounts"
            )));
        }
        if !core {
            self.install_packages(id, &["core".to_string()]).await?;
        }
        let _ = self.directory.set(id);
        Ok(id)
    }

    /// The directory, if the cluster has one.
    pub async fn directory(&self) -> Result<Option<Uuid>> {
        if let Some(id) = self.directory.get() {
            return Ok(Some(*id));
        }
        let mut tx = self.db.system_tx().await?;
        let id: Option<Uuid> = sqlx::query_scalar("select id from tenant where slug = $1 and settings->'directory' = 'true'::jsonb")
            .bind(DIRECTORY)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        if let Some(id) = id {
            let _ = self.directory.set(id);
        }
        Ok(id)
    }

    async fn directory_id(&self) -> Result<Uuid> {
        self.directory().await?.ok_or_else(|| Error::Unavailable("this cluster does not sign people in through accounts".into()))
    }

    /// The identity a request at the accounts host comes from (a person of the directory).
    pub async fn identity(&self, ctx: &Ctx) -> Result<Identity> {
        let again = || Error::Unauthorized("sign in again".into());
        if Some(ctx.tenant) != self.directory().await? {
            return Err(again());
        }
        let mut tx = self.db.system_tx().await?;
        let row: Option<(String, String, bool, bool)> = sqlx::query_as(
            "select email::text, full_name, email_verified_at is not null, totp_enabled_at is not null from app_user \
             where tenant_id = $1 and id = $2 and is_active",
        )
        .bind(ctx.tenant)
        .bind(ctx.user)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let (email, name, verified, two_factor) = row.ok_or_else(again)?;
        Ok(Identity { id: ctx.user, email, name, verified, two_factor })
    }

    /// The identity a membership joined, if any: its id and address.
    pub async fn identity_of(&self, tenant: Uuid, user: Uuid) -> Result<Option<(Uuid, String)>> {
        let mut tx = self.db.system_tx().await?;
        let row: Option<(Uuid, String)> = sqlx::query_as(
            "select i.id, i.email::text from app_user u join app_user i on i.id = u.identity_id \
             join tenant d on d.id = i.tenant_id and d.slug = $3 where u.tenant_id = $1 and u.id = $2",
        )
        .bind(tenant)
        .bind(user)
        .bind(DIRECTORY)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row)
    }

    // ── signing in at accounts ──

    /// The password step of signing in at the accounts host. Without an identity for the address,
    /// a membership the password opens makes one ([`AccountSignIn::Verify`]).
    pub async fn account_sign_in(&self, email: &str, password: &str) -> Result<AccountSignIn> {
        let dir = self.directory_id().await?;
        let denied = || Error::Unauthorized("wrong email or password".into());
        let Ok(email) = normalized_email(email) else { return Err(denied()) };
        let mut tx = self.db.system_tx().await?;
        let has_identity: bool =
            sqlx::query_scalar("select exists(select 1 from app_user where tenant_id = $1 and email = $2)").bind(dir).bind(&email).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        if has_identity {
            return match self.sign_in(DIRECTORY, &email, password).await {
                Ok(SignIn::SecondFactor(s)) => Ok(AccountSignIn::SecondFactor { session: s, found: false }),
                Ok(SignIn::Session(s) | SignIn::SetupRequired(s)) => Ok(AccountSignIn::Signed(s)),
                Err(Error::Unauthorized(_)) => Err(denied()),
                Err(e) => Err(e),
            };
        }
        self.found_identity(&email, password).await
    }

    /// No identity has the address: the membership this password opens (one of the address's
    /// memberships with a password of its own, most recently used first) is proven.
    async fn found_identity(&self, email: &str, password: &str) -> Result<AccountSignIn> {
        let denied = || Error::Unauthorized("wrong email or password".into());
        #[derive(sqlx::FromRow)]
        struct M {
            tenant_id: Uuid,
            id: Uuid,
            password_hash: String,
            token_version: i32,
            two_factor: bool,
            locked: bool,
        }
        let mut tx = self.db.system_tx().await?;
        // A workspace that requires single sign-on takes no password from those it applies to.
        let found: Vec<M> = sqlx::query_as(
            "select u.tenant_id, u.id, u.password_hash, u.token_version, u.totp_enabled_at is not null as two_factor, \
               coalesce(u.locked_until > now(), false) as locked \
             from app_user u join tenant t on t.id = u.tenant_id \
             where u.email = $1 and u.kind = 'member' and u.is_active and u.identity_id is null and u.password_hash is not null \
               and t.status = 'active' and t.slug <> $2 \
               and not (coalesce(t.settings->'require_sso' = 'true'::jsonb, false) and not u.is_admin) \
             order by u.last_login_at desc nulls last, u.created_at limit $3",
        )
        .bind(email)
        .bind(DIRECTORY)
        .bind(FOUND_MAX)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        if found.is_empty() {
            let _ = verify_password(password.to_string(), DUMMY_HASH.to_string()).await;
            return Err(denied());
        }
        let mut hit = None;
        for m in found.iter().filter(|m| !m.locked) {
            if verify_password(password.to_string(), m.password_hash.clone()).await {
                hit = Some(m);
                break;
            }
        }
        let Some(m) = hit else {
            // As a wrong password on each workspace's own sign-in page would.
            let mut tx = self.db.system_tx().await?;
            for m in found.iter().filter(|m| !m.locked) {
                count_failure(&mut tx, m.tenant_id, m.id).await?;
            }
            tx.commit().await?;
            if found.iter().all(|m| m.locked) {
                return Err(Error::TooManyRequests(format!("too many failed attempts; try again in {} minutes", crate::auth::LOCK_MINUTES)));
            }
            return Err(denied());
        };
        let membership = Session { tenant: m.tenant_id, user: m.id, token_version: m.token_version };
        if m.two_factor {
            return Ok(AccountSignIn::SecondFactor { session: membership, found: true });
        }
        self.found_next(membership, email).await
    }

    /// A proven membership makes an identity: when the server sends mail, a code to the address
    /// comes first; without mail the identity is made at once, its address unproven.
    async fn found_next(&self, membership: Session, email: &str) -> Result<AccountSignIn> {
        if self.mailer.is_some() {
            self.send_email_code(email, "finish setting up your Ferrum account").await?;
            return Ok(AccountSignIn::Verify(membership));
        }
        let mut tx = self.db.system_tx().await?;
        let s = self.identity_from(&mut tx, &membership, false).await?;
        tx.commit().await?;
        self.invalidate_user(membership.tenant, membership.user);
        Ok(AccountSignIn::Signed(s))
    }

    /// The authenticator code of the membership whose password is making an identity
    /// ([`AccountSignIn::SecondFactor`] with `found`).
    pub async fn account_found_second_factor(&self, tenant: Uuid, user: Uuid, token_version: i32, code: &str) -> Result<AccountSignIn> {
        let s = self.second_factor(tenant, user, token_version, code).await?;
        let email = self.member_email(tenant, user).await?;
        self.found_next(s, &email).await
    }

    /// The code sent to the address finishes making the identity of a proven membership
    /// ([`AccountSignIn::Verify`]).
    pub async fn account_verify(&self, membership: &Session, code: &str) -> Result<Session> {
        let email = self.member_email(membership.tenant, membership.user).await?;
        let mut tx = self.db.system_tx().await?;
        if let Err(e) = self.take_email_code(&mut tx, &email, code).await {
            tx.commit().await?;
            return Err(e);
        }
        let s = self.identity_from(&mut tx, membership, true).await?;
        tx.commit().await?;
        self.invalidate_user(membership.tenant, membership.user);
        Ok(s)
    }

    async fn member_email(&self, tenant: Uuid, user: Uuid) -> Result<String> {
        let mut tx = self.db.system_tx().await?;
        let email: Option<String> =
            sqlx::query_scalar("select email::text from app_user where tenant_id = $1 and id = $2").bind(tenant).bind(user).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        email.ok_or_else(|| Error::Unauthorized("sign in again".into()))
    }

    /// Makes the identity of a proven membership: it takes the membership's password,
    /// authenticator app and unused recovery codes, and the membership joins it.
    async fn identity_from(&self, tx: &mut Tx, m: &Session, verified: bool) -> Result<Session> {
        let again = || Error::Unauthorized("sign in again".into());
        let dir = self.directory_id().await?;
        #[derive(sqlx::FromRow)]
        struct U {
            email: String,
            full_name: String,
            password_hash: Option<String>,
            totp_secret: Option<String>,
            totp_on: bool,
            totp_last_step: Option<i64>,
            token_version: i32,
            usable: bool,
            identity_id: Option<Uuid>,
        }
        let u: U = sqlx::query_as(
            "select u.email::text as email, u.full_name, u.password_hash, u.totp_secret, u.totp_enabled_at is not null as totp_on, u.totp_last_step, \
               u.token_version, u.is_active and u.kind = 'member' and t.status = 'active' as usable, u.identity_id \
             from app_user u join tenant t on t.id = u.tenant_id where u.tenant_id = $1 and u.id = $2 for update of u",
        )
        .bind(m.tenant)
        .bind(m.user)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(again)?;
        if !u.usable || u.token_version != m.token_version || u.identity_id.is_some() {
            return Err(again());
        }
        let id = Uuid::now_v7();
        sqlx::query(
            "insert into app_user (tenant_id, id, email, full_name, password_hash, email_verified_at, last_login_at) \
             values ($1, $2, $3, $4, $5, case when $6 then now() end, now())",
        )
        .bind(dir)
        .bind(id)
        .bind(&u.email)
        .bind(&u.full_name)
        .bind(&u.password_hash)
        .bind(verified)
        .execute(&mut **tx)
        .await
        // Made meanwhile, by this person in another tab: they sign in to it.
        .map_err(|e| match Error::from(e) {
            Error::Conflict(_) => again(),
            other => other,
        })?;
        if u.totp_on
            && let Some(sealed) = &u.totp_secret
        {
            sqlx::query("update app_user set totp_secret = $3, totp_enabled_at = now(), totp_last_step = $4 where tenant_id = $1 and id = $2")
                .bind(dir)
                .bind(id)
                .bind(self.reseal_totp((m.tenant, m.user), (dir, id), sealed)?)
                .bind(u.totp_last_step)
                .execute(&mut **tx)
                .await?;
            sqlx::query(
                "insert into user_recovery_code (tenant_id, user_id, code_hash) \
                 select $1, $2, code_hash from user_recovery_code where tenant_id = $3 and user_id = $4 and used_at is null",
            )
            .bind(dir)
            .bind(id)
            .bind(m.tenant)
            .bind(m.user)
            .execute(&mut **tx)
            .await?;
        }
        sqlx::query("update app_user set identity_id = $3, failed_logins = 0, locked_until = null, last_login_at = now() where tenant_id = $1 and id = $2")
            .bind(m.tenant)
            .bind(m.user)
            .bind(id)
            .execute(&mut **tx)
            .await?;
        self.audit_join(tx, m, id, "password").await?;
        Ok(Session { tenant: dir, user: id, token_version: 1 })
    }

    /// Records in the workspace's audit trail that a membership joined an identity, and how it was
    /// proven.
    async fn audit_join(&self, tx: &mut Tx, m: &Session, identity: Uuid, proof: &str) -> Result<()> {
        let mut ctx = Ctx::system(m.tenant, self.registry.schema(m.tenant).await?);
        ctx.user = m.user;
        crate::audit::record(tx, &ctx, "User", m.user, "join_identity", None, json!({"identity": identity, "proof": proof})).await
    }

    // ── codes sent to an address ──

    /// Sends a code to an address, to prove it is the person's (`purpose` finishes "Your code to
    /// …"). The answer says nothing about the address: only one a member of this cluster has is
    /// sent a code, at most five an hour.
    pub async fn send_email_code(&self, email: &str, purpose: &str) -> Result<()> {
        if self.mailer.is_none() {
            return Err(Error::Unavailable("this server sends no email, so it cannot send codes".into()));
        }
        let dir = self.directory_id().await?;
        let email = normalized_email(email)?;
        let mut tx = self.db.system_tx().await?;
        sqlx::query("delete from email_code where id in (select id from email_code where expires_at < now() - interval '1 day' limit 100)")
            .execute(&mut *tx)
            .await?;
        let known: bool = sqlx::query_scalar(
            "select exists(select 1 from app_user u join tenant t on t.id = u.tenant_id \
             where u.email = $1 and u.kind = 'member' and u.is_active and t.status = 'active')",
        )
        .bind(&email)
        .fetch_one(&mut *tx)
        .await?;
        let lately: i64 = sqlx::query_scalar("select count(*) from email_code where email = $1 and created_at > now() - interval '1 hour'")
            .bind(&email)
            .fetch_one(&mut *tx)
            .await?;
        if !known || lately >= CODES_PER_HOUR {
            tx.commit().await?;
            return Ok(());
        }
        // Only the newest code works.
        sqlx::query("update email_code set used_at = now() where email = $1 and used_at is null").bind(&email).execute(&mut *tx).await?;
        let id = Uuid::new_v4();
        let code = format!("{:06}", rand::rng().random_range(0..1_000_000u32));
        sqlx::query("insert into email_code (id, email, code_hash, expires_at) values ($1, $2, $3, now() + make_interval(mins => $4))")
            .bind(id)
            .bind(&email)
            .bind(code_hash(id, &code))
            .bind(CODE_MINUTES)
            .execute(&mut *tx)
            .await?;
        let mut what = String::new();
        crate::coedit::esc(purpose, &mut what);
        let html = format!(
            "<p>Your code to {what}:</p><p style=\"font-size:28px;font-weight:700;letter-spacing:4px\">{code}</p>\
             <p>It works once, for {CODE_MINUTES} minutes. If you did not ask for it, you can ignore this email.</p>"
        );
        self.queue_mail(&mut tx, dir, &email, &format!("{code} is your Ferrum code"), &html).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Takes the newest code sent to the address when `code` is it. A wrong one counts; five spend
    /// it. The caller commits `tx` either way, so the count stands.
    async fn take_email_code(&self, tx: &mut Tx, email: &str, code: &str) -> Result<()> {
        let denied = || Error::Unauthorized("that code is wrong or has expired; ask for a new one".into());
        let Some(code) = six_digits(code) else { return Err(denied()) };
        let live: Option<(Uuid, String, i32)> = sqlx::query_as(
            "select id, code_hash, attempts from email_code where email = $1 and used_at is null and expires_at > now() \
             order by created_at desc limit 1 for update",
        )
        .bind(email)
        .fetch_optional(&mut **tx)
        .await?;
        let Some((id, hash, attempts)) = live else { return Err(denied()) };
        if code_hash(id, &code) != hash {
            sqlx::query("update email_code set attempts = attempts + 1, used_at = case when attempts + 1 >= $2 then now() end where id = $1")
                .bind(id)
                .bind(CODE_TRIES)
                .execute(&mut **tx)
                .await?;
            return Err(if attempts + 1 >= CODE_TRIES {
                Error::Unauthorized("that code is wrong, and has now stopped working; ask for a new one".into())
            } else {
                denied()
            });
        }
        sqlx::query("update email_code set used_at = now() where id = $1").bind(id).execute(&mut **tx).await?;
        Ok(())
    }

    /// Sets the password of the identity with this address, proven by the code sent to it: a
    /// forgotten password, or a first identity for someone whose memberships have none (added
    /// without one, or signing in through single sign-on). Every other session of the identity
    /// ends.
    pub async fn account_password(&self, email: &str, code: &str, password: &str) -> Result<AccountSignIn> {
        let dir = self.directory_id().await?;
        let email = normalized_email(email)?;
        validate_password(password)?;
        let hash = hash_password(password.to_string()).await?;
        let mut tx = self.db.system_tx().await?;
        if let Err(e) = self.take_email_code(&mut tx, &email, code).await {
            tx.commit().await?;
            return Err(e);
        }
        let existing: Option<(Uuid, bool, bool)> =
            sqlx::query_as("select id, totp_enabled_at is not null, is_active from app_user where tenant_id = $1 and email = $2 for update")
                .bind(dir)
                .bind(&email)
                .fetch_optional(&mut *tx)
                .await?;
        let (id, two_factor, token_version) = match existing {
            Some((_, _, false)) => {
                tx.commit().await?;
                return Err(Error::Forbidden("this account is turned off".into()));
            }
            Some((id, two_factor, true)) => {
                let tv: i32 = sqlx::query_scalar(
                    "update app_user set password_hash = $3, email_verified_at = coalesce(email_verified_at, now()), token_version = token_version + 1, \
                       failed_logins = 0, locked_until = null, updated_at = now() where tenant_id = $1 and id = $2 returning token_version",
                )
                .bind(dir)
                .bind(id)
                .bind(&hash)
                .fetch_one(&mut *tx)
                .await?;
                sqlx::query("update user_session set revoked_at = now() where tenant_id = $1 and user_id = $2 and revoked_at is null")
                    .bind(dir)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                notify_users(&mut tx, dir, Some(id)).await?;
                (id, two_factor, tv)
            }
            None => {
                let id = Uuid::now_v7();
                sqlx::query(
                    "insert into app_user (tenant_id, id, email, full_name, password_hash, email_verified_at) values ($1, $2, $3, \
                       coalesce((select full_name from app_user where email = $3 and tenant_id <> $1 order by last_login_at desc nulls last limit 1), ''), $4, now())",
                )
                .bind(dir)
                .bind(id)
                .bind(&email)
                .bind(&hash)
                .execute(&mut *tx)
                .await?;
                (id, false, 1)
            }
        };
        tx.commit().await?;
        self.invalidate_user(dir, id);
        let s = Session { tenant: dir, user: id, token_version };
        Ok(if two_factor { AccountSignIn::SecondFactor { session: s, found: false } } else { AccountSignIn::Signed(s) })
    }

    // ── workspaces ──

    /// Where the identity stands in a workspace it is entering.
    pub async fn enter(&self, identity: &Ctx, tenant: Uuid) -> Result<Entry> {
        let me = self.identity(identity).await?;
        let mut tx = self.db.system_tx().await?;
        let policy: Option<(String, bool, bool)> = sqlx::query_as(
            "select status, coalesce(settings->'require_sso' = 'true'::jsonb, false), coalesce(settings->'require_two_factor' = 'true'::jsonb, false) \
             from tenant where id = $1 and slug <> $2",
        )
        .bind(tenant)
        .bind(DIRECTORY)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((_, sso, two_factor)) = policy.filter(|p| p.0 == "active") else {
            tx.commit().await?;
            return Ok(Entry::NoAccess);
        };
        let joined: Option<(Uuid, bool, String, bool, i32)> =
            sqlx::query_as("select id, is_active, kind, is_admin, token_version from app_user where tenant_id = $1 and identity_id = $2")
                .bind(tenant)
                .bind(me.id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some((user, active, kind, admin, token_version)) = joined {
            tx.commit().await?;
            return Ok(if !active || kind != "member" {
                Entry::NoAccess
            } else if sso && !admin {
                Entry::Sso
            } else if two_factor && !me.two_factor {
                Entry::TwoFactor
            } else {
                Entry::Ready(Session { tenant, user, token_version })
            });
        }
        let candidate: Option<(bool, bool)> = sqlx::query_as(
            "select password_hash is not null, totp_enabled_at is not null from app_user \
             where tenant_id = $1 and email = $2 and identity_id is null and kind = 'member' and is_active",
        )
        .bind(tenant)
        .bind(&me.email)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(match candidate {
            Some((password, two_factor)) => Entry::Join { click: me.verified && !two_factor, password, two_factor },
            None if me.verified => Entry::NoAccess,
            // An unproven address learns nothing about who is a member where: the same page,
            // and proofs that fail.
            None => Entry::Join { click: false, password: true, two_factor: false },
        })
    }

    /// Joins the identity's membership in a workspace (one with the identity's address), proven
    /// by `proof`.
    pub async fn join(&self, identity: &Ctx, tenant: Uuid, proof: Proof<'_>) -> Result<()> {
        let me = self.identity(identity).await?;
        let denied = || Error::Unauthorized("that did not prove it; check the password or the code".into());
        let mut tx = self.db.system_tx().await?;
        let candidate: Option<(Uuid, Option<String>, bool, i32, bool)> = sqlx::query_as(
            "select u.id, u.password_hash, u.totp_enabled_at is not null, u.token_version, coalesce(u.locked_until > now(), false) \
             from app_user u join tenant t on t.id = u.tenant_id \
             where u.tenant_id = $1 and u.email = $2 and u.identity_id is null and u.kind = 'member' and u.is_active and t.status = 'active' and t.slug <> $3",
        )
        .bind(tenant)
        .bind(&me.email)
        .bind(DIRECTORY)
        .fetch_optional(&mut *tx)
        .await?;
        // The first proof: a code sent to the address, the membership's password, or (an address
        // already proven, no authenticator app of its own) confirming.
        let how = if let Some(code) = proof.email_code {
            let taken = self.take_email_code(&mut tx, &me.email, code).await;
            tx.commit().await?;
            taken?;
            "code"
        } else if let Some(password) = proof.password {
            tx.commit().await?;
            let ok = match &candidate {
                Some((_, _, _, _, true)) => {
                    return Err(Error::TooManyRequests(format!("too many failed attempts; try again in {} minutes", crate::auth::LOCK_MINUTES)));
                }
                Some((_, Some(hash), ..)) => verify_password(password.to_string(), hash.clone()).await,
                _ => {
                    let _ = verify_password(password.to_string(), DUMMY_HASH.to_string()).await;
                    false
                }
            };
            if !ok {
                if let Some((user, ..)) = &candidate {
                    let mut tx = self.db.system_tx().await?;
                    count_failure(&mut tx, tenant, *user).await?;
                    tx.commit().await?;
                }
                return Err(denied());
            }
            "password"
        } else {
            tx.commit().await?;
            if !me.verified || candidate.as_ref().is_none_or(|c| c.2) {
                return Err(denied());
            }
            "confirm"
        };
        let Some((user, _, two_factor, token_version, _)) = candidate else { return Err(denied()) };
        // The membership's own authenticator app, when it has one.
        if two_factor {
            let code = proof.totp.filter(|c| !c.trim().is_empty()).ok_or_else(|| Error::field("totp", "the code from your authenticator app is needed"))?;
            self.second_factor(tenant, user, token_version, code).await?;
        }
        let mut tx = self.db.system_tx().await?;
        let joined = sqlx::query(
            "update app_user set identity_id = $3, failed_logins = 0, locked_until = null where tenant_id = $1 and id = $2 and identity_id is null and token_version = $4",
        )
        .bind(tenant)
        .bind(user)
        .bind(me.id)
        .bind(token_version)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if joined != 1 {
            return Err(Error::Conflict("this account changed meanwhile; try again".into()));
        }
        self.audit_join(&mut tx, &Session { tenant, user, token_version }, me.id, how).await?;
        tx.commit().await?;
        self.invalidate_user(tenant, user);
        Ok(())
    }

    /// Joins the membership a workspace's single sign-on just proved (`m`) to the identity signed
    /// in at accounts, when both have the same address. Whether it joined.
    pub async fn join_proven(&self, identity: &Ctx, m: &Session) -> Result<bool> {
        let me = self.identity(identity).await?;
        let mut tx = self.db.system_tx().await?;
        let joined = sqlx::query(
            "update app_user set identity_id = $3 where tenant_id = $1 and id = $2 and identity_id is null and email = $4 and kind = 'member' and is_active \
             and not exists (select 1 from app_user o where o.tenant_id = $1 and o.identity_id = $3)",
        )
        .bind(m.tenant)
        .bind(m.user)
        .bind(me.id)
        .bind(&me.email)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if joined {
            self.audit_join(&mut tx, m, me.id, "sso").await?;
        }
        tx.commit().await?;
        if joined {
            self.invalidate_user(m.tenant, m.user);
        }
        Ok(joined)
    }

    /// The identity's workspaces, for the account chooser: those it joined (with their main
    /// address, `null` when they have none yet), then, once its address is proven, those with a
    /// membership for the address that has yet to join.
    pub async fn identity_workspaces(&self, identity: &Ctx) -> Result<Vec<Value>> {
        let me = self.identity(identity).await?;
        let mut tx = self.db.system_tx().await?;
        #[derive(sqlx::FromRow)]
        struct W {
            id: Uuid,
            slug: String,
            name: String,
            usable: bool,
            joined: bool,
            is_admin: bool,
            host: Option<String>,
        }
        let rows: Vec<W> = sqlx::query_as(
            "select t.id, t.slug, t.name, t.status = 'active' and u.is_active as usable, u.identity_id is not null as joined, u.is_admin, \
               (select h.host from tenant_host h where h.tenant_id = t.id and h.target = 'shell' and h.is_primary and h.released_at is null \
                  and (h.kind = 'subdomain' or h.verified_at is not null) limit 1) as host \
             from app_user u join tenant t on t.id = u.tenant_id \
             where t.slug <> $3 and u.kind = 'member' \
               and (u.identity_id = $1 or ($4 and u.identity_id is null and u.email = $2 and u.is_active and t.status = 'active')) \
             order by (u.identity_id is not null) desc, lower(t.name) limit 500",
        )
        .bind(me.id)
        .bind(&me.email)
        .bind(DIRECTORY)
        .bind(me.verified)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|w| json!({"id": w.id, "slug": w.slug, "name": w.name, "host": w.host, "joined": w.joined, "active": w.usable, "admin": w.is_admin}))
            .collect())
    }

    /// Signs the identity out everywhere: its own sessions and those of every workspace it joined,
    /// on every device. Other hosts notice on their next request.
    pub async fn identity_sign_out_everywhere(&self, identity: &Ctx) -> Result<()> {
        let me = self.identity(identity).await?;
        let mut tx = self.db.system_tx().await?;
        let people: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "update app_user set token_version = token_version + 1 where (tenant_id = $1 and id = $2) or identity_id = $2 returning tenant_id, id",
        )
        .bind(identity.tenant)
        .bind(me.id)
        .fetch_all(&mut *tx)
        .await?;
        for (tenant, user) in &people {
            sqlx::query("update user_session set revoked_at = now() where tenant_id = $1 and user_id = $2 and revoked_at is null")
                .bind(tenant)
                .bind(user)
                .execute(&mut *tx)
                .await?;
            notify_users(&mut tx, *tenant, Some(*user)).await?;
        }
        tx.commit().await?;
        for (tenant, user) in people {
            self.invalidate_user(tenant, user);
        }
        Ok(())
    }

    // ── handing a sign-in to a workspace's host ──

    /// A one-use code that signs membership `m` in on `host`, good for a minute. `identity`: the
    /// identity it is entered through (none when the workspace's single sign-on proved it).
    pub async fn mint_host_code(&self, m: &Session, identity: Option<Uuid>, host: &str) -> Result<String> {
        let host = normalize_host(host).ok_or_else(|| Error::invalid("not a host name"))?;
        let code = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        let mut tx = self.db.system_tx().await?;
        sqlx::query("delete from host_code where code_hash in (select code_hash from host_code where expires_at < now() - interval '1 hour' limit 100)")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "insert into host_code (code_hash, tenant_id, user_id, identity_id, host, expires_at) values ($1, $2, $3, $4, $5, now() + make_interval(secs => $6))",
        )
        .bind(sha256_hex(&code))
        .bind(m.tenant)
        .bind(m.user)
        .bind(identity)
        .bind(&host)
        .bind(HOST_CODE_SECONDS)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(code)
    }

    /// Takes a code minted for this host: the membership to open a session for. The membership
    /// must still be usable and, when an identity carried it, still joined to that identity.
    pub async fn redeem_host_code(&self, code: &str, host: &str) -> Result<Session> {
        let expired = || Error::Unauthorized("this sign-in link has expired or was used; sign in again".into());
        let host = normalize_host(host).ok_or_else(expired)?;
        if code.is_empty() || code.len() > 100 {
            return Err(expired());
        }
        let mut tx = self.db.system_tx().await?;
        let taken: Option<(Uuid, Uuid, Option<Uuid>, String)> = sqlx::query_as(
            "update host_code set used_at = now() where code_hash = $1 and used_at is null and expires_at > now() returning tenant_id, user_id, identity_id, host",
        )
        .bind(sha256_hex(code))
        .fetch_optional(&mut *tx)
        .await?;
        let Some((tenant, user, identity, _)) = taken.filter(|t| t.3 == host) else {
            tx.commit().await?;
            return Err(expired());
        };
        let member: Option<(i32, bool, Option<Uuid>)> = sqlx::query_as(
            "select u.token_version, u.is_active and u.kind = 'member' and t.status = 'active', u.identity_id \
             from app_user u join tenant t on t.id = u.tenant_id where u.tenant_id = $1 and u.id = $2",
        )
        .bind(tenant)
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        match member {
            Some((token_version, true, joined)) if identity.is_none() || joined == identity => Ok(Session { tenant, user, token_version }),
            _ => Err(expired()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_addresses_as_typed() {
        assert_eq!(six_digits(" 123 456 "), Some("123456".into()));
        assert_eq!(six_digits("12345"), None);
        assert_eq!(six_digits("12345a"), None);
        assert_eq!(normalized_email(" Ana@Acme.Test ").unwrap(), "ana@acme.test");
        assert!(normalized_email("ana").is_err());
        assert!(normalized_email("a@b@c").is_err());
        assert!(normalized_email("a b@c.d").is_err());
        assert!(RESERVED_SLUGS.contains(&DIRECTORY));
    }
}
