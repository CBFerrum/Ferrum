//! List views a team shares. A personal view lives in its owner's preferences (prefs.rs); sharing
//! one puts it here, where everyone who can read the entity finds it. A view is a question, not an
//! answer: what each person gets from it is still what they may read. Whoever made a view, or an
//! administrator, changes it.

use crate::ctx::Action;
use crate::error::{Error, Result};
use crate::{Ctx, Kernel};
use serde_json::{Value, json};
use uuid::Uuid;

/// Largest view, serialized.
const MAX_BYTES: usize = 16 * 1024;
/// Most views one entity's list offers: past this they are no help to anyone.
const MAX_VIEWS: i64 = 200;

fn checked(ctx: &Ctx, entity: &str, name: &str, value: &Value) -> Result<String> {
    ctx.require(entity, Action::Read)?;
    if ctx.user.is_nil() {
        return Err(Error::Forbidden("views are shared by a signed-in user".into()));
    }
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        return Err(Error::field("name", "use 1 to 80 characters"));
    }
    if !value.is_object() || value.to_string().len() > MAX_BYTES {
        return Err(Error::field("value", format!("must be an object of at most {} KB", MAX_BYTES / 1024)));
    }
    Ok(name.to_string())
}

/// The view's author, or an administrator; anyone else is told so.
async fn may_change(ctx: &Ctx, tx: &mut crate::db::Tx, entity: &str, id: Uuid) -> Result<()> {
    let by: Option<Uuid> = sqlx::query_scalar("select created_by from shared_view where tenant_id = $1 and entity = $2 and id = $3")
        .bind(ctx.tenant)
        .bind(entity)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    match by {
        None => Err(Error::NotFound("this view".into())),
        Some(by) if by == ctx.user || ctx.perms.admin => Ok(()),
        Some(_) => Err(Error::Forbidden("only whoever shared this view, or an administrator, can change it".into())),
    }
}

impl Kernel {
    /// The views shared on an entity, by name, each saying who shared it.
    pub async fn shared_views(&self, ctx: &Ctx, entity: &str) -> Result<Vec<Value>> {
        ctx.require(entity, Action::Read)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<(Uuid, String, Value, i32, Uuid, Option<String>)> = sqlx::query_as(
            "select v.id, v.name, v.value, v.version, v.created_by, u.full_name from shared_view v \
             left join app_user u on u.tenant_id = v.tenant_id and u.id = v.created_by \
             where v.tenant_id = $1 and v.entity = $2 order by lower(v.name)",
        )
        .bind(ctx.tenant)
        .bind(entity)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|(id, name, value, version, by, by_name)| {
                json!({"id": id, "name": name, "value": value, "version": version, "by": by_name, "can_change": by == ctx.user || ctx.perms.admin})
            })
            .collect())
    }

    /// Shares a view; its name must be free on this list.
    pub async fn share_view(&self, ctx: &Ctx, entity: &str, name: &str, value: &Value) -> Result<Value> {
        let name = checked(ctx, entity, name, value)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let count: i64 = sqlx::query_scalar("select count(*) from shared_view where tenant_id = $1 and entity = $2")
            .bind(ctx.tenant)
            .bind(entity)
            .fetch_one(&mut *tx)
            .await?;
        if count >= MAX_VIEWS {
            return Err(Error::invalid(format!("this list already shares {MAX_VIEWS} views; remove some first")));
        }
        let id = Uuid::now_v7();
        let added =
            sqlx::query("insert into shared_view (tenant_id, id, entity, name, value, created_by) values ($1, $2, $3, $4, $5, $6) on conflict do nothing")
                .bind(ctx.tenant)
                .bind(id)
                .bind(entity)
                .bind(&name)
                .bind(value)
                .bind(ctx.user)
                .execute(&mut *tx)
                .await?;
        if added.rows_affected() == 0 {
            return Err(Error::Conflict(format!("this list already shares a view called “{name}”")));
        }
        tx.commit().await?;
        Ok(json!({"id": id, "name": name, "value": value, "version": 1}))
    }

    /// Changes a shared view. `version` is what the caller read: someone else's change in between
    /// is a conflict, not lost.
    pub async fn change_shared_view(&self, ctx: &Ctx, entity: &str, id: Uuid, name: &str, value: &Value, version: i32) -> Result<Value> {
        let name = checked(ctx, entity, name, value)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        may_change(ctx, &mut tx, entity, id).await?;
        let taken: bool =
            sqlx::query_scalar("select exists (select 1 from shared_view where tenant_id = $1 and entity = $2 and lower(name) = lower($3) and id <> $4)")
                .bind(ctx.tenant)
                .bind(entity)
                .bind(&name)
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        if taken {
            return Err(Error::Conflict(format!("this list already shares a view called “{name}”")));
        }
        let now: Option<i32> = sqlx::query_scalar(
            "update shared_view set name = $4, value = $5, version = version + 1, updated_at = now() \
             where tenant_id = $1 and entity = $2 and id = $3 and version = $6 returning version",
        )
        .bind(ctx.tenant)
        .bind(entity)
        .bind(id)
        .bind(&name)
        .bind(value)
        .bind(version)
        .fetch_optional(&mut *tx)
        .await?;
        let version = now.ok_or_else(|| Error::Conflict("someone changed this view a moment ago; reload it and try again".into()))?;
        tx.commit().await?;
        Ok(json!({"id": id, "name": name, "value": value, "version": version}))
    }

    /// Stops sharing a view. Copies people saved as their own stay theirs.
    pub async fn unshare_view(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Result<()> {
        ctx.require(entity, Action::Read)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        may_change(ctx, &mut tx, entity, id).await?;
        sqlx::query("delete from shared_view where tenant_id = $1 and entity = $2 and id = $3")
            .bind(ctx.tenant)
            .bind(entity)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}
