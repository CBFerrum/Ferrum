//! Merging duplicates of a shared record: one customer, however many apps knew it by a different row.
//!
//! An entity opts in (`mergeable: true`). A merge moves every link to the duplicate onto the record
//! that stays: in every entity and child table, and in the engine tables packages declare
//! (`merge_links`), with comments, tags, assignments, followers and attached files. It is one
//! transaction, and either happens whole or not at all.
//!
//! The duplicate's row stays, as an alias: append-only history (a journal) may name it for ever, and
//! that is not rewritten. [`merged_ids`] gives whoever reports on such history every id a
//! record has had. Asked for by its old id, a record answers with what it became; lists no longer show
//! it; a link saved with the old id lands on the record that stayed. A merge is not undone: the links
//! it moved are not told apart from those that were there.

use crate::ctx::{Action, Ctx, row_scope};
use crate::db::Tx;
use crate::ddl::q;
use crate::error::{Error, Result};
use crate::privacy::record_held;
use crate::query::readable;
use crate::{Doc, Kernel, audit, outbox};
use ferrum_meta::{ChildInfo, EffEntity, FieldType, Schema, Storage};
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;
use uuid::Uuid;

/// What became of `id`, if it was merged into another record.
pub(crate) async fn canonical(tx: &mut Tx, tenant: Uuid, entity: &str, id: Uuid) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar("select canonical_id from record_alias where tenant_id = $1 and entity = $2 and alias_id = $3")
        .bind(tenant)
        .bind(entity)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?)
}

/// Puts a document's links to merged records on the records that stayed: a form opened before the
/// merge still saves what it means.
pub(crate) async fn resolve_links(ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Doc) -> Result<()> {
    for f in ent.value_fields().filter(|f| f.ftype == FieldType::Link) {
        let Some(target) = f.to.as_deref().filter(|t| ctx.schema.entity(t).is_some_and(|e| e.def.mergeable)) else { continue };
        let Some(id) = doc.get(&f.name).and_then(Value::as_str).and_then(|s| s.parse().ok()) else { continue };
        if let Some(stays) = canonical(tx, ctx.tenant, target, id).await? {
            doc.insert(f.name.clone(), json!(stays.to_string()));
        }
    }
    Ok(())
}

/// Every place a link to `entity` is kept: table, how to read it, and for the same entity's own rows.
fn link_columns(schema: &Schema, entity: &str) -> Vec<(String, String, bool, bool)> {
    let mut out = Vec::new();
    for e in schema.entities.values().filter(|e| !e.table.is_empty()) {
        for f in e.value_fields().filter(|f| f.ftype == FieldType::Link && f.to.as_deref() == Some(entity)) {
            out.push((e.table.clone(), f.name.clone(), e.storage_of(&f.name) == Some(Storage::Ext), e.def.name == entity));
        }
        // Its lines, and a line's own lines (BD-06).
        let mut rows: Vec<&ChildInfo> = e.children.values().collect();
        while let Some(info) = rows.pop() {
            let Some(c) = schema.entity(&info.entity) else { continue };
            for f in c.value_fields().filter(|f| f.ftype == FieldType::Link && f.to.as_deref() == Some(entity)) {
                out.push((info.table.clone(), f.name.clone(), c.storage_of(&f.name) == Some(Storage::Ext), false));
            }
            rows.extend(c.children.values());
        }
    }
    out
}

/// Every id a record has had: its own, and those merged into it. For reports on append-only
/// history, which keeps the id it was posted under.
pub async fn merged_ids(tx: &mut Tx, tenant: Uuid, entity: &str, id: Uuid) -> Result<Vec<Uuid>> {
    let stays = canonical(tx, tenant, entity, id).await?.unwrap_or(id);
    let mut ids: Vec<Uuid> =
        sqlx::query_scalar("select alias_id from record_alias where tenant_id = $1 and entity = $2 and canonical_id = $3 order by alias_id")
            .bind(tenant)
            .bind(entity)
            .bind(stays)
            .fetch_all(&mut **tx)
            .await?;
    ids.insert(0, stays);
    Ok(ids)
}

impl Kernel {
    /// Merges `duplicate` into `keep`, for someone who could read both and delete the duplicate.
    pub async fn merge_records(&self, ctx: &Ctx, entity: &str, keep: Uuid, duplicate: Uuid, reason: &str) -> Result<Value> {
        self.merge_records_with(ctx, entity, keep, duplicate, reason, None).await
    }

    /// [`Kernel::merge_records`], first writing onto the record that stays the values chosen from the
    /// duplicate (`values`, at the `version` they were chosen against), in the same transaction: a merge
    /// that fails leaves no chosen value behind, and a record changed since the choice is not merged.
    pub async fn merge_records_with(
        &self,
        ctx: &Ctx,
        entity: &str,
        keep: Uuid,
        duplicate: Uuid,
        reason: &str,
        values: Option<(Doc, Option<i64>)>,
    ) -> Result<Value> {
        let ent = self.entity(ctx, entity)?;
        if !ent.def.mergeable {
            return Err(Error::invalid(format!("{} records cannot be merged", ent.def.label)));
        }
        ctx.require(entity, Action::Delete)?;
        let reason = reason.trim();
        if reason.is_empty() || reason.chars().count() > 500 {
            return Err(Error::field("reason", "say why, in at most 500 characters"));
        }
        if keep == duplicate {
            return Err(Error::invalid("a record cannot be merged into itself"));
        }
        // As the caller, so they cannot merge what they cannot see.
        let stays = self.get(ctx, entity, keep, false).await?;
        let goes = self.get(ctx, entity, duplicate, false).await?;
        if stays.get("id") != Some(&json!(keep.to_string())) {
            return Err(Error::invalid("this record was itself merged into another: merge into that one"));
        }
        if goes.get("id") != Some(&json!(duplicate.to_string())) {
            return Err(Error::Conflict("this record was already merged".into()));
        }
        if ent.def.company_scoped && stays.get("company") != goes.get("company") {
            return Err(Error::invalid("these records belong to different companies"));
        }

        let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
        // Two merges of the same records take their turns, in the same order whichever way round.
        let (first, second) = if keep < duplicate { (keep, duplicate) } else { (duplicate, keep) };
        for id in [first, second] {
            sqlx::query(AssertSqlSafe(format!("select 1 from {} where tenant_id = $1 and id = $2 for update", q(&ent.table))))
                .bind(ctx.tenant)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        if canonical(&mut tx, ctx.tenant, entity, keep).await?.is_some() || canonical(&mut tx, ctx.tenant, entity, duplicate).await?.is_some() {
            return Err(Error::Conflict("one of these records was merged a moment ago".into()));
        }
        for id in [keep, duplicate] {
            if record_held(&mut tx, ctx.tenant, entity, id).await? {
                return Err(Error::Conflict("a legal hold covers this record; release it first".into()));
            }
        }
        // The values chosen to stay, as the caller may write them, in this same transaction.
        if let Some((chosen, version)) = values.filter(|(v, _)| !v.is_empty()) {
            self.update_tx(ctx, &mut tx, entity, keep, chosen, version).await?;
        }

        // The schema as it is now: a link left behind by an older view would point at nothing.
        let schema = self.registry.schema(ctx.tenant).await?;
        let (mut links, keep_text, dup_text) = (0u64, keep.to_string(), duplicate.to_string());
        for (table, column, in_ext, own) in link_columns(&schema, entity) {
            let t = q(&table);
            let sql = if in_ext {
                format!("update {t} set ext = jsonb_set(ext, array[$4], to_jsonb($1::text)) where tenant_id = $2 and ext->>$4 = $3")
            } else if own {
                // The record that stays no longer points at what it absorbed.
                let c = q(&column);
                format!("update {t} set {c} = case when id = $1::uuid then null else $1::uuid end where tenant_id = $2 and {c} = $3::uuid and $4 = $4")
            } else {
                let c = q(&column);
                format!("update {t} set {c} = $1::uuid where tenant_id = $2 and {c} = $3::uuid and $4 = $4")
            };
            links += sqlx::query(AssertSqlSafe(sql)).bind(&keep_text).bind(ctx.tenant).bind(&dup_text).bind(&column).execute(&mut *tx).await?.rows_affected();
        }
        for link in schema.merge_links.iter().filter(|l| l.to == entity) {
            let sql = format!("update {t} set {c} = $1 where tenant_id = $2 and {c} = $3", t = q(&link.table), c = q(&link.column));
            links += sqlx::query(AssertSqlSafe(sql)).bind(keep).bind(ctx.tenant).bind(duplicate).execute(&mut *tx).await?.rows_affected();
        }

        // What hangs off the record comes along; a tag or follower both had stays once.
        for table in ["record_comment", "record_assignment", "file_object"] {
            let sql = format!("update {table} set record_id = $1 where tenant_id = $2 and entity = $3 and record_id = $4");
            links += sqlx::query(AssertSqlSafe(sql)).bind(keep).bind(ctx.tenant).bind(entity).bind(duplicate).execute(&mut *tx).await?.rows_affected();
        }
        for (table, column) in [("record_tag", "tag"), ("record_follower", "user_id")] {
            let sql = format!(
                "delete from {table} d where d.tenant_id = $2 and d.entity = $3 and d.record_id = $4 \
                 and exists (select 1 from {table} k where k.tenant_id = d.tenant_id and k.entity = d.entity and k.record_id = $1 and k.{column} = d.{column})"
            );
            sqlx::query(AssertSqlSafe(sql)).bind(keep).bind(ctx.tenant).bind(entity).bind(duplicate).execute(&mut *tx).await?;
            let sql = format!("update {table} set record_id = $1 where tenant_id = $2 and entity = $3 and record_id = $4");
            links += sqlx::query(AssertSqlSafe(sql)).bind(keep).bind(ctx.tenant).bind(entity).bind(duplicate).execute(&mut *tx).await?.rows_affected();
        }

        // Records at a provider linked to the duplicate stand for the record that stays from now on: what
        // changes here goes there, and theirs comes here. Each is marked changed here since it was last
        // synced (no version is -1), so a provider's next change meets the merge as any change made here.
        // Where the record that stays is linked in that stream already, two records there are one here:
        // that link stays as it was, for a person to settle at the provider.
        links += sqlx::query(
            "update connection_link d set record_id = $1, local_version = -1 where d.tenant_id = $2 and d.entity = $3 and d.record_id = $4 \
             and not exists (select 1 from connection_link k where k.tenant_id = d.tenant_id and k.connection_id = d.connection_id and k.stream = d.stream and k.record_id = $1)",
        )
        .bind(keep)
        .bind(ctx.tenant)
        .bind(entity)
        .bind(duplicate)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let twice: Vec<(Uuid, String, String)> =
            sqlx::query_as("select connection_id, stream, external_id from connection_link where tenant_id = $1 and entity = $2 and record_id = $3")
                .bind(ctx.tenant)
                .bind(entity)
                .bind(duplicate)
                .fetch_all(&mut *tx)
                .await?;
        for (connection, stream, external_id) in twice {
            sqlx::query(
                "insert into connection_drift (tenant_id, id, connection_id, stream, external_id, kind, detail) values ($1, $2, $3, $4, $5, 'conflict', $6) \
                 on conflict (tenant_id, connection_id, stream, external_id, kind) where resolved_at is null do update set detail = excluded.detail",
            )
            .bind(ctx.tenant)
            .bind(Uuid::now_v7())
            .bind(connection)
            .bind(&stream)
            .bind(&external_id)
            .bind(json!({"record": keep, "merged": duplicate, "why": "two records there are one here since a merge: merge or unlink them there"}))
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query("update record_alias set canonical_id = $1 where tenant_id = $2 and entity = $3 and canonical_id = $4")
            .bind(keep)
            .bind(ctx.tenant)
            .bind(entity)
            .bind(duplicate)
            .execute(&mut *tx)
            .await?;
        sqlx::query("insert into record_alias (tenant_id, entity, alias_id, canonical_id, reason, merged_by) values ($1, $2, $3, $4, $5, $6)")
            .bind(ctx.tenant)
            .bind(entity)
            .bind(duplicate)
            .bind(keep)
            .bind(reason)
            .bind((!ctx.user.is_nil()).then_some(ctx.user))
            .execute(&mut *tx)
            .await?;
        audit::record(&mut tx, ctx, entity, keep, "merge", None, json!({"absorbed": duplicate, "links_moved": links, "reason": reason})).await?;
        audit::record(&mut tx, ctx, entity, duplicate, "merged_into", None, json!({"into": keep, "reason": reason})).await?;
        // Apps that keep copies of a record's name or address hear of it here.
        outbox::enqueue(&mut tx, ctx.tenant, &format!("{entity}.merged"), Some(entity), Some(keep), json!({"keep": keep, "duplicate": duplicate})).await?;
        tx.commit().await?;
        Ok(json!({"keep": keep, "merged": duplicate, "links_moved": links}))
    }

    /// Records that look like one another: equal, apart from case and spaces, in a field the entity
    /// lists (`duplicate_fields`). The caller's view: fields they cannot read and records outside
    /// their reach do not count.
    pub async fn duplicates(&self, ctx: &Ctx, entity: &str, limit: i64) -> Result<Vec<Value>> {
        let ent = self.entity(ctx, entity)?;
        if !ent.def.mergeable {
            return Err(Error::invalid(format!("{} records cannot be merged", ent.def.label)));
        }
        ctx.require(entity, Action::Delete)?;
        let scope = row_scope(ctx, &ent, Action::Read)?;
        let mut out = Vec::new();
        let mut tx = self.db.tenant_tx_with_timeout(ctx.tenant, self.db.config.report_timeout_ms).await?;
        for field in ent.def.duplicate_fields.iter().filter(|f| readable(ctx, &ent, f)) {
            let Some(expr) = crate::store::field_expr(&ent, field, "a") else { continue };
            let key = format!("regexp_replace(lower({expr}::text), '\\s+', '', 'g')");
            let mut qb = sqlx::QueryBuilder::new(format!("select {key}, array_agg(a.id order by a.created_at) from {} a where a.tenant_id = ", q(&ent.table)));
            qb.push_bind(ctx.tenant);
            qb.push(crate::store::entity_predicate(&ent, "a"));
            scope.push_sql(&mut qb, "a");
            qb.push(format!(" and {key} <> '' and not exists (select 1 from record_alias m where m.tenant_id = a.tenant_id and m.entity = "));
            qb.push_bind(entity);
            qb.push(" and m.alias_id = a.id) group by 1 having count(*) > 1 order by 1 limit ");
            qb.push_bind(limit.clamp(1, 200));
            let groups: Vec<(String, Vec<Uuid>)> = qb.build_query_as().fetch_all(&mut *tx).await?;
            out.extend(groups.into_iter().map(|(value, ids)| json!({"field": field, "value": value, "ids": ids})));
        }
        tx.commit().await?;
        Ok(out)
    }
}
