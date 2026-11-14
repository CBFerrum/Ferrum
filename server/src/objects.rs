//! Direct access to the object store behind Loco's storage (the same bucket or directory, the same
//! keys): downloads stream instead of loading whole files into memory, and with S3 the browser
//! uploads large files straight into the bucket through presigned URLs, so no API replica holds
//! them. A file larger than one request may also come through the server in parts, on any store:
//! after a dropped connection the upload goes on from what has arrived (FERRUM-OPS-006).
//! Completing an upload checks the object and records it like any other file.

use crate::state::Settings;
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ferrum_kernel::{Ctx, Error, Kernel, Result};
use futures_util::StreamExt;
use opendal::services::{Fs, S3};
use opendal::{Operator, Writer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::ops::Range;
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
    op: Operator,
    presigns: bool,
}

fn storage_err(e: impl std::fmt::Display) -> Error {
    Error::Unavailable(format!("storage: {e}"))
}

impl Objects {
    /// The store `build_storage` configures for Loco, opened again for streaming and presigning.
    pub fn new(settings: &Settings) -> std::result::Result<Objects, String> {
        if settings.s3_bucket.is_empty() {
            let op = Operator::new(Fs::default().root(&settings.files_dir)).map_err(|e| e.to_string())?;
            return Ok(Objects { op, presigns: false });
        }
        // OpenDAL talks HTTP through a process-wide transport that must be installed first (safe to repeat).
        opendal::install_default();
        let region = if settings.s3_region.is_empty() { "auto" } else { settings.s3_region.as_str() };
        let mut s3 = S3::default().bucket(&settings.s3_bucket).region(region);
        if !settings.s3_endpoint.is_empty() {
            let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
            s3 = s3.endpoint(&settings.s3_endpoint);
            if let (Some(id), Some(secret)) = (env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY")) {
                s3 = s3.access_key_id(&id).secret_access_key(&secret);
            }
            if let Some(token) = env("AWS_SESSION_TOKEN") {
                s3 = s3.session_token(&token);
            }
        }
        Ok(Objects { op: Operator::new(s3).map_err(|e| e.to_string())?, presigns: true })
    }

    /// A store on a local directory (tests, a single host).
    pub fn local(dir: &str) -> std::result::Result<Objects, String> {
        Ok(Objects { op: Operator::new(Fs::default().root(dir)).map_err(|e| e.to_string())?, presigns: false })
    }

    /// Whether the browser can upload straight to the store (S3 and compatibles; not a local disk).
    /// The keys under `prefix` (for journals; not for anything a request waits on).
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        match self.op.list_with(prefix).recursive(true).await {
            Ok(entries) => Ok(entries.into_iter().map(|e| e.path().to_string()).filter(|p| !p.ends_with('/')).collect()),
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(storage_err(e)),
        }
    }

    pub fn direct_uploads(&self) -> bool {
        self.presigns
    }

    /// An object's bytes as a response body, and its length: only the range a `Range` header's
    /// value asks for when it asks one the object holds ([`byte_range`]), and that range.
    pub async fn stream(&self, key: &str, range: Option<&str>) -> Result<(u64, Option<Range<u64>>, Body)> {
        let size = self.op.stat(key).await.map_err(storage_err)?.content_length();
        let part = byte_range(range, size);
        let reader = self.op.reader(key).await.map_err(storage_err)?;
        let stream = match part.clone() {
            Some(r) => reader.into_bytes_stream(r).await,
            None => reader.into_bytes_stream(..).await,
        }
        .map_err(storage_err)?;
        Ok((size, part, Body::from_stream(stream)))
    }

    /// The bytes of `range` of an object, read as that range only (a connection's upload in chunks).
    pub async fn range(&self, key: &str, range: Range<u64>) -> Result<Vec<u8>> {
        let read = self.op.read_with(key).range(range).await.map_err(|e| match e.kind() {
            opendal::ErrorKind::NotFound => Error::NotFound(format!("stored object {key}")),
            _ => storage_err(e),
        })?;
        Ok(read.to_vec())
    }

    /// Size and SHA-256 of an object, read once as a stream; `None` when nothing is stored there.
    async fn digest(&self, key: &str) -> Result<Option<(u64, String)>> {
        let reader = match self.op.reader(key).await {
            Ok(r) => r,
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(storage_err(e)),
        };
        let mut stream = match reader.into_bytes_stream(..).await {
            Ok(s) => s,
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(storage_err(e)),
        };
        let (mut hash, mut size) = (Sha256::new(), 0u64);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(storage_err)?;
            size += chunk.len() as u64;
            hash.update(&chunk);
        }
        Ok(Some((size, hex::encode(hash.finalize()))))
    }

    /// Writes, reads and deletes a probe object: whether this store works, and not merely answers.
    pub async fn probe(&self) -> Result<()> {
        let key = format!(".ferrum-probe/{}", Uuid::now_v7());
        self.op.write(&key, b"ok".to_vec()).await.map_err(storage_err)?;
        let back = self.op.read(&key).await.map_err(storage_err)?;
        self.op.delete(&key).await.map_err(storage_err)?;
        if back.to_bytes().as_ref() == b"ok" { Ok(()) } else { Err(Error::Unavailable("storage: a probe came back different".into())) }
    }

    pub(crate) async fn exists(&self, key: &str) -> Result<bool> {
        match self.op.stat(key).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(storage_err(e)),
        }
    }

    async fn delete(&self, key: &str) {
        if let Err(e) = self.op.delete(key).await {
            tracing::warn!(%key, "could not delete an upload's object: {e}");
        }
    }

    /// Deletes the parts of an upload sent in parts, once it is recorded or forgotten.
    async fn drop_parts(&self, tenant: Uuid, id: Uuid) {
        let dir = format!("{}/", parts_dir(tenant, id));
        if let Err(e) = self.op.delete_with(&dir).recursive(true).await {
            tracing::warn!(%dir, "could not delete an upload's parts: {e}");
        }
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
        let part = objects.op.read(&part_key(tenant, id, offset)).await.map_err(storage_err)?;
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
           where expires_at < now() - interval '5 minutes' order by expires_at limit 500 for update skip locked) \
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
