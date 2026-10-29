//! Authentication (passwords, API keys, lockout) and the cached per-user request context.

use crate::Kernel;
use crate::ctx::{Ctx, PermSet};
use crate::db::Tx;
use crate::error::{Error, Result};
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use ferrum_meta::PermissionDef;
use rand::Rng;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

const MAX_FAILED_LOGINS: i32 = 5;
pub(crate) const LOCK_MINUTES: i32 = 15;

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
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$ZmVycnVtLWR1bW15LWhhc2gtZm9yLXRpbWluZw";

#[derive(Debug, Clone)]
pub struct Session {
    pub tenant: Uuid,
    pub user: Uuid,
    pub token_version: i32,
}

#[derive(Debug, Clone)]
pub struct CtxCore {
    pub email: String,
    pub full_name: String,
    pub roles: Vec<String>,
    pub is_admin: bool,
    pub token_version: i32,
    pub scopes: BTreeMap<String, Vec<Uuid>>,
    pub default_company: Option<Uuid>,
    pub fy_start_month: u32,
    pub perms: Arc<PermSet>,
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
        }
        let user: Option<U> = sqlx::query_as(
            "select u.tenant_id, u.id, u.password_hash, u.is_active, u.token_version, \
             coalesce(u.locked_until > now(), false) as locked, t.status = 'active' as tenant_active, \
             u.totp_enabled_at is not null as two_factor, coalesce(t.settings->'require_two_factor' = 'true'::jsonb, false) as two_factor_required \
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

    /// API keys look like `fk_<prefix>_<secret>`; only a SHA-256 of the secret is stored.
    pub async fn create_api_key(&self, ctx: &Ctx, user: Uuid, name: &str, expires_at: Option<chrono::DateTime<chrono::Utc>>) -> Result<(Uuid, String)> {
        if !ctx.is_admin && user != ctx.user {
            return Err(Error::Forbidden("you can only create API keys for yourself".into()));
        }
        let (prefix, secret) = {
            let mut rng = rand::rng();
            let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
            let mut token = |n: usize| -> String { (0..n).map(|_| alphabet[rng.random_range(0..alphabet.len())] as char).collect() };
            (token(10), token(40))
        };
        let id = Uuid::now_v7();
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("insert into api_key (tenant_id, id, user_id, name, prefix, secret_hash, expires_at) values ($1, $2, $3, $4, $5, $6, $7)")
            .bind(ctx.tenant)
            .bind(id)
            .bind(user)
            .bind(name)
            .bind(&prefix)
            .bind(sha256_hex(&secret))
            .bind(expires_at)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok((id, format!("fk_{prefix}_{secret}")))
    }

    pub async fn verify_api_key(&self, raw: &str) -> Result<Session> {
        let denied = || Error::Unauthorized("invalid API key".into());
        let rest = raw.strip_prefix("fk_").ok_or_else(denied)?;
        let (prefix, secret) = rest.split_once('_').ok_or_else(denied)?;
        let mut tx = self.db.system_tx().await?;
        let row: Option<(Uuid, Uuid, String, bool, i32)> = sqlx::query_as(
            "select k.tenant_id, k.user_id, k.secret_hash, \
             (k.revoked_at is null and (k.expires_at is null or k.expires_at > now()) and u.is_active and t.status = 'active'), u.token_version \
             from api_key k join app_user u on u.tenant_id = k.tenant_id and u.id = k.user_id join tenant t on t.id = k.tenant_id \
             where k.prefix = $1",
        )
        .bind(prefix)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((tenant, user, hash, valid, tv)) = row else {
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
        Ok(Session { tenant, user, token_version: tv })
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
        let row: Option<(String, String, bool, bool, i32, serde_json::Value)> = sqlx::query_as(
            "select u.email::text, u.full_name, u.is_admin, u.is_active and t.status = 'active', u.token_version, t.settings \
             from app_user u join tenant t on t.id = u.tenant_id where u.tenant_id = $1 and u.id = $2",
        )
        .bind(tenant)
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((email, full_name, is_admin, active, token_version, settings)) = row else {
            return Err(Error::Unauthorized("user no longer exists".into()));
        };
        if !active {
            return Err(Error::Unauthorized("user or tenant is disabled".into()));
        }
        let roles: Vec<String> = sqlx::query_scalar("select role from user_role where tenant_id = $1 and user_id = $2 order by role")
            .bind(tenant)
            .bind(user)
            .fetch_all(&mut *tx)
            .await?;
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
        Ok(CtxCore { email, full_name, roles, is_admin, token_version, scopes, default_company, fy_start_month, perms: Arc::new(perms) })
    }

    /// Builds the request context; `token_version` (from a session token) must match the user's
    /// current version, which is bumped on logout-all / password change / deactivation.
    pub async fn load_ctx(&self, tenant: Uuid, user: Uuid, token_version: Option<i32>, request_id: String) -> Result<Ctx> {
        let key = (tenant, user);
        let cached = self.users.get(&key).filter(|c| c.1.elapsed() < self.settings.ctx_ttl).map(|c| c.0.clone());
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
            tenant,
            user,
            email: core.email.clone(),
            full_name: core.full_name.clone(),
            roles: core.roles.clone(),
            is_admin: core.is_admin,
            is_system: false,
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
                .map_err(|_| Error::field("roles", format!("unknown role {r}")))?;
        }
        tx.commit().await?;
        Ok(id)
    }

    pub async fn set_user_roles(&self, ctx: &Ctx, user: Uuid, roles: &[String]) -> Result<()> {
        Self::require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("delete from user_role where tenant_id = $1 and user_id = $2").bind(ctx.tenant).bind(user).execute(&mut *tx).await?;
        for r in roles {
            sqlx::query("insert into user_role (tenant_id, user_id, role) values ($1, $2, $3)")
                .bind(ctx.tenant)
                .bind(user)
                .bind(r)
                .execute(&mut *tx)
                .await
                .map_err(|_| Error::field("roles", format!("unknown role {r}")))?;
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
        Self::require_admin(ctx)?;
        if p.entity != "*" && ctx.schema.entity(&p.entity).is_none() {
            return Err(Error::field("entity", "unknown entity"));
        }
        let ent = ctx.schema.entity(&p.entity).cloned();
        p.check_condition(|f| ent.as_deref().and_then(|e| e.field(f))).map_err(|m| Error::field("where", m))?;
        let condition = (!p.condition.is_empty()).then(|| serde_json::to_value(&p.condition).unwrap_or_default());
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("insert into role (tenant_id, name) values ($1, $2) on conflict do nothing").bind(ctx.tenant).bind(role).execute(&mut *tx).await?;
        sqlx::query(
            "insert into role_permission (tenant_id, role, entity, can_read, can_write, can_create, can_delete, can_submit, can_cancel, \
             can_export, can_import, if_owner, read_groups, write_groups, match_rule, rule_key) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16) \
             on conflict (tenant_id, role, entity, if_owner, rule_key) do update set can_read = excluded.can_read, can_write = excluded.can_write, \
             can_create = excluded.can_create, can_delete = excluded.can_delete, can_submit = excluded.can_submit, can_cancel = excluded.can_cancel, \
             can_export = excluded.can_export, can_import = excluded.can_import, read_groups = excluded.read_groups, write_groups = excluded.write_groups",
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
        .execute(&mut *tx)
        .await?;
        notify_users(&mut tx, ctx.tenant, None).await?;
        tx.commit().await?;
        self.invalidate_tenant_users(ctx.tenant);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
