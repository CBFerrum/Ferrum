//! Zero-code customization: tenant overlays are validated against the effective schema *before*
//! they are stored, can be disabled (rollback) at any time, and can be exported/imported as a
//! change-set to promote sandbox work into production.

use crate::Kernel;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::registry::Registry;
use ferrum_meta::{Overlay, OverlayOp, effective};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct OverlayRow {
    pub id: Uuid,
    pub seq: i64,
    pub entity: String,
    pub op: Value,
    pub is_active: bool,
    pub note: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

fn require_admin(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("only administrators can customize".into())) }
}

impl Kernel {
    async fn active_overlays(&self, tx: &mut crate::db::Tx, tenant: Uuid) -> Result<Vec<Overlay>> {
        let rows: Vec<(String, Value)> =
            sqlx::query_as("select entity, op from meta_overlay where tenant_id = $1 and is_active order by seq").bind(tenant).fetch_all(&mut **tx).await?;
        Ok(rows.into_iter().filter_map(|(entity, op)| serde_json::from_value::<OverlayOp>(op).ok().map(|op| Overlay { entity, ops: vec![op] })).collect())
    }

    /// Builds the schema with `extra` appended; rejects the change if it introduces new warnings.
    fn check_overlays(&self, ctx: &Ctx, base: &[Overlay], extra: &[Overlay]) -> Result<()> {
        let mut all = base.to_vec();
        let before = effective::build(&self.registry.catalog(), &ctx.schema.packages, &all, 0).map_err(|e| Error::invalid(e.to_string()))?;
        all.extend_from_slice(extra);
        let after = effective::build(&self.registry.catalog(), &ctx.schema.packages, &all, 0).map_err(|e| Error::invalid(e.to_string()))?;
        if after.warnings.len() > before.warnings.len() {
            let msgs: Vec<String> = after.warnings[before.warnings.len()..].iter().map(|w| format!("{}: {}", w.entity, w.message)).collect();
            return Err(Error::invalid(format!("customization rejected: {}", msgs.join("; "))));
        }
        Ok(())
    }

    pub async fn add_customization(&self, ctx: &Ctx, entity: &str, op: OverlayOp, note: Option<String>) -> Result<Uuid> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let base = self.active_overlays(&mut tx, ctx.tenant).await?;
        let target = match &op {
            OverlayOp::DefineEntity { entity: def } => def.name.clone(),
            _ => entity.to_string(),
        };
        self.check_overlays(ctx, &base, &[Overlay { entity: target.clone(), ops: vec![op.clone()] }])?;
        let id = Uuid::now_v7();
        sqlx::query("insert into meta_overlay (tenant_id, id, entity, op, note, created_by) values ($1, $2, $3, $4, $5, $6)")
            .bind(ctx.tenant)
            .bind(id)
            .bind(&target)
            .bind(serde_json::to_value(&op).map_err(Error::internal)?)
            .bind(note)
            .bind((!ctx.user.is_nil()).then_some(ctx.user))
            .execute(&mut *tx)
            .await?;
        Registry::bump_version(&mut tx, ctx.tenant).await?;
        tx.commit().await?;
        self.registry.invalidate(ctx.tenant);
        Ok(id)
    }

    pub async fn list_customizations(&self, ctx: &Ctx) -> Result<Vec<OverlayRow>> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows =
            sqlx::query_as::<_, OverlayRow>("select id, seq, entity, op, is_active, note, created_at from meta_overlay where tenant_id = $1 order by seq")
                .bind(ctx.tenant)
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Enable/disable one customization. Data in custom fields is kept while disabled.
    pub async fn set_customization_active(&self, ctx: &Ctx, id: Uuid, active: bool) -> Result<()> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        if active {
            let row: Option<(String, Value)> = sqlx::query_as("select entity, op from meta_overlay where tenant_id = $1 and id = $2")
                .bind(ctx.tenant)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
            let (entity, op) = row.ok_or_else(|| Error::NotFound("customization".into()))?;
            let op: OverlayOp = serde_json::from_value(op).map_err(|e| Error::invalid(e.to_string()))?;
            let base = self.active_overlays(&mut tx, ctx.tenant).await?;
            self.check_overlays(ctx, &base, &[Overlay { entity, ops: vec![op] }])?;
        }
        let n = sqlx::query("update meta_overlay set is_active = $3 where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(id)
            .bind(active)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("customization".into()));
        }
        Registry::bump_version(&mut tx, ctx.tenant).await?;
        tx.commit().await?;
        self.registry.invalidate(ctx.tenant);
        Ok(())
    }

    /// Change-set export: every active customization in order.
    pub async fn export_customizations(&self, ctx: &Ctx) -> Result<Value> {
        let rows = self.list_customizations(ctx).await?;
        let ops: Vec<Value> = rows.into_iter().filter(|r| r.is_active).map(|r| json!({"entity": r.entity, "op": r.op, "note": r.note})).collect();
        Ok(json!({"format": "ferrum.changeset/1", "packages": ctx.schema.packages, "customizations": ops}))
    }

    /// Change-set import: validated as a whole, applied atomically, skipping exact duplicates.
    pub async fn import_customizations(&self, ctx: &Ctx, bundle: &Value) -> Result<usize> {
        require_admin(ctx)?;
        if bundle.get("format").and_then(Value::as_str) != Some("ferrum.changeset/1") {
            return Err(Error::invalid("not a ferrum.changeset/1 bundle"));
        }
        let items = bundle.get("customizations").and_then(Value::as_array).ok_or_else(|| Error::invalid("customizations missing"))?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let existing: Vec<(String, Value)> =
            sqlx::query_as("select entity, op from meta_overlay where tenant_id = $1 and is_active").bind(ctx.tenant).fetch_all(&mut *tx).await?;
        let mut new = Vec::new();
        for it in items {
            let entity = it.get("entity").and_then(Value::as_str).ok_or_else(|| Error::invalid("entity missing"))?.to_string();
            let op_v = it.get("op").cloned().ok_or_else(|| Error::invalid("op missing"))?;
            if existing.iter().any(|(e, o)| *e == entity && *o == op_v) {
                continue;
            }
            let op: OverlayOp = serde_json::from_value(op_v.clone()).map_err(|e| Error::invalid(e.to_string()))?;
            new.push((entity, op, op_v, it.get("note").and_then(Value::as_str).map(str::to_string)));
        }
        let base = self.active_overlays(&mut tx, ctx.tenant).await?;
        let extra: Vec<Overlay> = new.iter().map(|(e, op, _, _)| Overlay { entity: e.clone(), ops: vec![op.clone()] }).collect();
        self.check_overlays(ctx, &base, &extra)?;
        for (entity, _, op_v, note) in &new {
            sqlx::query("insert into meta_overlay (tenant_id, id, entity, op, note, created_by) values ($1, $2, $3, $4, $5, $6)")
                .bind(ctx.tenant)
                .bind(Uuid::now_v7())
                .bind(entity)
                .bind(op_v)
                .bind(note)
                .bind((!ctx.user.is_nil()).then_some(ctx.user))
                .execute(&mut *tx)
                .await?;
        }
        if !new.is_empty() {
            Registry::bump_version(&mut tx, ctx.tenant).await?;
        }
        tx.commit().await?;
        self.registry.invalidate(ctx.tenant);
        Ok(new.len())
    }
}
