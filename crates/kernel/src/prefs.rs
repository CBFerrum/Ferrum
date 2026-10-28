//! Per-user interface preferences: small JSON documents keyed by scope (`list:SalesInvoice`,
//! `report:<id>`), one per user and scope. Each write bumps a version; a writer that sends the
//! version it read is refused when someone (another tab) wrote in between.

use crate::error::{Error, Result};
use crate::{Ctx, Kernel};
use serde_json::{Map, Value, json};

/// Largest preference value, serialized.
const MAX_BYTES: usize = 64 * 1024;
/// Most scopes one user may keep.
const MAX_SCOPES: i64 = 500;

fn valid_scope(scope: &str) -> bool {
    (1..=120).contains(&scope.len()) && scope.chars().all(|c| c.is_ascii_alphanumeric() || ":_.-".contains(c))
}

fn signed_in(ctx: &Ctx) -> Result<()> {
    if ctx.user.is_nil() {
        return Err(Error::Forbidden("preferences belong to a signed-in user".into()));
    }
    Ok(())
}

impl Kernel {
    /// Every preference of the caller: `{scope: {value, version}}`.
    pub async fn prefs(&self, ctx: &Ctx) -> Result<Value> {
        signed_in(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<(String, Value, i64)> = sqlx::query_as("select scope, value, version from ui_pref where tenant_id = $1 and user_id = $2 order by scope")
            .bind(ctx.tenant)
            .bind(ctx.user)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Value::Object(rows.into_iter().map(|(s, value, version)| (s, json!({"value": value, "version": version}))).collect::<Map<_, _>>()))
    }

    /// Stores a preference and returns its new version. With `expected_version`, a newer write by
    /// someone else is a conflict instead of being overwritten.
    pub async fn set_pref(&self, ctx: &Ctx, scope: &str, value: &Value, expected_version: Option<i64>) -> Result<i64> {
        signed_in(ctx)?;
        if !valid_scope(scope) {
            return Err(Error::field("scope", "use 1-120 letters, digits or : _ . -"));
        }
        if value.to_string().len() > MAX_BYTES {
            return Err(Error::field("value", format!("is larger than {} KB", MAX_BYTES / 1024)));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let (count, exists): (i64, bool) =
            sqlx::query_as("select count(*), coalesce(bool_or(scope = $3), false) from ui_pref where tenant_id = $1 and user_id = $2")
                .bind(ctx.tenant)
                .bind(ctx.user)
                .bind(scope)
                .fetch_one(&mut *tx)
                .await?;
        if !exists && count >= MAX_SCOPES {
            return Err(Error::invalid(format!("at most {MAX_SCOPES} preferences per user; reset some first")));
        }
        let version: Option<i64> = sqlx::query_scalar(
            "insert into ui_pref (tenant_id, user_id, scope, value) values ($1, $2, $3, $4) \
             on conflict (tenant_id, user_id, scope) do update set value = excluded.value, version = ui_pref.version + 1, updated_at = now() \
             where $5::bigint is null or ui_pref.version = $5 \
             returning version",
        )
        .bind(ctx.tenant)
        .bind(ctx.user)
        .bind(scope)
        .bind(value)
        .bind(expected_version)
        .fetch_optional(&mut *tx)
        .await?;
        let version = version.ok_or_else(|| Error::Conflict("this preference changed elsewhere; reload it and try again".into()))?;
        tx.commit().await?;
        Ok(version)
    }

    /// Forgets a preference (back to the defaults).
    pub async fn delete_pref(&self, ctx: &Ctx, scope: &str) -> Result<()> {
        signed_in(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("delete from ui_pref where tenant_id = $1 and user_id = $2 and scope = $3")
            .bind(ctx.tenant)
            .bind(ctx.user)
            .bind(scope)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scopes() {
        assert!(super::valid_scope("list:SalesInvoice"));
        assert!(super::valid_scope("report:0193a1b2-c3d4.v2_x"));
        for bad in ["", "list SalesInvoice", "a/b", &"x".repeat(121)] {
            assert!(!super::valid_scope(bad), "{bad}");
        }
    }
}
