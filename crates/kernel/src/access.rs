//! Why someone may or may not do something: what an administrator asks when a rule surprises them,
//! before changing it or after a complaint. The answer comes from the rules requests run
//! ([`row_scope`] and [`RowScope::matches`](crate::ctx::RowScope)), so it cannot drift from what is
//! enforced. Passing `roles` answers for those roles instead of the person's own: what would change
//! if they were given them, or lost them. Nothing is changed by asking.

use crate::Kernel;
use crate::ctx::{Action, Ctx, PermSet, row_scope};
use crate::error::{Error, Result};
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;
use std::sync::Arc;
use uuid::Uuid;

impl Kernel {
    /// Whether `user` may do `action` on `entity`, and on `record` when one is named, and through
    /// which of their rules (administrators only). Their own account's state counts: someone
    /// inactive may do nothing, and that is the answer.
    pub async fn explain_access(&self, ctx: &Ctx, user: Uuid, entity: &str, action: Action, record: Option<Uuid>, roles: Option<Vec<String>>) -> Result<Value> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("administrator access required".into()));
        }
        let ent = self.entity(ctx, entity)?;
        let mut who = match self.load_ctx(ctx.tenant, user, None, ctx.request_id.clone()).await {
            Ok(who) => who,
            Err(Error::Unauthorized(why)) => return Ok(json!({"allowed": false, "reason": why})),
            Err(e) => return Err(e),
        };
        let simulated = roles.is_some();
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        if let Some(roles) = roles {
            who.perms = Arc::new(PermSet::load(&mut tx, ctx.tenant, &roles, false).await?);
            who.is_admin = false;
            who.roles = roles;
        }
        if who.is_admin {
            tx.commit().await?;
            return Ok(json!({"allowed": true, "reason": "administrators may do anything in their workspace", "roles": who.roles, "simulated": simulated}));
        }
        let column = format!("can_{}", action.verb());
        let rules: Vec<Value> = sqlx::query_scalar(AssertSqlSafe(format!(
            "select jsonb_build_object('role', role, 'entity', entity, 'if_owner', if_owner, 'where', match_rule, 'via', via) from role_permission \
             where tenant_id = $1 and role = any($2) and entity in ($3, '*') and {column} order by role, rule_key"
        )))
        .bind(ctx.tenant)
        .bind(&who.roles)
        .bind(entity)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let verb = action.verb();
        let scope = match row_scope(&who, &ent, action) {
            Ok(scope) => scope,
            Err(_) => {
                let reason = format!("none of their roles allows them to {verb} {entity}");
                return Ok(json!({"allowed": false, "reason": reason, "roles": who.roles, "rules": rules, "simulated": simulated}));
            }
        };
        let scopes: Vec<&str> = scope.conds.iter().map(|c| c.field.as_deref().unwrap_or("id")).collect();
        let mut answer = json!({
            "allowed": true, "roles": who.roles, "rules": rules, "scoped_by": scopes, "simulated": simulated,
            "reason": if scope.is_unrestricted() { format!("they may {verb} every {entity}") } else { format!("they may {verb} some of {entity}: those their rules reach") },
        });
        if let Some(id) = record {
            let system = Ctx::system(ctx.tenant, ctx.schema.clone());
            let doc = self.get(&system, entity, id, false).await?;
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            // Which rule reached it, and through which path (BD-02), looked up as requests do.
            let by = if scope.grants.is_empty() {
                if scope.matches(&doc) { Some(None) } else { None }
            } else {
                scope.reached_by(&mut tx, ctx.tenant, &doc).await?.map(Some)
            };
            tx.commit().await?;
            let reached = by.is_some();
            let via = by.flatten().and_then(|i| scope.grants[i].via.as_ref()).map(|v| v.path.clone());
            answer["allowed"] = json!(reached);
            answer["reason"] = json!(match (&via, reached) {
                (Some(path), _) => format!("a rule of theirs reaches this {entity} through {path}, which leads to their email"),
                (None, true) => format!("a rule of theirs reaches this {entity}"),
                (None, false) => format!("their rules reach other records of {entity}, not this one"),
            });
            answer["record"] = json!({"id": id, "reached": reached, "via": via});
        }
        Ok(answer)
    }
}

impl Kernel {
    /// The entities whose records the caller's portal roles reach as theirs (docs/builder/decisions.md
    /// BD-02), for the portal's generic pages, each with what they may do there; reference data they
    /// may read whole to pick from is not listed. Empty for someone holding none.
    pub async fn portal_entities(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        if ctx.roles.is_empty() {
            return Ok(vec![]);
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let names: Vec<String> = sqlx::query_scalar(
            "select distinct p.entity from role_permission p join role r on r.tenant_id = p.tenant_id and r.name = p.role \
             where p.tenant_id = $1 and r.portal and p.role = any($2) and p.can_read and (p.if_owner or p.via is not null) order by 1",
        )
        .bind(ctx.tenant)
        .bind(&ctx.roles)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(names
            .into_iter()
            .filter_map(|n| {
                let e = ctx.schema.entity(&n)?;
                Some(json!({
                    "name": n, "label": e.def.label, "label_plural": e.def.plural_label(), "description": e.def.description, "icon": e.def.icon,
                    "permissions": crate::describe::permissions(ctx, &n),
                }))
            })
            .collect())
    }
}
