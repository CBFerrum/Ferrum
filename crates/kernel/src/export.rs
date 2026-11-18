//! Work the job queue does for the platform itself: bulk operations over more records than a request
//! should wait for, and a workspace's export, the copy a customer takes with them.
//!
//! An export holds everything a workspace can read back without Ferrum: every record as JSON lines
//! with its child rows, the engine tables packages declare (`export_tables`: journals, stock moves),
//! every attached file (one the storage has lost is named in the manifest instead), the audit
//! trail, and the configuration (roles, permissions, customizations, automations, people). It never holds passwords, secret values or sealed settings, and its
//! manifest names every workspace table it left out, so what it lacks is never a surprise. Large
//! exports go into several zip files of about [`PART_BYTES`], readable only by administrators
//! ([`Kernel::export_part`], never `/files/{id}`), and are deleted after [`EXPORT_DAYS`] days, or
//! sooner by a personal-data erasure, which no copy outlives: an export that one lands on while it
//! is being made starts again.

use crate::bulk::{BulkOp, BulkTarget, MAX_BULK};
use crate::ctx::Ctx;
use crate::db::Tx;
use crate::ddl::q;
use crate::error::{Error, Result};
use crate::files::{FileStore, ObjectWriter, object_key};
use crate::jobs::{Job, JobHandler, NewJob, enqueue};
use crate::outbox::{PRIORITY_BULK, release_objects};
use crate::query::{ListQuery, MAX_LIMIT, parse_filters};
use crate::{Kernel, audit};
use async_trait::async_trait;
use ferrum_meta::EntityKind;
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;
use zip::write::{SimpleFileOptions, StreamWriter};

pub const BULK_KIND: &str = "platform.bulk";
pub const EXPORT_KIND: &str = "platform.export";
/// Deletes one export's stored parts, or every export's in a workspace, their objects once `tx`
/// commits. Says how many exports lost theirs.
pub(crate) async fn drop_exports(tx: &mut Tx, tenant: Uuid, job: Option<Uuid>) -> Result<usize> {
    let parts: Vec<(Uuid, String)> = sqlx::query_as(
        "delete from file_object where tenant_id = $1 and entity = $2 and ($3::uuid is null or record_id = $3) returning record_id, storage_key",
    )
    .bind(tenant)
    .bind(EXPORT_ENTITY)
    .bind(job)
    .fetch_all(&mut **tx)
    .await?;
    let keys: Vec<String> = parts.iter().map(|p| p.1.clone()).collect();
    release_objects(tx, tenant, &keys).await?;
    let jobs: Vec<Uuid> = parts.iter().map(|p| p.0).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    // What an erasure took says so on the export, so nobody is offered parts that are gone.
    if job.is_none() {
        sqlx::query(
            "update job set result = coalesce(result, '{}'::jsonb) || '{\"deleted\": \"erasure\"}'::jsonb where tenant_id = $1 and kind = $2 and id = any($3)",
        )
        .bind(tenant)
        .bind(EXPORT_KIND)
        .bind(&jobs)
        .execute(&mut **tx)
        .await?;
    }
    Ok(jobs.len())
}

/// A part of an export closes at about this much before compression (a larger file gets a part to
/// itself). A part is written to storage as it grows, and a stored file read [`CHUNK`] at a time, so
/// an export holds a few chunks whatever the size of the files.
const PART_BYTES: u64 = 128 << 20;
/// How much of a stored file is read at once.
const CHUNK: u64 = 8 << 20;
/// A file from this size goes into the export as it is: most large files are compressed already
/// (images, video, PDFs, archives).
const STORED_FROM: u64 = 1 << 20;
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
                let page = k.list_with_timeout(&ctx, entity, &q, k.db.config.report_timeout_ms).await?;
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

/// What the zip writer writes, taken away a piece at a time to the open part's object.
#[derive(Clone, Default)]
struct Spool(Arc<Mutex<Vec<u8>>>);

impl Spool {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Write for Spool {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn written(e: impl std::fmt::Display) -> Error {
    Error::internal(format!("writing the export: {e}"))
}

/// The part being written: a zip with no going back (each entry's sizes after its bytes), sent to its object as it grows.
struct Open<'a> {
    id: Uuid,
    zip: zip::ZipWriter<StreamWriter<Spool>>,
    out: Box<dyn ObjectWriter + 'a>,
    sha: Sha256,
    bytes: u64,
}

impl Open<'_> {
    /// What the zip wrote since last time, to the object.
    async fn send(&mut self, spool: &Spool) -> Result<()> {
        let piece = spool.take();
        if piece.is_empty() {
            return Ok(());
        }
        self.sha.update(&piece);
        self.bytes += piece.len() as u64;
        self.out.write(piece).await
    }
}

/// The parts of an export.
struct Parts<'a> {
    k: &'a Kernel,
    ctx: &'a Ctx,
    job: &'a Job,
    spool: Spool,
    open: Option<Open<'a>>,
    /// Bytes put into the open part, before compression.
    size: u64,
    stored: Vec<Value>,
    /// The progress last said.
    said: Value,
}

impl<'a> Parts<'a> {
    fn new(k: &'a Kernel, ctx: &'a Ctx, job: &'a Job) -> Self {
        Parts { k, ctx, job, spool: Spool::default(), open: None, size: 0, stored: Vec::new(), said: json!({}) }
    }

    /// Says how far the export has got, and which object the open part is written to: when its worker dies with it
    /// unfinished, the next attempt deletes it (a disk keeps what was written so far).
    async fn progress(&mut self, said: Value) -> Result<()> {
        self.said = said;
        let mut said = self.said.clone();
        if let (Some(open), Some(m)) = (&self.open, said.as_object_mut()) {
            m.insert("open".into(), json!(object_key(self.ctx.tenant, open.id)));
        }
        self.job.progress(self.k, said).await.map(drop)
    }

    /// Begins entry `name` of `size` bytes, in a new part when the open one is full.
    async fn begin(&mut self, name: &str, size: u64) -> Result<()> {
        if self.size > 0 && self.size + size > PART_BYTES {
            self.close().await?;
        }
        if self.open.is_none() {
            let id = Uuid::now_v7();
            let out = self.k.file_store()?.writer(&object_key(self.ctx.tenant, id)).await?;
            self.open = Some(Open { id, zip: zip::ZipWriter::new_stream(self.spool.clone()), out, sha: Sha256::new(), bytes: 0 });
            self.progress(self.said.clone()).await?;
        }
        let method = if size >= STORED_FROM { zip::CompressionMethod::Stored } else { zip::CompressionMethod::Deflated };
        let options = SimpleFileOptions::default().compression_method(method).large_file(size >= u32::MAX as u64);
        let open = self.open.as_mut().ok_or_else(|| Error::internal("no part is open"))?;
        open.zip.start_file(name, options).map_err(written)?;
        self.size += size;
        Ok(())
    }

    /// More of the entry begun last.
    async fn write(&mut self, data: &[u8]) -> Result<()> {
        let open = self.open.as_mut().ok_or_else(|| Error::internal("no part is open"))?;
        open.zip.write_all(data).map_err(written)?;
        open.send(&self.spool).await
    }

    async fn add(&mut self, name: &str, data: &[u8]) -> Result<()> {
        self.begin(name, data.len() as u64).await?;
        self.write(data).await
    }

    /// Finishes the open part, its object and its file; nothing is left of it when that fails.
    async fn close(&mut self) -> Result<()> {
        let Some(mut open) = self.open.take() else { return Ok(()) };
        let (k, ctx, job, spool) = (self.k, self.ctx, self.job, self.spool.clone());
        let name = format!("workspace-export-{}-part-{}.zip", job.id, self.stored.len() + 1);
        let key = object_key(ctx.tenant, open.id);
        let kept = async move {
            std::mem::replace(&mut open.zip, zip::ZipWriter::new_stream(Spool::default())).finish().map_err(written)?;
            open.send(&spool).await?;
            open.out.close().await?;
            let sha = hex::encode(open.sha.finalize());
            k.record_file(ctx, Some((EXPORT_ENTITY, job.id)), open.id, &name, "application/zip", open.bytes, &sha, "unscanned").await
        }
        .await;
        match kept {
            Ok(file) => {
                self.stored.push(file);
                self.size = 0;
                Ok(())
            }
            Err(e) => {
                if let Err(gone) = self.k.file_store()?.delete(&key).await {
                    tracing::warn!(%key, "could not delete an unfinished part of an export: {gone}");
                }
                Err(e)
            }
        }
    }
}

impl Drop for Parts<'_> {
    /// A part left open (the export failed, was cancelled, ran out of time, or its worker was stopped) is deleted. An
    /// object store never showed it: S3 drops the unfinished upload by the bucket's rule (server/src/store.rs); a disk
    /// keeps what was written, which goes here.
    fn drop(&mut self) {
        let (Some(open), Ok(store), Ok(rt)) = (self.open.take(), self.k.file_store(), tokio::runtime::Handle::try_current()) else { return };
        let (store, key) = (store.clone(), object_key(self.ctx.tenant, open.id));
        drop(open);
        rt.spawn(async move {
            if let Err(e) = store.delete(&key).await {
                tracing::warn!(%key, "could not delete an unfinished part of an export: {e}");
            }
        });
    }
}

/// Copies stored object `key` (`size` bytes, as recorded) into entry `name`, [`CHUNK`] at a time. False when storage
/// has lost it: before its first byte, or while it was read, when the entry holds what was read before and the
/// manifest names the file missing. One shorter than recorded goes as it is.
async fn copy_file(parts: &mut Parts<'_>, store: &dyn FileStore, name: &str, key: &str, size: u64) -> Result<bool> {
    if size == 0 {
        if !store.exists(key).await? {
            return Ok(false);
        }
        parts.add(name, &[]).await?;
        return Ok(true);
    }
    let mut at = 0;
    loop {
        let piece = match store.get_range(key, at..size.min(at + CHUNK)).await {
            Err(Error::NotFound(_)) => return Ok(false),
            piece => piece?,
        };
        if at == 0 {
            parts.begin(name, size).await?;
        }
        at += piece.len() as u64;
        parts.write(&piece).await?;
        if piece.is_empty() || at >= size {
            return Ok(true);
        }
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
        "select jsonb_build_object('id', id, 'name', name, 'url', url, 'topics', topics, 'is_active', is_active, 'created_at', created_at, 'app', app) \
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
        // A part an earlier attempt's worker died writing (its progress names it) goes first.
        if let Some(open) = job.resumed["open"].as_str().filter(|key| key.starts_with(&format!("tenants/{}/", ctx.tenant))) {
            k.file_store()?.delete(open).await?;
        }
        // What an earlier attempt stored, or a read that failed on a replica (made once more from the primary), goes:
        // each attempt makes the whole export.
        k.db.reading(|| {
            let ctx = &ctx;
            async move {
                let mut tx = k.db.tenant_tx(ctx.tenant).await?;
                drop_exports(&mut tx, ctx.tenant, Some(job.id)).await?;
                tx.commit().await?;
                make(k, job, ctx).await
            }
        })
        .await
    }
}

/// One attempt at a workspace's export.
async fn make(k: &Kernel, job: &Job, ctx: &Ctx) -> Result<Value> {
    // All of it, whatever record scopes the administrator happens to have; within the zone they asked from (an
    // administrator admitted where a zone limits staff to some apps exports those apps').
    let all = Ctx::system_for(ctx);
    let erasures = k.last_erasure(ctx.tenant).await?;
    let mut parts = Parts::new(k, ctx, job);
    let mut entities = Vec::new();
    let total = all.schema.entities.len();
    for (i, (name, ent)) in all.schema.entities.iter().enumerate() {
        if ent.def.kind == EntityKind::Child || ent.table.is_empty() || !all.sees(name) {
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
            // Recorded for whoever asked, before the job is done and its parts can be downloaded (crate::controls).
            let read = crate::controls::Read::Many { how: "export", filters: &[], search: false, rows: count as i64, to: Some("workspace export") };
            crate::controls::record_read_alone(k, &ctx.recorded_via("workspace export job"), name, read).await?;
        }
        parts.progress(json!({"step": "records", "done": i + 1, "total": total})).await?;
    }

    // What the API does not read: each table in one snapshot, so a journal comes out balanced.
    let ledgers = all.schema.entities.values().filter(|e| e.def.kind == EntityKind::Ledger).map(|e| e.table.clone());
    let engines: Vec<String> = ledgers.chain(all.schema.export_tables.iter().cloned()).collect();
    for table in &engines {
        // Reading only, so on a replica when there is one fit to read from (`Db::read_tx`).
        let mut tx = k.db.read_tx(ctx.tenant, TABLE_TIMEOUT_MS).await?;
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
        let mut tx = k.db.read_tx(ctx.tenant, k.db.config.statement_timeout_ms).await?;
        let rows: Vec<Value> = sqlx::query_scalar(*sql).bind(ctx.tenant).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        parts.add(&format!("{name}.jsonl"), &lines(&rows)?).await?;
    }

    let (mut after, mut file) = (0i64, 0);
    loop {
        let mut tx = k.db.read_tx(ctx.tenant, k.db.config.statement_timeout_ms).await?;
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
    parts.progress(json!({"step": "files", "done": 0})).await?;

    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    // The workspace's files, not the kinds the platform and apps keep for their own work (`<owner>.<name>`, which no
    // entity's name is: crate::files::FileKind), which their apps hand out their own way.
    let files: Vec<(Uuid, String, String, String, Option<String>, Option<Uuid>, i64)> = sqlx::query_as(
        "select id, file_name, content_type, storage_key, entity, record_id, size_bytes from file_object where tenant_id = $1 \
           and (entity is null or strpos(entity, '.') = 0) order by id",
    )
    .bind(ctx.tenant)
    .fetch_all(&mut *tx)
    .await?;
    let kinds_left_out: Vec<Value> = sqlx::query_scalar(
        "select jsonb_build_object('kind', entity, 'files', count(*), 'bytes', coalesce(sum(size_bytes), 0)::int8, \
           'why', 'its app keeps them for its own work and hands them out its own way') \
         from file_object where tenant_id = $1 and strpos(entity, '.') > 0 and entity <> $2 group by entity order by entity",
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
        // `custom_key` is what custom records' unique keys hold, made again from the records.
        .chain(["audit_event", "file_object", "tenant", "custom_key"])
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
    let (mut index, mut missing) = (Vec::with_capacity(files.len()), Vec::new());
    for (i, (id, name, ctype, key, entity, record, size)) in files.iter().enumerate() {
        // A file storage has lost is named, not a reason to give the customer nothing.
        if copy_file(&mut parts, &*store, &format!("files/{id}"), key, *size as u64).await? {
            index.push(json!({"id": id, "file_name": name, "content_type": ctype, "entity": entity, "record_id": record}));
        } else {
            missing.push(json!({"id": id, "file_name": name, "entity": entity, "record_id": record}));
        }
        if i % 100 == 99 {
            parts.progress(json!({"step": "files", "done": i + 1, "total": files.len()})).await?;
        }
    }
    parts.add("files/index.jsonl", &lines(&index)?).await?;
    let manifest = json!({
        "format": 1,
        "workspace": {"slug": workspace.0, "name": workspace.1},
        "exported_at": chrono::Utc::now().to_rfc3339(),
        "packages": ctx.schema.packages,
        "entities": entities,
        "files": index.len(),
        "not_included": ["passwords and sign-in factors", "secret field values", "sealed settings", "API keys", "sessions"],
        "tables_left_out": left_out,
        "files_left_out": kinds_left_out,
        "files_missing": missing,
    });
    parts.add("manifest.json", &serde_json::to_vec_pretty(&manifest).map_err(Error::internal)?).await?;
    parts.close().await?;
    // An erasure meanwhile deleted what was stored of this, but not what it had read and not yet
    // stored: it starts again, reading what the erasure left.
    if k.last_erasure(ctx.tenant).await? != erasures {
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        drop_exports(&mut tx, ctx.tenant, Some(job.id)).await?;
        tx.commit().await?;
        return Err(Error::Wait("a personal-data erasure landed while this export was made; it is made again".into(), 0));
    }
    Ok(json!({"parts": parts.stored, "entities": entities, "files": index.len(), "files_missing": missing.len(), "expires_in_days": EXPORT_DAYS}))
}

impl Kernel {
    /// A page of an entity's records with their child rows, and the cursor for the next: the rows of the whole page in
    /// one statement, on a replica when there is one fit to read from, as the page itself is.
    async fn whole_records(&self, all: &Ctx, entity: &str, with_rows: bool, cursor: Option<String>) -> Result<(Vec<Value>, Option<String>)> {
        let q = ListQuery { sort: Some("created_at".into()), limit: Some(MAX_LIMIT), cursor, ..Default::default() };
        let page = self.list_with_timeout(all, entity, &q, self.db.config.report_timeout_ms).await?;
        if !with_rows {
            return Ok((page.rows.into_iter().map(Value::Object).collect(), page.next_cursor));
        }
        let ent = self.entity(all, entity)?;
        let ids: Vec<Uuid> = page.rows.iter().filter_map(|r| r.get("id")?.as_str()?.parse().ok()).collect();
        let mut tx = self.db.read_tx(all.tenant, self.db.config.report_timeout_ms).await?;
        let mut whole: std::collections::HashMap<String, crate::Doc> = crate::store::load_many_whole(&mut tx, &all.schema, &ent, all.tenant, &ids)
            .await?
            .into_iter()
            .filter_map(|d| Some((d.get("id")?.as_str()?.to_string(), d)))
            .collect();
        // Which secret fields hold a value (as a record read one at a time says), for the page at once.
        let secrets: Vec<(Uuid, String)> = if ent.secret_fields().next().is_some() {
            sqlx::query_as("select record_id, field from record_secret where tenant_id = $1 and entity = $2 and record_id = any($3)")
                .bind(all.tenant)
                .bind(&ent.def.name)
                .bind(&ids)
                .fetch_all(&mut *tx)
                .await?
        } else {
            Vec::new()
        };
        tx.commit().await?;
        let mut rows = Vec::with_capacity(ids.len());
        for id in ids {
            // One deleted since the page was read is left out.
            let Some(mut doc) = whole.remove(&id.to_string()) else { continue };
            for f in ent.secret_fields() {
                let set = secrets.iter().any(|(r, field)| *r == id && *field == f.name);
                doc.insert(f.name.clone(), if set { Value::Bool(true) } else { Value::Null });
            }
            rows.push(Value::Object(self.mask(all, &ent, doc)));
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
        audit::record_admin(&mut tx, ctx, "Workspace", ctx.tenant, "export", None, json!({"job": id})).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// One part of a finished export, to an administrator: its name and the object to stream it from. Each download is on
    /// the audit trail.
    pub async fn export_part(&self, ctx: &Ctx, job: Uuid, part: Uuid) -> Result<(String, String)> {
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
        audit::record_admin(&mut tx, ctx, "Workspace", ctx.tenant, "export_download", None, json!({"job": job, "part": part})).await?;
        tx.commit().await?;
        Ok((name, key))
    }

    /// Deletes exports past their time, and jobs long over.
    pub(crate) async fn purge_exports(&self) -> Result<u64> {
        let parts = self
            .db
            .in_batches(|tx| {
                Box::pin(async move {
                    let old: Vec<(Uuid, String)> = sqlx::query_as(
                        "delete from file_object where ctid = any(array(select ctid from file_object where entity = $1 \
                           and created_at < now() - make_interval(days => $2) and not ferrum_workspace_held(tenant_id) limit $3)) \
                         returning tenant_id, storage_key",
                    )
                    .bind(EXPORT_ENTITY)
                    .bind(EXPORT_DAYS)
                    .bind(crate::db::PURGE_BATCH as i64)
                    .fetch_all(&mut **tx)
                    .await?;
                    let purged = old.len() as u64;
                    let mut by_tenant: BTreeMap<Uuid, Vec<String>> = BTreeMap::new();
                    for (tenant, key) in old {
                        by_tenant.entry(tenant).or_default().push(key);
                    }
                    for (tenant, keys) in by_tenant {
                        release_objects(tx, tenant, &keys).await?;
                    }
                    Ok(purged)
                })
            })
            .await;
        let jobs = self.purge_jobs(30).await;
        Ok(parts? + jobs?)
    }
}
