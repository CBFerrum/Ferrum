//! Direct access to the object store behind Loco's storage (the same bucket or directory, the same
//! keys): downloads stream instead of loading whole files into memory, and with S3 the browser
//! uploads large files straight into the bucket through presigned URLs, so no API replica holds
//! them. Completing an upload checks the object and records it like any other file.

use crate::state::Settings;
use axum::body::Body;
use ferrum_kernel::{Ctx, Error, Kernel, Result};
use futures_util::StreamExt;
use opendal::Operator;
use opendal::services::{Fs, S3};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;
use uuid::Uuid;

/// How long a presigned upload URL (and its pending upload) stays valid.
pub const UPLOAD_TTL: Duration = Duration::from_secs(3600);

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

    /// Whether the browser can upload straight to the store (S3 and compatibles; not a local disk).
    pub fn direct_uploads(&self) -> bool {
        self.presigns
    }

    /// An object's bytes as a response body, and its length.
    pub async fn stream(&self, key: &str) -> Result<(u64, Body)> {
        let size = self.op.stat(key).await.map_err(storage_err)?.content_length();
        let stream = self.op.reader(key).await.map_err(storage_err)?.into_bytes_stream(..).await.map_err(storage_err)?;
        Ok((size, Body::from_stream(stream)))
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

    async fn exists(&self, key: &str) -> Result<bool> {
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
}

/// Content types never stored as declared (they could run script in the app's origin).
const ACTIVE_TYPES: &[&str] = &["text/html", "application/xhtml+xml", "image/svg+xml", "application/javascript", "text/javascript"];

pub struct NewUpload {
    pub file_name: String,
    pub content_type: String,
    pub size: u64,
    pub attach_to: Option<(String, Uuid)>,
}

/// Starts a direct upload: records it as pending and returns the presigned request to send the
/// bytes with — `{upload_id, method, url, headers, expires_at}`.
pub async fn start(k: &Kernel, objects: &Objects, ctx: &Ctx, max_bytes: u64, up: NewUpload) -> Result<Value> {
    if !objects.direct_uploads() {
        return Err(Error::Conflict("direct uploads need S3-compatible object storage; upload through POST /api/files instead".into()));
    }
    if ctx.user.is_nil() {
        return Err(Error::Forbidden("direct uploads need a signed-in user".into()));
    }
    if up.size == 0 || up.size > max_bytes {
        return Err(Error::field("size", format!("must be between 1 and {max_bytes} bytes")));
    }
    if let Some((entity, record)) = &up.attach_to {
        k.get(ctx, entity, *record, false).await?;
    }
    let name: String = up.file_name.chars().filter(|c| !c.is_control() && *c != '/' && *c != '\\').take(200).collect();
    let name = if name.trim().is_empty() { "upload.bin".to_string() } else { name };
    let ctype = up.content_type.trim().to_lowercase();
    let ctype = if ctype.is_empty() || ctype.len() > 200 || ACTIVE_TYPES.contains(&ctype.as_str()) { "application/octet-stream".to_string() } else { ctype };
    let id = Uuid::now_v7();
    let key = format!("tenants/{}/{}", ctx.tenant, id);
    let signed = objects.op.presign_write_with(&staging_key(ctx.tenant, id), UPLOAD_TTL).content_type(&ctype).await.map_err(storage_err)?;
    let expires_at = chrono::Utc::now() + chrono::Duration::from_std(UPLOAD_TTL).unwrap_or_default();
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    sqlx::query(
        "insert into file_upload (tenant_id, id, entity, record_id, file_name, content_type, size_bytes, storage_key, created_by, expires_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
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
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let headers: serde_json::Map<String, Value> =
        signed.header().iter().filter_map(|(k, v)| Some((k.as_str().to_string(), Value::String(v.to_str().ok()?.to_string())))).collect();
    Ok(json!({"upload_id": id, "method": signed.method().as_str(), "url": signed.uri().to_string(), "headers": headers, "expires_at": expires_at}))
}

/// Where a direct upload's bytes land. The presigned URL stays valid after completion, so the file
/// is recorded from a copy under its own key: nothing sent to the URL later can change it.
fn staging_key(tenant: Uuid, id: Uuid) -> String {
    format!("uploads/{tenant}/{id}")
}

/// Completes a direct upload of the caller's: copies the uploaded object to the file's key, checks
/// its size, then records it as a file — the same `{id, url, file_name, size, sha256}` an upload
/// returns.
pub async fn complete(k: &Kernel, objects: &Objects, ctx: &Ctx, id: Uuid) -> Result<Value> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let row: Option<(Option<String>, Option<Uuid>, String, String, i64, String, bool)> = sqlx::query_as(
        "select entity, record_id, file_name, content_type, size_bytes, storage_key, expires_at < now() from file_upload \
         where tenant_id = $1 and id = $2 and created_by = $3",
    )
    .bind(ctx.tenant)
    .bind(id)
    .bind(ctx.user)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let (entity, record, name, ctype, size, key, expired) = row.ok_or_else(|| Error::NotFound("upload".into()))?;
    if expired {
        return Err(Error::Conflict("this upload expired; start a new one".into()));
    }
    if let (Some(e), Some(r)) = (&entity, record) {
        k.get(ctx, e, r, false).await?;
    }
    let staged = staging_key(ctx.tenant, id);
    if !objects.exists(&staged).await? {
        return Err(Error::Conflict("nothing has been uploaded yet: send the file to the upload URL first".into()));
    }
    objects.op.copy(&staged, &key).await.map_err(storage_err)?;
    objects.delete(&staged).await;
    let Some((stored, sha)) = objects.digest(&key).await? else {
        return Err(Error::Unavailable("storage: the uploaded file vanished while it was being recorded".into()));
    };
    if stored != size as u64 {
        objects.delete(&key).await;
        forget(k, ctx.tenant, id).await?;
        return Err(Error::field("size", format!("{stored} bytes arrived, {size} were announced; start a new upload")));
    }
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    // Deleting the pending row first makes completion happen once, however many requests race.
    let claimed = sqlx::query("delete from file_upload where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).execute(&mut *tx).await?.rows_affected();
    if claimed == 0 {
        return Err(Error::NotFound("upload".into()));
    }
    sqlx::query(
        "insert into file_object (tenant_id, id, entity, record_id, file_name, content_type, size_bytes, storage_key, sha256, created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
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
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(json!({"id": id, "url": format!("/files/{id}"), "file_name": name, "size": size, "sha256": sha, "public": false}))
}

async fn forget(k: &Kernel, tenant: Uuid, id: Uuid) -> Result<()> {
    let mut tx = k.db.tenant_tx(tenant).await?;
    sqlx::query("delete from file_upload where tenant_id = $1 and id = $2").bind(tenant).bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Deletes uploads that expired without completing, and whatever bytes they left in the store.
pub async fn sweep(k: &Kernel, objects: &Objects) -> Result<usize> {
    let mut tx = k.db.system_tx().await?;
    let gone: Vec<(Uuid, Uuid)> = sqlx::query_as(
        // Replicas sweep at the same time: each takes rows nobody else holds.
        "delete from file_upload where (tenant_id, id) in (select tenant_id, id from file_upload \
           where expires_at < now() - interval '5 minutes' order by expires_at limit 500 for update skip locked) \
         returning tenant_id, id",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    for (tenant, id) in &gone {
        objects.delete(&staging_key(*tenant, *id)).await;
    }
    Ok(gone.len())
}
