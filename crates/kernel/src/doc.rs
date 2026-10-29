//! The document lifecycle engine: one pipeline for every metadata-defined entity.
//!
//! insert/update: input → normalize → defaults → fetch_from → hooks.before_validate → computed →
//! validate (types, required, links, cycles, CEL rules) → hooks.validate → scope check →
//! numbering → write → hooks.after_save → audit + outbox. Every public method runs in one
//! transaction and is retried on serialization conflicts; `*_tx` variants compose inside hooks.

use crate::audit;
use crate::ctx::{Action, Ctx, row_scope};
use crate::db::{Tx, with_retry};
use crate::error::{Error, FieldError, Result};
use crate::hooks::HookCtx;
use crate::numbering;
use crate::outbox;
use crate::query::{self, Filter, ListQuery, MAX_LIMIT, MAX_OFFSET, Page};
use crate::store::{self, SYSTEM_KEYS, field_expr};
use crate::value::{is_empty, normalize};
use crate::{Doc, Kernel};
use ferrum_meta::{EffEntity, EntityKind, FieldDef, FieldType, NumberingOn, RuleOn, Scope, Storage};
use serde_json::{Map, Value, json};
use sqlx::{AssertSqlSafe, QueryBuilder};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

const MAX_CHILD_ROWS: usize = 5_000;
const MAX_EVENT_DOC_BYTES: usize = 256 * 1024;

pub fn uuid_of(v: Option<&Value>) -> Option<Uuid> {
    v.and_then(Value::as_str).and_then(|s| s.parse().ok())
}

fn version_of(doc: &Doc) -> i64 {
    doc.get("version").and_then(Value::as_i64).unwrap_or(1)
}

fn docstatus_of(doc: &Doc) -> i64 {
    doc.get("docstatus").and_then(Value::as_i64).unwrap_or(0)
}

fn guard_kind(ent: &EffEntity) -> Result<()> {
    match ent.def.kind {
        EntityKind::Child => Err(Error::invalid(format!("{} rows are edited through their parent document", ent.def.name))),
        EntityKind::Ledger => Err(Error::Forbidden(format!("{} is posted by the ledger engine and is read-only", ent.def.name))),
        _ => Ok(()),
    }
}

fn check_version(old: &Doc, expected: Option<i64>) -> Result<()> {
    match expected {
        Some(v) if v != version_of(old) => Err(Error::Conflict("this record was changed by someone else after you opened it; reload and try again".into())),
        _ => Ok(()),
    }
}

impl Kernel {
    pub fn entity(&self, ctx: &Ctx, name: &str) -> Result<Arc<EffEntity>> {
        ctx.schema.entity(name).cloned().ok_or_else(|| Error::NotFound(format!("entity {name}")))
    }

    // ───────────────────────────── reads ─────────────────────────────

    pub async fn get(&self, ctx: &Ctx, entity: &str, id: Uuid, with_titles: bool) -> Result<Doc> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let mut doc = self.get_tx(ctx, &mut tx, entity, id, false).await?;
        if with_titles {
            let ent = self.entity(ctx, entity)?;
            self.add_titles(ctx, &mut tx, &ent, std::slice::from_mut(&mut doc)).await?;
        }
        tx.commit().await?;
        Ok(doc)
    }

    pub async fn get_tx(&self, ctx: &Ctx, tx: &mut Tx, entity: &str, id: Uuid, lock: bool) -> Result<Doc> {
        let ent = self.entity(ctx, entity)?;
        guard_kind(&ent)?;
        let scope = row_scope(ctx, &ent, Action::Read)?;
        let doc = self.load_marked(tx, ctx, &ent, id, lock).await?.ok_or_else(|| Error::NotFound(ent.def.label.clone()))?;
        if !scope.matches(&doc) {
            return Err(Error::NotFound(ent.def.label.clone()));
        }
        Ok(self.mask(ctx, &ent, doc))
    }

    /// Removes fields in permission groups the caller cannot read.
    pub fn mask(&self, ctx: &Ctx, ent: &EffEntity, mut doc: Doc) -> Doc {
        if ctx.perms.admin {
            return doc;
        }
        for f in &ent.def.fields {
            if !ctx.perms.can_read_field(&ent.def.name, f) {
                doc.remove(&f.name);
            } else if f.ftype == FieldType::Table {
                let cent = f.of.as_deref().and_then(|c| ctx.schema.entity(c)).cloned();
                if let (Some(cent), Some(Value::Array(rows))) = (cent, doc.get_mut(&f.name)) {
                    for r in rows.iter_mut() {
                        if let Value::Object(m) = r {
                            for cf in &cent.def.fields {
                                if !ctx.perms.can_read_field(&cent.def.name, cf) {
                                    m.remove(&cf.name);
                                }
                            }
                        }
                    }
                }
            }
        }
        doc
    }

    pub async fn list(&self, ctx: &Ctx, entity: &str, q: &ListQuery) -> Result<Page> {
        let ent = self.entity(ctx, entity)?;
        guard_kind(&ent)?;
        let scope = row_scope(ctx, &ent, Action::Read)?;
        let limit = q.limit.unwrap_or(20).clamp(1, MAX_LIMIT) as i64;
        let offset = q.offset.unwrap_or(0);
        if offset > MAX_OFFSET {
            return Err(Error::invalid(format!("offset is limited to {MAX_OFFSET}; use cursor pagination")));
        }
        let (mut sort_field, mut desc) = query::parse_sort(&ent, q.sort.as_deref())?;
        if !query::readable(ctx, &ent, &sort_field) {
            // The order would reveal the field's values; a default sort the caller cannot see falls back.
            if q.sort.is_some() {
                return Err(Error::Forbidden(format!("you cannot sort by '{sort_field}'")));
            }
            (sort_field, desc) = ("created_at".to_string(), true);
        }
        let keyset_ok = matches!(sort_field.as_str(), "created_at" | "updated_at");
        if let Some(fields) = &q.fields {
            for f in fields {
                if !SYSTEM_KEYS.contains(&f.as_str()) && ent.field(f).is_none() {
                    return Err(Error::invalid(format!("unknown field '{f}'")));
                }
            }
        }
        let mut tx = self.db.tenant_tx_with_timeout(ctx.tenant, self.db.config.report_timeout_ms).await?;
        let mut qb = QueryBuilder::new(format!("select to_jsonb(x) from (select {}", store::select_list(&ent, "a")));
        query::push_from_where_for(&mut qb, ctx, &ent, &scope, &q.filters, q.search.as_deref())?;
        let dir = if desc { "desc" } else { "asc" };
        if let Some(c) = &q.cursor {
            if !keyset_ok {
                return Err(Error::invalid("cursor pagination requires sorting by created_at or updated_at"));
            }
            let (ts, id) = query::decode_cursor(c)?;
            let ts = crate::value::parse_datetime(&ts).map_err(Error::invalid)?;
            qb.push(format!(" and (a.{sort_field}, a.id) {} (", if desc { "<" } else { ">" })).push_bind(ts).push(", ").push_bind(id).push(")");
        }
        let sort_expr = field_expr(&ent, &sort_field, "a").unwrap_or_else(|| "a.created_at".into());
        qb.push(format!(" order by {sort_expr} {dir} nulls last, a.id {dir} limit ")).push_bind(limit + 1);
        if q.cursor.is_none() && offset > 0 {
            qb.push(" offset ").push_bind(offset as i64);
        }
        qb.push(") x");
        let rows: Vec<Value> = qb.build_query_scalar().fetch_all(&mut *tx).await?;
        let has_more = rows.len() as i64 > limit;
        let mut docs: Vec<Doc> = rows.into_iter().take(limit as usize).map(|r| self.mask(ctx, &ent, store::row_to_doc(&ent, &ctx.schema, r))).collect();
        let next_cursor = if has_more && keyset_ok {
            docs.last().and_then(|d| {
                let ts = d.get(&sort_field)?.as_str()?;
                let id = d.get("id")?.as_str()?;
                Some(query::encode_cursor(ts, id))
            })
        } else {
            None
        };
        let total = if q.with_count {
            let mut cq = QueryBuilder::new("select count(*)");
            query::push_from_where_for(&mut cq, ctx, &ent, &scope, &q.filters, q.search.as_deref())?;
            Some(cq.build_query_scalar::<i64>().fetch_one(&mut *tx).await?)
        } else {
            None
        };
        if q.with_titles {
            self.add_titles(ctx, &mut tx, &ent, &mut docs).await?;
        }
        tx.commit().await?;
        if let Some(fields) = &q.fields {
            for d in docs.iter_mut() {
                d.retain(|k, _| k == "id" || k == "_titles" || fields.iter().any(|f| f == k));
            }
        }
        Ok(Page { rows: docs, next_cursor, total })
    }

    /// Permission-checked lookup inside a transaction (for hooks and domain engines).
    pub async fn find_tx(&self, ctx: &Ctx, tx: &mut Tx, entity: &str, filters: &[Filter], sort: Option<&str>, limit: u32) -> Result<Vec<Doc>> {
        let ent = self.entity(ctx, entity)?;
        let scope = row_scope(ctx, &ent, Action::Read)?;
        let (sort_field, desc) = query::parse_sort(&ent, sort)?;
        let mut qb = QueryBuilder::new(format!("select to_jsonb(x) from (select {}", store::select_list(&ent, "a")));
        query::push_from_where(&mut qb, &ctx.schema, &ent, ctx.tenant, &scope, filters, None)?;
        let sort_expr = field_expr(&ent, &sort_field, "a").unwrap_or_else(|| "a.created_at".into());
        qb.push(format!(" order by {sort_expr} {}, a.id limit ", if desc { "desc" } else { "asc" })).push_bind(limit.clamp(1, 100_000) as i64).push(") x");
        let rows: Vec<Value> = qb.build_query_scalar().fetch_all(&mut **tx).await?;
        Ok(rows.into_iter().map(|r| store::row_to_doc(&ent, &ctx.schema, r)).collect())
    }

    /// Adds `_titles: {link_field: "display title"}` to documents and their child rows.
    pub async fn add_titles(&self, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, docs: &mut [Doc]) -> Result<()> {
        let schema = ctx.schema.clone();
        let mut wanted: HashMap<String, HashSet<Uuid>> = HashMap::new();
        let collect = |e: &EffEntity, d: &Doc, wanted: &mut HashMap<String, HashSet<Uuid>>| {
            for f in e.value_fields().filter(|f| f.ftype == FieldType::Link) {
                if let (Some(to), Some(id)) = (f.to.as_ref(), uuid_of(d.get(&f.name))) {
                    wanted.entry(to.clone()).or_default().insert(id);
                }
            }
        };
        for d in docs.iter() {
            collect(ent, d, &mut wanted);
            for (field, info) in &ent.children {
                if let (Some(cent), Some(Value::Array(rows))) = (schema.entity(&info.entity), d.get(field)) {
                    for r in rows.iter().filter_map(Value::as_object) {
                        collect(cent, r, &mut wanted);
                    }
                }
            }
        }
        let mut titles: HashMap<Uuid, String> = HashMap::new();
        for (target, ids) in wanted {
            let Some(tent) = schema.entity(&target) else { continue };
            if tent.table.is_empty() || tent.def.kind == EntityKind::Ledger {
                continue;
            }
            let title = tent.def.title_field.as_deref().and_then(|f| field_expr(tent, f, "a")).map(|e| format!("{e}::text")).unwrap_or_else(|| "null".into());
            let sql = format!(
                "select a.id, coalesce(nullif({title}, ''), a.number, a.id::text) from {} a where a.tenant_id = $1 and a.id = any($2){}",
                crate::ddl::q(&tent.table),
                store::entity_predicate(tent, "a")
            );
            let ids: Vec<Uuid> = ids.into_iter().collect();
            let rows: Vec<(Uuid, String)> = sqlx::query_as(AssertSqlSafe(sql)).bind(ctx.tenant).bind(&ids).fetch_all(&mut **tx).await?;
            titles.extend(rows);
        }
        let annotate = |e: &EffEntity, d: &mut Map<String, Value>| {
            let mut t = Map::new();
            for f in e.value_fields().filter(|f| f.ftype == FieldType::Link) {
                if let Some(title) = uuid_of(d.get(&f.name)).and_then(|id| titles.get(&id)) {
                    t.insert(f.name.clone(), Value::String(title.clone()));
                }
            }
            if !t.is_empty() {
                d.insert("_titles".into(), Value::Object(t));
            }
        };
        for d in docs.iter_mut() {
            annotate(ent, d);
            for (field, info) in &ent.children {
                if let (Some(cent), Some(Value::Array(rows))) = (schema.entity(&info.entity), d.get_mut(field)) {
                    for r in rows.iter_mut() {
                        if let Value::Object(m) = r {
                            annotate(cent, m);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    // ───────────────────────────── writes ─────────────────────────────

    pub async fn insert(&self, ctx: &Ctx, entity: &str, input: Doc) -> Result<Doc> {
        with_retry(|| async {
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            let doc = self.insert_tx(ctx, &mut tx, entity, input.clone()).await?;
            tx.commit().await?;
            Ok(doc)
        })
        .await
    }

    pub async fn insert_tx(&self, ctx: &Ctx, tx: &mut Tx, entity: &str, input: Doc) -> Result<Doc> {
        self.insert_inner(ctx, tx, entity, input, None).await
    }

    /// Insert guarded by an idempotency key: the key and the response commit in the same
    /// transaction as the record, so a retried request returns the original result instead of
    /// creating a duplicate. Returns `(doc, replayed)`.
    pub async fn insert_idempotent(&self, ctx: &Ctx, entity: &str, input: Doc, key: &str, request_hash: &str) -> Result<(Doc, bool)> {
        self.insert_idempotent_with(ctx, entity, input, key, request_hash, false).await
    }

    /// Creates and submits a document in one transaction: it is never left as a draft (offline
    /// clients syncing finished sales, integrations posting final documents).
    pub async fn insert_submit(&self, ctx: &Ctx, entity: &str, input: Doc) -> Result<Doc> {
        with_retry(|| async {
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            let doc = self.insert_tx(ctx, &mut tx, entity, input.clone()).await?;
            let doc = self.submit_tx(ctx, &mut tx, entity, store::doc_id(&doc)?, None).await?;
            tx.commit().await?;
            Ok(doc)
        })
        .await
    }

    /// Like `insert_idempotent`; with `submit` the document is also submitted in the same transaction.
    pub async fn insert_idempotent_with(&self, ctx: &Ctx, entity: &str, input: Doc, key: &str, request_hash: &str, submit: bool) -> Result<(Doc, bool)> {
        if key.len() < 8 || key.len() > 200 {
            return Err(Error::invalid("Idempotency-Key must be 8-200 characters"));
        }
        with_retry(|| async {
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            let inserted = sqlx::query("insert into idempotency_key (tenant_id, key, request_hash) values ($1, $2, $3) on conflict do nothing")
                .bind(ctx.tenant)
                .bind(key)
                .bind(request_hash)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if inserted == 0 {
                let (hash, response): (String, Option<Value>) =
                    sqlx::query_as("select request_hash, response from idempotency_key where tenant_id = $1 and key = $2")
                        .bind(ctx.tenant)
                        .bind(key)
                        .fetch_one(&mut *tx)
                        .await?;
                tx.rollback().await?;
                if hash != request_hash {
                    return Err(Error::Conflict("this Idempotency-Key was already used for a different request".into()));
                }
                return match response {
                    Some(Value::Object(m)) => Ok((m, true)),
                    _ => Err(Error::Conflict("a request with this Idempotency-Key is still being processed".into())),
                };
            }
            let mut doc = self.insert_tx(ctx, &mut tx, entity, input.clone()).await?;
            if submit {
                doc = self.submit_tx(ctx, &mut tx, entity, store::doc_id(&doc)?, None).await?;
            }
            sqlx::query("update idempotency_key set status_code = 201, response = $3 where tenant_id = $1 and key = $2")
                .bind(ctx.tenant)
                .bind(key)
                .bind(Value::Object(doc.clone()))
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok((doc, false))
        })
        .await
    }

    /// Runs the derivation part of the pipeline (defaults, fetches, hooks, computed fields) on an
    /// unsaved document and returns the result; nothing is written. Used for live totals in forms.
    pub async fn preview(&self, ctx: &Ctx, entity: &str, input: Doc) -> Result<Doc> {
        let ent = self.entity(ctx, entity)?;
        guard_kind(&ent)?;
        ctx.require(entity, Action::Read)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let existing = match uuid_of(input.get("id")) {
            Some(id) => self.load_marked(&mut tx, ctx, &ent, id, false).await?,
            None => None,
        };
        let mut doc = existing.clone().unwrap_or_default();
        let mut errors = Vec::new();
        self.apply_input(ctx, &ent, &mut doc, &input, existing.as_ref(), &mut errors, false, &mut Vec::new());
        if !errors.is_empty() {
            return Err(Error::Validation(errors));
        }
        doc.entry("docstatus".to_string()).or_insert(json!(0));
        if existing.is_none() {
            self.fill_defaults(ctx, &ent, &mut doc)?;
        }
        self.fetch_links(ctx, &mut tx, &ent, &mut doc, existing.as_ref()).await?;
        self.hook_before_validate(ctx, &mut tx, &ent, &mut doc).await?;
        self.compute(ctx, &ent, &mut doc)?;
        tx.rollback().await?;
        Ok(self.mask(ctx, &ent, doc))
    }

    async fn insert_inner(&self, ctx: &Ctx, tx: &mut Tx, entity: &str, input: Doc, amended_from: Option<Uuid>) -> Result<Doc> {
        let ent = self.entity(ctx, entity)?;
        guard_kind(&ent)?;
        ctx.require(entity, Action::Create)?;
        let id = match input.get("id") {
            None | Some(Value::Null) => Uuid::now_v7(),
            Some(v) => uuid_of(Some(v)).ok_or_else(|| Error::field("id", "must be a UUID"))?,
        };
        let mut doc = Doc::new();
        let mut errors = Vec::new();
        // An amendment copies the cancelled original: its read-only values (links to the documents
        // it was made from, derived fields) are the server's own, not user input.
        let mut secrets = Vec::new();
        self.apply_input(ctx, &ent, &mut doc, &input, None, &mut errors, amended_from.is_some(), &mut secrets);
        if !errors.is_empty() {
            return Err(Error::Validation(errors));
        }
        let actor = if !ctx.user.is_nil() { Value::String(ctx.user.to_string()) } else { Value::Null };
        doc.insert("id".into(), json!(id.to_string()));
        doc.insert("docstatus".into(), json!(0));
        doc.insert("version".into(), json!(1));
        doc.insert("created_by".into(), actor.clone());
        doc.insert("updated_by".into(), actor);
        doc.insert("amended_from".into(), amended_from.map(|u| json!(u.to_string())).unwrap_or(Value::Null));
        doc.insert("number".into(), Value::Null);
        doc.insert("workflow_state".into(), ent.workflow.as_ref().map(|w| json!(w.def.initial_state)).unwrap_or(Value::Null));
        self.fill_defaults(ctx, &ent, &mut doc)?;
        self.fetch_links(ctx, tx, &ent, &mut doc, None).await?;
        self.hook_before_validate(ctx, tx, &ent, &mut doc).await?;
        self.compute(ctx, &ent, &mut doc)?;
        self.validate(ctx, tx, &ent, &doc, None, RuleOn::Save).await?;
        self.hook_validate(ctx, tx, &ent, &doc, None).await?;
        let scope = row_scope(ctx, &ent, Action::Create)?;
        if !scope.matches(&doc) {
            return Err(Error::Forbidden("this record is outside the companies or scopes you are permitted to use".into()));
        }
        if ent.def.numbering.as_ref().is_some_and(|n| n.on == NumberingOn::Insert) {
            let n = numbering::assign(&self.db, ctx, tx, &ent, &doc).await?;
            doc.insert("number".into(), json!(n));
        }
        store::insert_row(tx, &ctx.schema, &ent, ctx.tenant, &doc).await?;
        self.write_secrets(ctx, tx, &ent, id, &secrets).await?;
        let stored = self.load_marked(tx, ctx, &ent, id, false).await?.ok_or_else(|| Error::internal("insert vanished"))?;
        self.hook_after_save(ctx, tx, &ent, &stored, None).await?;
        self.emit(ctx, tx, &ent, id, "created", "create", Some(1), None, Some(&stored)).await?;
        Ok(self.mask(ctx, &ent, stored))
    }

    pub async fn update(&self, ctx: &Ctx, entity: &str, id: Uuid, patch: Doc, expected_version: Option<i64>) -> Result<Doc> {
        with_retry(|| async {
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            let doc = self.update_tx(ctx, &mut tx, entity, id, patch.clone(), expected_version).await?;
            tx.commit().await?;
            Ok(doc)
        })
        .await
    }

    pub async fn update_tx(&self, ctx: &Ctx, tx: &mut Tx, entity: &str, id: Uuid, patch: Doc, expected_version: Option<i64>) -> Result<Doc> {
        let ent = self.entity(ctx, entity)?;
        guard_kind(&ent)?;
        let scope = row_scope(ctx, &ent, Action::Write)?;
        let old = self.load_marked(tx, ctx, &ent, id, true).await?.ok_or_else(|| Error::NotFound(ent.def.label.clone()))?;
        if !scope.matches(&old) {
            return Err(Error::NotFound(ent.def.label.clone()));
        }
        check_version(&old, expected_version)?;
        if ent.is_submittable() && docstatus_of(&old) != 0 {
            return Err(Error::invalid(format!(
                "a {} {} cannot be edited; cancel and amend it instead",
                if docstatus_of(&old) == 1 { "submitted" } else { "cancelled" },
                ent.def.label
            )));
        }
        self.check_workflow_edit(ctx, &ent, &old)?;
        let mut doc = old.clone();
        let mut errors = Vec::new();
        let mut secrets = Vec::new();
        self.apply_input(ctx, &ent, &mut doc, &patch, Some(&old), &mut errors, false, &mut secrets);
        if !errors.is_empty() {
            return Err(Error::Validation(errors));
        }
        self.fill_new_row_defaults(ctx, &ent, &mut doc, &old)?;
        self.fetch_links(ctx, tx, &ent, &mut doc, Some(&old)).await?;
        self.hook_before_validate(ctx, tx, &ent, &mut doc).await?;
        self.compute(ctx, &ent, &mut doc)?;
        self.validate(ctx, tx, &ent, &doc, Some(&old), RuleOn::Save).await?;
        self.hook_validate(ctx, tx, &ent, &doc, Some(&old)).await?;
        if !scope.matches(&doc) {
            return Err(Error::Forbidden("you cannot move this record outside your permitted scope".into()));
        }
        let version = version_of(&old) + 1;
        doc.insert("version".into(), json!(version));
        doc.insert("updated_by".into(), if !ctx.user.is_nil() { json!(ctx.user.to_string()) } else { Value::Null });
        store::update_row(tx, &ctx.schema, &ent, ctx.tenant, &doc, &old).await?;
        self.write_secrets(ctx, tx, &ent, id, &secrets).await?;
        let stored = self.load_marked(tx, ctx, &ent, id, false).await?.ok_or_else(|| Error::internal("update vanished"))?;
        self.hook_after_save(ctx, tx, &ent, &stored, Some(&old)).await?;
        self.emit(ctx, tx, &ent, id, "updated", "update", Some(version), Some(&old), Some(&stored)).await?;
        Ok(self.mask(ctx, &ent, stored))
    }

    pub async fn delete(&self, ctx: &Ctx, entity: &str, id: Uuid, expected_version: Option<i64>) -> Result<()> {
        with_retry(|| async {
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            self.delete_tx(ctx, &mut tx, entity, id, expected_version).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    pub async fn delete_tx(&self, ctx: &Ctx, tx: &mut Tx, entity: &str, id: Uuid, expected_version: Option<i64>) -> Result<()> {
        let ent = self.entity(ctx, entity)?;
        guard_kind(&ent)?;
        let scope = row_scope(ctx, &ent, Action::Delete)?;
        let old = self.load_marked(tx, ctx, &ent, id, true).await?.ok_or_else(|| Error::NotFound(ent.def.label.clone()))?;
        if !scope.matches(&old) {
            return Err(Error::NotFound(ent.def.label.clone()));
        }
        check_version(&old, expected_version)?;
        if ent.is_submittable() && docstatus_of(&old) == 1 {
            return Err(Error::invalid("submitted documents cannot be deleted; cancel first"));
        }
        for h in self.hooks.for_tenant(&ent.def.name, &ctx.schema.packages) {
            let mut hc = HookCtx { kernel: self, ctx, tx: &mut *tx, entity: &ent };
            h.before_delete(&mut hc, &old).await?;
        }
        self.refuse_if_linked_in_ext(ctx, tx, &ent, id).await?;
        store::delete_row(tx, &ent, ctx.tenant, id).await?;
        // Collaboration on the record goes with it (followers are told by the deleted event first).
        for table in ["record_comment", "record_assignment", "record_tag", "record_secret"] {
            sqlx::query(AssertSqlSafe(format!("delete from {table} where tenant_id = $1 and entity = $2 and record_id = $3")))
                .bind(ctx.tenant)
                .bind(&ent.def.name)
                .bind(id)
                .execute(&mut **tx)
                .await?;
        }
        self.emit(ctx, tx, &ent, id, "deleted", "delete", None, Some(&old), None).await
    }

    /// Link fields stored in the JSON column (custom fields, fields apps add to other packages'
    /// entities) have no foreign keys, so a delete checks them as a key would: a record that
    /// another record still links to stays.
    // ponytail: one scan per such field; add expression indexes on `ext->>field` if deletes of
    // heavily referenced records get slow.
    async fn refuse_if_linked_in_ext(&self, ctx: &Ctx, tx: &mut Tx, target: &EffEntity, id: Uuid) -> Result<()> {
        let key = id.to_string();
        let mut checks: Vec<(String, String, &'static str)> = Vec::new();
        for e in ctx.schema.entities.values().filter(|e| !e.table.is_empty()) {
            for f in e.value_fields().filter(|f| f.ftype == FieldType::Link && f.to.as_deref() == Some(&target.def.name)) {
                if e.storage_of(&f.name) == Some(ferrum_meta::Storage::Ext) {
                    checks.push((e.table.clone(), f.name.clone(), "id"));
                }
            }
            for info in e.children.values() {
                let Some(c) = ctx.schema.entity(&info.entity) else { continue };
                for f in c.value_fields().filter(|f| f.ftype == FieldType::Link && f.to.as_deref() == Some(&target.def.name)) {
                    if c.storage_of(&f.name) == Some(ferrum_meta::Storage::Ext) {
                        checks.push((info.table.clone(), f.name.clone(), "parent_id"));
                    }
                }
            }
        }
        for (table, field, own) in checks {
            // The record's own rows (a self-link, its child rows) go with it.
            let sql = format!("select exists(select 1 from {} where tenant_id = $1 and ext->>$2 = $3 and {own} <> $4)", crate::ddl::q(&table));
            let linked: bool = sqlx::query_scalar(AssertSqlSafe(sql)).bind(ctx.tenant).bind(&field).bind(&key).bind(id).fetch_one(&mut **tx).await?;
            if linked {
                return Err(Error::Conflict(format!("cannot delete this {}: other records link to it ({field})", target.def.label.to_lowercase())));
            }
        }
        Ok(())
    }

    pub async fn submit(&self, ctx: &Ctx, entity: &str, id: Uuid, expected_version: Option<i64>) -> Result<Doc> {
        with_retry(|| async {
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            let doc = self.submit_tx(ctx, &mut tx, entity, id, expected_version).await?;
            tx.commit().await?;
            Ok(doc)
        })
        .await
    }

    pub async fn submit_tx(&self, ctx: &Ctx, tx: &mut Tx, entity: &str, id: Uuid, expected_version: Option<i64>) -> Result<Doc> {
        self.submit_inner(ctx, tx, entity, id, expected_version, None).await
    }

    async fn submit_inner(&self, ctx: &Ctx, tx: &mut Tx, entity: &str, id: Uuid, expected_version: Option<i64>, workflow_state: Option<String>) -> Result<Doc> {
        let ent = self.entity(ctx, entity)?;
        guard_kind(&ent)?;
        if !ent.is_submittable() {
            return Err(Error::invalid(format!("{} is not submittable", ent.def.label)));
        }
        if ent.workflow.is_some() && workflow_state.is_none() && !ctx.is_system {
            return Err(Error::invalid("this document is controlled by a workflow; use a workflow action"));
        }
        let scope = row_scope(ctx, &ent, Action::Submit)?;
        let old = self.load_marked(tx, ctx, &ent, id, true).await?.ok_or_else(|| Error::NotFound(ent.def.label.clone()))?;
        if !scope.matches(&old) {
            return Err(Error::NotFound(ent.def.label.clone()));
        }
        check_version(&old, expected_version)?;
        if docstatus_of(&old) != 0 {
            return Err(Error::invalid("only draft documents can be submitted"));
        }
        let mut doc = old.clone();
        self.hook_before_validate(ctx, tx, &ent, &mut doc).await?;
        self.compute(ctx, &ent, &mut doc)?;
        self.validate(ctx, tx, &ent, &doc, Some(&old), RuleOn::Submit).await?;
        self.hook_validate(ctx, tx, &ent, &doc, Some(&old)).await?;
        for h in self.hooks.for_tenant(&ent.def.name, &ctx.schema.packages) {
            let mut hc = HookCtx { kernel: self, ctx, tx: &mut *tx, entity: &ent };
            h.before_submit(&mut hc, &mut doc).await?;
        }
        if ent.def.numbering.as_ref().is_some_and(|n| n.on == NumberingOn::Submit) && is_empty(doc.get("number").unwrap_or(&Value::Null)) {
            let n = numbering::assign(&self.db, ctx, tx, &ent, &doc).await?;
            doc.insert("number".into(), json!(n));
        }
        if let Some(ws) = workflow_state {
            doc.insert("workflow_state".into(), json!(ws));
        }
        let version = version_of(&old) + 1;
        doc.insert("docstatus".into(), json!(1));
        doc.insert("version".into(), json!(version));
        doc.insert("updated_by".into(), if !ctx.user.is_nil() { json!(ctx.user.to_string()) } else { Value::Null });
        store::update_row(tx, &ctx.schema, &ent, ctx.tenant, &doc, &old).await?;
        let stored = self.load_marked(tx, ctx, &ent, id, false).await?.ok_or_else(|| Error::internal("submit vanished"))?;
        for h in self.hooks.for_tenant(&ent.def.name, &ctx.schema.packages) {
            let mut hc = HookCtx { kernel: self, ctx, tx: &mut *tx, entity: &ent };
            h.on_submit(&mut hc, &stored).await?;
        }
        // Hooks derive values (status, outstanding, progress); return and publish the final state.
        let stored = self.load_marked(tx, ctx, &ent, id, false).await?.ok_or_else(|| Error::internal("submit vanished"))?;
        self.emit(ctx, tx, &ent, id, "submitted", "submit", Some(version), Some(&old), Some(&stored)).await?;
        Ok(self.mask(ctx, &ent, stored))
    }

    pub async fn cancel(&self, ctx: &Ctx, entity: &str, id: Uuid, expected_version: Option<i64>) -> Result<Doc> {
        with_retry(|| async {
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            let doc = self.cancel_tx(ctx, &mut tx, entity, id, expected_version).await?;
            tx.commit().await?;
            Ok(doc)
        })
        .await
    }

    pub async fn cancel_tx(&self, ctx: &Ctx, tx: &mut Tx, entity: &str, id: Uuid, expected_version: Option<i64>) -> Result<Doc> {
        self.cancel_inner(ctx, tx, entity, id, expected_version, None).await
    }

    async fn cancel_inner(&self, ctx: &Ctx, tx: &mut Tx, entity: &str, id: Uuid, expected_version: Option<i64>, workflow_state: Option<String>) -> Result<Doc> {
        let ent = self.entity(ctx, entity)?;
        guard_kind(&ent)?;
        if !ent.is_submittable() {
            return Err(Error::invalid(format!("{} is not submittable", ent.def.label)));
        }
        if ent.workflow.is_some() && workflow_state.is_none() && !ctx.is_system {
            return Err(Error::invalid("this document is controlled by a workflow; use a workflow action"));
        }
        let scope = row_scope(ctx, &ent, Action::Cancel)?;
        let old = self.load_marked(tx, ctx, &ent, id, true).await?.ok_or_else(|| Error::NotFound(ent.def.label.clone()))?;
        if !scope.matches(&old) {
            return Err(Error::NotFound(ent.def.label.clone()));
        }
        check_version(&old, expected_version)?;
        if docstatus_of(&old) != 1 {
            return Err(Error::invalid("only submitted documents can be cancelled"));
        }
        for h in self.hooks.for_tenant(&ent.def.name, &ctx.schema.packages) {
            let mut hc = HookCtx { kernel: self, ctx, tx: &mut *tx, entity: &ent };
            h.before_cancel(&mut hc, &old).await?;
        }
        let mut doc = old.clone();
        if let Some(ws) = workflow_state {
            doc.insert("workflow_state".into(), json!(ws));
        }
        let version = version_of(&old) + 1;
        doc.insert("docstatus".into(), json!(2));
        doc.insert("version".into(), json!(version));
        doc.insert("updated_by".into(), if !ctx.user.is_nil() { json!(ctx.user.to_string()) } else { Value::Null });
        store::update_row(tx, &ctx.schema, &ent, ctx.tenant, &doc, &old).await?;
        let stored = self.load_marked(tx, ctx, &ent, id, false).await?.ok_or_else(|| Error::internal("cancel vanished"))?;
        for h in self.hooks.for_tenant(&ent.def.name, &ctx.schema.packages) {
            let mut hc = HookCtx { kernel: self, ctx, tx: &mut *tx, entity: &ent };
            h.on_cancel(&mut hc, &stored).await?;
        }
        // Hooks derive values (status, outstanding, progress); return and publish the final state.
        let stored = self.load_marked(tx, ctx, &ent, id, false).await?.ok_or_else(|| Error::internal("cancel vanished"))?;
        self.emit(ctx, tx, &ent, id, "cancelled", "cancel", Some(version), Some(&old), Some(&stored)).await?;
        Ok(self.mask(ctx, &ent, stored))
    }

    /// Creates a new draft from a cancelled document (`amended_from` links them).
    pub async fn amend(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Result<Doc> {
        with_retry(|| async {
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            let ent = self.entity(ctx, entity)?;
            let src = self.get_tx(ctx, &mut tx, entity, id, true).await?;
            if docstatus_of(&src) != 2 {
                return Err(Error::invalid("only cancelled documents can be amended"));
            }
            let input = copy_for_new(&ent, &ctx.schema, &src);
            let doc = self.insert_inner(ctx, &mut tx, entity, input, Some(id)).await?;
            tx.commit().await?;
            Ok(doc)
        })
        .await
    }

    /// Runs a custom action declared in metadata and implemented by a hook.
    pub async fn run_action(&self, ctx: &Ctx, entity: &str, id: Uuid, action: &str, params: Value) -> Result<Value> {
        let read_only = self.entity(ctx, entity).is_ok_and(|e| e.def.actions.iter().any(|a| a.name == action && a.read_only));
        with_retry(|| async {
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            if read_only {
                // The action promised not to write; the database holds it to that.
                sqlx::query("set transaction read only").execute(&mut *tx).await?;
            }
            let out = self.run_action_tx(ctx, &mut tx, entity, id, action, &params).await?;
            tx.commit().await?;
            Ok(out)
        })
        .await
    }

    pub async fn run_action_tx(&self, ctx: &Ctx, tx: &mut Tx, entity: &str, id: Uuid, action: &str, params: &Value) -> Result<Value> {
        let ent = self.entity(ctx, entity)?;
        guard_kind(&ent)?;
        let def = ent.def.actions.iter().find(|a| a.name == action).ok_or_else(|| Error::NotFound(format!("action {action}")))?;
        let perm = Action::parse(&def.permission).unwrap_or(Action::Write);
        let scope = row_scope(ctx, &ent, perm)?;
        let doc = self.load_marked(tx, ctx, &ent, id, !def.read_only).await?.ok_or_else(|| Error::NotFound(ent.def.label.clone()))?;
        if !scope.matches(&doc) {
            return Err(Error::NotFound(ent.def.label.clone()));
        }
        if !def.docstatus.is_empty() && !def.docstatus.contains(&(docstatus_of(&doc) as i16)) {
            return Err(Error::invalid(format!("'{}' is not available in this document status", def.label)));
        }
        if let Some(expr) = ent.exprs.action_conditions.get(action) {
            let view = ent.cel_view(&Value::Object(doc.clone()), &ctx.schema);
            let user = ctx.user_value();
            if !expr.eval_bool(&Scope { doc: &view, old: None, parent: None, user: &user }).map_err(Error::invalid)? {
                return Err(Error::invalid(format!("'{}' is not available for this document", def.label)));
            }
        }
        let hooks = self.hooks.for_tenant(&ent.def.name, &ctx.schema.packages);
        for h in &hooks {
            let mut hc = HookCtx { kernel: self, ctx, tx: &mut *tx, entity: &ent };
            if let Some(mut out) = h.action(&mut hc, action, &doc, params).await? {
                if !def.read_only {
                    let now = self.load_marked(tx, ctx, &ent, id, false).await?.ok_or_else(|| Error::NotFound(ent.def.label.clone()))?;
                    for other in &hooks {
                        let mut hc = HookCtx { kernel: self, ctx, tx: &mut *tx, entity: &ent };
                        other.after_action(&mut hc, action, &now, &mut out).await?;
                    }
                    audit::record(tx, ctx, &ent.def.name, id, &format!("action:{action}"), None, params.clone()).await?;
                }
                return Ok(out);
            }
        }
        Err(Error::NotFound(format!("handler for action {action}")))
    }

    /// Applies a workflow transition, submitting/cancelling when the target state requires it.
    pub async fn transition(&self, ctx: &Ctx, entity: &str, id: Uuid, action: &str, expected_version: Option<i64>) -> Result<Doc> {
        with_retry(|| async {
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            let doc = self.transition_tx(ctx, &mut tx, entity, id, action, expected_version).await?;
            tx.commit().await?;
            Ok(doc)
        })
        .await
    }

    pub async fn transition_tx(&self, ctx: &Ctx, tx: &mut Tx, entity: &str, id: Uuid, action: &str, expected_version: Option<i64>) -> Result<Doc> {
        let ent = self.entity(ctx, entity)?;
        let wf = ent.workflow.clone().ok_or_else(|| Error::invalid(format!("{} has no workflow", ent.def.label)))?;
        let scope = row_scope(ctx, &ent, Action::Read)?;
        let doc = self.load_marked(tx, ctx, &ent, id, true).await?.ok_or_else(|| Error::NotFound(ent.def.label.clone()))?;
        if !scope.matches(&doc) {
            return Err(Error::NotFound(ent.def.label.clone()));
        }
        check_version(&doc, expected_version)?;
        let current = doc.get("workflow_state").and_then(Value::as_str).unwrap_or(&wf.def.initial_state).to_string();
        let t = wf
            .def
            .transitions
            .iter()
            .find(|t| t.action == action && t.from == current)
            .ok_or_else(|| Error::invalid(format!("'{action}' is not allowed in state '{current}'")))?;
        if !t.roles.is_empty() && !t.roles.iter().any(|r| ctx.has_role(r)) {
            return Err(Error::Forbidden(format!("'{action}' requires one of the roles: {}", t.roles.join(", "))));
        }
        if t.not_creator && !ctx.is_system && uuid_of(doc.get("created_by")) == Some(ctx.user) {
            return Err(Error::Forbidden("the creator of a document cannot approve it".into()));
        }
        if let Some(guard) = wf.guards.get(&(t.action.clone(), t.from.clone())) {
            let view = ent.cel_view(&Value::Object(doc.clone()), &ctx.schema);
            let user = ctx.user_value();
            if !guard.eval_bool(&Scope { doc: &view, old: None, parent: None, user: &user }).map_err(Error::invalid)? {
                return Err(Error::invalid(format!("the conditions for '{action}' are not met")));
            }
        }
        let target = wf.def.states.iter().find(|s| s.name == t.to).ok_or_else(|| Error::internal("workflow target state missing"))?;
        let from_status = docstatus_of(&doc) as i16;
        let version = version_of(&doc);
        match (from_status, target.docstatus) {
            (0, 1) => return self.submit_inner(ctx, tx, entity, id, Some(version), Some(t.to.clone())).await,
            (1, 2) => return self.cancel_inner(ctx, tx, entity, id, Some(version), Some(t.to.clone())).await,
            (a, b) if a == b => {}
            _ => return Err(Error::invalid("this workflow transition would move the document backwards")),
        }
        let mut values = Doc::new();
        values.insert("workflow_state".into(), json!(t.to));
        sqlx::query(AssertSqlSafe(format!(
            "update {} a set workflow_state = $3, version = version + 1, updated_at = now(), updated_by = $4 where a.tenant_id = $1 and a.id = $2{}",
            crate::ddl::q(&ent.table),
            store::entity_predicate(&ent, "a")
        )))
        .bind(ctx.tenant)
        .bind(id)
        .bind(&t.to)
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .execute(&mut **tx)
        .await?;
        let stored = self.load_marked(tx, ctx, &ent, id, false).await?.ok_or_else(|| Error::internal("transition vanished"))?;
        self.emit(ctx, tx, &ent, id, "transitioned", &format!("workflow:{action}"), Some(version + 1), Some(&doc), Some(&stored)).await?;
        Ok(self.mask(ctx, &ent, stored))
    }

    /// Engine-level update of derived fields (status, progress, outstanding). No validation/hooks/audit:
    /// these are consequences of an audited action in the same transaction.
    pub async fn set_values_tx(&self, ctx: &Ctx, tx: &mut Tx, entity: &str, id: Uuid, values: Doc) -> Result<()> {
        let ent = self.entity(ctx, entity)?;
        let mut normalized = Doc::new();
        for (k, v) in values {
            let f = ent.field(&k).ok_or_else(|| Error::Internal(format!("set_values: unknown field {k}")))?;
            normalized.insert(k.clone(), normalize(f, &v).map_err(|m| Error::Internal(format!("set_values {k}: {m}")))?);
        }
        store::set_values(tx, &ent, ctx.tenant, id, &normalized).await
    }

    /// Derived-field update on one child row (e.g. delivered/billed quantities on order lines).
    pub async fn set_child_values_tx(&self, ctx: &Ctx, tx: &mut Tx, parent: &str, table_field: &str, row_id: Uuid, values: Doc) -> Result<()> {
        let ent = self.entity(ctx, parent)?;
        let info = ent.children.get(table_field).ok_or_else(|| Error::Internal(format!("{parent} has no table {table_field}")))?;
        let cent = self.entity(ctx, &info.entity)?;
        let mut normalized = Doc::new();
        for (k, v) in values {
            let f = cent.field(&k).ok_or_else(|| Error::Internal(format!("set_child_values: unknown field {k}")))?;
            normalized.insert(k.clone(), normalize(f, &v).map_err(|m| Error::Internal(format!("set_child_values {k}: {m}")))?);
        }
        store::set_values_in(tx, &info.table, &cent, ctx.tenant, row_id, &normalized).await
    }

    /// Engine-kept rows of one child table (a work order's crew taken from its appointments, say):
    /// replaces them with `rows`, each normalized by field type and keeping its `id` when given,
    /// without the parent's hooks or a new version.
    pub async fn set_child_rows_tx(&self, ctx: &Ctx, tx: &mut Tx, parent: &str, table_field: &str, parent_id: Uuid, rows: Vec<Doc>) -> Result<()> {
        let ent = self.entity(ctx, parent)?;
        let info = ent.children.get(table_field).ok_or_else(|| Error::Internal(format!("{parent} has no table {table_field}")))?;
        let cent = self.entity(ctx, &info.entity)?;
        let mut out = Vec::with_capacity(rows.len());
        for (i, r) in rows.into_iter().enumerate() {
            let mut row = Doc::new();
            for (k, v) in r {
                if k == "id" {
                    let rid = uuid_of(Some(&v)).ok_or_else(|| Error::Internal("set_child_rows: id must be a UUID".into()))?;
                    row.insert(k, json!(rid.to_string()));
                    continue;
                }
                let f = cent.field(&k).ok_or_else(|| Error::Internal(format!("set_child_rows: unknown field {k}")))?;
                row.insert(k.clone(), normalize(f, &v).map_err(|m| Error::Internal(format!("set_child_rows {k}: {m}")))?);
            }
            row.entry("id".to_string()).or_insert_with(|| json!(Uuid::now_v7().to_string()));
            row.insert("idx".into(), json!(i));
            out.push(Value::Object(row));
        }
        store::set_child_rows(tx, &info.table, &cent, ctx.tenant, parent_id, out).await
    }

    /// Loads a stored record and marks which of its secret fields are set.
    async fn load_marked(&self, tx: &mut Tx, ctx: &Ctx, ent: &EffEntity, id: Uuid, lock: bool) -> Result<Option<Doc>> {
        let mut doc = store::load(tx, &ctx.schema, ent, ctx.tenant, id, lock).await?;
        if let Some(d) = doc.as_mut() {
            self.mark_secrets(tx, ctx.tenant, ent, d).await?;
        }
        Ok(doc)
    }

    // ───────────────────────────── pipeline ─────────────────────────────

    fn check_workflow_edit(&self, ctx: &Ctx, ent: &EffEntity, doc: &Doc) -> Result<()> {
        let Some(wf) = &ent.workflow else { return Ok(()) };
        let state = doc.get("workflow_state").and_then(Value::as_str).unwrap_or(&wf.def.initial_state);
        if let Some(s) = wf.def.states.iter().find(|s| s.name == state)
            && !s.edit_roles.is_empty()
            && !s.edit_roles.iter().any(|r| ctx.has_role(r))
        {
            return Err(Error::Forbidden(format!("documents in state '{state}' can only be edited by: {}", s.edit_roles.join(", "))));
        }
        Ok(())
    }

    /// `trusted`: the input is a copy made by the server (amend), so read-only and field-permission
    /// limits that guard user input do not apply.
    #[allow(clippy::too_many_arguments)]
    fn apply_input(
        &self,
        ctx: &Ctx,
        ent: &EffEntity,
        doc: &mut Doc,
        input: &Doc,
        old: Option<&Doc>,
        errors: &mut Vec<FieldError>,
        trusted: bool,
        secrets: &mut Vec<crate::secret::SecretChange>,
    ) {
        for (k, v) in input {
            if SYSTEM_KEYS.contains(&k.as_str()) || k.starts_with('_') {
                continue;
            }
            let Some(f) = ent.field(k) else {
                errors.push(FieldError::new(k, "unknown field"));
                continue;
            };
            if f.computed.is_some() || (f.read_only && !ctx.is_system && !trusted) {
                continue;
            }
            if !trusted && !ctx.perms.can_write_field(&ent.def.name, f) {
                let unchanged = old.map(|o| o.get(k) == Some(v)).unwrap_or(v.is_null());
                if !unchanged {
                    errors.push(FieldError::new(k, "you are not permitted to change this field"));
                }
                continue;
            }
            if f.ftype == FieldType::Secret {
                match crate::secret::take_input(k, v, secrets) {
                    Ok(Some(marker)) => {
                        doc.insert(k.clone(), marker);
                    }
                    Ok(None) => {}
                    Err(m) => errors.push(FieldError::new(k, m)),
                }
                continue;
            }
            if f.ftype == FieldType::Table {
                let Some(cent) = f.of.as_deref().and_then(|c| ctx.schema.entity(c)).cloned() else { continue };
                match v {
                    Value::Null => {
                        doc.insert(k.clone(), Value::Array(vec![]));
                    }
                    Value::Array(rows) => {
                        if rows.len() > MAX_CHILD_ROWS {
                            errors.push(FieldError::new(k, format!("at most {MAX_CHILD_ROWS} rows")));
                            continue;
                        }
                        let old_rows = old.and_then(|o| o.get(k)).and_then(Value::as_array).cloned().unwrap_or_default();
                        let merged = self.merge_rows(ctx, &cent, k, rows, &old_rows, errors, trusted);
                        doc.insert(k.clone(), Value::Array(merged));
                    }
                    _ => errors.push(FieldError::new(k, "must be a list of rows")),
                }
                continue;
            }
            match normalize(f, v) {
                Ok(n) => {
                    doc.insert(k.clone(), n);
                }
                Err(m) => errors.push(FieldError::new(k, m)),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn merge_rows(
        &self,
        ctx: &Ctx,
        cent: &EffEntity,
        table: &str,
        rows: &[Value],
        old_rows: &[Value],
        errors: &mut Vec<FieldError>,
        trusted: bool,
    ) -> Vec<Value> {
        let old_by_id: HashMap<String, &Map<String, Value>> =
            old_rows.iter().filter_map(Value::as_object).filter_map(|r| r.get("id").and_then(Value::as_str).map(|i| (i.to_string(), r))).collect();
        let mut seen = HashSet::new();
        let mut out = Vec::with_capacity(rows.len());
        for (idx, r) in rows.iter().enumerate() {
            let Some(r) = r.as_object() else {
                errors.push(FieldError::new(table, "each row must be an object").in_row(table, idx));
                continue;
            };
            let incoming = r.get("id").and_then(Value::as_str).map(str::to_string);
            let (id, base) = match incoming {
                Some(i) if old_by_id.contains_key(&i) && seen.insert(i.clone()) => (i.clone(), old_by_id[&i].clone()),
                _ => (Uuid::now_v7().to_string(), Map::new()),
            };
            let mut row = base;
            for (k, v) in r {
                if k == "id" || k == "idx" || k.starts_with('_') {
                    continue;
                }
                let Some(f) = cent.field(k) else {
                    errors.push(FieldError::new(k, "unknown field").in_row(table, idx));
                    continue;
                };
                if f.computed.is_some() || (f.read_only && !ctx.is_system && !trusted) {
                    continue;
                }
                if !trusted && !ctx.perms.can_write_field(&cent.def.name, f) {
                    continue;
                }
                match normalize(f, v) {
                    Ok(n) => {
                        row.insert(k.clone(), n);
                    }
                    Err(m) => errors.push(FieldError::new(k, m).in_row(table, idx)),
                }
            }
            row.insert("id".into(), json!(id));
            row.insert("idx".into(), json!(idx));
            out.push(Value::Object(row));
        }
        out
    }

    fn fill_defaults(&self, ctx: &Ctx, ent: &EffEntity, doc: &mut Doc) -> Result<()> {
        let user = ctx.user_value();
        fill_entity_defaults(ent, doc, None, &user, &ctx.schema)?;
        let parent_view = ent.cel_view(&Value::Object(doc.clone()), &ctx.schema);
        for (field, info) in &ent.children {
            let Some(cent) = ctx.schema.entity(&info.entity) else { continue };
            if let Some(Value::Array(rows)) = doc.get_mut(field) {
                for r in rows.iter_mut() {
                    if let Value::Object(m) = r {
                        fill_entity_defaults(cent, m, Some(&parent_view), &user, &ctx.schema)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Child rows added while editing get their defaults, like rows of a new document.
    fn fill_new_row_defaults(&self, ctx: &Ctx, ent: &EffEntity, doc: &mut Doc, old: &Doc) -> Result<()> {
        let user = ctx.user_value();
        let parent_view = ent.cel_view(&Value::Object(doc.clone()), &ctx.schema);
        for (field, info) in &ent.children {
            let Some(cent) = ctx.schema.entity(&info.entity) else { continue };
            let existing: HashSet<&str> = old
                .get(field)
                .and_then(Value::as_array)
                .map(|rows| rows.iter().filter_map(|r| r.get("id").and_then(Value::as_str)).collect())
                .unwrap_or_default();
            if let Some(Value::Array(rows)) = doc.get_mut(field) {
                for r in rows.iter_mut() {
                    if let Value::Object(m) = r
                        && !m.get("id").and_then(Value::as_str).is_some_and(|id| existing.contains(id))
                    {
                        fill_entity_defaults(cent, m, Some(&parent_view), &user, &ctx.schema)?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn fetch_links(&self, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Doc, old: Option<&Doc>) -> Result<()> {
        let empty = Map::new();
        fetch_for(self, ctx, tx, ent, doc, old.unwrap_or(&empty), old.is_none()).await?;
        for (field, info) in &ent.children {
            let Some(cent) = ctx.schema.entity(&info.entity).cloned() else { continue };
            if !cent.def.fields.iter().any(|f| f.fetch_from.is_some()) {
                continue;
            }
            let old_rows: HashMap<String, Map<String, Value>> = old
                .and_then(|o| o.get(field))
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter().filter_map(Value::as_object).filter_map(|r| r.get("id").and_then(Value::as_str).map(|i| (i.to_string(), r.clone()))).collect()
                })
                .unwrap_or_default();
            let mut rows = match doc.get(field) {
                Some(Value::Array(r)) => r.clone(),
                _ => continue,
            };
            for r in rows.iter_mut() {
                if let Value::Object(m) = r {
                    let rid = m.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                    let (base, is_new) = match old_rows.get(&rid) {
                        Some(o) => (o.clone(), false),
                        None => (Map::new(), true),
                    };
                    fetch_for(self, ctx, tx, &cent, m, &base, is_new).await?;
                }
            }
            doc.insert(field.clone(), Value::Array(rows));
        }
        Ok(())
    }

    fn compute(&self, ctx: &Ctx, ent: &EffEntity, doc: &mut Doc) -> Result<()> {
        let user = ctx.user_value();
        compute_entity(ent, doc, None, &user, &ctx.schema)?;
        if ent.children.is_empty() {
            return Ok(());
        }
        let parent_view = ent.cel_view(&Value::Object(doc.clone()), &ctx.schema);
        for (field, info) in &ent.children {
            let Some(cent) = ctx.schema.entity(&info.entity) else { continue };
            if cent.exprs.computed.is_empty() {
                continue;
            }
            if let Some(Value::Array(rows)) = doc.get_mut(field) {
                for r in rows.iter_mut() {
                    if let Value::Object(m) = r {
                        compute_entity(cent, m, Some(&parent_view), &user, &ctx.schema)?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn validate(&self, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &Doc, old: Option<&Doc>, on: RuleOn) -> Result<()> {
        let user = ctx.user_value();
        let view = ent.cel_view(&Value::Object(doc.clone()), &ctx.schema);
        let old_view = old.map(|o| ent.cel_view(&Value::Object(o.clone()), &ctx.schema));
        let mut errors = Vec::new();
        check_required(ent, doc, &view, None, old_view.as_ref(), &user, &mut errors, None);
        for f in ent.table_fields() {
            let rows = doc.get(&f.name).and_then(Value::as_array).cloned().unwrap_or_default();
            if f.required && rows.is_empty() {
                errors.push(FieldError::new(&f.name, "add at least one row"));
            }
            let Some(cent) = f.of.as_deref().and_then(|c| ctx.schema.entity(c)).cloned() else { continue };
            for (i, r) in rows.iter().enumerate() {
                let Some(rm) = r.as_object() else { continue };
                let rview = cent.cel_view(r, &ctx.schema);
                check_required(&cent, rm, &rview, Some(&view), None, &user, &mut errors, Some((&f.name, i)));
                for (rule, expr) in &cent.exprs.rules {
                    if rule.on == RuleOn::Submit && on != RuleOn::Submit {
                        continue;
                    }
                    match expr.eval_bool(&Scope { doc: &rview, old: None, parent: Some(&view), user: &user }) {
                        Ok(true) => {}
                        Ok(false) => errors.push(FieldError::new(&rule.name, &rule.message).in_row(&f.name, i)),
                        Err(e) => errors.push(FieldError::new(&rule.name, format!("rule could not be evaluated: {e}")).in_row(&f.name, i)),
                    }
                }
                self.check_links(ctx, tx, &cent, rm, &mut errors, Some((&f.name, i))).await?;
            }
        }
        self.check_links(ctx, tx, ent, doc, &mut errors, None).await?;
        for (rule, expr) in &ent.exprs.rules {
            if rule.on == RuleOn::Submit && on != RuleOn::Submit {
                continue;
            }
            match expr.eval_bool(&Scope { doc: &view, old: old_view.as_ref(), parent: None, user: &user }) {
                Ok(true) => {}
                Ok(false) => errors.push(FieldError::new(&rule.name, &rule.message)),
                Err(e) => errors.push(FieldError::new(&rule.name, format!("rule could not be evaluated: {e}"))),
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    /// Existence for links without a foreign key, link filters, and tree cycles.
    async fn check_links(&self, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &Doc, errors: &mut Vec<FieldError>, row: Option<(&str, usize)>) -> Result<()> {
        let push = |errors: &mut Vec<FieldError>, f: &str, m: String| {
            let e = FieldError::new(f, m);
            errors.push(match row {
                Some((t, i)) => e.in_row(t, i),
                None => e,
            });
        };
        for f in ent.value_fields().filter(|f| f.ftype == FieldType::Link) {
            let Some(id) = uuid_of(doc.get(&f.name)) else { continue };
            let Some(target) = f.to.as_deref().and_then(|t| ctx.schema.entity(t)).cloned() else {
                push(errors, &f.name, "links to an entity that is not installed".into());
                continue;
            };
            let needs_lookup = f.link_filters.is_some() || ent.storage_of(&f.name) == Some(Storage::Ext) || target.custom || ent.custom;
            if needs_lookup {
                let found = store::load_many(tx, &ctx.schema, &target, ctx.tenant, &[id]).await?;
                match found.first() {
                    None => push(errors, &f.name, format!("{} does not exist", target.def.label)),
                    Some(t) => {
                        if let Some(filters) = &f.link_filters {
                            for (k, expected) in filters {
                                let actual = t.get(k).cloned().unwrap_or(Value::Null);
                                let same = match (expected, &actual) {
                                    (Value::Bool(b), Value::Null) => !*b,
                                    _ => &actual == expected,
                                };
                                if !same {
                                    push(errors, &f.name, format!("this {} cannot be used here ({k} must be {expected})", target.def.label));
                                }
                            }
                        }
                    }
                }
            }
            if f.to.as_deref() == Some(ent.def.name.as_str()) && ent.storage_of(&f.name) == Some(Storage::Column) && row.is_none() {
                let self_id = uuid_of(doc.get("id"));
                if self_id == Some(id) {
                    push(errors, &f.name, "a record cannot be its own parent".into());
                    continue;
                }
                let sql = format!(
                    "with recursive anc(id, parent, depth) as ( \
                       select a.id, a.{c}, 1 from {t} a where a.tenant_id = $1 and a.id = $2 \
                       union all select a.id, a.{c}, anc.depth + 1 from {t} a join anc on a.id = anc.parent \
                       where a.tenant_id = $1 and anc.depth < 200) \
                     select exists(select 1 from anc where id = $3)",
                    c = crate::ddl::q(&f.name),
                    t = crate::ddl::q(&ent.table)
                );
                let cycle: bool = sqlx::query_scalar(AssertSqlSafe(sql)).bind(ctx.tenant).bind(id).bind(self_id).fetch_one(&mut **tx).await?;
                if cycle {
                    push(errors, &f.name, "this would create a cycle in the hierarchy".into());
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn emit(
        &self,
        ctx: &Ctx,
        tx: &mut Tx,
        ent: &EffEntity,
        id: Uuid,
        event: &str,
        audit_action: &str,
        version: Option<i64>,
        old: Option<&Doc>,
        new: Option<&Doc>,
    ) -> Result<()> {
        audit::record(tx, ctx, &ent.def.name, id, audit_action, version.map(|v| v as i32), audit::diff(old, new)).await?;
        let mut payload = json!({
            "entity": ent.def.name,
            "id": id,
            "event": event,
            "actor": (!ctx.user.is_nil()).then_some(ctx.user),
            "depth": ctx.depth,
            "request_id": ctx.request_id,
        });
        if let Some(d) = new.or(old) {
            let v = Value::Object(d.clone());
            if serde_json::to_vec(&v).map(|b| b.len()).unwrap_or(usize::MAX) <= MAX_EVENT_DOC_BYTES {
                payload["doc"] = v;
            }
        }
        outbox::enqueue(tx, ctx.tenant, &format!("{}.{event}", ent.def.name), Some(&ent.def.name), Some(id), payload).await
    }

    async fn hook_before_validate(&self, ctx: &Ctx, tx: &mut Tx, ent: &Arc<EffEntity>, doc: &mut Doc) -> Result<()> {
        for h in self.hooks.for_tenant(&ent.def.name, &ctx.schema.packages) {
            let mut hc = HookCtx { kernel: self, ctx, tx: &mut *tx, entity: ent };
            h.before_validate(&mut hc, doc).await?;
        }
        Ok(())
    }

    async fn hook_validate(&self, ctx: &Ctx, tx: &mut Tx, ent: &Arc<EffEntity>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        for h in self.hooks.for_tenant(&ent.def.name, &ctx.schema.packages) {
            let mut hc = HookCtx { kernel: self, ctx, tx: &mut *tx, entity: ent };
            h.validate(&mut hc, doc).await?;
            h.validate_change(&mut hc, old, doc).await?;
        }
        Ok(())
    }

    async fn hook_after_save(&self, ctx: &Ctx, tx: &mut Tx, ent: &Arc<EffEntity>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        for h in self.hooks.for_tenant(&ent.def.name, &ctx.schema.packages) {
            let mut hc = HookCtx { kernel: self, ctx, tx: &mut *tx, entity: ent };
            h.after_save(&mut hc, doc, old).await?;
        }
        Ok(())
    }
}

fn fill_entity_defaults(ent: &EffEntity, doc: &mut Map<String, Value>, parent: Option<&Value>, user: &Value, schema: &ferrum_meta::Schema) -> Result<()> {
    for f in ent.value_fields() {
        if !doc.get(&f.name).is_none_or(Value::is_null) {
            continue;
        }
        let value = if let Some(lit) = &f.default {
            Some(normalize(f, lit).map_err(|m| Error::Internal(format!("default of {}.{}: {m}", ent.def.name, f.name)))?)
        } else if let Some(expr) = ent.exprs.default_expr.get(&f.name) {
            let view = ent.cel_view(&Value::Object(doc.clone()), schema);
            let v =
                expr.eval(&Scope { doc: &view, old: None, parent, user }).map_err(|e| Error::field(&f.name, format!("default could not be evaluated: {e}")))?;
            Some(normalize(f, &v).map_err(|m| Error::field(&f.name, m))?)
        } else if f.ftype == FieldType::Check {
            Some(Value::Bool(false))
        } else {
            None
        };
        if let Some(v) = value {
            doc.insert(f.name.clone(), v);
        }
    }
    Ok(())
}

fn compute_entity(ent: &EffEntity, doc: &mut Map<String, Value>, parent: Option<&Value>, user: &Value, schema: &ferrum_meta::Schema) -> Result<()> {
    for (name, expr) in &ent.exprs.computed {
        let Some(f) = ent.field(name) else { continue };
        let view = ent.cel_view(&Value::Object(doc.clone()), schema);
        let v = expr.eval(&Scope { doc: &view, old: None, parent, user }).map_err(|e| Error::field(name, format!("could not be computed: {e}")))?;
        doc.insert(name.clone(), normalize(f, &v).map_err(|m| Error::field(name, m))?);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn check_required(
    ent: &EffEntity,
    doc: &Map<String, Value>,
    view: &Value,
    parent: Option<&Value>,
    old: Option<&Value>,
    user: &Value,
    errors: &mut Vec<FieldError>,
    row: Option<(&str, usize)>,
) {
    let scope = Scope { doc: view, old, parent, user };
    for f in ent.value_fields() {
        if f.ftype == FieldType::Check {
            continue;
        }
        let required = f.required || ent.exprs.required_if.get(&f.name).is_some_and(|e| e.eval_bool(&scope).unwrap_or(false));
        if !required || !is_empty(doc.get(&f.name).unwrap_or(&Value::Null)) {
            continue;
        }
        let visible = ent.exprs.visible_if.get(&f.name).is_none_or(|e| e.eval_bool(&scope).unwrap_or(true));
        if visible {
            let e = FieldError::new(&f.name, format!("{} is required", f.label()));
            errors.push(match row {
                Some((t, i)) => e.in_row(t, i),
                None => e,
            });
        }
    }
}

async fn fetch_for(k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Map<String, Value>, old: &Map<String, Value>, is_new: bool) -> Result<()> {
    let fields: Vec<&FieldDef> = ent.value_fields().filter(|f| f.fetch_from.is_some()).collect();
    if fields.is_empty() {
        return Ok(());
    }
    let mut cache: HashMap<(String, Uuid), Option<Doc>> = HashMap::new();
    for f in fields {
        let (link, target_field) = f.fetch_from.as_deref().and_then(|s| s.split_once('.')).unwrap_or_default();
        let current = doc.get(link).cloned().unwrap_or(Value::Null);
        let changed = is_new || old.get(link) != Some(&current);
        let empty = is_empty(doc.get(&f.name).unwrap_or(&Value::Null));
        if !changed && !empty {
            continue;
        }
        let Some(id) = uuid_of(Some(&current)) else {
            if changed {
                doc.insert(f.name.clone(), Value::Null);
            }
            continue;
        };
        let Some(target) = ent.field(link).and_then(|lf| lf.to.as_deref()).and_then(|t| ctx.schema.entity(t)).cloned() else { continue };
        let key = (target.def.name.clone(), id);
        if !cache.contains_key(&key) {
            let found = store::load_many(tx, &ctx.schema, &target, ctx.tenant, &[id]).await?.into_iter().next();
            cache.insert(key.clone(), found);
        }
        let value = cache[&key].as_ref().and_then(|d| d.get(target_field)).cloned().unwrap_or(Value::Null);
        let value = normalize(f, &value).unwrap_or(Value::Null);
        doc.insert(f.name.clone(), value);
    }
    let _ = k;
    Ok(())
}

/// A copy of a document suitable for creating a new record (amend/duplicate).
pub fn copy_for_new(ent: &EffEntity, schema: &ferrum_meta::Schema, src: &Doc) -> Doc {
    let mut out = Doc::new();
    for f in &ent.def.fields {
        if f.no_copy || f.computed.is_some() || f.ftype == FieldType::Secret {
            continue;
        }
        let Some(v) = src.get(&f.name) else { continue };
        if f.ftype == FieldType::Table {
            let Some(cent) = f.of.as_deref().and_then(|c| schema.entity(c)) else { continue };
            let rows: Vec<Value> = v
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|r| r.as_object().cloned())
                .map(|r| {
                    let mut m = Map::new();
                    for cf in &cent.def.fields {
                        if !cf.no_copy
                            && cf.computed.is_none()
                            && let Some(x) = r.get(&cf.name)
                        {
                            m.insert(cf.name.clone(), x.clone());
                        }
                    }
                    Value::Object(m)
                })
                .collect();
            out.insert(f.name.clone(), Value::Array(rows));
        } else {
            out.insert(f.name.clone(), v.clone());
        }
    }
    out
}
