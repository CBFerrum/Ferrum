//! What the app shell needs from the kernel (docs/plan/apps-ui.md): browser sessions per device,
//! host names that lead to a workspace, the kinds of people, and interface releases.

use crate::auth::Session;
use crate::{Ctx, Error, Kernel, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long a replica trusts its answer about a session or a host before asking the database
/// again. Revocation on the same replica is immediate; on others it takes at most this long.
const CACHE_TTL: Duration = Duration::from_secs(20);
/// `last_seen_at` is written at most this often per session.
const SEEN_EVERY: chrono::Duration = chrono::Duration::minutes(5);
/// Hosts whose domain stopped pointing at us are marked unverified after this many failed checks.
pub const HOST_MAX_FAILURES: i32 = 7;
/// A released host (renamed or deleted workspace) redirects to the workspace for this long…
pub const HOST_REDIRECT_DAYS: i64 = 90;
/// …and nobody may claim it for this long after its release.
pub const HOST_QUARANTINE_DAYS: i64 = 365;
/// Interface files stay this long after no workspace uses their release.
pub const UI_RETENTION_DAYS: i64 = 30;
/// A sensitive action needs the person to have proved themselves again this recently.
pub const STEP_UP_SECS: i64 = 600;

/// A browser session as the people who own it, and administrators, see it.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct DeviceSession {
    pub id: Uuid,
    pub user_id: Uuid,
    pub actor: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub user_agent: String,
    pub ip: String,
}

/// What a valid browser session adds to who the person is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionState {
    /// Who really acts, when someone acts as the session's person.
    pub actor: Option<Uuid>,
    pub step_up_at: Option<DateTime<Utc>>,
}

impl SessionState {
    /// Whether the person proved themselves again recently enough for a sensitive action.
    pub fn stepped_up(&self) -> bool {
        self.step_up_at.is_some_and(|t| Utc::now() - t < chrono::Duration::seconds(STEP_UP_SECS))
    }
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TenantHost {
    pub host: String,
    pub kind: String,
    pub target: String,
    pub is_primary: bool,
    pub verify_token: String,
    pub verified_at: Option<DateTime<Utc>>,
    pub checked_at: Option<DateTime<Utc>>,
    pub failures: i32,
    pub released_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Where a host name leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostRoute {
    /// A host of this workspace, serving `target` (`shell`, `portal`, `site:{app}`).
    Serve { tenant: Uuid, target: String },
    /// A released host still redirecting: to the workspace's primary host for the same target
    /// (`None` when the workspace has none any more).
    Moved { tenant: Uuid, to: Option<String> },
    /// Not one of ours, or quarantined.
    Unknown,
}

/// The kinds of people (`app_user.kind`).
pub const USER_KINDS: [&str; 4] = ["member", "guest", "portal", "service"];

/// An interface release: the files of an app's UI, named by the digest of their contents.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct UiRelease {
    pub app: String,
    pub digest: String,
    /// Path (under the release) → `{"size", "type"}`.
    pub files: Value,
    /// The entry page split around where the shell goes: `{"head", "body", "tail"}`.
    pub entry: Value,
    pub created_at: DateTime<Utc>,
}

/// A host name as typed or sent in a `Host` header → its canonical form (lowercase, no port, no
/// trailing dot), or `None` when it cannot be one of ours.
pub fn normalize_host(raw: &str) -> Option<String> {
    let h = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    // IPv6 literals and ports.
    let h = if h.starts_with('[') { return None } else { h.split(':').next().unwrap_or_default().to_string() };
    let label_ok =
        |l: &str| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    (h.len() <= 253 && h.contains('.') && h.split('.').all(label_ok)).then_some(h)
}

impl Kernel {
    // ── sessions ──

    /// Records a new browser session for `s` (acted in by `actor` when someone else acts as the
    /// person) and returns its id (the cookie's token names it). Signing in proves the person, so
    /// a new session counts as stepped up.
    pub async fn open_session(&self, s: &Session, ttl: Duration, user_agent: &str, ip: &str, actor: Option<Uuid>) -> Result<Uuid> {
        let id = Uuid::now_v7();
        let ua: String = user_agent.chars().filter(|c| !c.is_control()).take(300).collect();
        let ip: String = ip.chars().take(64).collect();
        let mut tx = self.db.tenant_tx(s.tenant).await?;
        sqlx::query(
            "insert into user_session (tenant_id, id, user_id, actor, step_up_at, expires_at, user_agent, ip) \
             values ($1, $2, $3, $4, now(), now() + make_interval(secs => $5), $6, $7)",
        )
        .bind(s.tenant)
        .bind(id)
        .bind(s.user)
        .bind(actor)
        .bind(ttl.as_secs_f64())
        .bind(ua)
        .bind(ip)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Whether the browser session `sid` of `user` is still valid (not revoked, not expired), and
    /// what it adds. Cached briefly; refreshes `last_seen_at` now and then.
    pub async fn check_session(&self, tenant: Uuid, user: Uuid, sid: Uuid) -> Result<SessionState> {
        let expired = || Error::Unauthorized("this device was signed out; please sign in again".into());
        if let Some(e) = self.sessions.get(&(tenant, sid))
            && e.1.elapsed() < CACHE_TTL
        {
            return e.0.filter(|(owner, _)| *owner == user).map(|(_, state)| state).ok_or_else(expired);
        }
        let mut tx = self.db.tenant_tx(tenant).await?;
        type Row = (Uuid, DateTime<Utc>, Option<Uuid>, Option<DateTime<Utc>>);
        let row: Option<Row> = sqlx::query_as(
            "select user_id, last_seen_at, actor, step_up_at from user_session \
             where tenant_id = $1 and id = $2 and revoked_at is null and expires_at > now()",
        )
        .bind(tenant)
        .bind(sid)
        .fetch_optional(&mut *tx)
        .await?;
        let row_owner = row.as_ref().map(|r| r.0);
        let state = row.filter(|r| r.0 == user).map(|r| (r.1, SessionState { actor: r.2, step_up_at: r.3 }));
        if let Some((seen, _)) = state
            && Utc::now() - seen > SEEN_EVERY
        {
            sqlx::query("update user_session set last_seen_at = now() where tenant_id = $1 and id = $2").bind(tenant).bind(sid).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        let state = state.map(|s| s.1);
        if self.sessions.len() > 200_000 {
            self.sessions.retain(|_, v| v.1.elapsed() < CACHE_TTL);
        }
        // Cached with its owner: a session id presented with another person is no session.
        let owner = row_owner.filter(|_| state.is_some());
        self.sessions.insert((tenant, sid), (owner.zip(state), Instant::now()));
        state.ok_or_else(expired)
    }

    /// Records that the person of session `sid` proved themselves again (password, passkey or a
    /// second factor) for a sensitive action.
    pub async fn step_up_session(&self, tenant: Uuid, sid: Uuid) -> Result<()> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        sqlx::query("update user_session set step_up_at = now() where tenant_id = $1 and id = $2 and revoked_at is null")
            .bind(tenant)
            .bind(sid)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.sessions.remove(&(tenant, sid));
        Ok(())
    }

    /// The signed-in devices of `user` (oneself, or anyone for an administrator).
    pub async fn list_sessions(&self, ctx: &Ctx, user: Uuid) -> Result<Vec<DeviceSession>> {
        if user != ctx.user && !ctx.is_admin {
            return Err(Error::Forbidden("only administrators see other people's devices".into()));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows = sqlx::query_as(
            "select id, user_id, actor, created_at, last_seen_at, expires_at, user_agent, ip from user_session \
             where tenant_id = $1 and user_id = $2 and revoked_at is null and expires_at > now() order by last_seen_at desc limit 200",
        )
        .bind(ctx.tenant)
        .bind(user)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Signs one device out (one's own, or anyone's for an administrator).
    pub async fn revoke_session(&self, ctx: &Ctx, sid: Uuid) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("update user_session set revoked_at = now() where tenant_id = $1 and id = $2 and revoked_at is null and (user_id = $3 or $4)")
            .bind(ctx.tenant)
            .bind(sid)
            .bind(ctx.user)
            .bind(ctx.is_admin)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("no such session".into()));
        }
        crate::audit::record(&mut tx, ctx, "UserSession", sid, "revoke", None, serde_json::json!({})).await?;
        tx.commit().await?;
        self.sessions.insert((ctx.tenant, sid), (None, Instant::now()));
        Ok(())
    }

    /// Signing out of this device: revokes its session (no permission check: it is the caller's).
    pub async fn end_session(&self, tenant: Uuid, sid: Uuid) -> Result<()> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        sqlx::query("update user_session set revoked_at = now() where tenant_id = $1 and id = $2 and revoked_at is null")
            .bind(tenant)
            .bind(sid)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.sessions.insert((tenant, sid), (None, Instant::now()));
        Ok(())
    }

    /// Deletes sessions that ended more than a week ago (maintenance).
    pub async fn purge_sessions(&self) -> Result<u64> {
        let mut tx = self.db.system_tx().await?;
        let n = sqlx::query("delete from user_session where expires_at < now() - interval '7 days' or revoked_at < now() - interval '7 days'")
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        self.sessions.retain(|_, v| v.1.elapsed() < CACHE_TTL);
        Ok(n)
    }

    // ── hosts ──

    /// Where a host name leads: a verified custom domain or a subdomain of a workspace, a released
    /// host still redirecting, or nothing of ours (the caller decides about a default workspace).
    pub async fn route_host(&self, host: &str) -> Result<HostRoute> {
        let Some(host) = normalize_host(host) else { return Ok(HostRoute::Unknown) };
        if let Some(e) = self.hosts.get(&host)
            && e.1.elapsed() < CACHE_TTL
        {
            return Ok(e.0.clone());
        }
        let mut tx = self.db.system_tx().await?;
        type Row = (Uuid, String, String, Option<DateTime<Utc>>, Option<DateTime<Utc>>);
        let row: Option<Row> = sqlx::query_as(
            "select h.tenant_id, h.kind, h.target, h.verified_at, h.released_at from tenant_host h join tenant t on t.id = h.tenant_id where h.host = $1",
        )
        .bind(&host)
        .fetch_optional(&mut *tx)
        .await?;
        let route = match row {
            None => HostRoute::Unknown,
            Some((tenant, _, target, _, Some(released))) => {
                if Utc::now() - released < chrono::Duration::days(HOST_REDIRECT_DAYS) {
                    let to: Option<String> = sqlx::query_scalar(
                        "select host from tenant_host where tenant_id = $1 and target = $2 and is_primary and released_at is null \
                         and (kind = 'subdomain' or verified_at is not null)",
                    )
                    .bind(tenant)
                    .bind(&target)
                    .fetch_optional(&mut *tx)
                    .await?;
                    HostRoute::Moved { tenant, to }
                } else {
                    HostRoute::Unknown
                }
            }
            Some((tenant, kind, target, verified, None)) if kind == "subdomain" || verified.is_some() => HostRoute::Serve { tenant, target },
            Some(_) => HostRoute::Unknown,
        };
        tx.commit().await?;
        if self.hosts.len() > 100_000 {
            self.hosts.retain(|_, v| v.1.elapsed() < CACHE_TTL);
        }
        self.hosts.insert(host, (route.clone(), Instant::now()));
        Ok(route)
    }

    /// The workspace a host name serves the shell for (verified custom domain or subdomain).
    pub async fn tenant_for_host(&self, host: &str) -> Result<Option<Uuid>> {
        Ok(match self.route_host(host).await? {
            HostRoute::Serve { tenant, target } if target == "shell" => Some(tenant),
            _ => None,
        })
    }

    /// The pending, verified and released host names of the caller's workspace.
    pub async fn list_hosts(&self, ctx: &Ctx) -> Result<Vec<TenantHost>> {
        let mut tx = self.db.system_tx().await?;
        let rows = sqlx::query_as(
            "select host, kind, target, is_primary, verify_token, verified_at, checked_at, failures, released_at, created_at \
             from tenant_host where tenant_id = $1 order by created_at",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Adds a subdomain of the cluster's base domain for a workspace (operators and the cloud's
    /// sign-up; no proof needed: the cluster owns the base domain). The first host of a target is
    /// its primary.
    pub async fn add_subdomain(&self, tenant: Uuid, host: &str) -> Result<()> {
        let host = normalize_host(host).ok_or_else(|| Error::field("host", "is not a valid domain name"))?;
        let mut tx = self.db.system_tx().await?;
        self.claimable(&mut tx, &host).await?;
        sqlx::query(
            "insert into tenant_host (host, tenant_id, kind, target, is_primary, verify_token, verified_at) \
             values ($1, $2, 'subdomain', 'shell', not exists (select 1 from tenant_host where tenant_id = $2 and target = 'shell' and is_primary and released_at is null), '', now())",
        )
        .bind(&host)
        .bind(tenant)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.hosts.remove(&host);
        Ok(())
    }

    /// Refuses a host that is in use, or released and still redirecting or quarantined.
    async fn claimable(&self, tx: &mut crate::db::Tx, host: &str) -> Result<()> {
        let row: Option<Option<DateTime<Utc>>> =
            sqlx::query_scalar("select released_at from tenant_host where host = $1").bind(host).fetch_optional(&mut **tx).await?;
        match row {
            None => Ok(()),
            Some(None) => Err(Error::Conflict("this domain is already in use".into())),
            Some(Some(at)) if Utc::now() - at < chrono::Duration::days(HOST_QUARANTINE_DAYS) => {
                Err(Error::Conflict("this domain belonged to another workspace recently and cannot be used yet".into()))
            }
            Some(Some(_)) => {
                sqlx::query("delete from tenant_host where host = $1").bind(host).execute(&mut **tx).await?;
                Ok(())
            }
        }
    }

    /// Adds a custom domain to the caller's workspace, unverified until [`Kernel::host_checked`]
    /// finds it pointing here. `target`: `shell`, `portal` or `site:{app}`.
    pub async fn add_host(&self, ctx: &Ctx, host: &str, target: &str) -> Result<TenantHost> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("administrator access required".into()));
        }
        let host = normalize_host(host).ok_or_else(|| Error::field("host", "is not a valid domain name"))?;
        let target_ok = matches!(target, "shell" | "portal")
            || target.strip_prefix("site:").is_some_and(|a| {
                a.len() >= 2
                    && a.len() <= 40
                    && a.starts_with(|c: char| c.is_ascii_lowercase())
                    && a.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            });
        if !target_ok {
            return Err(Error::field("target", "must be shell, portal or site:{app}"));
        }
        let mut tx = self.db.system_tx().await?;
        let count: i64 =
            sqlx::query_scalar("select count(*) from tenant_host where tenant_id = $1 and released_at is null").bind(ctx.tenant).fetch_one(&mut *tx).await?;
        if count >= 20 {
            return Err(Error::invalid("a workspace has at most 20 domains"));
        }
        self.claimable(&mut tx, &host).await?;
        let token = hex::encode(rand::random::<[u8; 16]>());
        sqlx::query("insert into tenant_host (host, tenant_id, kind, target, verify_token) values ($1, $2, 'custom', $3, $4)")
            .bind(&host)
            .bind(ctx.tenant)
            .bind(target)
            .bind(&token)
            .execute(&mut *tx)
            .await?;
        crate::audit::record(&mut tx, ctx, "WorkspaceDomain", ctx.tenant, "add", None, serde_json::json!({"host": host, "target": target})).await?;
        tx.commit().await?;
        self.hosts.remove(&host);
        Ok(self.list_hosts(ctx).await?.into_iter().find(|h| h.host == host).expect("just added"))
    }

    /// Makes a verified host the primary one of its workspace and target: links, emails and
    /// redirects use it, and the workspace's other hosts of that target redirect to it.
    pub async fn make_primary_host(&self, ctx: &Ctx, host: &str) -> Result<()> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("administrator access required".into()));
        }
        let host = normalize_host(host).ok_or_else(|| Error::NotFound("no such domain".into()))?;
        let mut tx = self.db.system_tx().await?;
        let target: Option<String> = sqlx::query_scalar(
            "select target from tenant_host where host = $1 and tenant_id = $2 and released_at is null and (kind = 'subdomain' or verified_at is not null)",
        )
        .bind(&host)
        .bind(ctx.tenant)
        .fetch_optional(&mut *tx)
        .await?;
        let target = target.ok_or_else(|| Error::invalid("only a verified domain of this workspace can be its primary one"))?;
        sqlx::query("update tenant_host set is_primary = (host = $3) where tenant_id = $1 and target = $2 and released_at is null")
            .bind(ctx.tenant)
            .bind(&target)
            .bind(&host)
            .execute(&mut *tx)
            .await?;
        crate::audit::record(&mut tx, ctx, "WorkspaceDomain", ctx.tenant, "primary", None, serde_json::json!({"host": host})).await?;
        tx.commit().await?;
        self.hosts.clear();
        Ok(())
    }

    /// The primary host of a workspace for `target`, if it has one.
    pub async fn primary_host(&self, tenant: Uuid, target: &str) -> Result<Option<String>> {
        let mut tx = self.db.system_tx().await?;
        let h = sqlx::query_scalar(
            "select host from tenant_host where tenant_id = $1 and target = $2 and is_primary and released_at is null and (kind = 'subdomain' or verified_at is not null)",
        )
        .bind(tenant)
        .bind(target)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(h)
    }

    /// Records the outcome of checking a custom domain (that it reaches this cluster and answers
    /// with its token). Success verifies it; enough failures in a row unverify it.
    pub async fn host_checked(&self, host: &str, ok: bool) -> Result<()> {
        let mut tx = self.db.system_tx().await?;
        if ok {
            sqlx::query(
                "update tenant_host set verified_at = coalesce(verified_at, now()), checked_at = now(), failures = 0 where host = $1 and released_at is null",
            )
            .bind(host)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query(
                "update tenant_host set checked_at = now(), failures = failures + 1, \
                 verified_at = case when failures + 1 >= $2 then null else verified_at end where host = $1 and kind = 'custom' and released_at is null",
            )
            .bind(host)
            .bind(HOST_MAX_FAILURES)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.hosts.remove(host);
        Ok(())
    }

    /// The token a pending custom domain must answer with, by host (checked before any session).
    pub async fn host_token(&self, host: &str) -> Result<Option<String>> {
        let Some(host) = normalize_host(host) else { return Ok(None) };
        let mut tx = self.db.system_tx().await?;
        let t = sqlx::query_scalar("select verify_token from tenant_host where host = $1 and kind = 'custom' and released_at is null")
            .bind(host)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(t)
    }

    /// Custom domains to check again (pending ones and verified ones not checked for a day).
    pub async fn hosts_to_check(&self, limit: i64) -> Result<Vec<String>> {
        let mut tx = self.db.system_tx().await?;
        let rows = sqlx::query_scalar(
            "select host from tenant_host where kind = 'custom' and released_at is null and (checked_at is null or (verified_at is null and checked_at < now() - interval '5 minutes') \
             or checked_at < now() - interval '1 day') order by checked_at nulls first limit $1",
        )
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Whether the edge may get a certificate for `host`: a verified custom domain, a subdomain,
    /// a pending custom domain (it must be served to be checked), or a released one still
    /// redirecting.
    pub async fn host_may_have_certificate(&self, host: &str) -> Result<bool> {
        Ok(!matches!(self.route_host(host).await?, HostRoute::Unknown) || self.host_token(host).await?.is_some())
    }

    /// Removes a custom domain from the caller's workspace. It is released, not forgotten: it
    /// redirects for [`HOST_REDIRECT_DAYS`] and stays quarantined for [`HOST_QUARANTINE_DAYS`],
    /// so old links and data in browsers never reach a new owner.
    pub async fn remove_host(&self, ctx: &Ctx, host: &str) -> Result<()> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("administrator access required".into()));
        }
        let host = normalize_host(host).ok_or_else(|| Error::NotFound("no such domain".into()))?;
        let mut tx = self.db.system_tx().await?;
        // Never verified: nothing can depend on it, so it is simply dropped.
        let dropped =
            sqlx::query("delete from tenant_host where host = $1 and tenant_id = $2 and kind = 'custom' and verified_at is null and released_at is null")
                .bind(&host)
                .bind(ctx.tenant)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        let released = if dropped == 0 {
            sqlx::query(
                "update tenant_host set released_at = now(), is_primary = false where host = $1 and tenant_id = $2 and kind = 'custom' and released_at is null",
            )
            .bind(&host)
            .bind(ctx.tenant)
            .execute(&mut *tx)
            .await?
            .rows_affected()
        } else {
            0
        };
        if dropped + released == 0 {
            return Err(Error::NotFound("no such domain".into()));
        }
        crate::audit::record(&mut tx, ctx, "WorkspaceDomain", ctx.tenant, "remove", None, serde_json::json!({"host": host})).await?;
        tx.commit().await?;
        self.hosts.remove(&host);
        Ok(())
    }

    /// Releases every host of a workspace (it was renamed away from them, or deleted).
    pub async fn release_hosts(&self, tenant: Uuid) -> Result<u64> {
        let mut tx = self.db.system_tx().await?;
        let n = sqlx::query("update tenant_host set released_at = now(), is_primary = false where tenant_id = $1 and released_at is null")
            .bind(tenant)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        self.hosts.clear();
        Ok(n)
    }

    /// A workspace's slug, name and status (pages and the bootstrap).
    pub async fn workspace_brief(&self, tenant: Uuid) -> Result<(String, String, String)> {
        let mut tx = self.db.system_tx().await?;
        let row: Option<(String, String, String)> =
            sqlx::query_as("select slug, name, status from tenant where id = $1").bind(tenant).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        row.ok_or_else(|| Error::NotFound("workspace".into()))
    }

    // ── people ──

    /// A person's kind (`member`, `guest`, `portal`, `service`).
    pub async fn user_kind(&self, tenant: Uuid, user: Uuid) -> Result<String> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let k: Option<String> =
            sqlx::query_scalar("select kind from app_user where tenant_id = $1 and id = $2").bind(tenant).bind(user).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        k.ok_or_else(|| Error::NotFound("no such user".into()))
    }

    /// Sets a person's kind (administrators). Changing it signs the person out everywhere.
    pub async fn set_user_kind(&self, ctx: &Ctx, user: Uuid, kind: &str) -> Result<()> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("administrator access required".into()));
        }
        if !USER_KINDS.contains(&kind) {
            return Err(Error::field("kind", "must be member, guest, portal or service"));
        }
        if user == ctx.user && kind != "member" {
            return Err(Error::invalid("you cannot change your own kind"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query(
            "update app_user set kind = $3, token_version = token_version + case when kind = $3 then 0 else 1 end where tenant_id = $1 and id = $2",
        )
        .bind(ctx.tenant)
        .bind(user)
        .bind(kind)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("no such user".into()));
        }
        crate::auth::notify_users(&mut tx, ctx.tenant, Some(user)).await?;
        crate::audit::record(&mut tx, ctx, "User", user, "kind", None, serde_json::json!({"kind": kind})).await?;
        tx.commit().await?;
        self.invalidate_user(ctx.tenant, user);
        Ok(())
    }

    // ── interface releases ──

    /// Records an interface release whose files are already stored under `ui/{app}/{digest}/`,
    /// and that app `version` runs it. Idempotent: the digest names the contents, and a version
    /// that did not change the interface shares the release of the one before.
    pub async fn register_ui_release(&self, app: &str, digest: &str, version: &str, files: &Value, entry: &Value) -> Result<()> {
        let mut tx = self.db.system_tx().await?;
        sqlx::query("insert into ui_release (app, digest, files, entry) values ($1, $2, $3, $4) on conflict (app, digest) do update set last_used_at = now()")
            .bind(app)
            .bind(digest)
            .bind(files)
            .bind(entry)
            .execute(&mut *tx)
            .await?;
        sqlx::query("insert into ui_release_version (app, version, digest) values ($1, $2, $3) on conflict do nothing")
            .bind(app)
            .bind(version)
            .bind(digest)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// A release by app and digest.
    pub async fn ui_release(&self, app: &str, digest: &str) -> Result<Option<UiRelease>> {
        let mut tx = self.db.system_tx().await?;
        let r = sqlx::query_as("select app, digest, files, entry, created_at from ui_release where app = $1 and digest = $2")
            .bind(app)
            .bind(digest)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(r)
    }

    /// The release an app version runs (the newest recorded for it).
    pub async fn ui_release_for(&self, app: &str, version: &str) -> Result<Option<UiRelease>> {
        let mut tx = self.db.system_tx().await?;
        let r = sqlx::query_as(
            "select r.app, r.digest, r.files, r.entry, r.created_at from ui_release_version v join ui_release r on r.app = v.app and r.digest = v.digest \
             where v.app = $1 and v.version = $2 order by v.created_at desc, v.digest limit 1",
        )
        .bind(app)
        .bind(version)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(r)
    }

    /// Releases of an app with the versions that run them, newest first (the admin status
    /// page's rollback choice).
    pub async fn ui_releases(&self, app: &str, limit: i64) -> Result<Vec<(UiRelease, Vec<String>)>> {
        let mut tx = self.db.system_tx().await?;
        let rows: Vec<UiRelease> =
            sqlx::query_as("select app, digest, files, entry, created_at from ui_release where app = $1 order by created_at desc limit $2")
                .bind(app)
                .bind(limit)
                .fetch_all(&mut *tx)
                .await?;
        let versions: Vec<(String, String)> =
            sqlx::query_as("select digest, version from ui_release_version where app = $1 order by created_at").bind(app).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let v = versions.iter().filter(|(d, _)| *d == r.digest).map(|(_, v)| v.clone()).collect();
                (r, v)
            })
            .collect())
    }

    /// The release a workspace is pinned to for an app, if any.
    pub async fn ui_pin(&self, tenant: Uuid, app: &str) -> Result<Option<String>> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let d =
            sqlx::query_scalar("select digest from tenant_ui_pin where tenant_id = $1 and app = $2").bind(tenant).bind(app).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        Ok(d)
    }

    /// Pins the caller's workspace to a release of an app's interface (a rollback while the
    /// installed one misbehaves, or a canary), or unpins it with `None`.
    pub async fn pin_ui(&self, ctx: &Ctx, app: &str, digest: Option<&str>) -> Result<()> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("administrator access required".into()));
        }
        if let Some(d) = digest
            && self.ui_release(app, d).await?.is_none()
        {
            return Err(Error::NotFound(format!("no interface release {d} of {app}")));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        match digest {
            Some(d) => {
                sqlx::query(
                    "insert into tenant_ui_pin (tenant_id, app, digest, pinned_by) values ($1, $2, $3, $4) \
                     on conflict (tenant_id, app) do update set digest = excluded.digest, pinned_at = now(), pinned_by = excluded.pinned_by",
                )
                .bind(ctx.tenant)
                .bind(app)
                .bind(d)
                .bind(ctx.user)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query("delete from tenant_ui_pin where tenant_id = $1 and app = $2").bind(ctx.tenant).bind(app).execute(&mut *tx).await?;
            }
        }
        crate::audit::record(
            &mut tx,
            ctx,
            "UiRelease",
            ctx.tenant,
            if digest.is_some() { "pin" } else { "unpin" },
            None,
            serde_json::json!({"app": app, "digest": digest}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Marks releases as in use (every replica calls this for what it served lately, at most
    /// hourly), so they are not purged while tabs may still load their files.
    pub async fn touch_ui_releases(&self, used: &[(String, String)]) -> Result<()> {
        if used.is_empty() {
            return Ok(());
        }
        let (apps, digests): (Vec<&str>, Vec<&str>) = used.iter().map(|(a, d)| (a.as_str(), d.as_str())).unzip();
        let mut tx = self.db.system_tx().await?;
        sqlx::query(
            "update ui_release r set last_used_at = now() from unnest($1::text[], $2::text[]) u(app, digest) where r.app = u.app and r.digest = u.digest",
        )
        .bind(&apps)
        .bind(&digests)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Forgets releases nobody used for [`UI_RETENTION_DAYS`] and nobody pins, except the one an
    /// app version runs; returns them so their files can be deleted.
    pub async fn purge_ui_releases(&self) -> Result<Vec<(String, String)>> {
        let mut tx = self.db.system_tx().await?;
        let gone = sqlx::query_as(
            "delete from ui_release r where r.last_used_at < now() - make_interval(days => $1) \
             and not exists (select 1 from tenant_ui_pin p where p.app = r.app and p.digest = r.digest) \
             and not exists (select 1 from ui_release_version v where v.app = r.app and v.digest = r.digest \
               and v.created_at = (select max(w.created_at) from ui_release_version w where w.app = v.app and w.version = v.version)) \
             returning app, digest",
        )
        .bind(UI_RETENTION_DAYS as i32)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(gone)
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_host;

    #[test]
    fn hosts_are_normalized_or_refused() {
        assert_eq!(normalize_host("ERP.Acme.com.").as_deref(), Some("erp.acme.com"));
        assert_eq!(normalize_host("acme.ferrum.cloud:443").as_deref(), Some("acme.ferrum.cloud"));
        for bad in ["localhost", "[::1]:5150", "-a.com", "a-.com", "a..com", "a_b.com", "", "é.com"] {
            assert_eq!(normalize_host(bad), None, "{bad}");
        }
        assert_eq!(normalize_host(&format!("{}.com", "a".repeat(64))), None);
    }
}
