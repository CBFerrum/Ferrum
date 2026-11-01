//! A device's identity: an Ed25519 key it made itself, enrolled once with a code a person issued,
//! which binds it to its workspace, site and device. The server keeps public keys only. Each request
//! is signed with a timestamp that must rise per key, so nothing can be replayed.
//!
//! Mutual TLS, where a site wants it, ends at a proxy; it does not replace this, because a proxy
//! cannot say which device of which workspace a certificate may speak for.

use crate::{authority, devices};
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use ferrum_kernel::{Ctx, Doc, Error, Kernel, Result, SiteRequest, Tx, audit};
use ferrum_ops_contract::Lifecycle;
use ferrum_ops_contract::sign::{Enroll, Enrolled, HEADER_KEY, HEADER_SIGNATURE, HEADER_TIME, MAX_CLOCK_SKEW_MS, message};
use rand::RngCore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// How long an enrollment code stays good.
const CODE_HOURS: i64 = 24;

fn hash_code(code: &str) -> String {
    hex::encode(Sha256::digest(code.trim().as_bytes()))
}

/// A one-time code for a claimed or quarantined device. Shown once, stored only as a hash.
pub async fn new_enrollment(k: &Kernel, ctx: &Ctx, device: Uuid) -> Result<(String, DateTime<Utc>)> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let lifecycle: Option<String> = sqlx::query_scalar("select lifecycle from e_device where tenant_id = $1 and id = $2 for update")
        .bind(ctx.tenant)
        .bind(device)
        .fetch_optional(&mut *tx)
        .await?;
    let lifecycle = devices::lifecycle_of(&lifecycle.ok_or_else(|| Error::NotFound("device".into()))?)?;
    if !lifecycle.can_move(Lifecycle::Provisioned) {
        return Err(Error::invalid(format!("a device that is {} cannot enroll a key", lifecycle.as_str())));
    }
    let mut bytes = [0u8; 20];
    rand::rng().fill_bytes(&mut bytes);
    let code = hex::encode(bytes);
    let expires = Utc::now() + Duration::hours(CODE_HOURS);
    sqlx::query("update op_enrollment set used_at = now() where tenant_id = $1 and device_id = $2 and used_at is null")
        .bind(ctx.tenant)
        .bind(device)
        .execute(&mut *tx)
        .await?;
    sqlx::query("insert into op_enrollment (tenant_id, code_hash, device_id, expires_at, created_by) values ($1, $2, $3, $4, $5)")
        .bind(ctx.tenant)
        .bind(hash_code(&code))
        .bind(device)
        .bind(expires)
        .bind(ctx.user)
        .execute(&mut *tx)
        .await?;
    audit::record(&mut tx, ctx, "Device", device, "ops.enrollment", None, json!({"expires": expires})).await?;
    tx.commit().await?;
    Ok((code, expires))
}

fn parse_key(hex_key: &str) -> Result<([u8; 32], VerifyingKey)> {
    let bytes: [u8; 32] = hex::decode(hex_key).ok().and_then(|b| b.try_into().ok()).ok_or_else(|| Error::field("public_key", "32 bytes of hex"))?;
    let key = VerifyingKey::from_bytes(&bytes).map_err(|_| Error::field("public_key", "not an Ed25519 key"))?;
    Ok((bytes, key))
}

/// Trades a code for a key. Earlier keys of the device stop working: enrolling again is how a
/// quarantined device comes back, and that must end whatever had its old key.
pub async fn enroll(k: &Kernel, tenant: Uuid, e: &Enroll) -> Result<Enrolled> {
    let (public_key, _) = parse_key(&e.public_key)?;
    let mut tx = k.db.tenant_tx(tenant).await?;
    let found: Option<(Uuid, DateTime<Utc>)> =
        sqlx::query_as("update op_enrollment set used_at = now() where tenant_id = $1 and code_hash = $2 and used_at is null returning device_id, expires_at")
            .bind(tenant)
            .bind(hash_code(&e.code))
            .fetch_optional(&mut *tx)
            .await?;
    let (device, expires) = found.ok_or_else(|| Error::Forbidden("this code is not valid, or was used".into()))?;
    if expires <= Utc::now() {
        return Err(Error::Forbidden("this code has expired".into()));
    }
    revoke_keys(&mut tx, tenant, device).await?;
    let key_id = Uuid::now_v7();
    sqlx::query("insert into op_device_key (tenant_id, id, device_id, public_key) values ($1, $2, $3, $4)")
        .bind(tenant)
        .bind(key_id)
        .bind(device)
        .bind(public_key.to_vec())
        .execute(&mut *tx)
        .await?;
    let target = devices::target(&mut tx, tenant, device).await?;
    if !target.lifecycle.can_move(Lifecycle::Provisioned) {
        return Err(Error::invalid(format!("a device that is {} cannot enroll a key", target.lifecycle.as_str())));
    }
    let ctx = Ctx::system(tenant, k.registry.schema(tenant).await?);
    let mut values = Doc::new();
    values.insert("lifecycle".into(), json!(Lifecycle::Provisioned.as_str()));
    k.set_values_tx(&ctx, &mut tx, "Device", device, values).await?;
    if target.kind == "Gateway" {
        authority::claim_if_free(&mut tx, tenant, &target.domain, target.site, device).await?;
    }
    audit::record(&mut tx, &ctx, "Device", device, "ops.enrolled", None, json!({"key": key_id})).await?;
    tx.commit().await?;
    Ok(Enrolled { key_id, device_id: device, site_id: target.site, domain: target.domain })
}

pub async fn revoke_keys(tx: &mut Tx, tenant: Uuid, device: Uuid) -> Result<u64> {
    Ok(sqlx::query("update op_device_key set revoked_at = now() where tenant_id = $1 and device_id = $2 and revoked_at is null")
        .bind(tenant)
        .bind(device)
        .execute(&mut **tx)
        .await?
        .rows_affected())
}

fn header<'a>(req: &'a SiteRequest, name: &str) -> Option<&'a str> {
    req.headers.get(name).and_then(Value::as_str)
}

/// Why a request was turned away, so that a site with a wrong clock can correct it.
#[derive(Debug)]
pub enum Refused {
    /// The request's time is too far from the server's: try again by this clock.
    Clock(DateTime<Utc>),
    Key(&'static str),
    Other(Error),
}

impl From<Error> for Refused {
    fn from(e: Error) -> Self {
        Refused::Other(e)
    }
}

/// Checks a signed request and returns the device it speaks for.
pub async fn verify(k: &Kernel, req: &SiteRequest) -> std::result::Result<Uuid, Refused> {
    let (Some(key_id), Some(time), Some(signature), Some(uri)) =
        (header(req, HEADER_KEY), header(req, HEADER_TIME), header(req, HEADER_SIGNATURE), header(req, "x-ferrum-uri"))
    else {
        return Err(Refused::Key("this request is not signed"));
    };
    let key_id: Uuid = key_id.parse().map_err(|_| Refused::Key("no such key"))?;
    let time_ms: i64 = time.parse().map_err(|_| Refused::Key("the time is not a number"))?;
    let now = Utc::now();
    if (now.timestamp_millis() - time_ms).abs() > MAX_CLOCK_SKEW_MS {
        return Err(Refused::Clock(now));
    }
    let signature: [u8; 64] = hex::decode(signature).ok().and_then(|b| b.try_into().ok()).ok_or(Refused::Key("the signature is malformed"))?;
    let mut tx = k.db.tenant_tx(req.tenant).await.map_err(Refused::Other)?;
    let row: Option<(Uuid, Vec<u8>)> = sqlx::query_as(
        "select device_id, public_key from op_device_key where tenant_id = $1 and id = $2 and revoked_at is null and (not_after is null or not_after > now())",
    )
    .bind(req.tenant)
    .bind(key_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| Refused::Other(e.into()))?;
    let (device, public_key) = row.ok_or(Refused::Key("no such key"))?;
    let public_key: [u8; 32] = public_key.try_into().map_err(|_| Refused::Other(Error::internal("a stored key of the wrong length")))?;
    let key = VerifyingKey::from_bytes(&public_key).map_err(|_| Refused::Other(Error::internal("a stored key that is not a key")))?;
    let signed = message(&req.method, uri, time_ms, &req.body);
    key.verify(&signed, &Signature::from_bytes(&signature)).map_err(|_| Refused::Key("the signature does not match"))?;
    // After the signature, so that nobody without the key can push the clock forward.
    let fresh = sqlx::query("update op_device_key set last_time_ms = $3 where tenant_id = $1 and id = $2 and last_time_ms < $3")
        .bind(req.tenant)
        .bind(key_id)
        .bind(time_ms)
        .execute(&mut *tx)
        .await
        .map_err(|e| Refused::Other(e.into()))?;
    if fresh.rows_affected() == 0 {
        return Err(Refused::Key("this request was already seen"));
    }
    tx.commit().await.map_err(|e| Refused::Other(e.into()))?;
    Ok(device)
}

/// The PIN of a person who may work at a site's console while the link is down.
pub fn pin_hash(pin: &str) -> String {
    let mut salt = [0u8; 8];
    rand::rng().fill_bytes(&mut salt);
    let salt = hex::encode(salt);
    format!("{salt}${}", hex::encode(Sha256::digest(format!("{salt}{pin}").as_bytes())))
}
