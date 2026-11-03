//! Secret fields: write-only values (passwords, API keys) sealed with AES-256-GCM and bound to
//! their tenant, entity, record and field (so a sealed value cannot be moved to another record).
//! Documents, lists, events, the audit trail and exports only ever show whether one is set
//! (`true`); server code reads the value with [`Kernel::secret`].
//!
//! Rotating the server's key: set the new key as `FERRUM_SECRET_KEY` and the old one in
//! `FERRUM_RETIRED_SECRET_KEYS`. Values open with either, and [`Kernel::reseal_secrets`] moves them
//! all to the new key. Losing a key loses what it sealed: docs/operations.md says what that means.

use crate::db::Tx;
use crate::error::{Error, Result};
use crate::{Ctx, Doc, Kernel};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ferrum_meta::EffEntity;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Longest secret accepted (certificates and service-account JSON fit).
pub const MAX_SECRET: usize = 16 * 1024;

/// The encryption key and a short id naming it (stored with each value, for rotation).
#[derive(Clone)]
pub struct SecretKey {
    key: [u8; 32],
    pub id: String,
}

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretKey({})", self.id)
    }
}

impl SecretKey {
    pub fn new(key: [u8; 32]) -> Self {
        let id = hex::encode(&Sha256::digest(key)[..4]);
        SecretKey { key, id }
    }

    /// The key as it is configured: base64 of its 32 bytes.
    pub fn to_base64(&self) -> String {
        B64.encode(self.key)
    }

    /// From base64 (32 random bytes), as configured.
    pub fn from_base64(s: &str) -> Result<Self> {
        let bytes = B64.decode(s.trim()).map_err(|_| Error::invalid("the secret key must be base64"))?;
        let key: [u8; 32] = bytes.try_into().map_err(|_| Error::invalid("the secret key must be 32 bytes (openssl rand -base64 32)"))?;
        Ok(SecretKey::new(key))
    }

    fn aead(&self) -> LessSafeKey {
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &self.key).expect("32-byte AES key"))
    }

    pub fn seal(&self, aad: &str, plain: &str) -> Result<String> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce).map_err(|_| Error::Internal("no randomness for sealing".into()))?;
        let mut data = plain.as_bytes().to_vec();
        self.aead()
            .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(aad.as_bytes()), &mut data)
            .map_err(|_| Error::Internal("sealing failed".into()))?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&data);
        Ok(B64.encode(out))
    }

    pub fn open(&self, aad: &str, sealed: &str) -> Result<String> {
        let raw = B64.decode(sealed).map_err(|_| Error::Internal("corrupt sealed secret".into()))?;
        if raw.len() < NONCE_LEN {
            return Err(Error::Internal("corrupt sealed secret".into()));
        }
        let (nonce, body) = raw.split_at(NONCE_LEN);
        let mut body = body.to_vec();
        let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| Error::Internal("corrupt sealed secret".into()))?;
        let plain = self
            .aead()
            .open_in_place(nonce, Aad::from(aad.as_bytes()), &mut body)
            .map_err(|_| Error::Internal("a secret could not be opened (wrong key, or it was tampered with)".into()))?;
        String::from_utf8(plain.to_vec()).map_err(|_| Error::Internal("corrupt sealed secret".into()))
    }
}

fn aad(tenant: Uuid, entity: &str, record: Uuid, field: &str) -> String {
    format!("{tenant}/{entity}/{record}/{field}")
}

/// How a sealed webhook secret is told from one stored before they were sealed.
const SEALED_WEBHOOK: &str = "sealed:";

fn webhook_label(tenant: Uuid, webhook: Uuid) -> String {
    format!("{tenant}/webhook/{webhook}")
}

/// What [`Kernel::reseal_secrets`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Resealed {
    pub moved: u64,
    /// Values none of the configured keys opens. Above zero, keep the retired keys.
    pub unopened: u64,
}

/// A change requested in input: set (`Some`) or clear (`None`).
pub type SecretChange = (String, Option<String>);

/// Reads a secret field's input: text sets it, null clears it, `true` (the read-back marker)
/// leaves it unchanged. Returns the marker to keep in the document.
pub fn take_input(field: &str, v: &Value, changes: &mut Vec<SecretChange>) -> std::result::Result<Option<Value>, String> {
    match v {
        Value::Bool(true) => Ok(None),
        Value::Null => {
            changes.push((field.to_string(), None));
            Ok(Some(Value::Null))
        }
        Value::String(s) if s.is_empty() => {
            changes.push((field.to_string(), None));
            Ok(Some(Value::Null))
        }
        Value::String(s) if s.len() > MAX_SECRET => Err(format!("longer than {MAX_SECRET} bytes")),
        Value::String(s) => {
            changes.push((field.to_string(), Some(s.clone())));
            Ok(Some(Value::Bool(true)))
        }
        _ => Err("send the secret as text (or null to remove it)".into()),
    }
}

impl Kernel {
    /// Stores requested secret changes for a record (inside its transaction).
    pub(crate) async fn write_secrets(&self, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, record: Uuid, changes: &[SecretChange]) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let entity = &ent.def.name;
        for (field, value) in changes {
            match value {
                Some(plain) => {
                    let key = self
                        .settings
                        .secret_key
                        .as_ref()
                        .ok_or_else(|| Error::Unavailable("secret fields need a configured secret key (FERRUM_SECRET_KEY)".into()))?;
                    let sealed = key.seal(&aad(ctx.tenant, entity, record, field), plain)?;
                    sqlx::query(
                        "insert into record_secret (tenant_id, entity, record_id, field, sealed, key_id, updated_by) values ($1, $2, $3, $4, $5, $6, $7) \
                         on conflict (tenant_id, entity, record_id, field) do update set sealed = excluded.sealed, key_id = excluded.key_id, \
                         updated_at = now(), updated_by = excluded.updated_by",
                    )
                    .bind(ctx.tenant)
                    .bind(entity)
                    .bind(record)
                    .bind(field)
                    .bind(sealed)
                    .bind(&key.id)
                    .bind((!ctx.user.is_nil()).then_some(ctx.user))
                    .execute(&mut **tx)
                    .await?;
                }
                None => {
                    sqlx::query("delete from record_secret where tenant_id = $1 and entity = $2 and record_id = $3 and field = $4")
                        .bind(ctx.tenant)
                        .bind(entity)
                        .bind(record)
                        .bind(field)
                        .execute(&mut **tx)
                        .await?;
                }
            }
        }
        // The audit trail records which secrets changed, never their values.
        let fields: Vec<&String> = changes.iter().map(|(f, _)| f).collect();
        crate::audit::record(tx, ctx, entity, record, "secrets", None, json!({"fields": fields})).await
    }

    /// Marks which secret fields of a loaded document are set (`true`) or not (`null`).
    pub(crate) async fn mark_secrets(&self, tx: &mut Tx, tenant: Uuid, ent: &EffEntity, doc: &mut Doc) -> Result<()> {
        if ent.secret_fields().next().is_none() {
            return Ok(());
        }
        let Some(id) = doc.get("id").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(()) };
        let set: Vec<String> = sqlx::query_scalar("select field from record_secret where tenant_id = $1 and entity = $2 and record_id = $3")
            .bind(tenant)
            .bind(&ent.def.name)
            .bind(id)
            .fetch_all(&mut **tx)
            .await?;
        for f in ent.secret_fields() {
            doc.insert(f.name.clone(), if set.contains(&f.name) { Value::Bool(true) } else { Value::Null });
        }
        Ok(())
    }

    /// A secret's value, for server-side use only (sending mail, calling a provider). Never return
    /// it to a client or put it in a document, event or log.
    pub async fn secret(&self, tx: &mut Tx, tenant: Uuid, entity: &str, record: Uuid, field: &str) -> Result<Option<String>> {
        let row: Option<(String, String)> =
            sqlx::query_as("select sealed, key_id from record_secret where tenant_id = $1 and entity = $2 and record_id = $3 and field = $4")
                .bind(tenant)
                .bind(entity)
                .bind(record)
                .bind(field)
                .fetch_optional(&mut **tx)
                .await?;
        let Some((sealed, key_id)) = row else { return Ok(None) };
        if self.settings.secret_key.is_none() {
            return Err(Error::Unavailable("secret fields need a configured secret key (FERRUM_SECRET_KEY)".into()));
        }
        let key = self.secret_keys().find(|k| k.id == key_id);
        let key = key.ok_or_else(|| {
            Error::Unavailable(format!("{entity}.{field} was sealed with another key ({key_id}); configure that key or set the secret again"))
        })?;
        key.open(&aad(tenant, entity, record, field), &sealed).map(Some)
    }

    /// The server's key, then those being rotated out.
    fn secret_keys(&self) -> impl Iterator<Item = &SecretKey> {
        self.settings.secret_key.iter().chain(&self.settings.retired_secret_keys)
    }

    /// Opens what was sealed with the server's key or one being rotated out. AES-GCM tells the right
    /// key from a wrong one, so a value need not say which sealed it.
    pub(crate) fn open_sealed(&self, aad: &str, sealed: &str) -> Result<String> {
        let mut last = Error::Unavailable("nothing can be opened without the server's secret key (FERRUM_SECRET_KEY)".into());
        for key in self.secret_keys() {
            match key.open(aad, sealed) {
                Ok(plain) => return Ok(plain),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// `sealed` under the current key, when another sealed it; `None` when it is current already.
    /// A value no key opens is counted and left as it is: it may yet open with a key configured later.
    fn moved(&self, aad: &str, sealed: &str, unopened: &mut u64) -> Result<Option<String>> {
        let Some(current) = self.settings.secret_key.as_ref() else { return Ok(None) };
        if current.open(aad, sealed).is_ok() {
            return Ok(None);
        }
        match self.open_sealed(aad, sealed) {
            Ok(plain) => current.seal(aad, &plain).map(Some),
            Err(_) => {
                *unopened += 1;
                Ok(None)
            }
        }
    }

    /// What is stored for a webhook's signing secret: sealed, when the server has a key.
    pub fn seal_webhook_secret(&self, tenant: Uuid, webhook: Uuid, secret: &str) -> Result<String> {
        match &self.settings.secret_key {
            Some(key) => Ok(format!("{SEALED_WEBHOOK}{}", key.seal(&webhook_label(tenant, webhook), secret)?)),
            None => Ok(secret.to_string()),
        }
    }

    /// A webhook's signing secret, from what is stored. A secret from before they were sealed reads
    /// as it is, until [`Kernel::reseal_secrets`] has reached it.
    pub(crate) fn webhook_secret(&self, tenant: Uuid, webhook: Uuid, stored: &str) -> Result<String> {
        match stored.strip_prefix(SEALED_WEBHOOK) {
            Some(sealed) => self.open_sealed(&webhook_label(tenant, webhook), sealed),
            None => Ok(stored.to_string()),
        }
    }

    /// Moves everything sealed with a retired key to the current one, and seals webhook secrets
    /// stored in the clear. Safe to repeat and beside traffic: a value changed meanwhile is left for
    /// the next run. Plugins' settings are their host's (`PluginHost::seal_stored`).
    pub async fn reseal_secrets(&self) -> Result<Resealed> {
        let mut done = Resealed::default();
        let Some(current) = self.settings.secret_key.as_ref() else { return Ok(done) };
        let mut tx = self.db.system_tx().await?;
        let hooks: Vec<(Uuid, Uuid, String)> = sqlx::query_as("select tenant_id, id, secret from webhook").fetch_all(&mut *tx).await?;
        for (tenant, id, stored) in hooks {
            let label = webhook_label(tenant, id);
            let plain = match stored.strip_prefix(SEALED_WEBHOOK) {
                Some(sealed) => self.moved(&label, sealed, &mut done.unopened)?.map(|s| format!("{SEALED_WEBHOOK}{s}")),
                None => Some(format!("{SEALED_WEBHOOK}{}", current.seal(&label, &stored)?)),
            };
            if let Some(new) = plain {
                done.moved += sqlx::query("update webhook set secret = $3 where tenant_id = $1 and id = $2 and secret = $4")
                    .bind(tenant)
                    .bind(id)
                    .bind(new)
                    .bind(&stored)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            }
        }
        if !self.settings.retired_secret_keys.is_empty() {
            let fields: Vec<(Uuid, String, Uuid, String, String)> =
                sqlx::query_as("select tenant_id, entity, record_id, field, sealed from record_secret where key_id <> $1")
                    .bind(&current.id)
                    .fetch_all(&mut *tx)
                    .await?;
            for (tenant, entity, record, field, sealed) in fields {
                let Some(new) = self.moved(&aad(tenant, &entity, record, &field), &sealed, &mut done.unopened)? else { continue };
                done.moved += sqlx::query(
                    "update record_secret set sealed = $5, key_id = $6 where tenant_id = $1 and entity = $2 and record_id = $3 and field = $4 and sealed = $7",
                )
                .bind(tenant)
                .bind(&entity)
                .bind(record)
                .bind(&field)
                .bind(new)
                .bind(&current.id)
                .bind(&sealed)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            }
            let people: Vec<(Uuid, Uuid, String)> =
                sqlx::query_as("select tenant_id, id, totp_secret from app_user where totp_secret is not null").fetch_all(&mut *tx).await?;
            for (tenant, user, sealed) in people {
                let Some(new) = self.moved(&crate::twofactor::sealed_label(tenant, user), &sealed, &mut done.unopened)? else { continue };
                done.moved += sqlx::query("update app_user set totp_secret = $3 where tenant_id = $1 and id = $2 and totp_secret = $4")
                    .bind(tenant)
                    .bind(user)
                    .bind(new)
                    .bind(&sealed)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            }
            let connections: Vec<(Uuid, Uuid, String)> =
                sqlx::query_as("select tenant_id, id, sealed from connection where sealed is not null").fetch_all(&mut *tx).await?;
            for (tenant, id, sealed) in connections {
                let Some(new) = self.moved(&crate::connect::sealed_label(tenant, id), &sealed, &mut done.unopened)? else { continue };
                done.moved += sqlx::query("update connection set sealed = $3 where tenant_id = $1 and id = $2 and sealed = $4")
                    .bind(tenant)
                    .bind(id)
                    .bind(new)
                    .bind(&sealed)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            }
            let providers: Vec<(Uuid, Uuid, String)> =
                sqlx::query_as("select tenant_id, id, client_secret from sso_provider where client_secret is not null").fetch_all(&mut *tx).await?;
            for (tenant, id, sealed) in providers {
                let Some(new) = self.moved(&crate::sso::sealed_label(tenant, id), &sealed, &mut done.unopened)? else { continue };
                done.moved += sqlx::query("update sso_provider set client_secret = $3 where tenant_id = $1 and id = $2 and client_secret = $4")
                    .bind(tenant)
                    .bind(id)
                    .bind(new)
                    .bind(&sealed)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            }
            let saml: Vec<(Uuid, String)> = sqlx::query_as("select tenant_id, private_key from saml_sp_key").fetch_all(&mut *tx).await?;
            for (tenant, sealed) in saml {
                let Some(new) = self.moved(&crate::sso::saml_key_label(tenant), &sealed, &mut done.unopened)? else { continue };
                done.moved += sqlx::query("update saml_sp_key set private_key = $2 where tenant_id = $1 and private_key = $3")
                    .bind(tenant)
                    .bind(new)
                    .bind(&sealed)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            }
            let server: Vec<(String, String)> = sqlx::query_as("select name, sealed from server_key").fetch_all(&mut *tx).await?;
            for (name, sealed) in server {
                let Some(new) = self.moved(crate::webpush::VAPID_LABEL, &sealed, &mut done.unopened)? else { continue };
                done.moved += sqlx::query("update server_key set sealed = $2 where name = $1 and sealed = $3")
                    .bind(name)
                    .bind(new)
                    .bind(&sealed)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
            }
        }
        tx.commit().await?;
        Ok(done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_values_are_bound_to_their_place() {
        let key = SecretKey::new([7u8; 32]);
        let sealed = key.seal("t/E/r/f", "hunter2").unwrap();
        assert_ne!(sealed, key.seal("t/E/r/f", "hunter2").unwrap(), "fresh nonce each time");
        assert_eq!(key.open("t/E/r/f", &sealed).unwrap(), "hunter2");
        assert!(key.open("t/E/other/f", &sealed).is_err(), "moved to another record");
        assert!(SecretKey::new([8u8; 32]).open("t/E/r/f", &sealed).is_err(), "wrong key");
        let mut tampered = B64.decode(&sealed).unwrap();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(key.open("t/E/r/f", &B64.encode(tampered)).is_err());
    }

    #[test]
    fn input_sets_clears_or_keeps() {
        let mut changes = vec![];
        assert_eq!(take_input("pw", &json!(true), &mut changes), Ok(None));
        assert_eq!(take_input("pw", &json!("s3cret"), &mut changes), Ok(Some(json!(true))));
        assert_eq!(take_input("pw", &json!(null), &mut changes), Ok(Some(Value::Null)));
        assert!(take_input("pw", &json!(42), &mut changes).is_err());
        assert_eq!(changes, vec![("pw".to_string(), Some("s3cret".to_string())), ("pw".to_string(), None)]);
        assert!(SecretKey::from_base64("short").is_err());
        assert_eq!(SecretKey::from_base64(&B64.encode([1u8; 32])).unwrap().id.len(), 8);
    }
}
