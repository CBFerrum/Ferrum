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
    /// The bytes of `range` of an object (a connection's upload in chunks, BD-55). By default it reads the whole object
    /// and keeps the range (ponytail: a store that can read a range should, so a large file is never held whole).
    async fn get_range(&self, key: &str, range: std::ops::Range<u64>) -> Result<Vec<u8>> {
        let all = self.get(key).await?;
        let end = (range.end as usize).min(all.len());
        Ok(all.get(range.start as usize..end).unwrap_or_default().to_vec())
    }
    /// Whether an object is there. By default it reads it; a store that can ask without reading may say
    /// so more cheaply.
    async fn exists(&self, key: &str) -> Result<bool> {
        match self.get(key).await {
            Ok(_) => Ok(true),
            Err(Error::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }
    /// An object's size, `None` when nothing is stored there. By default it reads it, as [`FileStore::exists`] does.
    async fn size(&self, key: &str) -> Result<Option<u64>> {
        match self.get(key).await {
            Ok(bytes) => Ok(Some(bytes.len() as u64)),
            Err(Error::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
    /// An object written a piece at a time (a part of a workspace's export), stored at `key` when it is closed. By
    /// default the pieces are collected and put whole (ponytail: a store that can write in parts should, as the
    /// server's does, so a large object is never held whole).
    async fn writer(&self, key: &str) -> Result<Box<dyn ObjectWriter + '_>> {
        Ok(Box::new(Collected { store: self, key: key.to_string(), data: Vec::new() }))
    }
}

/// What [`FileStore::writer`] gives. One dropped before it is closed stores nothing an object store shows; a disk
/// keeps what was written so far, which its writer's owner deletes (crate::export).
#[async_trait]
pub trait ObjectWriter: Send {
    async fn write(&mut self, piece: Vec<u8>) -> Result<()>;
    async fn close(self: Box<Self>) -> Result<()>;
}

struct Collected<'a, S: ?Sized> {
    store: &'a S,
    key: String,
    data: Vec<u8>,
}

#[async_trait]
impl<S: FileStore + ?Sized> ObjectWriter for Collected<'_, S> {
    async fn write(&mut self, piece: Vec<u8>) -> Result<()> {
        self.data.extend_from_slice(&piece);
        Ok(())
    }
    async fn close(self: Box<Self>) -> Result<()> {
        self.store.put(&self.key, &self.data).await
    }
}

/// The most an app reads of a file at once (FERRUM-OPS-006), the size of the largest an app works on
/// whole (Learn's SCORM packages): larger ones are streamed to whoever downloads them
/// ([`crate::SiteResponse::file`]) rather than read into a replica's memory.
pub const MAX_READ_WHOLE: i64 = 256 << 20;

/// Where stored file `id` of `tenant` is kept.
pub(crate) fn object_key(tenant: Uuid, id: Uuid) -> String {
    format!("tenants/{tenant}/{id}")
}

/// What [`Kernel::file_info`] says of a stored file.
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub name: String,
    pub content_type: String,
    pub size: i64,
    pub(crate) key: String,
}

/// Whether a stored file's `entity` is a kind of file the platform or an app keeps for its own work (`<owner>.<name>`:
/// `ferrum.export`, an app's [`FileKind`]), not an entity whose records hold the file: no entity's name has a dot.
pub(crate) fn is_kind(entity: &str) -> bool {
    entity.contains('.')
}

/// A kind of file an app keeps for its own work, such as built artifacts or databases' backups: the workspace's files,
/// counted, kept and deleted as any (its quota, the deletion lag, the doctor, the restore drill, a move, a sandbox), but
/// not its content to hand out. Nobody reaches one through the platform's routes for files, administrators included
/// (not found, as a file that is not there): not downloaded, copied, deleted or tied to a record; a workspace's export
/// leaves them out and says so. Its app keeps, reads and lets go of them with this handle, and answers with one from its
/// own site ([`crate::SiteResponse::file`]): that is their only way out, and the app owes the customer one.
///
/// Declared in the app's registration ([`crate::HookRegistry::file_kind`]) as `<app id>.<name>`, by that app alone;
/// a kind no app declares any more (its app left the build) is still nobody's to reach.
#[derive(Clone)]
pub struct FileKind {
    kind: Arc<str>,
    app: Arc<str>,
    /// What the registry that gave it out noted for the app (its vault handle's), checked at each use.
    mint: u128,
}

impl std::fmt::Debug for FileKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FileKind({})", self.kind)
    }
}

impl FileKind {
    pub(crate) fn new(kind: String, app: &str, mint: u128) -> FileKind {
        FileKind { kind: kind.into(), app: app.into(), mint }
    }

    /// `<app id>.<name>`: what `file_object.entity` holds for each file of the kind.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Refused unless `k` booted with the registry that gave this handle out, the kind declared by its app.
    fn given(&self, k: &Kernel) -> Result<()> {
        let ok = self.mint != 0 && k.hooks.vault_mint(&self.app) == Some(self.mint) && k.hooks.file_kind_app(&self.kind) == Some(&*self.app);
        if ok { Ok(()) } else { Err(Error::Forbidden(format!("the kind of file {} was not declared to this server", self.kind))) }
    }

    /// Keeps `data` as a file of the kind in `tenant`'s storage, tied to `id` (whatever the app names it by: an artifact,
    /// a backup): counted against the workspace's storage and scanned as an upload is. `{id, file_name, size, sha256}`.
    pub async fn keep(&self, k: &Kernel, tenant: Uuid, id: Uuid, file_name: &str, content_type: &str, data: &[u8]) -> Result<Value> {
        self.given(k)?;
        let ctx = Ctx::system(tenant, k.registry.schema(tenant).await?);
        k.check_storage_quota(tenant, data.len() as u64).await?;
        let scanned = k.scan_stream(&ctx, file_name, data.len() as u64, futures::stream::iter([Ok::<_, Error>(data)])).await?;
        let mut kept = k.keep_file(&ctx, Some((&self.kind, id)), file_name, content_type, data, scanned).await?;
        if let Some(m) = kept.as_object_mut() {
            // No address of the platform's serves it: the app answers with it from its own site.
            m.remove("url");
        }
        Ok(kept)
    }

    /// A file of the kind's name, content type and size, to answer with it from the app's site
    /// ([`crate::SiteResponse::file`]). Not found when it is not this kind's.
    pub async fn info(&self, k: &Kernel, tenant: Uuid, file: Uuid) -> Result<FileInfo> {
        self.given(k)?;
        let mut tx = k.db.tenant_tx(tenant).await?;
        let row: Option<(String, String, i64, String)> =
            sqlx::query_as("select file_name, content_type, size_bytes, storage_key from file_object where tenant_id = $1 and id = $2 and entity = $3")
                .bind(tenant)
                .bind(file)
                .bind(&*self.kind)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        let (name, content_type, size, key) = row.ok_or_else(|| Error::NotFound("file".into()))?;
        Ok(FileInfo { name, content_type, size, key })
    }

    /// A file of the kind's name, content type and bytes; one larger than [`MAX_READ_WHOLE`] is streamed instead.
    pub async fn read(&self, k: &Kernel, tenant: Uuid, file: Uuid) -> Result<(String, String, Vec<u8>)> {
        let f = self.info(k, tenant, file).await?;
        if f.size > MAX_READ_WHOLE {
            return Err(Error::invalid(format!("{} is larger than {} MB, more than is read whole: it is sent as a stream", f.name, MAX_READ_WHOLE >> 20)));
        }
        Ok((f.name, f.content_type, k.file_store()?.get(&f.key).await?))
    }

    /// Lets `files` of the kind go in the caller's transaction: their rows now, their bytes after the platform's
    /// deletion lag once it commits (crate::outbox::release_objects). Another kind's, or none, are left alone. How many.
    pub async fn release(&self, k: &Kernel, tx: &mut Tx, tenant: Uuid, files: &[Uuid]) -> Result<u64> {
        self.given(k)?;
        if files.is_empty() {
            return Ok(0);
        }
        let keys: Vec<String> = sqlx::query_scalar("delete from file_object where tenant_id = $1 and id = any($2) and entity = $3 returning storage_key")
            .bind(tenant)
            .bind(files)
            .bind(&*self.kind)
            .fetch_all(&mut **tx)
            .await?;
        crate::outbox::release_objects(tx, tenant, &keys).await?;
        Ok(keys.len() as u64)
    }
}

impl Kernel {
    /// Whether the site of `package` may answer with a stored file tied to `entity` ([`crate::SiteResponse::file`]): any
    /// file but a kind's ([`FileKind`]); a kind's only from a site of the app that declared it.
    pub fn site_may_send(&self, package: &str, entity: Option<&str>) -> bool {
        match entity.filter(|e| is_kind(e)) {
            None => true,
            Some(kind) => self.hooks.file_kind_app(kind).is_some_and(|app| self.hooks.package_owner(package) == Some(app)),
        }
    }
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
pub(crate) const ACTIVE_TYPES: &[&str] = &["text/html", "application/xhtml+xml", "image/svg+xml", "application/javascript", "text/javascript"];

/// Whether a browser may run what a file of `content_type` holds (its parameters aside): the active types, and XML,
/// which a browser renders as a document too. Such a file is only ever served as an attachment that runs nothing.
pub fn runs_in_browser(content_type: &str) -> bool {
    let base = content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    ACTIVE_TYPES.contains(&base.as_str()) || base == "application/xml" || base == "text/xml" || base.ends_with("+xml")
}

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
        match attach_to {
            // An outside person attaches as the upload route has them do (BD-22).
            Some((entity, record)) if ctx.outside => self.may_attach(ctx, entity, record).await?,
            Some((entity, record)) => drop(self.get(ctx, entity, record, false).await?),
            None => {}
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
        let id = Uuid::now_v7();
        self.file_store()?.put(&object_key(ctx.tenant, id), data).await?;
        let sha = hex::encode(Sha256::digest(data));
        self.record_file(ctx, attach_to, id, file_name, content_type, data.len() as u64, &sha, scanned).await
    }

    /// Records object `id` already stored at [`object_key`] as a file: [`Kernel::keep_file`]'s second half, for an
    /// object written in pieces (an export's part).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_file(
        &self,
        ctx: &Ctx,
        attach_to: Option<(&str, Uuid)>,
        id: Uuid,
        file_name: &str,
        content_type: &str,
        size: u64,
        sha: &str,
        scanned: &str,
    ) -> Result<Value> {
        let name: String = file_name.chars().filter(|c| !c.is_control() && *c != '/' && *c != '\\').take(200).collect();
        let name = if name.trim().is_empty() { "file.bin".to_string() } else { name };
        let ctype = content_type.to_lowercase();
        let ctype = if ACTIVE_TYPES.contains(&ctype.as_str()) { "application/octet-stream".to_string() } else { ctype };
        let key = object_key(ctx.tenant, id);
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query(
            "insert into file_object (tenant_id, id, entity, record_id, file_name, content_type, size_bytes, storage_key, sha256, created_by, scan_status, shared) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(attach_to.map(|a| a.0))
        .bind(attach_to.map(|a| a.1))
        .bind(&name)
        .bind(&ctype)
        .bind(size as i64)
        .bind(&key)
        .bind(sha)
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .bind(scanned)
        // What an outside person uploads is shared by nature (BD-22).
        .bind(ctx.outside)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(json!({"id": id, "url": format!("/files/{id}"), "file_name": name, "size": size, "sha256": sha}))
    }

    /// A stored file's name, content type and size, if the caller may read the record it is
    /// attached to: what an app needs to answer with the file ([`crate::SiteResponse::file`])
    /// without reading it.
    pub async fn file_info(&self, ctx: &Ctx, id: Uuid) -> Result<FileInfo> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(String, String, String, Option<String>, Option<Uuid>, i64, Option<Uuid>, bool)> = sqlx::query_as(
            "select file_name, content_type, storage_key, entity, record_id, size_bytes, created_by, shared from file_object where tenant_id = $1 and id = $2",
        )
        .bind(ctx.tenant)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let (name, content_type, key, entity, record, size, by, shared) = row.ok_or_else(|| Error::NotFound("file".into()))?;
        self.may_download_file(ctx, id, entity.as_deref(), record, by, shared).await?;
        Ok(FileInfo { name, content_type, size, key })
    }

    /// Whether `ctx` may download a file: one tied to a record, whoever may read the record (an approver too, while it
    /// awaits them: BD-19); one tied to none, its
    /// uploader and administrators (`by`: who uploaded it). Not found otherwise. Not knowing which file, it refuses an
    /// outside person any file tied to a record: [`Kernel::may_download_file`] decides theirs.
    pub async fn may_download(&self, ctx: &Ctx, entity: Option<&str>, record: Option<Uuid>, by: Option<Uuid>) -> Result<()> {
        match (entity, record) {
            // A kind of file an app keeps for its own work: nobody's through these routes ([`FileKind`]).
            (Some(e), _) if is_kind(e) => Err(Error::NotFound("file".into())),
            (Some(_), Some(_)) if ctx.outside => Err(Error::NotFound("file".into())),
            // Tied to a record of an app the caller's zone leaves out (crate::zones, level 2): not there.
            (Some(e), Some(_)) if !ctx.sees(e) => Err(Error::NotFound("file".into())),
            (Some(e), Some(r)) => self.read_record(ctx, e, r, false).await.map(drop),
            _ if owns_loose(ctx, by) => Ok(()),
            _ => Err(Error::NotFound("file".into())),
        }
    }

    /// [`Kernel::may_download`] for stored file `file` (`shared`: whether it is shared with outside people). An outside
    /// person downloads a file of a record they read when it is shared and their rule shows them shared files, when
    /// they uploaded it, or when a field of the record as they read it names it (BD-22); otherwise it is not found,
    /// the same as a file that is not there.
    pub async fn may_download_file(&self, ctx: &Ctx, file: Uuid, entity: Option<&str>, record: Option<Uuid>, by: Option<Uuid>, shared: bool) -> Result<()> {
        let (Some(e), Some(r), true) = (entity.filter(|e| !is_kind(e)), record, ctx.outside) else { return self.may_download(ctx, entity, record, by).await };
        let doc = self.read_record(ctx, e, r, false).await.map_err(|_| Error::NotFound("file".into()))?;
        let seen = (shared && ctx.perms.shared(e).is_some())
            || (!ctx.user.is_nil() && by == Some(ctx.user))
            || ctx.schema.entity(e).is_some_and(|ent| named_files(ctx, ent, &doc).contains(&file));
        if seen { Ok(()) } else { Err(Error::NotFound("file".into())) }
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
            // An id naming no stored file gives nothing away, nor one of a kind an app keeps for its own work ([`FileKind`]):
            // neither is tied.
            let Some((entity, record, by)) = row.filter(|r| !r.0.as_deref().is_some_and(is_kind)) else { continue };
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

    /// The files `ids` going out with, or kept on, record `record` of `entity` (a reply's attachments), in the caller's
    /// transaction (BD-43): each must already be on that record, or be a loose upload of the caller's (the system's:
    /// any loose one), and the loose ones are attached to it. Asked as attaching asks (whoever may change the record;
    /// an outside person whose portal rule lets them add to its conversation). Each file's `{id, file_name,
    /// content_type, size}`, in the order asked; a file that is not there, not theirs or another record's is refused,
    /// and nothing is attached.
    pub async fn attach_files_tx(&self, ctx: &Ctx, tx: &mut Tx, ids: &[Uuid], entity: &str, record: Uuid) -> Result<Vec<Value>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        self.may_attach_tx(ctx, tx, entity, record).await?;
        type Stored = (Uuid, String, String, i64, Option<String>, Option<Uuid>, Option<Uuid>);
        let rows: Vec<Stored> = sqlx::query_as(
            "select id, file_name, content_type, size_bytes, entity, record_id, created_by from file_object where tenant_id = $1 and id = any($2) for update",
        )
        .bind(ctx.tenant)
        .bind(ids)
        .fetch_all(&mut **tx)
        .await?;
        let mut out = Vec::new();
        let mut loose = Vec::new();
        for wanted in ids {
            let usable = rows.iter().find(|r| {
                r.0 == *wanted
                    && match (&r.4, r.5) {
                        (Some(e), Some(rec)) => e == entity && rec == record,
                        (None, None) => ctx.is_system || r.6 == Some(ctx.user),
                        _ => false,
                    }
            });
            let Some((id, name, ctype, size, on, ..)) = usable else {
                return Err(Error::field("files", format!("file {wanted} is not there, is not yours, or belongs to another record")));
            };
            if on.is_none() && !loose.contains(id) {
                loose.push(*id);
            }
            out.push(serde_json::json!({"id": id, "file_name": name, "content_type": ctype, "size": size}));
        }
        let outside = ctx.outside;
        sqlx::query(
            "update file_object set entity = $3, record_id = $4, shared = shared or $5 where tenant_id = $1 and id = any($2) and entity is null and record_id is null",
        )
        .bind(ctx.tenant)
        .bind(&loose)
        .bind(entity)
        .bind(record)
        .bind(outside)
        .execute(&mut **tx)
        .await?;
        Ok(out)
    }

    /// Copies a file the caller can read onto another record (a mail attachment kept on the record
    /// it concerns, a signed PDF filed with its contract). A new object, so deleting either copy
    /// never affects the other.
    pub async fn copy_file(&self, ctx: &Ctx, id: Uuid, attach_to: Option<(&str, Uuid)>) -> Result<Value> {
        let (name, ctype, data) = self.read_file(ctx, id).await?;
        self.store_file(ctx, attach_to, &name, &ctype, &data).await
    }
}
