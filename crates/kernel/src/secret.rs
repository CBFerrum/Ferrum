//! Secret fields: write-only values (passwords, API keys), each a secret of the app whose package declares the field, in
//! the vault (crate::vault::kept), and bound to their tenant, entity, record and field (so a value cannot be moved to
//! another record). Documents, lists, events, the audit trail and exports only ever show whether one is set (`true`); an
//! app reads the value of its own with its vault handle ([`crate::vault::Vault::field`]).

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
use zeroize::Zeroizing;

/// Longest secret accepted (certificates and service-account JSON fit).
pub const MAX_SECRET: usize = 16 * 1024;

/// A key of 32 bytes: what the platform derives from the vault for what it keys or seals outside it
/// (crate::vault::keyed), and what a key-management service or a move's transfer gives back.
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

    /// Its 32 bytes as base64, as [`SecretKey::from_base64`] takes them.
    pub fn to_base64(&self) -> String {
        B64.encode(self.key)
    }

    /// From base64 (32 random bytes), as a key-management service, a key command or a move's transfer gives it.
    pub fn from_base64(s: &str) -> Result<Self> {
        let bytes = B64.decode(s.trim()).map_err(|_| Error::invalid("a key must be base64"))?;
        let key: [u8; 32] = bytes.try_into().map_err(|_| Error::invalid("a key must be 32 bytes (openssl rand -base64 32)"))?;
        Ok(SecretKey::new(key))
    }

    fn aead(&self) -> LessSafeKey {
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &self.key).expect("32-byte AES key"))
    }

    /// Its 32 bytes, for the vault's slots that a key-management service's key opens (crate::vault).
    pub(crate) fn key_bytes(&self) -> &[u8; 32] {
        &self.key
    }

    /// A fingerprint of `data` under this key (HMAC-SHA256, hex), for telling equal secrets apart without
    /// keeping them: the same data gives the same fingerprint, and the fingerprint gives away nothing.
    pub fn fingerprint(&self, label: &str, data: &str) -> String {
        use hmac::Mac;
        let mut mac = hmac::Hmac::<Sha256>::new_from_slice(&self.key).expect("any key length");
        mac.update(label.as_bytes());
        mac.update(&[0]);
        mac.update(data.as_bytes());
        hex::encode(mac.finalize().into_bytes())
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
            let place = crate::vault::kept::Place::field(ctx.tenant, &self.field_owner(entity, field).await?, entity, record, field);
            match value {
                Some(plain) => {
                    self.keep(tx, &place, plain).await?;
                    sqlx::query(
                        "insert into record_secret (tenant_id, entity, record_id, field, updated_by) values ($1, $2, $3, $4, $5) \
                         on conflict (tenant_id, entity, record_id, field) do update set updated_at = now(), updated_by = excluded.updated_by",
                    )
                    .bind(ctx.tenant)
                    .bind(entity)
                    .bind(record)
                    .bind(field)
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
                    self.unkeep(tx, &place).await?;
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

    /// Keeps a webhook's new signing secret in the vault (the platform's, in the workspace), in the transaction that
    /// stores the webhook (docs/plan/stateless.md, D5).
    pub async fn keep_webhook_secret(&self, tx: &mut Tx, tenant: Uuid, webhook: Uuid, secret: &str) -> Result<()> {
        self.keep(tx, &crate::vault::kept::Place::webhook(tenant, webhook), secret).await
    }

    /// Removes a webhook's signing secret from the vault, in the transaction that deletes the webhook.
    pub async fn forget_webhook_secret(&self, tx: &mut Tx, tenant: Uuid, webhook: Uuid) -> Result<()> {
        crate::vault::kept::forget(tx, &crate::vault::kept::Place::webhook(tenant, webhook)).await
    }

    /// A webhook's signing secret, from the vault.
    pub(crate) async fn webhook_secret(&self, tenant: Uuid, webhook: Uuid) -> Result<Zeroizing<String>> {
        self.kept_alone(&crate::vault::kept::Place::webhook(tenant, webhook)).await
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
