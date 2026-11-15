//! Direct access to the object store behind Loco's storage (the same bucket or directory, the same
//! keys): downloads stream instead of loading whole files into memory, and with S3 the browser
//! uploads large files straight into the bucket through presigned URLs, so no API replica holds
//! them. A file larger than one request may also come through the server in parts, on any store:
//! after a dropped connection the upload goes on from what has arrived (FERRUM-OPS-006).
//! Completing an upload checks the object and records it like any other file.
//!
//! Loco's storage is these objects too ([`Files`]), so that everything the server stores goes one way: while files move
//! from the local directory to the store, a read looks in the store first and the directory second, a write goes to the
//! store, and a deletion takes both; and a deletion waits when `files_delete_after_days` says so (crate::store).

use crate::state::Settings;
use crate::store::Releases;
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::{Ctx, Error, Kernel, Result};
use futures_util::StreamExt;
use opendal::services::{Fs, S3};
use opendal::{Operator, Writer};
use serde_json::{Value, json};
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// The one range of bytes a `Range` header's value asks of an object `size` bytes long
/// (`bytes=a-b`, `bytes=a-`, or its last n with `bytes=-n`), cut to the object, when the object
/// holds any of it. Anything else (several ranges, other units, nothing of the object) is answered
/// with the whole object, as HTTP allows: a player seeking in a video asks one range at a time.
pub fn byte_range(header: Option<&str>, size: u64) -> Option<Range<u64>> {
    let (from, to) = header?.trim().strip_prefix("bytes=")?.split_once('-')?;
    let (from, to) = (from.trim(), to.trim());
    let range = if from.is_empty() {
        size.saturating_sub(to.parse().ok()?)..size
    } else {
        let end = if to.is_empty() { size } else { to.parse::<u64>().ok()?.saturating_add(1).min(size) };
        from.parse().ok()?..end
    };
    (range.start < range.end).then_some(range)
}

/// A streamed object ([`Objects::stream`]) as a response with `status`, or as the range it holds
/// (206, with its `Content-Range`). Either way it says ranges may be asked for.
pub fn streamed_response(status: StatusCode, (size, part, body): (u64, Option<Range<u64>>, Body)) -> Response {
    let (status, length) = match &part {
        Some(r) => (StatusCode::PARTIAL_CONTENT, r.end - r.start),
        None => (status, size),
    };
    let mut resp = (status, body).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(Ok(v)) = part.map(|r| HeaderValue::from_str(&format!("bytes {}-{}/{size}", r.start, r.end - 1))) {
        h.insert(header::CONTENT_RANGE, v);
    }
    resp
}

/// How long a presigned upload URL (and its pending upload) stays valid.
pub const UPLOAD_TTL: Duration = Duration::from_secs(3600);

/// The largest part of an upload sent in parts (`PUT /api/files/uploads/{id}/parts/{offset}`).
pub const PART_BYTES: u64 = 8 << 20;

/// How long an upload sent in parts waits for its next part before it is swept.
pub const PARTS_TTL: Duration = Duration::from_secs(24 * 3600);

pub struct Objects {
    pub(crate) op: Operator,
    presigns: bool,
    /// While files move from the local directory to the store (crate::store): read where the store lacks a key, and
    /// deleted from as well. Never written.
    pub(crate) directory: Option<Operator>,
    /// Deletions that wait (`files_delete_after_days`).
    pub(crate) releases: Arc<Releases>,
}

fn storage_err(e: impl std::fmt::Display) -> Error {
    Error::Unavailable(format!("storage: {e}"))
}

fn not_found(e: &opendal::Error) -> bool {
    e.kind() == opendal::ErrorKind::NotFound
}

/// Variables the S3 library reads to find a machine's own identity when no keys are given: a web identity token (IRSA
/// on EKS, or any OpenID Connect provider an S3-compatible store trusts), or a container's credentials (EKS Pod
/// Identity, ECS). Read here only to tell a machine that has one from a configuration that forgot the keys.
const IDENTITY: [&str; 3] = ["AWS_WEB_IDENTITY_TOKEN_FILE", "AWS_CONTAINER_CREDENTIALS_FULL_URI", "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"];

/// The object store `settings` name, as OpenDAL reaches it (`None` without a bucket). Its keys: the environment's, else
/// the vault's or those the bundled store handed over (crate::secrets); none, and the S3 library's own chain finds them
/// where the machine has an identity: AWS's own environment and profile, a web identity (IRSA), a container's or an
/// instance's role. An S3-compatible store (an endpoint) without keys is refused unless the machine names an identity.
pub fn s3(settings: &Settings) -> std::result::Result<Option<Operator>, String> {
    if settings.s3_bucket.is_empty() {
        return Ok(None);
    }
    // OpenDAL talks HTTP through a process-wide transport that must be installed first (safe to repeat).
    opendal::install_default();
    let region = if settings.s3_region.is_empty() { "auto" } else { settings.s3_region.as_str() };
    let mut s3 = S3::default().bucket(&settings.s3_bucket).region(region);
    let given = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty()).or_else(|| settings.revealed.get(&k.to_ascii_lowercase()).map(str::to_string));
    if !settings.s3_endpoint.is_empty() {
        s3 = s3.endpoint(&settings.s3_endpoint);
    }
    match (given("AWS_ACCESS_KEY_ID"), given("AWS_SECRET_ACCESS_KEY")) {
        (Some(id), Some(secret)) => {
            s3 = s3.access_key_id(&id).secret_access_key(&secret);
            if let Some(token) = given("AWS_SESSION_TOKEN") {
                s3 = s3.session_token(&token);
            }
        }
        _ if !settings.s3_endpoint.is_empty() && !IDENTITY.iter().any(|v| std::env::var(v).is_ok_and(|v| !v.trim().is_empty())) => {
            return Err("the object store at FERRUM_S3_ENDPOINT has no keys: set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY (or keep them in the \
                        vault: ferrum task secret_set), or give this machine an identity the store trusts (AWS_WEB_IDENTITY_TOKEN_FILE, \
                        AWS_CONTAINER_CREDENTIALS_FULL_URI)"
                .into());
        }
        _ => {}
    }
    Operator::new(s3).map(Some).map_err(|e| e.to_string())
}

/// A local directory as a store.
pub fn directory(dir: &str) -> std::result::Result<Operator, String> {
    Operator::new(Fs::default().root(dir)).map_err(|e| e.to_string())
}

impl Objects {
    /// The store the settings name: S3 when a bucket is set, else the local directory (crate::store decides whether the
    /// directory may be used, and whether it is read while files move).
    pub fn new(settings: &Settings) -> std::result::Result<Objects, String> {
        match s3(settings)? {
            Some(op) => Ok(Objects { op, presigns: true, directory: None, releases: Arc::default() }),
            None => Objects::local(&settings.files_dir),
        }
    }

    /// A store on a local directory (tests, a single host).
    pub fn local(dir: &str) -> std::result::Result<Objects, String> {
        Ok(Objects { op: directory(dir)?, presigns: false, directory: None, releases: Arc::default() })
    }

    /// This store, with the local directory `dir` read where it lacks a key and deleted from too, while files move.
    pub fn reading_directory(mut self, dir: &str) -> std::result::Result<Objects, String> {
        self.directory = Some(directory(dir)?);
        Ok(self)
    }

    /// This store, its deletions waiting as `releases` says.
    pub fn with_releases(mut self, releases: Arc<Releases>) -> Objects {
        self.releases = releases;
        self
    }

    /// Whether the local directory is read where the store lacks a key (files are moving to the store).
    pub fn reads_directory(&self) -> bool {
        self.directory.is_some()
    }

    /// Where `key` is, and its size: the store, else (while files move) the directory.
    async fn find(&self, key: &str) -> std::result::Result<(&Operator, u64), opendal::Error> {
        match (self.op.stat(key).await, &self.directory) {
            (Ok(m), _) => Ok((&self.op, m.content_length())),
            (Err(e), Some(dir)) if not_found(&e) => dir.stat(key).await.map(|m| (dir, m.content_length())),
            (Err(e), _) => Err(e),
        }
    }

    /// The keys under `prefix` (for journals; not for anything a request waits on): the store's, and the directory's
    /// while files move.
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let mut keys = std::collections::BTreeSet::new();
        for op in std::iter::once(&self.op).chain(&self.directory) {
            match op.list_with(prefix).recursive(true).await {
                Ok(entries) => keys.extend(entries.into_iter().map(|e| e.path().to_string()).filter(|p| !p.ends_with('/'))),
                Err(e) if not_found(&e) => {}
                Err(e) => return Err(storage_err(e)),
            }
        }
        Ok(keys.into_iter().collect())
    }

    /// Whether the browser can upload straight to the store (S3 and compatibles; not a local disk).
    pub fn direct_uploads(&self) -> bool {
        self.presigns
    }

    /// An object's bytes as a response body, and its length: only the range a `Range` header's
    /// value asks for when it asks one the object holds ([`byte_range`]), and that range.
    pub async fn stream(&self, key: &str, range: Option<&str>) -> Result<(u64, Option<Range<u64>>, Body)> {
        let (op, size) = self.find(key).await.map_err(storage_err)?;
        let part = byte_range(range, size);
        let reader = op.reader(key).await.map_err(storage_err)?;
        let stream = match part.clone() {
            Some(r) => reader.into_bytes_stream(r).await,
            None => reader.into_bytes_stream(..).await,
        }
        .map_err(storage_err)?;
        Ok((size, part, Body::from_stream(stream)))
    }

    /// The bytes of `range` of an object, read as that range only (a connection's upload in chunks).
    pub async fn range(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>> {
        let gone = |e: opendal::Error| if not_found(&e) { Error::NotFound(format!("stored object {key}")) } else { storage_err(e) };
        let (op, _) = self.find(key).await.map_err(gone)?;
        Ok(op.read_with(key).range(range).await.map_err(gone)?.to_vec())
    }

    /// A link that reads `key` from the store for `ttl`, answered with `content_type` and `disposition`
    /// (`files_direct_links`).
    pub async fn presign_download(&self, key: &str, ttl: Duration, content_type: &str, disposition: &str) -> Result<String> {
        let signed =
            self.op.presign_read_with(key, ttl).override_content_type(content_type).override_content_disposition(disposition).await.map_err(storage_err)?;
        Ok(signed.uri().to_string())
    }

    /// An object's length and SHA-256, from wherever [`Objects::find`] finds it; `None` when it is nowhere.
    pub(crate) async fn digest_found(&self, key: &str) -> std::result::Result<Option<(u64, String)>, opendal::Error> {
        match self.find(key).await {
            Ok((op, _)) => crate::store::digest_of(op, key).await,
            Err(e) if not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// A whole object, from wherever [`Objects::find`] finds it.
    pub(crate) async fn read(&self, key: &str) -> std::result::Result<bytes::Bytes, opendal::Error> {
        let (op, _) = self.find(key).await?;
        Ok(op.read(key).await?.to_bytes())
    }

    /// Writes an object to the store. A key that is written again after it was deleted (an interface's or an app's files,
    /// named by their content's digest) is first taken off the deletions that wait.
    pub(crate) async fn write(&self, key: &str, bytes: bytes::Bytes) -> Result<()> {
        self.releases.take_back(key).await?;
        self.op.write(key, bytes).await.map_err(storage_err)?;
        Ok(())
    }

    /// Deletes an object, or records its deletion to happen once `files_delete_after_days` have passed. While files
    /// move, from the directory first, then from the store: a copy `files_move` makes meanwhile goes too.
    pub(crate) async fn remove(&self, key: &str) -> Result<()> {
        if self.releases.defer(key).await? {
            return Ok(());
        }
        self.remove_now(key).await
    }

    /// Deletes an object now, from the directory first while files move, then from the store.
    pub(crate) async fn remove_now(&self, key: &str) -> Result<()> {
        if let Some(dir) = &self.directory {
            dir.delete(key).await.map_err(storage_err)?;
        }
        self.op.delete(key).await.map_err(storage_err)
    }

    /// Size and SHA-256 of an object, read once as a stream; `None` when nothing is stored there.
    async fn digest(&self, key: &str) -> Result<Option<(u64, String)>> {
        crate::store::digest_of(&self.op, key).await.map_err(storage_err)
    }

    /// Writes, reads and deletes a probe object: whether this store works, and not merely answers.
    pub async fn probe(&self) -> Result<()> {
        let key = format!(".ferrum-probe/{}", Uuid::now_v7());
        self.op.write(&key, b"ok".to_vec()).await.map_err(storage_err)?;
        let back = self.op.read(&key).await.map_err(storage_err)?;
        self.op.delete(&key).await.map_err(storage_err)?;
        if back.to_bytes().as_ref() == b"ok" { Ok(()) } else { Err(Error::Unavailable("storage: a probe came back different".into())) }
    }

    /// Whether an object is there: in the store, or (while files move) in the directory. One whose deletion waits and
    /// that may be written again is not, so that whoever asks writes it again ([`Objects::write`]).
    pub(crate) async fn exists(&self, key: &str) -> Result<bool> {
        Ok(self.size(key).await?.is_some() && !self.releases.pending(key).await?)
    }

    pub(crate) async fn size(&self, key: &str) -> Result<Option<u64>> {
        match self.find(key).await {
            Ok((_, size)) => Ok(Some(size)),
            Err(e) if not_found(&e) => Ok(None),
            Err(e) => Err(storage_err(e)),
        }
    }

    /// Deletes what an upload left (nothing a record names), now.
    async fn delete(&self, key: &str) {
        if let Err(e) = self.remove_now(key).await {
            tracing::warn!(%key, "could not delete an upload's object: {e}");
        }
    }

    /// Deletes the parts of an upload sent in parts, once it is recorded or forgotten.
    async fn drop_parts(&self, tenant: Uuid, id: Uuid) {
        let dir = format!("{}/", parts_dir(tenant, id));
        for op in self.directory.iter().chain(std::iter::once(&self.op)) {
            if let Err(e) = op.delete_with(&dir).recursive(true).await {
                tracing::warn!(%dir, "could not delete an upload's parts: {e}");
            }
        }
    }
}

/// Loco's storage over [`Objects`] (crate::app::build_storage): what handlers, the shell, the App Manager and the kernel's
/// file store keep goes through the same reads, writes and deletions as streams and uploads do.
pub struct Files(pub Arc<Objects>);

fn loco_err(e: Error) -> loco_rs::storage::StorageError {
    loco_rs::storage::StorageError::Any(e.to_string().into())
}

fn key(path: &std::path::Path) -> String {
    path.display().to_string()
}

#[async_trait::async_trait]
impl loco_rs::storage::drivers::StoreDriver for Files {
    async fn upload(&self, path: &std::path::Path, content: &bytes::Bytes) -> loco_rs::storage::StorageResult<loco_rs::storage::drivers::UploadResponse> {
        self.0.write(&key(path), content.clone()).await.map_err(loco_err)?;
        Ok(loco_rs::storage::drivers::UploadResponse::new(None, None))
    }
    async fn get(&self, path: &std::path::Path) -> loco_rs::storage::StorageResult<loco_rs::storage::drivers::GetResponse> {
        Ok(loco_rs::storage::drivers::GetResponse::from_bytes(self.0.read(&key(path)).await?))
    }
    async fn delete(&self, path: &std::path::Path) -> loco_rs::storage::StorageResult<()> {
        self.0.remove(&key(path)).await.map_err(loco_err)
    }
    async fn rename(&self, from: &std::path::Path, to: &std::path::Path) -> loco_rs::storage::StorageResult<()> {
        Ok(self.0.op.rename(&key(from), &key(to)).await?)
    }
    async fn copy(&self, from: &std::path::Path, to: &std::path::Path) -> loco_rs::storage::StorageResult<()> {
        self.0.op.copy(&key(from), &key(to)).await?;
        Ok(())
    }
    async fn exists(&self, path: &std::path::Path) -> loco_rs::storage::StorageResult<bool> {
        self.0.exists(&key(path)).await.map_err(loco_err)
    }
    async fn list(&self, path: &std::path::Path, recursive: bool) -> loco_rs::storage::StorageResult<Vec<loco_rs::storage::drivers::ListEntry>> {
        let entries = self.0.op.list_with(&key(path)).recursive(recursive).await?;
        Ok(entries
            .into_iter()
            .map(|e| loco_rs::storage::drivers::ListEntry::new(e.path().to_string(), e.metadata().is_dir(), Some(e.metadata().content_length()), None, None))
            .collect())
    }
    async fn stat(&self, path: &std::path::Path) -> loco_rs::storage::StorageResult<loco_rs::storage::drivers::ListEntry> {
        let (_, size) = self.0.find(&key(path)).await?;
        Ok(loco_rs::storage::drivers::ListEntry::new(key(path), false, Some(size), None, None))
    }
}

/// Content types never stored as declared (they could run script in the app's origin).
const ACTIVE_TYPES: &[&str] = &["text/html", "application/xhtml+xml", "image/svg+xml", "application/javascript", "text/javascript"];

pub struct NewUpload {
    pub file_name: String,
    pub content_type: String,
    pub size: u64,
    pub attach_to: Option<(String, Uuid)>,
    /// Sent through the server in parts (any store) rather than straight to object storage.
    pub parts: bool,
}

/// Starts an upload: records it as pending and returns the presigned request to send the bytes
/// with — `{upload_id, method, url, headers, expires_at}` — or, for one sent in parts, where to
/// start — `{upload_id, parts: true, part_bytes, size, offset: 0, expires_at}`.
pub async fn start(k: &Kernel, objects: &Objects, ctx: &Ctx, max_bytes: u64, up: NewUpload) -> Result<Value> {
    if !up.parts && !objects.direct_uploads() {
        return Err(Error::Conflict("direct uploads need S3-compatible object storage; upload through POST /api/files, or in parts (`parts: true`)".into()));
    }
    if ctx.user.is_nil() {
        return Err(Error::Forbidden("direct uploads need a signed-in user".into()));
    }
    if up.size == 0 || up.size > max_bytes {
        return Err(Error::field("size", format!("must be between 1 and {max_bytes} bytes")));
    }
    match &up.attach_to {
        Some((entity, record)) => k.may_attach(ctx, entity, *record).await?,
        None => k.count_loose_upload(ctx).await?,
    }
    let name: String = up.file_name.chars().filter(|c| !c.is_control() && *c != '/' && *c != '\\').take(200).collect();
    let name = if name.trim().is_empty() { "upload.bin".to_string() } else { name };
    let ctype = up.content_type.trim().to_lowercase();
    let ctype = if ctype.is_empty() || ctype.len() > 200 || ACTIVE_TYPES.contains(&ctype.as_str()) { "application/octet-stream".to_string() } else { ctype };
    // An upload in parts is refused now, not after its last part, when the workspace has no room.
    if up.parts {
        k.check_storage_quota(ctx.tenant, up.size).await?;
    }
    let id = Uuid::now_v7();
    let key = format!("tenants/{}/{}", ctx.tenant, id);
    let signed = if up.parts {
        None
    } else {
        Some(objects.op.presign_write_with(&staging_key(ctx.tenant, id), UPLOAD_TTL).content_type(&ctype).await.map_err(storage_err)?)
    };
    let expires_at = chrono::Utc::now() + chrono::Duration::from_std(if up.parts { PARTS_TTL } else { UPLOAD_TTL }).unwrap_or_default();
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    sqlx::query(
        "insert into file_upload (tenant_id, id, entity, record_id, file_name, content_type, size_bytes, storage_key, created_by, expires_at, parts) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(ctx.tenant)
    .bind(id)
    .bind(up.attach_to.as_ref().map(|a| a.0.clone()))
    .bind(up.attach_to.as_ref().map(|a| a.1))
    .bind(&name)
    .bind(&ctype)
    .bind(up.size as i64)
    .bind(&key)
    .bind(ctx.user)
    .bind(expires_at)
    .bind(up.parts)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let Some(signed) = signed else {
        return Ok(json!({"upload_id": id, "parts": true, "part_bytes": PART_BYTES, "size": up.size, "offset": 0, "expires_at": expires_at}));
    };
    let headers: serde_json::Map<String, Value> =
        signed.header().iter().filter_map(|(k, v)| Some((k.as_str().to_string(), Value::String(v.to_str().ok()?.to_string())))).collect();
    Ok(json!({"upload_id": id, "method": signed.method().as_str(), "url": signed.uri().to_string(), "headers": headers, "expires_at": expires_at}))
}

/// Where a direct upload's bytes land. The presigned URL stays valid after completion, so the file
/// is recorded from a copy under its own key: nothing sent to the URL later can change it.
fn staging_key(tenant: Uuid, id: Uuid) -> String {
    format!("uploads/{tenant}/{id}")
}

/// Where the parts of an upload sent in parts wait, each under its first byte's offset (padded, so
/// they list in order).
fn parts_dir(tenant: Uuid, id: Uuid) -> String {
    format!("uploads/{tenant}/{id}.parts")
}

fn part_key(tenant: Uuid, id: Uuid, offset: u64) -> String {
    format!("{}/{offset:020}", parts_dir(tenant, id))
}

/// Where an upload of the caller's stands — `{upload_id, parts, size, offset, part_bytes,
/// expires_at}`: after a dropped connection, the next part goes from `offset`.
pub async fn status(k: &Kernel, ctx: &Ctx, id: Uuid) -> Result<Value> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let row: Option<(i64, i64, bool, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("select size_bytes, received_bytes, parts, expires_at from file_upload where tenant_id = $1 and id = $2 and created_by = $3")
            .bind(ctx.tenant)
            .bind(id)
            .bind(ctx.user)
            .fetch_optional(&mut *tx)
            .await?;
    tx.commit().await?;
    let (size, received, parts, expires_at) = row.ok_or_else(|| Error::NotFound("upload".into()))?;
    Ok(json!({"upload_id": id, "parts": parts, "size": size, "offset": received, "part_bytes": PART_BYTES, "expires_at": expires_at}))
}

/// Stores the part of an upload of the caller's that starts at `offset`, which must be where the
/// upload stands: a part sent again after its answer was lost is refused, and the caller reads
/// where it stands. Each part keeps the upload for another day. Returns `{upload_id, size, offset}`.
pub async fn append(k: &Kernel, objects: &Objects, ctx: &Ctx, id: Uuid, offset: u64, part: bytes::Bytes) -> Result<Value> {
    if part.is_empty() || part.len() as u64 > PART_BYTES {
        return Err(Error::field("part", format!("must be between 1 and {PART_BYTES} bytes")));
    }
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    // Locked while the part is stored: two parts for one offset never both land.
    let row: Option<(i64, i64, bool, bool)> = sqlx::query_as(
        "select size_bytes, received_bytes, parts, expires_at < now() from file_upload where tenant_id = $1 and id = $2 and created_by = $3 for update",
    )
    .bind(ctx.tenant)
    .bind(id)
    .bind(ctx.user)
    .fetch_optional(&mut *tx)
    .await?;
    let (size, received, parts, expired) = row.ok_or_else(|| Error::NotFound("upload".into()))?;
    if !parts {
        return Err(Error::Conflict("this upload goes straight to object storage, not in parts".into()));
    }
    if expired {
        return Err(Error::Conflict("this upload expired; start a new one".into()));
    }
    let (size, received) = (size as u64, received as u64);
    if offset != received {
        return Err(Error::Conflict(format!("{received} of {size} bytes have arrived: send the part that starts there")));
    }
    let end = offset + part.len() as u64;
    if end > size {
        return Err(Error::field("part", format!("goes past the {size} bytes announced")));
    }
    objects.op.write(&part_key(ctx.tenant, id, offset), part).await.map_err(storage_err)?;
    let expires_at = chrono::Utc::now() + chrono::Duration::from_std(PARTS_TTL).unwrap_or_default();
    sqlx::query("update file_upload set received_bytes = $3, expires_at = $4 where tenant_id = $1 and id = $2")
        .bind(ctx.tenant)
        .bind(id)
        .bind(end as i64)
        .bind(expires_at)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(json!({"upload_id": id, "size": size, "offset": end}))
}

/// Joins an upload's parts, in order, into the object at `key`.
async fn join_parts(objects: &Objects, tenant: Uuid, id: Uuid, size: u64, key: &str) -> Result<()> {
    let mut w = objects.op.writer_with(key).chunk(PART_BYTES as usize).await.map_err(storage_err)?;
    match write_parts(objects, &mut w, tenant, id, size).await {
        Ok(()) => {
            w.close().await.map_err(storage_err)?;
            Ok(())
        }
        Err(e) => {
            let _ = w.abort().await;
            Err(e)
        }
    }
}

async fn write_parts(objects: &Objects, w: &mut Writer, tenant: Uuid, id: Uuid, size: u64) -> Result<()> {
    let mut offset = 0u64;
    while offset < size {
        // From the directory too, for an upload begun before files moved to the store.
        let part = objects.read(&part_key(tenant, id, offset)).await.map_err(storage_err)?;
        if part.is_empty() {
            return Err(Error::Unavailable(format!("storage: the part of this upload at byte {offset} is empty")));
        }
        offset += part.len() as u64;
        w.write(part).await.map_err(storage_err)?;
    }
    Ok(())
}

/// Completes an upload of the caller's: copies the uploaded object (or joins the parts) to the
/// file's key, checks its size, then records it as a file — the same `{id, url, file_name, size,
/// sha256}` an upload returns.
pub async fn complete(k: &Kernel, objects: &Objects, ctx: &Ctx, id: Uuid) -> Result<Value> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    #[allow(clippy::type_complexity)]
    let row: Option<(Option<String>, Option<Uuid>, String, String, i64, String, bool, bool, i64)> = sqlx::query_as(
        "select entity, record_id, file_name, content_type, size_bytes, storage_key, expires_at < now(), parts, received_bytes from file_upload \
         where tenant_id = $1 and id = $2 and created_by = $3",
    )
    .bind(ctx.tenant)
    .bind(id)
    .bind(ctx.user)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let (entity, record, name, ctype, size, key, expired, parts, received) = row.ok_or_else(|| Error::NotFound("upload".into()))?;
    if expired {
        return Err(Error::Conflict("this upload expired; start a new one".into()));
    }
    if let (Some(e), Some(r)) = (&entity, record) {
        k.get(ctx, e, r, false).await?;
    }
    if parts {
        if received != size {
            return Err(Error::Conflict(format!("{received} of {size} bytes have arrived: send the rest first")));
        }
        // The parts stay until the file is recorded: completing again joins them again.
        join_parts(objects, ctx.tenant, id, size as u64, &key).await?;
    } else {
        let staged = staging_key(ctx.tenant, id);
        if !objects.exists(&staged).await? {
            return Err(Error::Conflict("nothing has been uploaded yet: send the file to the upload URL first".into()));
        }
        objects.op.copy(&staged, &key).await.map_err(storage_err)?;
        objects.delete(&staged).await;
    }
    let Some((stored, sha)) = objects.digest(&key).await? else {
        return Err(Error::Unavailable("storage: the uploaded file vanished while it was being recorded".into()));
    };
    if stored != size as u64 {
        objects.delete(&key).await;
        forget(k, objects, ctx.tenant, id).await?;
        return Err(Error::field("size", format!("{stored} bytes arrived, {size} were announced; start a new upload")));
    }
    if let Err(e) = k.check_storage_quota(ctx.tenant, stored).await {
        objects.delete(&key).await;
        forget(k, objects, ctx.tenant, id).await?;
        return Err(e);
    }
    let bytes = objects.op.reader(&key).await.map_err(storage_err)?.into_bytes_stream(..).await.map_err(storage_err)?;
    let scanned = match k.scan_stream(ctx, &name, stored, bytes.map(|chunk| chunk.map_err(storage_err))).await {
        Ok(status) => status,
        // Found: the bytes go, the same way as ones of the wrong size. A scanner that is down keeps
        // them, unrecorded, for another try at completing.
        Err(Error::Invalid(why)) => {
            objects.delete(&key).await;
            forget(k, objects, ctx.tenant, id).await?;
            return Err(Error::Invalid(why));
        }
        Err(e) => return Err(e),
    };
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    // Deleting the pending row first makes completion happen once, however many requests race.
    let claimed = sqlx::query("delete from file_upload where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).execute(&mut *tx).await?.rows_affected();
    if claimed == 0 {
        // Cancelled while it was joined: what was joined goes too. Completed by a racing request
        // instead, it is that file's, and stays.
        let recorded: bool = sqlx::query_scalar("select exists (select 1 from file_object where tenant_id = $1 and id = $2)")
            .bind(ctx.tenant)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        if !recorded {
            objects.delete(&key).await;
        }
        return Err(Error::NotFound("upload".into()));
    }
    sqlx::query(
        "insert into file_object (tenant_id, id, entity, record_id, file_name, content_type, size_bytes, storage_key, sha256, created_by, scan_status, shared) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(ctx.tenant)
    .bind(id)
    .bind(&entity)
    .bind(record)
    .bind(&name)
    .bind(&ctype)
    .bind(size)
    .bind(&key)
    .bind(&sha)
    .bind(ctx.user)
    .bind(scanned)
    // What an outside person uploads is shared by nature (BD-22).
    .bind(ctx.outside)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if parts {
        objects.drop_parts(ctx.tenant, id).await;
    }
    Ok(json!({"id": id, "url": format!("/files/{id}"), "file_name": name, "size": size, "sha256": sha, "public": false}))
}

/// Cancels an upload in parts of the caller's: forgets it and deletes the parts that arrived, now.
/// Idempotent: one already cancelled, completed or never the caller's is left as it is (a
/// completed upload is a file, deleted as files are). A part waiting for the upload's row lock
/// stores its bytes first and then the parts go with it; one that comes after finds no upload
/// (`404`) and cannot bring it back. Nothing is held of the workspace's room for an upload (its
/// size is checked against what is stored when it starts and again when it completes), so there
/// is nothing to give back.
pub async fn cancel(k: &Kernel, objects: &Objects, ctx: &Ctx, id: Uuid) -> Result<()> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let parts: Option<bool> = sqlx::query_scalar("select parts from file_upload where tenant_id = $1 and id = $2 and created_by = $3 for update")
        .bind(ctx.tenant)
        .bind(id)
        .bind(ctx.user)
        .fetch_optional(&mut *tx)
        .await?;
    match parts {
        None => return Ok(()),
        // Its presigned URL takes bytes for the rest of the hour, which the sweep removes after it.
        Some(false) => return Err(Error::Conflict("this upload goes straight to object storage: it expires within the hour".into())),
        Some(true) => {}
    }
    sqlx::query("delete from file_upload where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    objects.drop_parts(ctx.tenant, id).await;
    Ok(())
}

async fn forget(k: &Kernel, objects: &Objects, tenant: Uuid, id: Uuid) -> Result<()> {
    let mut tx = k.db.tenant_tx(tenant).await?;
    sqlx::query("delete from file_upload where tenant_id = $1 and id = $2").bind(tenant).bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    objects.drop_parts(tenant, id).await;
    Ok(())
}

/// Deletes uploads that expired without completing, and whatever bytes they left in the store.
pub async fn sweep(k: &Kernel, objects: &Objects) -> Result<usize> {
    let mut tx = k.db.system_tx().await?;
    let gone: Vec<(Uuid, Uuid, bool)> = sqlx::query_as(
        // Replicas sweep at the same time: each takes rows nobody else holds.
        "delete from file_upload where (tenant_id, id) in (select tenant_id, id from file_upload \
           where expires_at < now() - interval '5 minutes' and not ferrum_workspace_held(tenant_id) order by expires_at limit 500 for update skip locked) \
         returning tenant_id, id, parts",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    for (tenant, id, parts) in &gone {
        if *parts {
            objects.drop_parts(*tenant, *id).await;
        } else {
            objects.delete(&staging_key(*tenant, *id)).await;
        }
    }
    Ok(gone.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_range_of_what_is_there() {
        let r = |h: &str| byte_range(Some(h), 1000);
        assert_eq!(r("bytes=0-99"), Some(0..100));
        assert_eq!(r("bytes=900-"), Some(900..1000));
        assert_eq!(r("bytes=-100"), Some(900..1000));
        assert_eq!(r("bytes=990-2000"), Some(990..1000), "cut to the object");
        assert_eq!(r("bytes=-5000"), Some(0..1000));
        for whole in ["bytes=1000-", "bytes=5-2", "bytes=0-1,5-9", "items=0-9", "bytes=-0", "bytes=x-9", ""] {
            assert_eq!(r(whole), None, "{whole}");
        }
        assert_eq!(byte_range(None, 1000), None);
        assert_eq!(byte_range(Some("bytes=0-"), 0), None, "nothing in an empty object");
    }

    #[test]
    fn a_range_is_answered_in_part() {
        let part = streamed_response(StatusCode::OK, (1000, Some(900..1000), Body::empty()));
        let h = |name| part.headers().get(name).and_then(|v| v.to_str().ok());
        assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!((h(header::CONTENT_LENGTH), h(header::CONTENT_RANGE), h(header::ACCEPT_RANGES)), (Some("100"), Some("bytes 900-999/1000"), Some("bytes")));
        let whole = streamed_response(StatusCode::OK, (1000, None, Body::empty()));
        assert_eq!((whole.status(), whole.headers().get(header::CONTENT_RANGE)), (StatusCode::OK, None));
        assert_eq!(whole.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()), Some("1000"));
    }
}
