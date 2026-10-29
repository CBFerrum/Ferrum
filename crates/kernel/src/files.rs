//! Files that packages create or receive (mail attachments, generated documents) go where
//! uploads go: the server hands the kernel its object store at boot (S3-compatible or disk), and
//! they are recorded in `file_object` like any upload, with the same access rules.

use crate::error::{Error, Result};
use crate::{Ctx, Kernel, Tx};
use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use uuid::Uuid;

#[async_trait]
pub trait FileStore: Send + Sync {
    async fn put(&self, key: &str, data: &[u8]) -> Result<()>;
    async fn get(&self, key: &str) -> Result<Vec<u8>>;
    async fn delete(&self, key: &str) -> Result<()>;
}

/// Content types never served inline as-is (they could run script in the app's origin).
const ACTIVE_TYPES: &[&str] = &["text/html", "application/xhtml+xml", "image/svg+xml", "application/javascript", "text/javascript"];

impl Kernel {
    /// Installs the object store (once, at boot).
    pub fn set_file_store(&self, store: Arc<dyn FileStore>) {
        let _ = self.files.set(store);
    }

    fn file_store(&self) -> Result<&Arc<dyn FileStore>> {
        self.files.get().ok_or_else(|| Error::Unavailable("no file storage is configured".into()))
    }

    /// Stores a file, optionally attached to a record the caller can read, and returns
    /// `{id, url, file_name, size, sha256}` — exactly what the upload API returns.
    pub async fn store_file(&self, ctx: &Ctx, attach_to: Option<(&str, Uuid)>, file_name: &str, content_type: &str, data: &[u8]) -> Result<Value> {
        if let Some((entity, record)) = attach_to {
            self.get(ctx, entity, record, false).await?;
        }
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
            "insert into file_object (tenant_id, id, entity, record_id, file_name, content_type, size_bytes, storage_key, sha256, created_by) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
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
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(json!({"id": id, "url": format!("/files/{id}"), "file_name": name, "size": data.len(), "sha256": sha}))
    }

    /// A stored file's name, content type and bytes, if the caller may read the record it is
    /// attached to.
    pub async fn read_file(&self, ctx: &Ctx, id: Uuid) -> Result<(String, String, Vec<u8>)> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(String, String, String, Option<String>, Option<Uuid>)> =
            sqlx::query_as("select file_name, content_type, storage_key, entity, record_id from file_object where tenant_id = $1 and id = $2")
                .bind(ctx.tenant)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        let (name, ctype, key, entity, record) = row.ok_or_else(|| Error::NotFound("file".into()))?;
        if let (Some(e), Some(r)) = (entity, record) {
            self.get(ctx, &e, r, false).await?;
        }
        Ok((name, ctype, self.file_store()?.get(&key).await?))
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
