//! Zero-code customization: tenant overlays are validated against the effective schema *before*
//! they are stored, can be disabled (rollback) at any time, and can be exported/imported as a
//! change-set to promote sandbox work into production.
//!
//! A customization remembers what it replaced. When a package's upgrade changes that same thing the
//! two have parted ways, and [`Kernel::customization_conflicts`] says so: an upgrade neither undoes a
//! customization nor is silently undone by it. The one case that does not wait for a person is a
//! field a package has come to require, which a customization made optional before it did: the
//! requirement applies until an administrator keeps the customization. An app's plan says the same
//! before anyone agrees to it ([`Kernel::upgrade_impact`]), with the records waiting in workflow
//! states the change removes or changes (FEO-056).

use crate::Kernel;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::registry::Registry;
use ferrum_meta::{Catalog, Overlay, OverlayOp, Schema, WorkflowDef, effective};
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;
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

/// A customization and an upgrade that parted ways.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Conflict {
    pub id: Uuid,
    pub entity: String,
    pub note: Option<String>,
    pub op: Value,
    /// What the customization replaced when it was made.
    pub was: Value,
    /// What the packages say now; `null` also when what it names is gone.
    pub now: Value,
    pub gone: bool,
    /// The customization is not applied until someone keeps it.
    pub suspended: bool,
}

/// Records waiting in a workflow state that a change of packages removes, or whose ways out (or what
/// it lets people do) it changes: they go on under the new definition.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OpenWork {
    pub entity: String,
    pub state: String,
    pub records: i64,
    pub removed: bool,
}

/// What a change of packages would do to a workspace's customizations and open work.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct UpgradeImpact {
    pub customizations: Vec<Conflict>,
    pub open_work: Vec<OpenWork>,
}

/// The ops that replace something a package says, and so can part ways with it.
const REPLACING: &str = "('set_property', 'set_entity_property', 'set_workflow', 'set_print_template')";

/// A state as `w` defines it, with its ways out in a stable order.
fn state_of(w: &WorkflowDef, state: &str) -> Option<Value> {
    let def = w.states.iter().find(|s| s.name == state)?;
    let mut out: Vec<Value> = w.transitions.iter().filter(|t| t.from == state).filter_map(|t| serde_json::to_value(t).ok()).collect();
    out.sort_by_key(|t| t.to_string());
    Some(json!([def, out]))
}

/// Whether `op` makes a field optional that was not required when it was made: such a customization
/// said nothing about a requirement that came later.
pub(crate) fn relaxes(op: &OverlayOp, base: Option<&Value>) -> bool {
    let OverlayOp::SetProperty { property, value, .. } = op else { return false };
    property == "required" && *value == json!(false) && base.is_some_and(|b| *b != json!(true))
}

pub(crate) fn now_required(packaged: &Schema, entity: &str, op: &OverlayOp) -> bool {
    let OverlayOp::SetProperty { field, .. } = op else { return false };
    packaged.entity(entity).and_then(|e| e.field(field)).is_some_and(|f| f.required)
}

/// Whether a custom entity has records (or rows), or, with `field`, records holding a value in it.
async fn holds(tx: &mut crate::db::Tx, tenant: Uuid, entity: &str, field: Option<&str>) -> Result<bool> {
    let value = if field.is_some() { " and coalesce(ext->$3, 'null'::jsonb) <> 'null'::jsonb" } else { " and $3::text is null" };
    let sql = format!("select exists (select 1 from custom_record where tenant_id = $1 and entity = $2{value})");
    Ok(sqlx::query_scalar(AssertSqlSafe(sql)).bind(tenant).bind(entity).bind(field).fetch_one(&mut **tx).await?)
}

/// At most this many operations in one batch of customizations.
pub const MAX_BATCH: usize = 100;

/// One customization to save: `entity` is ignored by the operations that name their entity.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct NewCustomization {
    pub entity: String,
    pub op: OverlayOp,
    #[serde(default)]
    pub note: Option<String>,
}

/// What an operation that is already active does in a list of customizations to save.
#[derive(Clone, Copy, PartialEq)]
enum Duplicates {
    /// Saved again (one customization at a time).
    Save,
    /// Left out (a change set's import).
    Skip,
    /// Refused (a batch: a screen's "nothing changed" is not hidden).
    Refuse,
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

    /// Custom entities' unique keys live in `custom_key`, kept by saves. In the transaction that changes
    /// the customizations (from `before` to what is active now): a key that goes takes its rows with it,
    /// and a key that comes is filled from the records there are, refused while two of them share a value.
    async fn sync_custom_keys(&self, ctx: &Ctx, tx: &mut crate::db::Tx, before: &Schema, after: &Schema) -> Result<()> {
        let keys = |s: &Schema| -> std::collections::BTreeMap<(String, String), (Vec<String>, String)> {
            s.entities
                .values()
                .filter(|e| e.custom)
                .flat_map(|e| e.unique_keys().into_iter().map(move |k| ((e.def.name.clone(), k.join(",")), (k, e.def.label.clone()))))
                .collect()
        };
        let (was, now) = (keys(before), keys(after));
        for (entity, key) in was.keys().filter(|k| !now.contains_key(*k)) {
            sqlx::query("delete from custom_key where tenant_id = $1 and entity = $2 and key = $3")
                .bind(ctx.tenant)
                .bind(entity)
                .bind(key)
                .execute(&mut **tx)
                .await?;
        }
        for ((entity, key), (fields, label)) in now.iter().filter(|(k, _)| !was.contains_key(*k)) {
            let value = crate::store::key_value_sql(fields, "a");
            let from = format!("from custom_record a where a.tenant_id = $1 and a.entity = $2 and a.docstatus <> 2 and {value} is not null");
            let shared: Vec<String> = sqlx::query_scalar(AssertSqlSafe(format!("select {value} {from} group by 1 having count(*) > 1 order by 1 limit 5")))
                .bind(ctx.tenant)
                .bind(entity)
                .fetch_all(&mut **tx)
                .await?;
            if !shared.is_empty() {
                let shown: Vec<String> = shared
                    .iter()
                    .map(|v| match serde_json::from_str::<Vec<Value>>(v) {
                        Ok(vals) => vals.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect::<Vec<_>>().join(" / "),
                        Err(_) => v.clone(),
                    })
                    .collect();
                return Err(Error::Conflict(format!(
                    "{fields} cannot be unique on {label}: records already share {}{}",
                    shown.join(", "),
                    if shared.len() == 5 { " and maybe more" } else { "" },
                    fields = fields.join(", ")
                )));
            }
            sqlx::query(AssertSqlSafe(format!("insert into custom_key (tenant_id, entity, key, value, record_id) select $1, $2, $3, {value}, a.id {from}")))
                .bind(ctx.tenant)
                .bind(entity)
                .bind(key)
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }

    fn build(&self, ctx: &Ctx, overlays: &[Overlay]) -> Result<Schema> {
        effective::build(&self.registry.catalog(), &ctx.schema.packages, overlays, 0).map_err(|e| Error::invalid(e.to_string()))
    }

    /// What a change of custom entities' definitions (from `before` to `after`) may not do to their
    /// records: change an entity's kind while it has records, or a field's type other than safely
    /// ([`ferrum_meta::FieldType::converts_to`]) while records hold values in it. A field that goes
    /// keeps its values in the records, out of sight, and nothing here deletes any.
    async fn guard_records(&self, ctx: &Ctx, tx: &mut crate::db::Tx, before: &Schema, after: &Schema) -> Result<()> {
        for (name, e) in after.entities.iter().filter(|(_, e)| e.custom) {
            let Some(old) = before.entity(name).filter(|o| o.custom) else { continue };
            if old.def.kind != e.def.kind && holds(tx, ctx.tenant, name, None).await? {
                return Err(Error::Conflict(format!(
                    "{}: its kind cannot change from {} to {} while it has records",
                    e.def.label,
                    json!(old.def.kind).as_str().unwrap_or_default(),
                    json!(e.def.kind).as_str().unwrap_or_default()
                )));
            }
            for f in &e.def.fields {
                let Some(was) = old.field(&f.name).map(|o| o.ftype).filter(|t| !t.converts_to(f.ftype)) else { continue };
                if holds(tx, ctx.tenant, name, Some(&f.name)).await? {
                    return Err(Error::Conflict(format!(
                        "field {}: records hold values in it, and a {} value does not read as {}; change it while it holds none, or add a new field",
                        f.name,
                        json!(was).as_str().unwrap_or_default(),
                        json!(f.ftype).as_str().unwrap_or_default()
                    )));
                }
            }
        }
        Ok(())
    }

    /// The packages' schema, without this workspace's customizations.
    fn packaged(&self, ctx: &Ctx) -> Result<Schema> {
        effective::build(&self.registry.catalog(), &ctx.schema.packages, &[], 0).map_err(|e| Error::invalid(e.to_string()))
    }

    /// Customizations that have parted ways with their packages since they were made, for an
    /// administrator to keep (their value stands) or switch off (the package's does).
    pub async fn customization_conflicts(&self, ctx: &Ctx) -> Result<Vec<Conflict>> {
        require_admin(ctx)?;
        let packaged = self.packaged(ctx)?;
        self.parted(ctx, &packaged, &packaged).await
    }

    /// The customizations whose packages say otherwise in `after` than when they were made, or last
    /// kept; one that has not said what it replaced (`base`) is compared from `before`.
    async fn parted(&self, ctx: &Ctx, before: &Schema, after: &Schema) -> Result<Vec<Conflict>> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<(Uuid, String, Value, Option<Value>, Option<String>)> =
            sqlx::query_as("select id, entity, op, base, note from meta_overlay where tenant_id = $1 and is_active order by seq")
                .bind(ctx.tenant)
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        let mut out = Vec::new();
        for (id, entity, raw, base, note) in rows {
            let Ok(op) = serde_json::from_value::<OverlayOp>(raw.clone()) else { continue };
            let Some(was) = base.or_else(|| before.entity(&entity).and_then(|e| effective::replaced_value(e, &op))) else { continue };
            let now = after.entity(&entity).and_then(|e| effective::replaced_value(e, &op));
            if now.as_ref() != Some(&was) {
                let suspended = relaxes(&op, Some(&was)) && now_required(after, &entity, &op);
                out.push(Conflict { id, entity, note, op: raw, was, gone: now.is_none(), now: now.unwrap_or(Value::Null), suspended });
            }
        }
        Ok(out)
    }

    /// What `catalog` in place of the cluster's, with this workspace's packages and `adding`, would
    /// do to its customizations and to the records waiting in its workflows: an app's plan shows it
    /// before anyone agrees to it (FEO-056).
    pub async fn upgrade_impact(&self, ctx: &Ctx, catalog: &Catalog, adding: &[String]) -> Result<UpgradeImpact> {
        require_admin(ctx)?;
        let packages: Vec<String> = ctx.schema.packages.iter().chain(adding).cloned().collect();
        let build = |overlays: &[Overlay]| effective::build(catalog, &packages, overlays, 0).map_err(|e| Error::invalid(e.to_string()));
        let before = self.packaged(ctx)?;
        // Only what this change moves: a customization that parted ways earlier, and still differs
        // by as much, is not its doing.
        let customizations = self
            .parted(ctx, &before, &build(&[])?)
            .await?
            .into_iter()
            .filter(|c| {
                let op = serde_json::from_value::<OverlayOp>(c.op.clone()).ok();
                let today = op.and_then(|op| before.entity(&c.entity).and_then(|e| effective::replaced_value(e, &op)));
                today != (!c.gone).then(|| c.now.clone())
            })
            .collect();
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let after = build(&self.active_overlays(&mut tx, ctx.tenant).await?)?;
        let mut open_work = Vec::new();
        for (name, e) in &ctx.schema.entities {
            let Some(w) = e.workflow.as_ref().filter(|_| !e.custom && !e.table.is_empty()) else { continue };
            let next = after.entity(name).and_then(|n| n.workflow.as_ref()).map(|n| &n.def);
            for state in &w.def.states {
                let then = next.and_then(|n| state_of(n, &state.name));
                if then.is_some() && then == state_of(&w.def, &state.name) {
                    continue;
                }
                let records: i64 =
                    sqlx::query_scalar(AssertSqlSafe(format!("select count(*) from \"{}\" where tenant_id = $1 and workflow_state = $2", e.table)))
                        .bind(ctx.tenant)
                        .bind(&state.name)
                        .fetch_one(&mut *tx)
                        .await?;
                if records > 0 {
                    open_work.push(OpenWork { entity: name.clone(), state: state.name.clone(), records, removed: then.is_none() });
                }
            }
        }
        tx.commit().await?;
        Ok(UpgradeImpact { customizations, open_work })
    }

    /// Customizations made before they kept what they replaced, and the workflows and print templates
    /// they replaced, which were not kept at all: compared from the packages as they are now, since
    /// what they replaced when they were made is not known. Once; returns how many.
    pub async fn adopt_customization_baselines(&self) -> Result<usize> {
        let pick = format!("base is null and is_active and op->>'op' in {REPLACING}");
        let mut tx = self.db.system_tx().await?;
        let tenants: Vec<Uuid> =
            sqlx::query_scalar(AssertSqlSafe(format!("select distinct tenant_id from meta_overlay where {pick}"))).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let mut adopted = 0;
        for tenant in tenants {
            let packaged = self.packaged(&Ctx::system(tenant, self.registry.schema(tenant).await?))?;
            let mut tx = self.db.system_tenant_tx(tenant).await?;
            let rows: Vec<(Uuid, String, Value)> =
                sqlx::query_as(AssertSqlSafe(format!("select id, entity, op from meta_overlay where tenant_id = $1 and {pick}")))
                    .bind(tenant)
                    .fetch_all(&mut *tx)
                    .await?;
            for (id, entity, op) in rows {
                let Ok(op) = serde_json::from_value::<OverlayOp>(op) else { continue };
                let Some(base) = packaged.entity(&entity).and_then(|e| effective::replaced_value(e, &op)) else { continue };
                sqlx::query("update meta_overlay set base = $3 where tenant_id = $1 and id = $2 and base is null")
                    .bind(tenant)
                    .bind(id)
                    .bind(base)
                    .execute(&mut *tx)
                    .await?;
                adopted += 1;
            }
            tx.commit().await?;
        }
        Ok(adopted)
    }

    /// An administrator looked at a conflict and the customization stands: it now replaces what the
    /// packages say today, and is compared from there. Locks and validation still apply.
    pub async fn keep_customization(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        require_admin(ctx)?;
        let packaged = self.packaged(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(String, Value)> = sqlx::query_as("select entity, op from meta_overlay where tenant_id = $1 and id = $2 and is_active")
            .bind(ctx.tenant)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        let (entity, op) = row.ok_or_else(|| Error::NotFound("customization".into()))?;
        let op: OverlayOp = serde_json::from_value(op).map_err(|e| Error::invalid(e.to_string()))?;
        let now = packaged
            .entity(&entity)
            .and_then(|e| effective::replaced_value(e, &op))
            .ok_or_else(|| Error::invalid("what this customization replaced is gone; switch it off"))?;
        sqlx::query("update meta_overlay set base = $3 where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).bind(now).execute(&mut *tx).await?;
        Registry::bump_version(&mut tx, ctx.tenant).await?;
        tx.commit().await?;
        self.registry.invalidate(ctx.tenant);
        Ok(())
    }

    /// Tells a workspace's administrators when an installation parted ways with its customizations.
    pub(crate) async fn report_conflicts(&self, tenant: Uuid) -> Result<()> {
        let system = Ctx::system(tenant, self.registry.schema(tenant).await?);
        let conflicts = self.customization_conflicts(&system).await?.len();
        if conflicts == 0 {
            return Ok(());
        }
        self.tell(tenant, "An update changed what your customizations replace", &format!("{conflicts} to look at, under Customizations")).await
    }

    /// Tells a workspace's administrators what a release made available on the cluster changed there.
    pub(crate) async fn tell_impact(&self, tenant: Uuid, release: &str, impact: &UpgradeImpact) -> Result<()> {
        let schema = self.registry.schema(tenant).await?;
        let mut lines = Vec::new();
        match impact.customizations.len() {
            0 => {}
            1 => lines.push("One of your customizations replaces what it changed: keep it or switch it off under Customizations.".to_string()),
            n => lines.push(format!("{n} of your customizations replace what it changed: keep or switch them off under Customizations.")),
        }
        for w in &impact.open_work {
            let label = schema.entity(&w.entity).map(|e| (e.def.label.clone(), e.def.label_plural.clone().unwrap_or_else(|| e.def.label.clone())));
            let (noun, verb) = match (w.records, label) {
                (1, Some((one, _))) => (one, "waits"),
                (_, Some((_, many))) => (many, "wait"),
                (n, None) => (w.entity.clone(), if n == 1 { "waits" } else { "wait" }),
            };
            let change = if w.removed { "which it removed" } else { "whose ways out it changed: they go on under the new ones" };
            lines.push(format!("{} {noun} {verb} in {}, {change}.", w.records, w.state));
        }
        self.tell(tenant, &format!("{release} changed this workspace"), &lines.join(" ")).await
    }

    async fn tell(&self, tenant: Uuid, title: &str, body: &str) -> Result<()> {
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        self.tell_admins(&mut tx, tenant, title, body).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn add_customization(&self, ctx: &Ctx, entity: &str, op: OverlayOp, note: Option<String>) -> Result<Uuid> {
        let one = vec![NewCustomization { entity: entity.to_string(), op, note }];
        let ids = self.save_customizations(ctx, one, Duplicates::Save, false).await.map_err(|(_, e)| e)?;
        ids.into_iter().next().ok_or_else(|| Error::internal("no customization saved"))
    }

    /// Saves `items` in order, in one transaction, or none of them (`POST /api/customize/batch`):
    /// each is checked against the schema those before it give, so a refusal names the operation
    /// that caused it (its 0-based index). A `dry_run` does everything, unique keys included, and
    /// rolls it back. Returns how many were saved.
    pub async fn batch_customizations(&self, ctx: &Ctx, items: Vec<NewCustomization>, dry_run: bool) -> std::result::Result<usize, (Option<usize>, Error)> {
        require_admin(ctx).map_err(|e| (None, e))?;
        if items.len() > MAX_BATCH {
            return Err((None, Error::invalid(format!("at most {MAX_BATCH} operations in one batch, not {}", items.len()))));
        }
        Ok(self.save_customizations(ctx, items, Duplicates::Refuse, dry_run).await?.len())
    }

    /// The one way customizations are saved: in order, each validated against the whole schema as
    /// the ones before it leave it, guarded against what it would do to records, with unique keys
    /// kept in step; then the version is bumped once and caches are dropped (not for a `dry_run`).
    async fn save_customizations(
        &self,
        ctx: &Ctx,
        items: Vec<NewCustomization>,
        duplicates: Duplicates,
        dry_run: bool,
    ) -> std::result::Result<Vec<Uuid>, (Option<usize>, Error)> {
        let none = |e: Error| (None, e);
        require_admin(ctx).map_err(none)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await.map_err(none)?;
        let mut overlays = self.active_overlays(&mut tx, ctx.tenant).await.map_err(none)?;
        let mut active: Vec<(String, Value)> = overlays.iter().filter_map(|o| Some((o.entity.clone(), serde_json::to_value(o.ops.first()?).ok()?))).collect();
        let packaged = self.packaged(ctx).map_err(none)?;
        let mut was = self.build(ctx, &overlays).map_err(none)?;
        let mut ids = Vec::new();
        for (i, it) in items.into_iter().enumerate() {
            let at = |e: Error| (Some(i), e);
            let target = match &it.op {
                OverlayOp::DefineEntity { entity } | OverlayOp::RedefineEntity { entity } => entity.name.clone(),
                _ => it.entity,
            };
            let raw = serde_json::to_value(&it.op).map_err(|e| at(Error::internal(e)))?;
            if duplicates != Duplicates::Save && active.iter().any(|(e, o)| *e == target && *o == raw) {
                if duplicates == Duplicates::Skip {
                    continue;
                }
                return Err(at(Error::Conflict(format!("this customization of {target} is already active: nothing would change"))));
            }
            overlays.push(Overlay { entity: target.clone(), ops: vec![it.op.clone()] });
            let now = self.build(ctx, &overlays).map_err(at)?;
            if now.warnings.len() > was.warnings.len() {
                let msgs: Vec<String> = now.warnings[was.warnings.len()..].iter().map(|w| format!("{}: {}", w.entity, w.message)).collect();
                return Err(at(Error::invalid(format!("customization rejected: {}", msgs.join("; ")))));
            }
            self.guard_records(ctx, &mut tx, &was, &now).await.map_err(at)?;
            let id = Uuid::now_v7();
            let replaced = packaged.entity(&target).and_then(|e| effective::replaced_value(e, &it.op));
            sqlx::query("insert into meta_overlay (tenant_id, id, entity, op, note, created_by, base) values ($1, $2, $3, $4, $5, $6, $7)")
                .bind(ctx.tenant)
                .bind(id)
                .bind(&target)
                .bind(&raw)
                .bind(it.note)
                .bind((!ctx.user.is_nil()).then_some(ctx.user))
                .bind(replaced)
                .execute(&mut *tx)
                .await
                .map_err(|e| at(e.into()))?;
            self.sync_custom_keys(ctx, &mut tx, &was, &now).await.map_err(at)?;
            active.push((target, raw));
            was = now;
            ids.push(id);
        }
        if dry_run || ids.is_empty() {
            tx.rollback().await.map_err(|e| none(e.into()))?;
            return Ok(ids);
        }
        Registry::bump_version(&mut tx, ctx.tenant).await.map_err(none)?;
        tx.commit().await.map_err(|e| none(e.into()))?;
        self.registry.invalidate(ctx.tenant);
        Ok(ids)
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
        let before = self.active_overlays(&mut tx, ctx.tenant).await?;
        let was = self.build(ctx, &before)?;
        if active {
            let row: Option<(String, Value)> = sqlx::query_as("select entity, op from meta_overlay where tenant_id = $1 and id = $2")
                .bind(ctx.tenant)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
            let (entity, op) = row.ok_or_else(|| Error::NotFound("customization".into()))?;
            let op: OverlayOp = serde_json::from_value(op).map_err(|e| Error::invalid(e.to_string()))?;
            self.check_overlays(ctx, &before, &[Overlay { entity, ops: vec![op] }])?;
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
        // Switching a redefinition off or on changes the definition back or forth: the same guards hold.
        let now = self.build(ctx, &self.active_overlays(&mut tx, ctx.tenant).await?)?;
        self.guard_records(ctx, &mut tx, &was, &now).await?;
        self.sync_custom_keys(ctx, &mut tx, &was, &now).await?;
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

    /// Change-set import: applied in order and atomically (as a batch is), skipping exact duplicates.
    pub async fn import_customizations(&self, ctx: &Ctx, bundle: &Value) -> Result<usize> {
        require_admin(ctx)?;
        if bundle.get("format").and_then(Value::as_str) != Some("ferrum.changeset/1") {
            return Err(Error::invalid("not a ferrum.changeset/1 bundle"));
        }
        let items = bundle.get("customizations").and_then(Value::as_array).ok_or_else(|| Error::invalid("customizations missing"))?;
        let mut new = Vec::new();
        for it in items {
            let entity = it.get("entity").and_then(Value::as_str).ok_or_else(|| Error::invalid("entity missing"))?.to_string();
            let op = serde_json::from_value(it.get("op").cloned().ok_or_else(|| Error::invalid("op missing"))?).map_err(|e| Error::invalid(e.to_string()))?;
            new.push(NewCustomization { entity, op, note: it.get("note").and_then(Value::as_str).map(str::to_string) });
        }
        Ok(self.save_customizations(ctx, new, Duplicates::Skip, false).await.map_err(|(_, e)| e)?.len())
    }
}
