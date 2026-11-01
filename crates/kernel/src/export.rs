//! Work the job queue does for the platform itself: bulk operations over more records than a request
//! should wait for, and a workspace's export, the copy a customer takes with them.
//!
//! An export holds everything a workspace can read back without Ferrum: every record as JSON lines
//! with its child rows, the engine tables packages declare (`export_tables`: journals, stock moves),
//! every attached file, the audit trail, and the configuration (roles, permissions, customizations,
//! automations, people). It never holds passwords, secret values or sealed settings, and its
//! manifest names every workspace table it left out, so what it lacks is never a surprise. Large
//! exports go into several zip files of about [`PART_BYTES`], readable only by administrators
//! ([`Kernel::export_part`], never `/files/{id}`), and are deleted after [`EXPORT_DAYS`] days.

use crate::bulk::{BulkOp, BulkTarget, MAX_BULK};
use crate::ctx::Ctx;
use crate::ddl::q;
use crate::error::{Error, Result};
use crate::jobs::{Job, JobHandler, NewJob, enqueue};
use crate::outbox::{PRIORITY_BULK, release_objects};
use crate::query::{ListQuery, MAX_LIMIT, parse_filters};
use crate::{Kernel, audit};
use async_trait::async_trait;
use ferrum_meta::EntityKind;
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;
use zip::write::SimpleFileOptions;

pub const BULK_KIND: &str = "platform.bulk";
pub const EXPORT_KIND: &str = "platform.export";
/// A part of an export closes at about this much before compression (a larger file gets a part to
/// itself, held in memory while it is written).
const PART_BYTES: usize = 128 << 20;
/// How long a finished export stays.
const EXPORT_DAYS: i32 = 7;
/// Stored parts are `file_object` rows of this "entity", with the job as their record: nothing
/// resolves it, so `/files/{id}` serves them to nobody.
const EXPORT_ENTITY: &str = "ferrum.export";
/// The most records one bulk job reaches.
pub const MAX_JOB_RECORDS: usize = 100_000;
/// Failures a bulk job's result lists; beyond this it counts them.
const LISTED_FAILURES: usize = 1000;
/// Audit events to a file of the export.
const AUDIT_PAGE: i64 = 10_000;
/// Rows to a file of an engine table's export, and how long reading one table may take.
const TABLE_PAGE: usize = 10_000;
const TABLE_TIMEOUT_MS: u64 = 30 * 60 * 1000;

pub(crate) fn handler(kind: &str) -> Option<Arc<dyn JobHandler>> {
    match kind {
        BULK_KIND => Some(Arc::new(BulkJob)),
        EXPORT_KIND => Some(Arc::new(ExportJob)),
        crate::retention::RETENTION_KIND => Some(Arc::new(crate::retention::RetentionJob)),
        _ => None,
    }
}

/// How far a bulk job has got. Ids are worked through in order, so a worker lost part-way leaves
/// the next attempt the rest, and repeats no more than the chunk it was on.
#[derive(Debug, Default, Serialize, Deserialize)]
struct BulkProgress {
    after: Option<Uuid>,
    total: usize,
    succeeded: u64,
    failed: u64,
    failures: Vec<Value>,
}

struct BulkJob;

#[async_trait]
impl JobHandler for BulkJob {
    fn kind(&self) -> &'static str {
        BULK_KIND
    }

    fn title(&self) -> &'static str {
        "Bulk operation"
    }

    async fn run(&self, k: &Kernel, job: &Job) -> Result<Value> {
        let entity = job.payload["entity"].as_str().ok_or_else(|| Error::invalid("a bulk job names its entity"))?;
        let op: BulkOp = serde_json::from_value(job.payload["op"].clone()).map_err(|e| Error::field("op", e.to_string()))?;
        let mut ids: Vec<Uuid> = job.payload["ids"].as_array().into_iter().flatten().filter_map(|v| v.as_str()?.parse().ok()).collect();
        if let Some(filters) = job.payload.get("filters").filter(|f| !f.is_null()) {
            // The set is fixed before anything changes: an operation that stops records matching
            // would otherwise shift the pages under it and skip some.
            let ctx = job.creator(k).await?;
            let filters = parse_filters(filters)?;
            let mut cursor = None;
            loop {
                let q = ListQuery {
                    filters: filters.clone(),
                    fields: Some(vec!["id".into()]),
                    sort: Some("created_at".into()),
                    limit: Some(MAX_LIMIT),
                    cursor,
                    ..Default::default()
                };
                let page = k.list(&ctx, entity, &q).await?;
                ids.extend(page.rows.iter().filter_map(|r| r.get("id")?.as_str()?.parse::<Uuid>().ok()));
                if ids.len() > MAX_JOB_RECORDS {
                    return Err(Error::invalid(format!("the filter matches more than {MAX_JOB_RECORDS} records")));
                }
                cursor = page.next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
        }
        ids.sort();
        ids.dedup();
        let mut done: BulkProgress = serde_json::from_value(job.resumed.clone()).unwrap_or_default();
        ids.retain(|id| done.after.is_none_or(|after| *id > after));
        done.total = (done.succeeded + done.failed) as usize + ids.len();
        for chunk in ids.chunks(MAX_BULK) {
            // As they are now, not as they were when this began: what they lost stops here.
            let ctx = job.creator(k).await?;
            let outcome = k.bulk(&ctx, entity, BulkTarget::Ids(chunk.to_vec()), &op).await?;
            done.succeeded += outcome["succeeded"].as_u64().unwrap_or(0);
            done.failed += outcome["failed"].as_u64().unwrap_or(0);
            let room = LISTED_FAILURES.saturating_sub(done.failures.len());
            done.failures.extend(outcome["results"].as_array().into_iter().flatten().filter(|r| r["ok"] == json!(false)).take(room).cloned());
            done.after = chunk.last().copied();
            job.progress(k, serde_json::to_value(&done).map_err(Error::internal)?).await?;
        }
        Ok(json!({"total": done.total, "succeeded": done.succeeded, "failed": done.failed, "failures": done.failures}))
    }
}

/// The open part of an export.
struct Parts<'a> {
    k: &'a Kernel,
    ctx: &'a Ctx,
    job: Uuid,
    zip: zip::ZipWriter<Cursor<Vec<u8>>>,
    size: usize,
    stored: Vec<Value>,
}

impl<'a> Parts<'a> {
    fn new(k: &'a Kernel, ctx: &'a Ctx, job: Uuid) -> Self {
        Parts { k, ctx, job, zip: zip::ZipWriter::new(Cursor::new(Vec::new())), size: 0, stored: Vec::new() }
    }

    async fn add(&mut self, name: &str, data: &[u8]) -> Result<()> {
        if self.size > 0 && self.size + data.len() > PART_BYTES {
            self.close().await?;
        }
        let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).large_file(data.len() >= u32::MAX as usize);
        self.zip.start_file(name, options).map_err(|e| Error::internal(format!("writing the export: {e}")))?;
        self.zip.write_all(data).map_err(|e| Error::internal(format!("writing the export: {e}")))?;
        self.size += data.len();
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        let finished = std::mem::replace(&mut self.zip, zip::ZipWriter::new(Cursor::new(Vec::new())));
        let bytes = finished.finish().map_err(|e| Error::internal(format!("writing the export: {e}")))?.into_inner();
        let name = format!("workspace-export-{}-part-{}.zip", self.job, self.stored.len() + 1);
        self.stored.push(self.k.keep_file(self.ctx, Some((EXPORT_ENTITY, self.job)), &name, "application/zip", &bytes, "unscanned").await?);
        self.size = 0;
        Ok(())
    }
}

fn lines(rows: &[Value]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut out, row).map_err(Error::internal)?;
        out.push(b'\n');
    }
    Ok(out)
}

/// The workspace's own tables that go into an export, beside its records: file, table, what to read.
/// Anything holding a secret is named column by column, so a column added later stays out until
/// someone decides otherwise.
const TABLES: &[(&str, &str, &str)] = &[
    (
        "config/people",
        "app_user",
        "select jsonb_build_object('id', id, 'email', email, 'full_name', full_name, 'kind', kind, 'is_active', is_active, 'is_admin', is_admin, \
         'created_at', created_at, 'last_login_at', last_login_at) from app_user where tenant_id = $1 order by id",
    ),
    ("config/roles", "role", "select to_jsonb(t) - 'tenant_id' from role t where tenant_id = $1 order by name"),
    ("config/role-members", "user_role", "select to_jsonb(t) - 'tenant_id' from user_role t where tenant_id = $1 order by user_id, role"),
    ("config/record-scopes", "user_scope", "select to_jsonb(t) - 'tenant_id' from user_scope t where tenant_id = $1 order by user_id, entity, value"),
    ("config/permissions", "role_permission", "select to_jsonb(t) - 'tenant_id' from role_permission t where tenant_id = $1 order by role, entity"),
    ("config/customizations", "meta_overlay", "select to_jsonb(t) - 'tenant_id' from meta_overlay t where tenant_id = $1 order by seq"),
    ("config/automations", "automation", "select to_jsonb(t) - 'tenant_id' from automation t where tenant_id = $1 order by id"),
    (
        "config/webhooks",
        "webhook",
        "select jsonb_build_object('id', id, 'name', name, 'url', url, 'topics', topics, 'is_active', is_active, 'created_at', created_at) \
         from webhook where tenant_id = $1 order by id",
    ),
    ("collaboration/comments", "record_comment", "select to_jsonb(t) - 'tenant_id' from record_comment t where tenant_id = $1"),
    ("collaboration/tags", "record_tag", "select to_jsonb(t) - 'tenant_id' from record_tag t where tenant_id = $1"),
    ("collaboration/assignments", "record_assignment", "select to_jsonb(t) - 'tenant_id' from record_assignment t where tenant_id = $1"),
    ("collaboration/followers", "record_follower", "select to_jsonb(t) - 'tenant_id' from record_follower t where tenant_id = $1"),
    ("compliance/legal-holds", "legal_hold", "select to_jsonb(t) - 'tenant_id' from legal_hold t where tenant_id = $1"),
];

struct ExportJob;

#[async_trait]
impl JobHandler for ExportJob {
    fn kind(&self) -> &'static str {
        EXPORT_KIND
    }

    fn title(&self) -> &'static str {
        "Workspace export"
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(6 * 3600)
    }

    async fn run(&self, k: &Kernel, job: &Job) -> Result<Value> {
        let ctx = job.creator(k).await?;
        if !ctx.is_admin {
            return Err(Error::Forbidden("exporting a workspace is for administrators".into()));
        }
        // All of it, whatever record scopes the administrator happens to have.
        let all = Ctx::system(ctx.tenant, ctx.schema.clone());
        let mut parts = Parts::new(k, &ctx, job.id);
        let mut entities = Vec::new();
        let total = all.schema.entities.len();
        for (i, (name, ent)) in all.schema.entities.iter().enumerate() {
            if ent.def.kind == EntityKind::Child || ent.table.is_empty() {
                continue;
            }
            let (mut file, mut count, mut cursor) = (0, 0usize, None::<String>);
            loop {
                let (rows, next) = k.whole_records(&all, name, !ent.children.is_empty(), cursor).await?;
                count += rows.len();
                if !rows.is_empty() {
                    parts.add(&format!("records/{name}/{file:06}.jsonl"), &lines(&rows)?).await?;
                    file += 1;
                }
                cursor = next;
                if cursor.is_none() {
                    break;
                }
            }
            if count > 0 {
                entities.push(json!({"entity": name, "records": count}));
            }
            job.progress(k, json!({"step": "records", "done": i + 1, "total": total})).await?;
        }

        // What the API does not read: each table in one snapshot, so a journal comes out balanced.
        let ledgers = all.schema.entities.values().filter(|e| e.def.kind == EntityKind::Ledger).map(|e| e.table.clone());
        let engines: Vec<String> = ledgers.chain(all.schema.export_tables.iter().cloned()).collect();
        for table in &engines {
            let mut tx = k.db.tenant_tx_with_timeout(ctx.tenant, TABLE_TIMEOUT_MS).await?;
            let sql = format!("select to_jsonb(t) - 'tenant_id' from {} t where t.tenant_id = $1", q(table));
            let mut rows = sqlx::query_scalar::<_, Value>(sqlx::AssertSqlSafe(sql)).bind(ctx.tenant).fetch(&mut *tx);
            let (mut page, mut file, mut count) = (Vec::with_capacity(TABLE_PAGE), 0, 0usize);
            while let Some(row) = rows.try_next().await? {
                page.push(row);
                count += 1;
                if page.len() == TABLE_PAGE {
                    parts.add(&format!("tables/{table}/{file:06}.jsonl"), &lines(&page)?).await?;
                    page.clear();
                    file += 1;
                }
            }
            drop(rows);
            tx.commit().await?;
            if !page.is_empty() {
                parts.add(&format!("tables/{table}/{file:06}.jsonl"), &lines(&page)?).await?;
            }
            if count > 0 {
                entities.push(json!({"table": table, "records": count}));
            }
        }

        for (name, _, sql) in TABLES {
            let mut tx = k.db.tenant_tx(ctx.tenant).await?;
            let rows: Vec<Value> = sqlx::query_scalar(*sql).bind(ctx.tenant).fetch_all(&mut *tx).await?;
            tx.commit().await?;
            parts.add(&format!("{name}.jsonl"), &lines(&rows)?).await?;
        }

        let (mut after, mut file) = (0i64, 0);
        loop {
            let mut tx = k.db.tenant_tx(ctx.tenant).await?;
            let rows: Vec<(i64, Value)> = sqlx::query_as(
                "select id, jsonb_build_object('id', id, 'at', at, 'actor', actor, 'entity', entity, 'record_id', record_id, 'action', action, 'version', version, \
                 'diff', diff, 'hash', encode(hash, 'hex'), 'prev_hash', encode(prev_hash, 'hex')) from audit_event where tenant_id = $1 and id > $2 order by id limit $3",
            )
            .bind(ctx.tenant)
            .bind(after)
            .bind(AUDIT_PAGE)
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            let Some((last, _)) = rows.last() else { break };
            after = *last;
            let events: Vec<Value> = rows.into_iter().map(|r| r.1).collect();
            parts.add(&format!("audit/{file:06}.jsonl"), &lines(&events)?).await?;
            file += 1;
        }
        job.progress(k, json!({"step": "files", "done": 0})).await?;

        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let files: Vec<(Uuid, String, String, String, Option<String>, Option<Uuid>)> = sqlx::query_as(
            "select id, file_name, content_type, storage_key, entity, record_id from file_object where tenant_id = $1 and entity is distinct from $2 order by id",
        )
        .bind(ctx.tenant)
        .bind(EXPORT_ENTITY)
        .fetch_all(&mut *tx)
        .await?;
        let workspace: (String, String) = sqlx::query_as("select slug, name from tenant where id = $1").bind(ctx.tenant).fetch_one(&mut *tx).await?;
        let carried: Vec<&str> = all
            .schema
            .entities
            .values()
            .flat_map(|e| std::iter::once(e.table.as_str()).chain(e.children.values().map(|c| c.table.as_str())))
            .chain(engines.iter().map(String::as_str))
            .chain(TABLES.iter().map(|t| t.1))
            .chain(["audit_event", "file_object", "tenant"])
            .collect();
        let left_out: Vec<String> = sqlx::query_scalar(
            "select c.relname::text from pg_class c join pg_attribute a on a.attrelid = c.oid and a.attname = 'tenant_id' and not a.attisdropped \
             where c.relnamespace = 'public'::regnamespace and c.relkind in ('r', 'p') and not c.relispartition and not c.relname = any($1) order by 1",
        )
        .bind(&carried as &[&str])
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let store = k.file_store()?.clone();
        let mut index = Vec::with_capacity(files.len());
        for (i, (id, name, ctype, key, entity, record)) in files.iter().enumerate() {
            parts.add(&format!("files/{id}"), &store.get(key).await?).await?;
            index.push(json!({"id": id, "file_name": name, "content_type": ctype, "entity": entity, "record_id": record}));
            if i % 100 == 99 {
                job.progress(k, json!({"step": "files", "done": i + 1, "total": files.len()})).await?;
            }
        }
        parts.add("files/index.jsonl", &lines(&index)?).await?;
        let manifest = json!({
            "format": 1,
            "workspace": {"slug": workspace.0, "name": workspace.1},
            "exported_at": chrono::Utc::now().to_rfc3339(),
            "packages": ctx.schema.packages,
            "entities": entities,
            "files": files.len(),
            "not_included": ["passwords and sign-in factors", "secret field values", "sealed settings", "API keys", "sessions"],
            "tables_left_out": left_out,
        });
        parts.add("manifest.json", &serde_json::to_vec_pretty(&manifest).map_err(Error::internal)?).await?;
        parts.close().await?;
        Ok(json!({"parts": parts.stored, "entities": entities, "files": files.len(), "expires_in_days": EXPORT_DAYS}))
    }
}

impl Kernel {
    /// A page of an entity's records with their child rows, and the cursor for the next.
    async fn whole_records(&self, all: &Ctx, entity: &str, with_rows: bool, cursor: Option<String>) -> Result<(Vec<Value>, Option<String>)> {
        let page = self.list(all, entity, &ListQuery { sort: Some("created_at".into()), limit: Some(MAX_LIMIT), cursor, ..Default::default() }).await?;
        let mut rows = Vec::with_capacity(page.rows.len());
        for row in page.rows {
            let id = row.get("id").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok());
            rows.push(Value::Object(match id.filter(|_| with_rows) {
                Some(id) => self.get(all, entity, id, false).await?,
                None => row,
            }));
        }
        Ok((rows, page.next_cursor))
    }

    /// Queues an operation over more records than a request should wait for: named ones, or every
    /// one a filter matches when it starts. It runs as the caller, with the rights they have then;
    /// what cannot be done to a record is listed in the job's result.
    pub async fn bulk_job(&self, ctx: &Ctx, entity: &str, ids: Option<Vec<Uuid>>, filters: Option<Value>, op: &BulkOp) -> Result<Uuid> {
        if ids.is_some() == filters.is_some() {
            return Err(Error::invalid("send either ids or filters"));
        }
        if ids.as_ref().is_some_and(|ids| ids.len() > MAX_JOB_RECORDS) {
            return Err(Error::invalid(format!("at most {MAX_JOB_RECORDS} records at once")));
        }
        // What can be refused now is: the entity, the caller's reading it, the filter.
        let parsed = filters.as_ref().map(parse_filters).transpose()?.unwrap_or_default();
        self.list(ctx, entity, &ListQuery { filters: parsed, fields: Some(vec!["id".into()]), limit: Some(1), ..Default::default() }).await?;
        let mut job = NewJob::new(BULK_KIND, json!({"entity": entity, "ids": ids, "filters": filters, "op": op}));
        job.priority = PRIORITY_BULK;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let id = enqueue(&mut tx, ctx.tenant, Some(ctx.user), job).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Queues the workspace's export (administrators only), or finds the one already on its way.
    pub async fn export_workspace(&self, ctx: &Ctx) -> Result<Uuid> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("exporting a workspace is for administrators".into()));
        }
        let mut job = NewJob::new(EXPORT_KIND, json!({}));
        job.priority = PRIORITY_BULK;
        job.max_attempts = 3;
        job.dedupe_key = Some("export".into());
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let id = enqueue(&mut tx, ctx.tenant, Some(ctx.user), job).await?;
        audit::record(&mut tx, ctx, "Workspace", ctx.tenant, "export", None, json!({"job": id})).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// One part of a finished export, to an administrator. Each download is on the audit trail.
    pub async fn export_part(&self, ctx: &Ctx, job: Uuid, part: Uuid) -> Result<(String, Vec<u8>)> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("exports are for administrators".into()));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(String, String)> =
            sqlx::query_as("select file_name, storage_key from file_object where tenant_id = $1 and id = $2 and entity = $3 and record_id = $4")
                .bind(ctx.tenant)
                .bind(part)
                .bind(EXPORT_ENTITY)
                .bind(job)
                .fetch_optional(&mut *tx)
                .await?;
        let (name, key) = row.ok_or_else(|| Error::NotFound("export".into()))?;
        audit::record(&mut tx, ctx, "Workspace", ctx.tenant, "export_download", None, json!({"job": job, "part": part})).await?;
        tx.commit().await?;
        Ok((name, self.file_store()?.get(&key).await?))
    }

    /// Deletes exports past their time, and jobs long over.
    pub(crate) async fn purge_exports(&self) -> Result<u64> {
        let mut tx = self.db.system_tx().await?;
        let old: Vec<(Uuid, String)> =
            sqlx::query_as("delete from file_object where entity = $1 and created_at < now() - make_interval(days => $2) returning tenant_id, storage_key")
                .bind(EXPORT_ENTITY)
                .bind(EXPORT_DAYS)
                .fetch_all(&mut *tx)
                .await?;
        let purged = old.len() as u64;
        let mut by_tenant: BTreeMap<Uuid, Vec<String>> = BTreeMap::new();
        for (tenant, key) in old {
            by_tenant.entry(tenant).or_default().push(key);
        }
        for (tenant, keys) in by_tenant {
            release_objects(&mut tx, tenant, &keys).await?;
        }
        tx.commit().await?;
        Ok(purged + self.purge_jobs(30).await?)
    }
}
