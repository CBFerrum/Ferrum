//! Files that packages create or receive (mail attachments, generated documents) go where
//! uploads go: the server hands the kernel its object store at boot (S3-compatible or disk), and
//! they are recorded in `file_object` like any upload, with the same access rules.

use crate::error::{Error, Result};
use crate::{Ctx, Doc, Kernel, Tx};
use async_trait::async_trait;
use ferrum_meta::{EffEntity, FieldType};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::sync::Arc;
use uuid::Uuid;

#[async_trait]
pub trait FileStore: Send + Sync {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()>;
    async fn get(&self, key: &str) -> Result<Vec<u8>>;
    async fn delete(&self, key: &str) -> Result<()>;
    /// Whether an object is there. By default it reads it; a store that can ask without reading may say
    /// so more cheaply.
    async fn exists(&self, key: &str) -> Result<bool> {
        match self.get(key).await {
            Ok(_) => Ok(true),
            Err(Error::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// The most an app reads of a file at once (FERRUM-OPS-006), the size of the largest an app works on
/// whole (Learn's SCORM packages): larger ones are streamed to whoever downloads them
/// ([`crate::SiteResponse::file`]) rather than read into a replica's memory.
pub const MAX_READ_WHOLE: i64 = 256 << 20;

/// What [`Kernel::file_info`] says of a stored file.
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub name: String,
    pub content_type: String,
    pub size: i64,
    key: String,
}

/// A file tied to no record is its uploader's, and an administrator's (and the system's).
fn owns_loose(ctx: &Ctx, by: Option<Uuid>) -> bool {
    ctx.is_admin || ctx.is_system || (!ctx.user.is_nil() && by == Some(ctx.user))
}

/// The stored files a record's `attach` fields name (`/files/{id}`), its child tables' included.
fn named_files(ctx: &Ctx, ent: &EffEntity, doc: &Doc) -> BTreeSet<Uuid> {
    let file = |v: Option<&Value>| v.and_then(Value::as_str).and_then(|s| s.strip_prefix("/files/")).and_then(|s| s.parse::<Uuid>().ok());
    let mut out: BTreeSet<Uuid> = ent.value_fields().filter(|f| f.ftype == FieldType::Attach).filter_map(|f| file(doc.get(&f.name))).collect();
    for (table, info) in &ent.children {
        let Some(c) = ctx.schema.entity(&info.entity) else { continue };
        for row in doc.get(table).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object) {
            // A line's own fields and its own lines' (BD-06).
            out.extend(named_files(ctx, c, row));
        }
    }
    out
}

/// Content types never served inline as-is (they could run script in the app's origin).
const ACTIVE_TYPES: &[&str] = &["text/html", "application/xhtml+xml", "image/svg+xml", "application/javascript", "text/javascript"];

impl Kernel {
    /// Installs the object store (once, at boot).
    pub fn set_file_store(&self, store: Arc<dyn FileStore>) {
        let _ = self.files.set(store);
    }

    pub(crate) fn file_store(&self) -> Result<&Arc<dyn FileStore>> {
        self.files.get().ok_or_else(|| Error::Unavailable("no file storage is configured".into()))
    }

    /// Stores a file, optionally attached to a record the caller can read, and returns
    /// `{id, url, file_name, size, sha256}` — exactly what the upload API returns.
    pub async fn store_file(&self, ctx: &Ctx, attach_to: Option<(&str, Uuid)>, file_name: &str, content_type: &str, data: &[u8]) -> Result<Value> {
        if let Some((entity, record)) = attach_to {
            self.get(ctx, entity, record, false).await?;
        }
        self.check_storage_quota(ctx.tenant, data.len() as u64).await?;
        let scanned = self.scan_stream(ctx, file_name, data.len() as u64, futures::stream::iter([Ok::<_, Error>(data)])).await?;
        self.keep_file(ctx, attach_to, file_name, content_type, data, scanned).await
    }

    /// [`Kernel::store_file`] for a caller that decided what `attach_to` may name and scanned it, or
    /// made it itself.
    pub(crate) async fn keep_file(
        &self,
        ctx: &Ctx,
        attach_to: Option<(&str, Uuid)>,
        file_name: &str,
        content_type: &str,
        data: &[u8],
        scanned: &str,
    ) -> Result<Value> {
        let name: String = file_name.chars().filter(|c| !c.is_control() && *c != '/' && *c != '\\').take(200).collect();
        let name = if name.trim().is_empty() { "file.bin".to_string() } else { name };
        let ctype = content_type.to_lowercase();
        let ctype = if ACTIVE_TYPES.contains(&ctype.as_str()) { "application/octet-stream".to_string() } else { ctype };
        let id = Uuid::now_v7();
        let key = format!("tenants/{}/{}", ctx.tenant, id);
        self.file_store()?.put(&key, data).await?;
        let sha = hex::encode(Sha256::digest(data));
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query(
            "insert into file_object (tenant_id, id, entity, record_id, file_name, content_type, size_bytes, storage_key, sha256, created_by, scan_status) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(attach_to.map(|a| a.0))
        .bind(attach_to.map(|a| a.1))
        .bind(&name)
        .bind(&ctype)
        .bind(data.len() as i64)
        .bind(&key)
        .bind(&sha)
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .bind(scanned)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(json!({"id": id, "url": format!("/files/{id}"), "file_name": name, "size": data.len(), "sha256": sha}))
    }

    /// A stored file's name, content type and size, if the caller may read the record it is
    /// attached to: what an app needs to answer with the file ([`crate::SiteResponse::file`])
    /// without reading it.
    pub async fn file_info(&self, ctx: &Ctx, id: Uuid) -> Result<FileInfo> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(String, String, String, Option<String>, Option<Uuid>, i64, Option<Uuid>)> = sqlx::query_as(
            "select file_name, content_type, storage_key, entity, record_id, size_bytes, created_by from file_object where tenant_id = $1 and id = $2",
        )
        .bind(ctx.tenant)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let (name, content_type, key, entity, record, size, by) = row.ok_or_else(|| Error::NotFound("file".into()))?;
        self.may_download(ctx, entity.as_deref(), record, by).await?;
        Ok(FileInfo { name, content_type, size, key })
    }

    /// Whether `ctx` may download a file: one tied to a record, whoever may read the record (an approver too, while it
    /// awaits them: BD-19); one tied to none, its
    /// uploader and administrators (`by`: who uploaded it). Not found otherwise.
    pub async fn may_download(&self, ctx: &Ctx, entity: Option<&str>, record: Option<Uuid>, by: Option<Uuid>) -> Result<()> {
        match (entity, record) {
            (Some(e), Some(r)) => self.read_record(ctx, e, r, false).await.map(drop),
            _ if owns_loose(ctx, by) => Ok(()),
            _ => Err(Error::NotFound("file".into())),
        }
    }

    /// Counts an outside person's upload tied to no record against 20 an hour (docs/limits.md); a member's is not
    /// counted. Size is limited as for everyone, by the route.
    pub async fn count_loose_upload(&self, ctx: &Ctx) -> Result<()> {
        if !ctx.outside {
            return Ok(());
        }
        let key = format!("files|loose|{}|{}", ctx.tenant, ctx.user);
        self.rate_limit(&key, std::time::Duration::from_secs(3600), 20, "too many uploads in an hour; try again later").await
    }

    /// Ties the files a saved record newly names in its `attach` fields to it, in the save's transaction: one tied to
    /// no record when the saver uploaded it (or is an administrator or the system); one tied to a record stays there
    /// when the saver may read that record. Any other is refused, so naming an id is no way to read a file.
    pub(crate) async fn claim_files(&self, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, id: Uuid, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        let before = old.map(|o| named_files(ctx, ent, o)).unwrap_or_default();
        for file in named_files(ctx, ent, doc).difference(&before) {
            let row: Option<(Option<String>, Option<Uuid>, Option<Uuid>)> =
                sqlx::query_as("select entity, record_id, created_by from file_object where tenant_id = $1 and id = $2 for update")
                    .bind(ctx.tenant)
                    .bind(file)
                    .fetch_optional(&mut **tx)
                    .await?;
            // An id naming no stored file gives nothing away.
            let Some((entity, record, by)) = row else { continue };
            let refused = || Error::Forbidden(format!("{} names a file you cannot download", ent.def.label));
            match (entity, record) {
                (Some(e), Some(r)) => {
                    self.get_tx(ctx, tx, &e, r, false).await.map_err(|_| refused())?;
                }
                _ if owns_loose(ctx, by) => {
                    sqlx::query("update file_object set entity = $3, record_id = $4 where tenant_id = $1 and id = $2")
                        .bind(ctx.tenant)
                        .bind(file)
                        .bind(&ent.def.name)
                        .bind(id)
                        .execute(&mut **tx)
                        .await?;
                }
                _ => return Err(refused()),
            }
        }
        Ok(())
    }

    /// A stored file's name, content type and bytes, if the caller may read the record it is
    /// attached to; refused for one larger than [`MAX_READ_WHOLE`], which is streamed instead.
    pub async fn read_file(&self, ctx: &Ctx, id: Uuid) -> Result<(String, String, Vec<u8>)> {
        let f = self.file_info(ctx, id).await?;
        if f.size > MAX_READ_WHOLE {
            return Err(Error::invalid(format!(
                "{} is larger than {} MB, more than is read whole: it is downloaded as a stream",
                f.name,
                MAX_READ_WHOLE >> 20
            )));
        }
        Ok((f.name, f.content_type, self.file_store()?.get(&f.key).await?))
    }

    /// Deletes a stored file — its record and its bytes — if the caller may: its uploader, or
    /// someone who can edit the record it is attached to. An object the store cannot delete is
    /// only logged (the record is gone, so nothing can read it).
    pub async fn remove_file(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        let key = self.delete_file(ctx, id).await?;
        if let Ok(store) = self.file_store()
            && let Err(e) = store.delete(&key).await
        {
            tracing::warn!(%key, "could not delete a file's object: {e}");
        }
        Ok(())
    }

    /// Attaches a file nobody attached yet to a record the caller can read, inside the caller's
    /// transaction (an upload that becomes an attachment when its document is saved). Only the
    /// uploader, or a system context, may attach it.
    pub async fn attach_file_tx(&self, ctx: &Ctx, tx: &mut Tx, id: Uuid, entity: &str, record: Uuid) -> Result<()> {
        self.get_tx(ctx, tx, entity, record, false).await?;
        let attached = sqlx::query(
            "update file_object set entity = $3, record_id = $4 where tenant_id = $1 and id = $2 and entity is null and record_id is null \
             and (created_by = $5 or $6)",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(entity)
        .bind(record)
        .bind(ctx.user)
        .bind(ctx.is_system)
        .execute(&mut **tx)
        .await?
        .rows_affected();
        if attached == 0 {
            return Err(Error::NotFound("file".into()));
        }
        Ok(())
    }

    /// Copies a file the caller can read onto another record (a mail attachment kept on the record
    /// it concerns, a signed PDF filed with its contract). A new object, so deleting either copy
    /// never affects the other.
    pub async fn copy_file(&self, ctx: &Ctx, id: Uuid, attach_to: Option<(&str, Uuid)>) -> Result<Value> {
        let (name, ctype, data) = self.read_file(ctx, id).await?;
        self.store_file(ctx, attach_to, &name, &ctype, &data).await
    }
}

/// Files tied, and files several records named, by [`Kernel::tie_named_files`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tied {
    pub tied: i64,
    pub shared: i64,
}

/// Loose files a batch of [`Kernel::tie_named_files`] looks at, each batch in its own transaction.
const TIE_BATCH: i64 = 1000;

impl Kernel {
    /// Ties the files of `tenant` that are tied to no record to the record whose `attach` field names them
    /// (`/files/{id}`), as a save now does: an entity's own fields, its lines' (tied to the record owning the line)
    /// and custom entities' alike. A file several records name goes to the first made, and is counted in `shared`.
    /// Batches of loose files in the order of their ids, each in its own transaction; a file a save ties meanwhile
    /// is left as the save tied it, and one tied already is never looked at, so running it again changes nothing.
    // ponytail: each batch scans every table with an attach field (values are not indexed); an upgrade step, once.
    pub async fn tie_named_files(&self, tenant: Uuid) -> Result<Tied> {
        let schema = self.registry.schema(tenant).await?;
        // For each place an attach field lives: the rows, the record a value belongs to and when it was made.
        let mut sources = Vec::new();
        for ent in schema.entities.values().filter(|e| !e.table.is_empty() && e.def.kind != ferrum_meta::EntityKind::Child) {
            let name = ent.def.name.replace('\'', "''");
            let own = format!("{} a", crate::ddl::q(&ent.table));
            for f in ent.value_fields().filter(|f| f.ftype == FieldType::Attach) {
                let Some(expr) = crate::store::field_expr(ent, &f.name, "a") else { continue };
                sources.push(format!(
                    "select b.id as file, '{name}'::text as entity, a.id as record, a.created_at as at from {own} join batch b on {expr} = '/files/' || b.id::text \
                     where a.tenant_id = $1{}",
                    crate::store::entity_predicate(ent, "a")
                ));
            }
            for info in ent.children.values() {
                let Some(c) = schema.entity(&info.entity) else { continue };
                for f in c.value_fields().filter(|f| f.ftype == FieldType::Attach) {
                    let Some(expr) = crate::store::field_expr(c, &f.name, "l") else { continue };
                    sources.push(format!(
                        "select b.id as file, '{name}'::text as entity, a.id as record, a.created_at as at from {} l \
                         join {own} on a.tenant_id = l.tenant_id and a.id = l.parent_id{} join batch b on {expr} = '/files/' || b.id::text \
                         where l.tenant_id = $1{}",
                        crate::ddl::q(&info.table),
                        crate::store::entity_predicate(ent, "a"),
                        info.predicate("l")
                    ));
                }
                // A line's own lines (BD-06), tied to the record owning their line.
                for sub in c.children.values() {
                    let Some(sc) = schema.entity(&sub.entity) else { continue };
                    for f in sc.value_fields().filter(|f| f.ftype == FieldType::Attach) {
                        let Some(expr) = crate::store::field_expr(sc, &f.name, "s") else { continue };
                        sources.push(format!(
                            "select b.id as file, '{name}'::text as entity, a.id as record, a.created_at as at from {} s \
                             join {} l on l.tenant_id = s.tenant_id and l.id = s.parent_id{} \
                             join {own} on a.tenant_id = l.tenant_id and a.id = l.parent_id{} join batch b on {expr} = '/files/' || b.id::text \
                             where s.tenant_id = $1{}",
                            crate::ddl::q(&sub.table),
                            crate::ddl::q(&info.table),
                            info.predicate("l"),
                            crate::store::entity_predicate(ent, "a"),
                            sub.predicate("s")
                        ));
                    }
                }
            }
        }
        let mut out = Tied::default();
        if sources.is_empty() {
            return Ok(out);
        }
        let step = format!(
            "with batch as (select id from file_object where tenant_id = $1 and record_id is null and id > $2 order by id limit $3), \
             cand as ({}), \
             pick as (select distinct on (file) file, entity, record from cand order by file, at, record), \
             tied as (update file_object o set entity = pick.entity, record_id = pick.record from pick \
                      where o.tenant_id = $1 and o.id = pick.file and o.record_id is null returning o.id) \
             select (select id from batch order by id desc limit 1), (select count(*) from tied), \
                    (select count(*) from (select file from cand group by file having count(distinct (entity, record)) > 1) s)",
            sources.join(" union all ")
        );
        let mut after = Uuid::nil();
        loop {
            let mut tx = self.db.system_tenant_tx(tenant).await?;
            let (last, tied, shared): (Option<Uuid>, i64, i64) =
                sqlx::query_as(sqlx::AssertSqlSafe(step.clone())).bind(tenant).bind(after).bind(TIE_BATCH).fetch_one(&mut *tx).await?;
            tx.commit().await?;
            out.tied += tied;
            out.shared += shared;
            match last {
                Some(last) => after = last,
                None => return Ok(out),
            }
        }
    }
}
