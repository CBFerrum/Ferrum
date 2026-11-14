//! Outside people signing themselves up to a workspace's portal (docs/api.md, "Public sign-up"; BD-02). Off unless an
//! administrator turns it on for a portal role ([`Kernel::set_portal_signup`]). Someone gives a name and an e-mail
//! address ([`Kernel::request_portal_signup`]) and is mailed a code; the code sign-in ([`Kernel::sign_in_with_code`])
//! confirms it, and only then are they a portal person of the workspace, holding the role, and
//! [`SIGNED_UP`] is raised with the role and what the sign-up page passed (an app enrols them). The answer is the same
//! whether or not the address is someone's already: a portal person is mailed the usual sign-in code instead, anyone
//! else nothing. Requests are limited per address and per network address, counted in the database so the limits hold
//! across replicas. A workspace's own sign-up for its owners (signup.rs) is another thing.

use crate::auth::{Session, code_hash};
use crate::ctx::Ctx;
use crate::db::Tx;
use crate::error::{Error, Result};
use crate::{Kernel, audit};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// Requests an address, and a network address, may make an hour.
pub const PER_ADDRESS_HOUR: i64 = 3;
pub const PER_NETWORK_HOUR: i64 = 20;
/// Raised when someone confirms their sign-up: `{user, email, name, role, passed}`.
pub const SIGNED_UP: &str = "ferrum.portal.signed_up";
const CODE_MINUTES: i32 = 10;
const CODE_TRIES: i32 = 5;

/// A portal role's public sign-up: e-mail domains it is for (any, when none), and the names of what a sign-up page
/// may pass on to the event (a course, say), as the app that reads them declares.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortalSignup {
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub passes: Vec<String>,
}

/// What the public sign-up route takes.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignupRequest {
    pub tenant: String,
    pub role: String,
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub passes: Map<String, Value>,
}

impl Kernel {
    /// Turns public sign-up on for portal role `role` (`Some`), or off. Refused for a role that is not a portal role.
    pub async fn set_portal_signup(&self, ctx: &Ctx, role: &str, setting: Option<&PortalSignup>) -> Result<()> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("administrator access required".into()));
        }
        if let Some(s) = setting {
            if s.domains.iter().any(|d| d.trim().is_empty() || d.contains('@') || !d.contains('.')) {
                return Err(Error::field("domains", "each is a domain, such as example.org"));
            }
            if let Some(p) = s.passes.iter().find(|p| !ferrum_meta::is_ident(p)) {
                return Err(Error::field("passes", format!("'{p}' is not a name")));
            }
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let portal: Option<bool> =
            sqlx::query_scalar("select portal from role where tenant_id = $1 and name = $2").bind(ctx.tenant).bind(role).fetch_optional(&mut *tx).await?;
        match portal {
            None => return Err(Error::NotFound(format!("role {role}"))),
            Some(false) if setting.is_some() => {
                return Err(Error::field("role", format!("{role} is not a portal role: only outside people sign themselves up")));
            }
            _ => {}
        }
        let value = setting.map(|s| json!({"domains": s.domains.iter().map(|d| d.trim().to_lowercase()).collect::<Vec<_>>(), "passes": s.passes}));
        sqlx::query("update role set public_signup = $3 where tenant_id = $1 and name = $2").bind(ctx.tenant).bind(role).bind(&value).execute(&mut *tx).await?;
        audit::record_admin(&mut tx, ctx, "Role", Uuid::nil(), "public_signup", None, json!({"role": role, "setting": value})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Someone outside asks to sign up as `req.role` from network address `network`: mailed a code to confirm, or, an
    /// address that is a portal person's already, the usual sign-in code; anyone else's, nothing. The same answer
    /// whichever. Refused when the workspace has no public sign-up for the role, the address is not of a domain it is
    /// for, the page passed what the role does not take, or a limit is reached.
    pub async fn request_portal_signup(&self, req: &SignupRequest, network: &str) -> Result<()> {
        if self.mailer.is_none() {
            return Err(Error::Unavailable("this server sends no email, so it cannot send sign-in codes".into()));
        }
        let email = req.email.trim().to_lowercase();
        if email.len() > 254 || email.split('@').count() != 2 || email.starts_with('@') || email.ends_with('@') {
            return Err(Error::field("email", "is not an email address"));
        }
        let name = req.name.trim();
        if name.is_empty() || name.chars().count() > 140 {
            return Err(Error::field("name", "is 1 to 140 characters"));
        }
        let slug = req.tenant.trim().to_lowercase();
        let mut tx = self.db.system_tx().await?;
        sqlx::query("delete from portal_signup where id in (select id from portal_signup where created_at < now() - interval '1 day' limit 100)")
            .execute(&mut *tx)
            .await?;
        let found: Option<(Uuid, String, Option<Value>)> = sqlx::query_as(
            "select t.id, t.name, r.public_signup from tenant t join role r on r.tenant_id = t.id and r.name = $2 and r.portal \
             where t.slug = $1 and t.status = 'active'",
        )
        .bind(&slug)
        .bind(&req.role)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((tenant, workspace, Some(setting))) = found else {
            return Err(Error::NotFound(format!("public sign-up as {}", req.role)));
        };
        let setting: PortalSignup = serde_json::from_value(setting).map_err(Error::internal)?;
        let domain = email.rsplit('@').next().unwrap_or_default();
        if !setting.domains.is_empty() && !setting.domains.iter().any(|d| d == domain) {
            return Err(Error::field("email", format!("sign-up here is for addresses at {}", setting.domains.join(", "))));
        }
        if let Some(k) = req.passes.keys().find(|k| !setting.passes.contains(k)) {
            return Err(Error::field("passes", format!("{k} is not something this sign-up takes")));
        }
        if req.passes.values().any(|v| !(v.is_string() || v.is_number() || v.is_boolean()) || v.to_string().len() > 200) {
            return Err(Error::field("passes", "each is a word, a number or yes/no, at most 200 characters"));
        }
        // Requests wait for one another per workspace while they count.
        sqlx::query("select pg_advisory_xact_lock(hashtextextended('ferrum.portal_signup ' || $1::text, 0))").bind(tenant).execute(&mut *tx).await?;
        let (theirs, here): (i64, i64) = sqlx::query_as(
            "select count(*) filter (where email = $2), count(*) filter (where network = $3) from portal_signup \
             where tenant_id = $1 and created_at > now() - interval '1 hour'",
        )
        .bind(tenant)
        .bind(&email)
        .bind(network)
        .fetch_one(&mut *tx)
        .await?;
        if theirs >= PER_ADDRESS_HOUR || here >= PER_NETWORK_HOUR {
            tx.commit().await?;
            return Err(Error::TooManyRequests("too many sign-ups asked for lately; try again in an hour".into()));
        }
        let known: Option<String> = sqlx::query_scalar("select kind from app_user where tenant_id = $1 and email = $2 and is_active")
            .bind(tenant)
            .bind(&email)
            .fetch_optional(&mut *tx)
            .await?;
        let id = Uuid::new_v4();
        let code = format!("{:06}", rand::rng().random_range(0..1_000_000u32));
        // Only the newest request's code works.
        sqlx::query("update portal_signup set used_at = now() where tenant_id = $1 and email = $2 and used_at is null")
            .bind(tenant)
            .bind(&email)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "insert into portal_signup (id, tenant_id, email, full_name, role, passed, network, code_hash, expires_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, now() + make_interval(mins => $9))",
        )
        .bind(id)
        .bind(tenant)
        .bind(&email)
        .bind(name)
        .bind(&req.role)
        .bind(Value::Object(req.passes.clone()))
        .bind(network)
        .bind(known.is_none().then(|| code_hash(id, &code)))
        .bind(CODE_MINUTES)
        .execute(&mut *tx)
        .await?;
        if known.is_none() {
            let mut place = String::new();
            crate::coedit::esc(&workspace, &mut place);
            let html = format!(
                "<p>Your code to sign up to {place}:</p><p style=\"font-size:28px;font-weight:700;letter-spacing:4px\">{code}</p>\
                 <p>It works once, for {CODE_MINUTES} minutes. If you did not ask for it, you can ignore this email: nothing was made for you.</p>"
            );
            self.queue_mail(&mut tx, tenant, &email, &format!("{code} is your code for {workspace}"), &html).await?;
        }
        tx.commit().await?;
        if known.as_deref() == Some("portal") {
            self.send_sign_in_code(&slug, &email).await?;
        }
        Ok(())
    }
}

/// Confirms the newest sign-up `email` asked for in workspace `slug` with `code`, in the code sign-in's transaction:
/// the portal person made with the role, on the audit trail, and [`SIGNED_UP`] raised. None when there is no such
/// sign-up, the code is wrong (a try spent) or the role no longer takes sign-ups.
pub(crate) async fn confirm(tx: &mut Tx, slug: &str, email: &str, code: &str) -> Result<Option<Session>> {
    #[allow(clippy::type_complexity)]
    let pending: Option<(Uuid, Uuid, Option<String>, i32, String, String, Value)> = sqlx::query_as(
        "select s.id, s.tenant_id, s.code_hash, s.attempts, s.full_name, s.role, s.passed from portal_signup s join tenant t on t.id = s.tenant_id \
         where t.slug = $1 and t.status = 'active' and s.email = $2 and s.used_at is null and s.expires_at > now() \
         order by s.created_at desc limit 1 for update of s",
    )
    .bind(slug.trim().to_lowercase())
    .bind(email)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((id, tenant, Some(hash), _, name, role, passed)) = pending else { return Ok(None) };
    if code_hash(id, code) != hash {
        sqlx::query("update portal_signup set attempts = attempts + 1, used_at = case when attempts + 1 >= $2 then now() end where id = $1")
            .bind(id)
            .bind(CODE_TRIES)
            .execute(&mut **tx)
            .await?;
        return Ok(None);
    }
    sqlx::query("update portal_signup set used_at = now() where id = $1").bind(id).execute(&mut **tx).await?;
    let open: bool = sqlx::query_scalar("select exists (select 1 from role where tenant_id = $1 and name = $2 and portal and public_signup is not null)")
        .bind(tenant)
        .bind(&role)
        .fetch_one(&mut **tx)
        .await?;
    if !open {
        return Ok(None);
    }
    let user = Uuid::now_v7();
    let token_version: i32 =
        sqlx::query_scalar("insert into app_user (tenant_id, id, email, full_name, kind) values ($1, $2, $3, $4, 'portal') returning token_version")
            .bind(tenant)
            .bind(user)
            .bind(email)
            .bind(&name)
            .fetch_one(&mut **tx)
            .await?;
    sqlx::query("insert into user_role (tenant_id, user_id, role) values ($1, $2, $3)").bind(tenant).bind(user).bind(&role).execute(&mut **tx).await?;
    let sys = Ctx::system(tenant, std::sync::Arc::new(ferrum_meta::Schema::default()));
    audit::record_admin(tx, &sys, "User", user, "portal_signup", None, json!({"role": role, "passed": passed})).await?;
    let event = json!({"user": user, "email": email, "name": name, "role": role, "passed": passed});
    crate::outbox::enqueue(tx, tenant, SIGNED_UP, None, None, event).await?;
    Ok(Some(Session { tenant, user, token_version }))
}
