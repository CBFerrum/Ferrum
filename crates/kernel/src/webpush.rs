//! Web Push (docs/plan/apps-ui.md §12.5): notifications reach people's devices through their
//! browsers' push services (RFC 8030), encrypted for each device (RFC 8291, `aes128gcm`) and
//! signed with the server's VAPID key (RFC 8292). The key is made once, sealed with the server's
//! secret key and shared by every replica. A device's subscription belongs to the browser session
//! it was made in: signing that device out, or out everywhere, stops its pushes. The core worker
//! (`/sw.js`) shows them and opens what they are about.

use crate::error::{Error, Result};
use crate::secret::SecretKey;
use crate::{Ctx, Kernel};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use ring::aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use ring::{agreement, hkdf};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Outbox topic of one notification for one device: `{"notification", "session"}`.
pub const PUSH_TOPIC: &str = "ferrum.notification.push";

/// The push services browsers use (Chrome, Edge, Firefox, Safari), by host; their subdomains too.
pub const PUSH_SERVICES: &[&str] = &["fcm.googleapis.com", "android.googleapis.com", "push.services.mozilla.com", "push.apple.com", "notify.windows.com"];

/// The record size messages declare; a message is one record.
const RECORD_SIZE: u32 = 4096;
/// The longest text a message carries: one record less its 86-byte header, the padding
/// delimiter and the 16-byte tag.
const MAX_PLAINTEXT: usize = RECORD_SIZE as usize - 86 - 1 - 16;
/// How long a push service keeps a message for a device that is offline (the bar has it anyway).
const TTL_SECONDS: &str = "86400";
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// The aad sealing the VAPID key.
const VAPID_LABEL: &str = "server_key|vapid";

fn internal(what: &str) -> Error {
    Error::Internal(format!("web push: {what}"))
}

struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand(prk: &hkdf::Prk, info: &[&[u8]], out: &mut [u8]) -> Result<()> {
    prk.expand(info, Len(out.len())).and_then(|okm| okm.fill(out)).map_err(|_| internal("key derivation failed"))
}

/// RFC 8291 §3.4 once the ECDH secret is agreed: `plaintext` encrypted for the device whose key is
/// `ua_public` and authentication secret `auth`, by the one-time key `as_public`, with `salt`.
fn seal_message(ecdh_secret: &[u8], auth: &[u8], ua_public: &[u8], as_public: &[u8], salt: &[u8; 16], plaintext: &[u8]) -> Result<Vec<u8>> {
    let prk_key = hkdf::Salt::new(hkdf::HKDF_SHA256, auth).extract(ecdh_secret);
    let mut ikm = [0u8; 32];
    expand(&prk_key, &[b"WebPush: info\0", ua_public, as_public], &mut ikm)?;
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
    let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
    expand(&prk, &[b"Content-Encoding: aes128gcm\0"], &mut cek)?;
    expand(&prk, &[b"Content-Encoding: nonce\0"], &mut nonce)?;
    let mut data = Vec::with_capacity(plaintext.len() + 17);
    data.extend_from_slice(plaintext);
    // The padding delimiter of the last (here, only) record.
    data.push(2);
    let key = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, &cek).map_err(|_| internal("bad content key"))?);
    key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut data).map_err(|_| internal("encryption failed"))?;
    let mut out = Vec::with_capacity(21 + as_public.len() + data.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(as_public.len() as u8);
    out.extend_from_slice(as_public);
    out.extend_from_slice(&data);
    Ok(out)
}

/// `plaintext` encrypted for the device whose subscription has the key `ua_public` (65 bytes, an
/// uncompressed P-256 point) and the authentication secret `auth` (16 bytes): a message body for
/// its push service (`Content-Encoding: aes128gcm`).
pub fn encrypt(ua_public: &[u8], auth: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    if plaintext.len() > MAX_PLAINTEXT {
        return Err(Error::invalid(format!("a push message carries at most {MAX_PLAINTEXT} bytes")));
    }
    if ua_public.len() != 65 || ua_public[0] != 4 || auth.len() != 16 {
        return Err(Error::invalid("these are not a push subscription's keys"));
    }
    let rng = SystemRandom::new();
    let ours = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).map_err(|_| internal("no randomness"))?;
    let as_public = ours.compute_public_key().map_err(|_| internal("no public key"))?.as_ref().to_vec();
    let theirs = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, ua_public);
    let secret = agreement::agree_ephemeral(ours, &theirs, <[u8]>::to_vec).map_err(|_| Error::invalid("the device's key is not a P-256 point"))?;
    let mut salt = [0u8; 16];
    rng.fill(&mut salt).map_err(|_| internal("no randomness"))?;
    seal_message(&secret, auth, ua_public, &as_public, &salt, plaintext)
}

/// The server's VAPID key (RFC 8292): it signs a short-lived token for each push service, which
/// checks it against the key the device subscribed with.
pub struct Vapid {
    pair: EcdsaKeyPair,
    /// The public half as browsers take it (`applicationServerKey`): base64url of the point.
    pub public: String,
}

impl Vapid {
    fn from_pkcs8(der: &[u8]) -> Result<Self> {
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, der, &SystemRandom::new()).map_err(|_| internal("corrupt VAPID key"))?;
        let public = B64URL.encode(pair.public_key().as_ref());
        Ok(Vapid { pair, public })
    }

    fn generate() -> Result<(Vec<u8>, Self)> {
        let der = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new()).map_err(|_| internal("no randomness"))?;
        let vapid = Self::from_pkcs8(der.as_ref())?;
        Ok((der.as_ref().to_vec(), vapid))
    }

    /// The `Authorization` header for the push service at `audience` (its origin): a token valid
    /// for 12 hours from `now` (Unix seconds), naming `contact` (`mailto:` or `https:`) when given.
    pub fn authorization(&self, audience: &str, contact: Option<&str>, now: i64) -> Result<String> {
        let header = B64URL.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let mut claims = json!({"aud": audience, "exp": now + 12 * 3600});
        if let Some(c) = contact {
            claims["sub"] = json!(c);
        }
        let signed = format!("{header}.{}", B64URL.encode(claims.to_string()));
        let sig = self.pair.sign(&SystemRandom::new(), signed.as_bytes()).map_err(|_| internal("signing failed"))?;
        Ok(format!("vapid t={signed}.{}, k={}", B64URL.encode(sig.as_ref()), self.public))
    }
}

/// Whether the server sends to `endpoint`: https on the default port of a push service in
/// `services` (a host, its subdomains too), or an origin listed exactly (`http://127.0.0.1:9000`,
/// for tests and private push services).
pub fn allowed_endpoint(endpoint: &str, services: &[String]) -> bool {
    let Ok(url) = reqwest::Url::parse(endpoint) else { return false };
    let Some(host) = url.host_str().map(str::to_ascii_lowercase) else { return false };
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return false;
    }
    let origin = url.origin().ascii_serialization();
    services.iter().map(|s| s.trim().trim_end_matches('/').to_ascii_lowercase()).filter(|s| !s.is_empty()).any(|s| {
        if s.contains("://") { origin == s } else { url.scheme() == "https" && url.port().is_none() && (host == s || host.ends_with(&format!(".{s}"))) }
    })
}

/// A subscription's key as browsers give it (base64url, padded or not) → its bytes, when it has
/// `len` of them.
fn key_bytes(s: &str, len: usize) -> Option<Vec<u8>> {
    B64URL.decode(s.trim().trim_end_matches('=')).ok().filter(|b| b.len() == len)
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max - 1).collect::<String>()) }
}

/// Whether this device has pushes on, and on how many of the person's devices they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct PushStatus {
    pub here: bool,
    pub devices: i64,
}

impl Kernel {
    /// The server's VAPID key, made the first time a replica needs it; `None` without the
    /// server's secret key (nothing to seal it with): then there is no Web Push.
    pub async fn vapid(&self) -> Result<Option<Arc<Vapid>>> {
        let Some(key) = self.settings.secret_key.as_ref() else { return Ok(None) };
        let v = self.vapid.get_or_try_init(|| async { self.load_vapid(key).await.map(Arc::new) }).await?;
        Ok(Some(v.clone()))
    }

    /// Reads the stored key, storing a new one when there is none. One that this secret key
    /// cannot open (the secret key was replaced) is replaced too: devices subscribed with it
    /// subscribe again on their next visit.
    async fn load_vapid(&self, key: &SecretKey) -> Result<Vapid> {
        let (der, fresh) = Vapid::generate()?;
        let sealed = key.seal(VAPID_LABEL, &B64URL.encode(&der))?;
        let mut tx = self.db.system_tx().await?;
        sqlx::query("insert into server_key (name, public, sealed) values ('vapid', $1, $2) on conflict (name) do nothing")
            .bind(&fresh.public)
            .bind(&sealed)
            .execute(&mut *tx)
            .await?;
        let stored: String = sqlx::query_scalar("select sealed from server_key where name = 'vapid' for update").fetch_one(&mut *tx).await?;
        let opened = key.open(VAPID_LABEL, &stored).ok().and_then(|b| B64URL.decode(b).ok()).and_then(|der| Vapid::from_pkcs8(&der).ok());
        let vapid = match opened {
            Some(v) => v,
            None => {
                tracing::error!(
                    "the stored VAPID key cannot be opened with this secret key (was FERRUM_SECRET_KEY replaced?); made a new one, devices subscribe again"
                );
                sqlx::query("update server_key set public = $1, sealed = $2, created_at = now() where name = 'vapid'")
                    .bind(&fresh.public)
                    .bind(&sealed)
                    .execute(&mut *tx)
                    .await?;
                fresh
            }
        };
        tx.commit().await?;
        Ok(vapid)
    }

    /// Turns pushes on for the device of browser session `session` (the caller's): its
    /// subscription with the browser's push service (`endpoint`; the keys `p256dh` and `auth`,
    /// base64url) replaces the one the session had, and any other session's with that endpoint.
    /// Someone acting as the person (support access) gets no pushes of theirs.
    pub async fn subscribe_push(&self, ctx: &Ctx, session: Uuid, endpoint: &str, p256dh: &str, auth: &str) -> Result<()> {
        let Some(vapid) = self.vapid().await? else {
            return Err(Error::Unavailable("notifications on devices need the server's secret key (FERRUM_SECRET_KEY)".into()));
        };
        if endpoint.len() > 2048 || !allowed_endpoint(endpoint, &self.settings.push_services) {
            return Err(Error::field("endpoint", "is not a push service this server sends to"));
        }
        let key = key_bytes(p256dh, 65).filter(|k| k[0] == 4).ok_or_else(|| Error::field("keys.p256dh", "is not a P-256 public key"))?;
        let secret = key_bytes(auth, 16).ok_or_else(|| Error::field("keys.auth", "is not 16 bytes"))?;
        // A point off the curve fails the key agreement: try it now rather than on every send.
        encrypt(&key, &secret, b"").map_err(|_| Error::field("keys.p256dh", "is not a P-256 public key"))?;
        let mut tx = self.db.system_tx().await?;
        let found: Option<(i32, Option<Uuid>)> = sqlx::query_as(
            "select u.token_version, s.actor from user_session s join app_user u on u.tenant_id = s.tenant_id and u.id = s.user_id \
             where s.tenant_id = $1 and s.id = $2 and s.user_id = $3 and s.revoked_at is null and s.expires_at > now() and u.is_active",
        )
        .bind(ctx.tenant)
        .bind(session)
        .bind(ctx.user)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((version, actor)) = found else { return Err(Error::Unauthorized("this device was signed out; please sign in again".into())) };
        if actor.is_some() {
            return Err(Error::Forbidden("someone acting as another person gets none of their notifications".into()));
        }
        sqlx::query("delete from push_subscription where endpoint = $1 and not (tenant_id = $2 and session_id = $3)")
            .bind(endpoint)
            .bind(ctx.tenant)
            .bind(session)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "insert into push_subscription (tenant_id, session_id, user_id, endpoint, p256dh, auth, vapid, token_version) values ($1, $2, $3, $4, $5, $6, $7, $8) \
             on conflict (tenant_id, session_id) do update set endpoint = excluded.endpoint, p256dh = excluded.p256dh, auth = excluded.auth, \
             vapid = excluded.vapid, token_version = excluded.token_version, created_at = now()",
        )
        .bind(ctx.tenant)
        .bind(session)
        .bind(ctx.user)
        .bind(endpoint)
        .bind(B64URL.encode(&key))
        .bind(B64URL.encode(&secret))
        .bind(&vapid.public)
        .bind(version)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Turns pushes off for the device of browser session `session`.
    pub async fn unsubscribe_push(&self, ctx: &Ctx, session: Uuid) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("delete from push_subscription where tenant_id = $1 and session_id = $2 and user_id = $3")
            .bind(ctx.tenant)
            .bind(session)
            .bind(ctx.user)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Whether the device of `session` has pushes on, and on how many of the person's devices
    /// (those still signed in).
    pub async fn push_status(&self, ctx: &Ctx, session: Option<Uuid>) -> Result<PushStatus> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let (here, devices): (bool, i64) = sqlx::query_as(
            "select coalesce(bool_or(p.session_id = $3), false), count(*) from push_subscription p \
             join app_user u on u.tenant_id = p.tenant_id and u.id = p.user_id and u.token_version = p.token_version \
             join user_session s on s.tenant_id = p.tenant_id and s.id = p.session_id and s.revoked_at is null and s.expires_at > now() \
             where p.tenant_id = $1 and p.user_id = $2",
        )
        .bind(ctx.tenant)
        .bind(ctx.user)
        .bind(session)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(PushStatus { here, devices })
    }

    /// Sends a notification to one device ([`PUSH_TOPIC`]'s handler). One read meanwhile, for a
    /// device signed out, for someone inactive, or for a subscription made with another key is
    /// not sent; a subscription the push service no longer knows is forgotten; a message it
    /// cannot take now is tried again later (the outbox's backoff).
    pub(crate) async fn push_notification(&self, tenant: Uuid, payload: &Value) -> Result<()> {
        let id_of = |k: &str| payload.get(k).and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok());
        let (Some(id), Some(session)) = (id_of("notification"), id_of("session")) else { return Ok(()) };
        let Some(vapid) = self.vapid().await? else { return Ok(()) };
        let mut tx = self.db.tenant_tx(tenant).await?;
        type Row = (Uuid, String, Option<String>, Option<String>, Option<Uuid>, String, String, String, String);
        let row: Option<Row> = sqlx::query_as(
            "select n.user_id, n.title, n.body, n.entity, n.record_id, p.endpoint, p.p256dh, p.auth, p.vapid from notification n \
             join push_subscription p on p.tenant_id = n.tenant_id and p.user_id = n.user_id and p.session_id = $3 \
             join app_user u on u.tenant_id = n.tenant_id and u.id = n.user_id and u.is_active and u.token_version = p.token_version \
             join user_session s on s.tenant_id = p.tenant_id and s.id = p.session_id and s.revoked_at is null and s.expires_at > now() \
             where n.tenant_id = $1 and n.id = $2 and n.read_at is null",
        )
        .bind(tenant)
        .bind(id)
        .bind(session)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((user, title, body, entity, record, endpoint, p256dh, auth, made_with)) = row else {
            tx.commit().await?;
            return Ok(());
        };
        if made_with != vapid.public {
            tx.commit().await?;
            return self.forget_push(tenant, session, &endpoint).await;
        }
        let unread: i64 = sqlx::query_scalar("select count(*) from notification where tenant_id = $1 and user_id = $2 and read_at is null")
            .bind(tenant)
            .bind(user)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        let url = match (entity, record) {
            (Some(e), Some(r)) => format!("/open/{e}/{r}"),
            _ => "/".to_string(),
        };
        let message = json!({"id": id, "title": cut(&title, 200), "body": body.map(|b| cut(&b, 400)), "url": url, "unread": unread});
        let (Some(key), Some(secret)) = (key_bytes(&p256dh, 65), key_bytes(&auth, 16)) else {
            return self.forget_push(tenant, session, &endpoint).await;
        };
        let sealed = encrypt(&key, &secret, message.to_string().as_bytes())?;
        let (client, url) = match self.vetted_client(&endpoint, self.settings.webhooks_allow_private, SEND_TIMEOUT, "Ferrum").await {
            Ok(c) => c,
            // Not a public address (any more): nothing will reach it.
            Err(Error::Invalid(why)) => {
                tracing::warn!(%tenant, "push subscription dropped: {why}");
                return self.forget_push(tenant, session, &endpoint).await;
            }
            Err(e) => return Err(e),
        };
        let audience = url.origin().ascii_serialization();
        let contact = self.settings.push_contact.as_deref().or(self.settings.public_url.as_deref().filter(|u| u.starts_with("https://")));
        // ponytail: sent in the outbox's batch, one after another; a delivery queue of its own (as
        // webhooks have) if bursts of pushes hold up other events.
        let res = client
            .post(url)
            .header("ttl", TTL_SECONDS)
            .header("urgency", "normal")
            .header("content-encoding", "aes128gcm")
            .header("content-type", "application/octet-stream")
            .header("authorization", vapid.authorization(&audience, contact, chrono::Utc::now().timestamp())?)
            .body(sealed)
            .send()
            .await
            .map_err(|e| Error::Unavailable(format!("push service unreachable: {e}")))?;
        let status = res.status().as_u16();
        match status {
            200..=299 => Ok(()),
            404 | 410 => self.forget_push(tenant, session, &endpoint).await,
            429 | 500..=599 => Err(Error::Unavailable(format!("push service answered {status}"))),
            _ => {
                let why = res.text().await.unwrap_or_default();
                tracing::warn!(%tenant, status, "push service refused a message: {}", cut(why.trim(), 300));
                Ok(())
            }
        }
    }

    /// Forgets a subscription that can no longer be pushed to (unless the session has made
    /// another meanwhile).
    async fn forget_push(&self, tenant: Uuid, session: Uuid, endpoint: &str) -> Result<()> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        sqlx::query("delete from push_subscription where tenant_id = $1 and session_id = $2 and endpoint = $3")
            .bind(tenant)
            .bind(session)
            .bind(endpoint)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> Vec<u8> {
        B64URL.decode(s.split_whitespace().collect::<String>()).unwrap()
    }

    /// RFC 8291 Appendix A: from the agreed secret on, every value as the RFC gives it.
    #[test]
    fn rfc_8291_example_encrypts_to_its_message() {
        let plaintext = b64("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24");
        assert_eq!(plaintext, b"When I grow up, I want to be a watermelon");
        let as_public = b64("BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIg Dll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8");
        let ua_public = b64("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcx aOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4");
        let auth = b64("BTBZMqHH6r4Tts7J_aSIgg");
        let salt: [u8; 16] = b64("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let ecdh_secret = b64("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs");
        let message = seal_message(&ecdh_secret, &auth, &ua_public, &as_public, &salt, &plaintext).unwrap();
        let expected = b64("DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml
             mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT
             pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN");
        // 86 bytes of header, 41 of text, the delimiter and the tag (the RFC's Content-Length says 145).
        assert_eq!(message.len(), 144);
        assert_eq!(B64URL.encode(&message), B64URL.encode(&expected));
    }

    /// What a browser does with a message: agree on the secret from its own key, derive the same
    /// keys, open the record and drop the padding.
    fn open_as_browser(ua_private: agreement::EphemeralPrivateKey, ua_public: &[u8], auth: &[u8], message: &[u8]) -> Vec<u8> {
        let (salt, rest) = message.split_at(16);
        assert_eq!(u32::from_be_bytes(rest[..4].try_into().unwrap()), RECORD_SIZE);
        let idlen = rest[4] as usize;
        let (as_public, body) = rest[5..].split_at(idlen);
        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public);
        let secret = agreement::agree_ephemeral(ua_private, &peer, <[u8]>::to_vec).unwrap();
        let prk_key = hkdf::Salt::new(hkdf::HKDF_SHA256, auth).extract(&secret);
        let mut ikm = [0u8; 32];
        expand(&prk_key, &[b"WebPush: info\0", ua_public, as_public], &mut ikm).unwrap();
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
        let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
        expand(&prk, &[b"Content-Encoding: aes128gcm\0"], &mut cek).unwrap();
        expand(&prk, &[b"Content-Encoding: nonce\0"], &mut nonce).unwrap();
        let key = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, &cek).unwrap());
        let mut data = body.to_vec();
        let plain = key.open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut data).unwrap();
        assert_eq!(plain.last(), Some(&2), "the last record's delimiter");
        plain[..plain.len() - 1].to_vec()
    }

    #[test]
    fn a_device_opens_what_was_encrypted_for_it() {
        let rng = SystemRandom::new();
        let ua_private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let ua_public = ua_private.compute_public_key().unwrap().as_ref().to_vec();
        let mut auth = [0u8; 16];
        rng.fill(&mut auth).unwrap();
        let text = "{\"title\":\"Ana mentioned you\"}".as_bytes();
        let message = encrypt(&ua_public, &auth, text).unwrap();
        assert_eq!(open_as_browser(ua_private, &ua_public, &auth, &message), text);
        // Too long for one record, and keys that are not a subscription's.
        assert!(encrypt(&ua_public, &auth, &vec![b'x'; MAX_PLAINTEXT + 1]).is_err());
        assert!(encrypt(&ua_public, &auth, &vec![b'x'; MAX_PLAINTEXT]).is_ok());
        assert!(encrypt(&ua_public[..64], &auth, text).is_err());
        assert!(encrypt(&ua_public, &auth[..15], text).is_err());
        let mut off_curve = ua_public.clone();
        off_curve[64] ^= 1;
        assert!(encrypt(&off_curve, &auth, text).is_err());
    }

    #[test]
    fn vapid_tokens_verify_with_the_public_key() {
        let (der, vapid) = Vapid::generate().unwrap();
        assert_eq!(Vapid::from_pkcs8(&der).unwrap().public, vapid.public);
        let header = vapid.authorization("https://fcm.googleapis.com", Some("mailto:ops@example.com"), 1_800_000_000).unwrap();
        let rest = header.strip_prefix("vapid t=").unwrap();
        let (token, k) = rest.split_once(", k=").unwrap();
        assert_eq!(k, vapid.public);
        let (signed, sig) = token.rsplit_once('.').unwrap();
        let public = B64URL.decode(k).unwrap();
        assert_eq!(public.len(), 65);
        ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, &public)
            .verify(signed.as_bytes(), &B64URL.decode(sig).unwrap())
            .expect("the token's signature");
        let (h, c) = signed.split_once('.').unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&B64URL.decode(h).unwrap()).unwrap(), json!({"typ": "JWT", "alg": "ES256"}));
        let claims: Value = serde_json::from_slice(&B64URL.decode(c).unwrap()).unwrap();
        assert_eq!(claims, json!({"aud": "https://fcm.googleapis.com", "exp": 1_800_000_000i64 + 43_200, "sub": "mailto:ops@example.com"}));
        let bare = vapid.authorization("https://web.push.apple.com", None, 0).unwrap();
        let c = bare.strip_prefix("vapid t=").unwrap().split('.').nth(1).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&B64URL.decode(c).unwrap()).unwrap(), json!({"aud": "https://web.push.apple.com", "exp": 43_200}));
    }

    #[test]
    fn only_push_services_are_sent_to() {
        let services: Vec<String> = PUSH_SERVICES.iter().map(|s| s.to_string()).chain(["http://127.0.0.1:9123".to_string()]).collect();
        for ok in [
            "https://fcm.googleapis.com/fcm/send/abc:APA91b",
            "https://updates.push.services.mozilla.com/wpush/v2/gAAAA",
            "https://web.push.apple.com/QGuQyavXutnMH",
            "https://wns2-par02p.notify.windows.com/w/?token=BQYAAA",
            "https://FCM.googleapis.com/wp/x",
            "https://fcm.googleapis.com:443/wp/x",
            "http://127.0.0.1:9123/push/1",
        ] {
            assert!(allowed_endpoint(ok, &services), "{ok}");
        }
        for no in [
            "http://fcm.googleapis.com/fcm/send/x",
            "https://fcm.googleapis.com:8443/x",
            "https://evilfcm.googleapis.com.example.com/x",
            "https://googleapis.com/x",
            "https://notfcm.googleapis.com.evil/x",
            "https://169.254.169.254/latest/meta-data",
            "https://user:pw@fcm.googleapis.com/x",
            "https://fcm.googleapis.com/x#frag",
            "http://127.0.0.1:9124/push/1",
            "https://127.0.0.1:9123/push/1",
            "not a url",
        ] {
            assert!(!allowed_endpoint(no, &services), "{no}");
        }
        assert!(!allowed_endpoint("https://fcm.googleapis.com/x", &[]));
    }

    #[test]
    fn subscription_keys_are_read_padded_or_not() {
        let key = [4u8; 65];
        let unpadded = B64URL.encode(key);
        assert_eq!(unpadded.len(), 87);
        assert_eq!(key_bytes(&unpadded, 65).unwrap(), key);
        assert_eq!(key_bytes(&format!("{unpadded}="), 65).unwrap(), key);
        assert!(key_bytes(&unpadded, 16).is_none());
        assert!(key_bytes("not base64!", 16).is_none());
        assert_eq!(cut("abcdef", 4), "abc…");
        assert_eq!(cut("abc", 4), "abc");
    }
}
