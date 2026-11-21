//! What a workspace can hand a reviewer: who can reach what, who outside it is involved, what
//! happens to personal data, and whether its history is whole. These are facts read from the system
//! when asked, never a claim of compliance: whether they satisfy a standard is for an accountant, an
//! auditor or a certifier to say. It holds no secret and no record's content, and taking one is
//! itself in the history.

use crate::error::{Error, Result};
use crate::{Ctx, Kernel, audit};
use serde_json::{Value, json};

/// A section: the rows of `select` as JSON, for this workspace (`$1`).
macro_rules! rows {
    ($tx:expr, $ctx:expr, $sql:expr) => {
        sqlx::query_scalar::<_, Value>(concat!("select coalesce(jsonb_agg(row), '[]') from (", $sql, ") row")).bind($ctx.tenant).fetch_one(&mut *$tx).await?
    };
}

impl Kernel {
    pub async fn evidence_pack(&self, ctx: &Ctx) -> Result<Value> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("the evidence pack is for administrators".into()));
        }
        let role_conflicts = self.role_conflicts(ctx).await?;
        let mut tx = self.db.tenant_tx_with_timeout(ctx.tenant, self.db.config.report_timeout_ms).await?;
        let people = rows!(
            tx,
            ctx,
            "select u.email, u.full_name as name, u.kind, u.is_active as active, u.is_admin as administrator, \
             u.totp_enabled_at is not null as two_factor, u.last_login_at as last_sign_in, u.created_at as since, \
             (select coalesce(jsonb_agg(r.role order by r.role), '[]') from user_role r where r.tenant_id = u.tenant_id and r.user_id = u.id and (r.expires_at is null or r.expires_at > now())) as roles, \
             (select coalesce(jsonb_agg(d.name order by d.name), '[]') from role d where d.tenant_id = u.tenant_id and d.portal and d.portal_default and u.kind = 'portal') \
               as held_by_every_portal_person \
             from app_user u where u.tenant_id = $1 order by u.email"
        );
        let keys = rows!(
            tx,
            ctx,
            // An agent's key is that agent's, reaching what its grants do, never the whole API (AG-1-2-fix 3).
            "select k.name, case when k.agent_id is null then k.scope else 'agent' end as scope, u.email as owner, a.name as agent, \
             a.grants as agent_grants, k.created_at, k.expires_at, k.revoked_at, k.last_used_at \
             from api_key k left join app_user u on u.tenant_id = k.tenant_id and u.id = k.user_id \
             left join agent a on a.tenant_id = k.tenant_id and a.id = k.agent_id where k.tenant_id = $1 order by k.created_at"
        );
        let agents = rows!(
            tx,
            ctx,
            "select a.name, a.state, u.email as owner, a.grants, a.classes, a.oauth_client, a.created_at, a.expires_at, a.last_used_at, a.revoked_at \
             from agent a join app_user u on u.tenant_id = a.tenant_id and u.id = a.owner_id where a.tenant_id = $1 order by a.created_at"
        );
        let support = rows!(
            tx,
            ctx,
            "select level, grantee, reason, can_write, entities, created_at, expires_at, revoked_at from support_grant where tenant_id = $1 order by created_at desc"
        );
        let sign_in = rows!(tx, ctx, "select label as name, issuer, enabled from sso_provider where tenant_id = $1 order by label");
        let connections =
            rows!(tx, ctx, "select provider, name, enabled, created_at as since, last_success_at from connection where tenant_id = $1 order by provider, name");
        let plugins = rows!(tx, ctx, "select name, version, enabled, manifest -> 'http_hosts' as reaches from plugin where tenant_id = $1 order by name");
        let retention = rows!(
            tx,
            ctx,
            "select entity, after_days, date_field, action, reason, is_active as active, last_run_at from retention_rule where tenant_id = $1 order by entity"
        );
        let holds: i64 =
            sqlx::query_scalar("select count(*) from legal_hold where tenant_id = $1 and released_at is null").bind(ctx.tenant).fetch_one(&mut *tx).await?;
        let (events, first, last): (i64, Option<chrono::DateTime<chrono::Utc>>, Option<chrono::DateTime<chrono::Utc>>) =
            sqlx::query_as("select count(*), min(at), max(at) from audit_event where tenant_id = $1").bind(ctx.tenant).fetch_one(&mut *tx).await?;
        let (days, anchored): (i64, Option<chrono::NaiveDate>) =
            sqlx::query_as("select count(*), max(day) from audit_anchor where tenant_id = $1").bind(ctx.tenant).fetch_one(&mut *tx).await?;
        audit::record_admin(&mut tx, ctx, "Workspace", ctx.tenant, "evidence_pack", None, json!({})).await?;
        tx.commit().await?;
        Ok(json!({
            "generated_at": chrono::Utc::now(),
            "platform": env!("CARGO_PKG_VERSION"),
            "note": "Facts read from the system when asked. Not a claim of compliance with any standard.",
            "access": {"people": people, "role_conflicts": role_conflicts, "api_keys": keys, "agents": agents, "support_grants": support, "single_sign_on": sign_in},
            "outside_parties": {"connections": connections, "plugins": plugins},
            "personal_data": {
                "declared": ctx.schema.personal_data,
                "not_declared": ctx.schema.undeclared_personal_data,
                "retention_rules": retention,
                "legal_holds_in_force": holds,
            },
            "history": {"events": events, "first": first, "last": last, "days_anchored": days, "last_anchored": anchored},
        }))
    }
}
