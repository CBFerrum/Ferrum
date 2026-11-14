//! Passkeys (WebAuthn; docs/plan/apps-ui.md §12.3, U6): signing in with a credential the person's
//! device keeps instead of a password. The relying party is the host the person signs in on, or
//! the domain `FERRUM_PASSKEY_RP` names for the hosts under it; a passkey is always checked against
//! the relying party it was made for, so one keeps working where it was made. Registration asks for no attestation ("none"): Ferrum
//! trusts the signed-in person, not a make of authenticator. Signing in finds the account by the
//! credential (discoverable credentials): nobody types an email. Each challenge is used once.
//!
//! Checked here, with `ring` and a CBOR reader rather than a WebAuthn library: the client data
//! (ceremony, challenge, origin, not inside another site's frame), the authenticator data (the
//! relying party's hash, the person present and verified, the signature counter), and the
//! signature, for ES256, RS256 (2048 bits or more) and Ed25519 keys.

use crate::Kernel;
use crate::auth::Session;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ciborium::value::Value as Cbor;
use ring::signature::{self, UnparsedPublicKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const ES256: i64 = -7;
pub const EDDSA: i64 = -8;
pub const RS256: i64 = -257;
/// How long a ceremony may take, from its options to its answer.
const CHALLENGE_SECS: f64 = 300.0;
/// The authenticator data's flags: the person was present, verified, a credential follows.
const UP: u8 = 0x01;
const UV: u8 = 0x04;
const AT: u8 = 0x40;
/// Most passkeys a person keeps.
const MAX_PER_PERSON: i64 = 20;

fn refused(why: &str) -> Error {
    Error::Unauthorized(format!("the passkey was not accepted: {why}"))
}

/// What the authenticator says, as WebAuthn lays it out.
#[derive(Debug, PartialEq)]
pub struct AuthData {
    pub rp_id_hash: [u8; 32],
    pub flags: u8,
    pub sign_count: u32,
    /// A new credential's id and public key (COSE), when registering.
    pub credential: Option<(Vec<u8>, Vec<u8>)>,
}

pub fn parse_auth_data(b: &[u8]) -> Result<AuthData> {
    if b.len() < 37 {
        return Err(refused("unreadable authenticator data"));
    }
    let rp_id_hash: [u8; 32] = b[..32].try_into().map_err(|_| refused("unreadable authenticator data"))?;
    let (flags, sign_count) = (b[32], u32::from_be_bytes([b[33], b[34], b[35], b[36]]));
    let credential = if flags & AT != 0 {
        // 16 bytes of authenticator model, the id's length, the id, then the key (one CBOR item).
        let len = usize::from(u16::from_be_bytes([
            *b.get(53).ok_or_else(|| refused("short credential data"))?,
            *b.get(54).ok_or_else(|| refused("short credential data"))?,
        ]));
        let key_at = 55 + len;
        let id = b.get(55..key_at).ok_or_else(|| refused("short credential data"))?.to_vec();
        let rest = b.get(key_at..).ok_or_else(|| refused("short credential data"))?;
        let mut cursor = std::io::Cursor::new(rest);
        let _: Cbor = ciborium::de::from_reader(&mut cursor).map_err(|_| refused("unreadable public key"))?;
        let used = usize::try_from(cursor.position()).unwrap_or(rest.len());
        Some((id, rest[..used].to_vec()))
    } else {
        None
    };
    Ok(AuthData { rp_id_hash, flags, sign_count, credential })
}

fn int_of(v: &Cbor) -> Option<i64> {
    v.as_integer().and_then(|i| i64::try_from(i128::from(i)).ok())
}

fn get(map: &[(Cbor, Cbor)], key: i64) -> Option<&Cbor> {
    map.iter().find(|(k, _)| int_of(k) == Some(key)).map(|(_, v)| v)
}

/// A credential's public key from its COSE form: (algorithm, the key as stored).
pub fn cose_key(bytes: &[u8]) -> Result<(i64, Vec<u8>)> {
    let v: Cbor = ciborium::de::from_reader(bytes).map_err(|_| refused("unreadable public key"))?;
    let map = v.as_map().ok_or_else(|| refused("unreadable public key"))?;
    let num = |k| get(map, k).and_then(int_of);
    let bytes_of = |k| get(map, k).and_then(Cbor::as_bytes).cloned().ok_or_else(|| refused("incomplete public key"));
    match (num(1), num(3)) {
        // EC2 on P-256.
        (Some(2), Some(ES256)) if num(-1) == Some(1) => {
            let (x, y) = (bytes_of(-2)?, bytes_of(-3)?);
            if x.len() != 32 || y.len() != 32 {
                return Err(refused("not a P-256 key"));
            }
            Ok((ES256, [&[4u8][..], &x, &y].concat()))
        }
        (Some(3), Some(RS256)) => {
            let (n, e) = (bytes_of(-1)?, bytes_of(-2)?);
            let n = n.iter().skip_while(|b| **b == 0).copied().collect::<Vec<u8>>();
            if n.len() < 256 || n.len() > 1024 || e.is_empty() || e.len() > 8 {
                return Err(refused("RSA keys of 2048 to 8192 bits"));
            }
            let len = u16::try_from(n.len()).map_err(|_| refused("RSA key too large"))?;
            Ok((RS256, [&len.to_be_bytes()[..], &n, &e].concat()))
        }
        // OKP on Ed25519.
        (Some(1), Some(EDDSA)) if num(-1) == Some(6) => {
            let x = bytes_of(-2)?;
            if x.len() != 32 {
                return Err(refused("not an Ed25519 key"));
            }
            Ok((EDDSA, x))
        }
        _ => Err(refused("a kind of key this server does not take (ES256, RS256 or Ed25519)")),
    }
}

/// Whether `signature` is the key's over `message`.
pub fn verify(alg: i64, key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    match alg {
        ES256 => UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_ASN1, key).verify(message, signature).is_ok(),
        EDDSA => UnparsedPublicKey::new(&signature::ED25519, key).verify(message, signature).is_ok(),
        RS256 if key.len() > 2 => {
            let len = usize::from(u16::from_be_bytes([key[0], key[1]]));
            let (Some(n), Some(e)) = (key.get(2..2 + len), key.get(2 + len..)) else { return false };
            signature::RsaPublicKeyComponents { n, e }.verify(&signature::RSA_PKCS1_2048_8192_SHA256, message, signature).is_ok()
        }
        _ => false,
    }
}

/// The client data: what the browser says was signed (the ceremony, the challenge, the page's
/// origin, and that it was not inside another site's frame).
pub fn check_client_data(json: &[u8], ceremony: &str, challenge: &[u8], origins: &[String]) -> Result<()> {
    let v: Value = serde_json::from_slice(json).map_err(|_| refused("unreadable client data"))?;
    if v.get("type").and_then(Value::as_str) != Some(ceremony) {
        return Err(refused("not this kind of request"));
    }
    if v.get("challenge").and_then(Value::as_str).and_then(|c| B64.decode(c).ok()).as_deref() != Some(challenge) {
        return Err(refused("not this request's challenge"));
    }
    if !v.get("origin").and_then(Value::as_str).is_some_and(|o| origins.iter().any(|x| x == o)) {
        return Err(refused("made for another site"));
    }
    if v.get("crossOrigin").and_then(Value::as_bool) == Some(true) {
        return Err(refused("made inside another site's frame"));
    }
    Ok(())
}

/// Whether a page on `host` may use relying party `rp`: the same name, or a name under it, label by
/// label (`erp.acme.com` under `acme.com`; never `evilacme.com`).
pub fn host_may_use_rp(host: &str, rp: &str) -> bool {
    let (host, rp) = (host.trim_end_matches('.').to_ascii_lowercase(), rp.trim_end_matches('.').to_ascii_lowercase());
    !rp.is_empty() && (host == rp || host.strip_suffix(rp.as_str()).is_some_and(|under| under.len() > 1 && under.ends_with('.')))
}

/// The relying party of a passkey made on `host`: the configured one (`FERRUM_PASSKEY_RP`) when the
/// host is it or under it, else the host itself.
pub fn rp_for(configured: Option<&str>, host: &str) -> String {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    match configured {
        Some(rp) if host_may_use_rp(&host, rp) => rp.trim_end_matches('.').to_ascii_lowercase(),
        _ => host,
    }
}

/// Refuses a ceremony answered on a page whose host may not use the relying party it was made for.
fn origins_may_use(origins: &[String], rp_id: &str) -> Result<()> {
    let ok = !origins.is_empty()
        && origins.iter().all(|o| reqwest::Url::parse(o).ok().and_then(|u| u.host_str().map(|h| host_may_use_rp(h, rp_id))).unwrap_or(false));
    if ok { Ok(()) } else { Err(refused("made for another site")) }
}

fn b64(v: &Value, what: &str) -> Result<Vec<u8>> {
    v.as_str().and_then(|s| B64.decode(s.trim_end_matches('=')).ok()).ok_or_else(|| refused(&format!("{what} is missing")))
}

fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

impl Kernel {
    /// The relying party for a passkey ceremony on `host` (a name, no port): [`rp_for`] with this
    /// server's `FERRUM_PASSKEY_RP`.
    pub fn passkey_rp(&self, host: &str) -> String {
        rp_for(self.settings.passkey_rp.as_deref(), host.split(':').next().unwrap_or_default())
    }

    /// What the browser needs to sign in with a passkey on `host`: a request for [`Kernel::passkey_rp`],
    /// and, when that is not the host itself and passkeys were made for the host before the setting
    /// (they keep their relying party), a second request for those under `also`. The sign-in page
    /// asks the device with the first and, when it offers none, with the second.
    pub async fn passkey_sign_in_offer(&self, host: &str) -> Result<Value> {
        let host = host.split(':').next().unwrap_or_default().to_ascii_lowercase();
        let rp = self.passkey_rp(&host);
        let mut offer = self.passkey_sign_in_options(&rp).await?;
        if rp != host {
            let mut tx = self.db.system_tx().await?;
            let older: bool = sqlx::query_scalar("select exists(select 1 from passkey where rp_id = $1)").bind(&host).fetch_one(&mut *tx).await?;
            tx.commit().await?;
            if older {
                offer["also"] = self.passkey_sign_in_options(&host).await?;
            }
        }
        Ok(offer)
    }

    async fn new_challenge(&self, purpose: &str, tenant: Option<Uuid>, user: Option<Uuid>, rp_id: &str) -> Result<(Uuid, [u8; 32])> {
        let (id, challenge) = (Uuid::now_v7(), rand::random::<[u8; 32]>());
        let mut tx = self.db.system_tx().await?;
        sqlx::query("delete from webauthn_challenge where expires_at < now()").execute(&mut *tx).await?;
        sqlx::query(
            "insert into webauthn_challenge (id, challenge, purpose, tenant_id, user_id, rp_id, expires_at) \
             values ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7))",
        )
        .bind(id)
        .bind(&challenge[..])
        .bind(purpose)
        .bind(tenant)
        .bind(user)
        .bind(rp_id)
        .bind(CHALLENGE_SECS)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((id, challenge))
    }

    /// Takes a challenge, which then cannot be used again: (challenge, workspace, person, relying party).
    async fn take_challenge(&self, id: Uuid, purpose: &str) -> Result<(Vec<u8>, Option<Uuid>, Option<Uuid>, String)> {
        let mut tx = self.db.system_tx().await?;
        let row: Option<(Vec<u8>, Option<Uuid>, Option<Uuid>, String)> = sqlx::query_as(
            "delete from webauthn_challenge where id = $1 and purpose = $2 and expires_at > now() returning challenge, tenant_id, user_id, rp_id",
        )
        .bind(id)
        .bind(purpose)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        row.ok_or_else(|| refused("the request expired; try again"))
    }

    /// What the browser needs to make a passkey for the caller on `rp_id` (the host): `{request,
    /// publicKey}`. Their existing passkeys there are excluded, so a device does not make two.
    pub async fn passkey_options(&self, ctx: &Ctx, rp_id: &str, rp_name: &str) -> Result<Value> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let mine: Vec<Vec<u8>> = sqlx::query_scalar("select credential_id from passkey where tenant_id = $1 and user_id = $2 and rp_id = $3")
            .bind(ctx.tenant)
            .bind(ctx.user)
            .bind(rp_id)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        let (id, challenge) = self.new_challenge("register", Some(ctx.tenant), Some(ctx.user), rp_id).await?;
        let shown = if ctx.full_name.trim().is_empty() { ctx.email.clone() } else { ctx.full_name.clone() };
        let algorithms: Vec<Value> = [ES256, EDDSA, RS256].iter().map(|alg| json!({"type": "public-key", "alg": alg})).collect();
        Ok(json!({
            "request": id,
            "publicKey": {
                "challenge": B64.encode(challenge),
                "rp": {"id": rp_id, "name": rp_name},
                "user": {"id": B64.encode(ctx.user.as_bytes()), "name": ctx.email, "displayName": shown},
                "pubKeyCredParams": algorithms,
                "authenticatorSelection": {"residentKey": "required", "requireResidentKey": true, "userVerification": "required"},
                "attestation": "none",
                "timeout": 120_000,
                "excludeCredentials": mine.iter().map(|c| json!({"type": "public-key", "id": B64.encode(c)})).collect::<Vec<_>>(),
            },
        }))
    }

    /// Keeps the passkey the browser made for the caller (`credential`: the browser's answer, its
    /// binary fields base64url), after checking it answers this request, for this site, and that
    /// the authenticator checked the person.
    pub async fn add_passkey(&self, ctx: &Ctx, request: Uuid, origins: &[String], credential: &Value, name: &str) -> Result<Value> {
        let (challenge, tenant, user, rp_id) = self.take_challenge(request, "register").await?;
        if tenant != Some(ctx.tenant) || user != Some(ctx.user) {
            return Err(refused("this request was someone else's"));
        }
        origins_may_use(origins, &rp_id)?;
        let res = &credential["response"];
        check_client_data(&b64(&res["clientDataJSON"], "the client data")?, "webauthn.create", &challenge, origins)?;
        let attestation: Cbor =
            ciborium::de::from_reader(&b64(&res["attestationObject"], "the attestation")?[..]).map_err(|_| refused("unreadable attestation"))?;
        let auth = attestation
            .as_map()
            .and_then(|m| m.iter().find(|(k, _)| k.as_text() == Some("authData")).and_then(|(_, v)| v.as_bytes().cloned()))
            .ok_or_else(|| refused("no authenticator data"))?;
        let data = parse_auth_data(&auth)?;
        if data.rp_id_hash != sha256(rp_id.as_bytes()) {
            return Err(refused("made for another site"));
        }
        if data.flags & UP == 0 || data.flags & UV == 0 {
            return Err(refused("the device did not check who you are"));
        }
        let (id, cose) = data.credential.ok_or_else(|| refused("no credential"))?;
        if !(16..=1023).contains(&id.len()) || b64(&credential["rawId"], "the credential id")? != id {
            return Err(refused("the credential does not match"));
        }
        let (alg, key) = cose_key(&cose)?;
        let transports: Vec<String> = res["transports"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|t| ["usb", "nfc", "ble", "internal", "hybrid", "smart-card"].contains(t))
            .map(String::from)
            .collect();
        let name: String = name.trim().chars().take(100).collect();
        let name = if name.is_empty() { "Passkey".to_string() } else { name };
        let pid = Uuid::now_v7();
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let count: i64 = sqlx::query_scalar("select count(*) from passkey where tenant_id = $1 and user_id = $2")
            .bind(ctx.tenant)
            .bind(ctx.user)
            .fetch_one(&mut *tx)
            .await?;
        if count >= MAX_PER_PERSON {
            return Err(Error::invalid(format!("at most {MAX_PER_PERSON} passkeys; remove one first")));
        }
        let inserted = sqlx::query(
            "insert into passkey (tenant_id, id, user_id, credential_id, rp_id, public_key, alg, sign_count, name, transports) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) on conflict (credential_id) do nothing",
        )
        .bind(ctx.tenant)
        .bind(pid)
        .bind(ctx.user)
        .bind(&id)
        .bind(&rp_id)
        .bind(&key)
        .bind(alg as i32)
        .bind(i64::from(data.sign_count))
        .bind(&name)
        .bind(&transports)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            return Err(Error::Conflict("this passkey is already kept".into()));
        }
        crate::audit::record_admin(&mut tx, ctx, "Passkey", pid, "add", None, json!({"name": name, "rp_id": rp_id})).await?;
        tx.commit().await?;
        Ok(json!({"id": pid, "name": name, "rp_id": rp_id, "created_at": chrono::Utc::now(), "last_used_at": null}))
    }

    /// What the browser needs to sign in with a passkey on `rp_id`: any of the person's there.
    pub async fn passkey_sign_in_options(&self, rp_id: &str) -> Result<Value> {
        let (id, challenge) = self.new_challenge("sign_in", None, None, rp_id).await?;
        Ok(json!({
            "request": id,
            "publicKey": {"challenge": B64.encode(challenge), "rpId": rp_id, "userVerification": "required", "timeout": 120_000, "allowCredentials": []},
        }))
    }

    /// Signs in with the browser's answer: the account the passkey belongs to, when the signature
    /// is its key's over this request, for this site, the person verified by the device, and the
    /// device's counter moved on (a copied key would not). A verified passkey is two factors in
    /// one: no authenticator code follows. `tenant`: the workspace this host serves, when it serves one.
    pub async fn passkey_sign_in(&self, request: Uuid, origins: &[String], credential: &Value, tenant: Option<Uuid>) -> Result<Session> {
        self.passkey_sign_in_at(request, origins, credential, tenant, false).await
    }

    /// [`Kernel::passkey_sign_in`] where staff are refused (crate::zones): a staff account's passkey
    /// is then one not known here, before its signature is checked.
    pub async fn passkey_sign_in_at(
        &self,
        request: Uuid,
        origins: &[String],
        credential: &Value,
        tenant: Option<Uuid>,
        from: impl Into<crate::gate::Arrival>,
    ) -> Result<Session> {
        let from = from.into();
        let (challenge, _, _, rp_id) = self.take_challenge(request, "sign_in").await?;
        origins_may_use(origins, &rp_id)?;
        let res = &credential["response"];
        let client = b64(&res["clientDataJSON"], "the client data")?;
        let auth = b64(&res["authenticatorData"], "the authenticator data")?;
        let sig = b64(&res["signature"], "the signature")?;
        let id = b64(&credential["rawId"], "the credential id")?;
        check_client_data(&client, "webauthn.get", &challenge, origins)?;
        let data = parse_auth_data(&auth)?;
        if data.rp_id_hash != sha256(rp_id.as_bytes()) {
            return Err(refused("made for another site"));
        }
        if data.flags & UP == 0 || data.flags & UV == 0 {
            return Err(refused("the device did not check who you are"));
        }
        #[derive(sqlx::FromRow)]
        struct Row {
            tenant_id: Uuid,
            id: Uuid,
            user_id: Uuid,
            rp_id: String,
            public_key: Vec<u8>,
            alg: i32,
            sign_count: i64,
            is_active: bool,
            token_version: i32,
            tenant_active: bool,
            sso_only: bool,
            break_glass: bool,
            kind: String,
        }
        let mut tx = self.db.system_tx().await?;
        let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select p.tenant_id, p.id, p.user_id, p.rp_id, p.public_key, p.alg, p.sign_count, u.is_active, u.token_version, \
             t.status = 'active' as tenant_active, {} as sso_only, {} as break_glass, u.kind \
             from passkey p join app_user u on u.tenant_id = p.tenant_id and u.id = p.user_id join tenant t on t.id = p.tenant_id \
             where p.credential_id = $1 for update of p",
            crate::gate::SSO_ONLY,
            crate::gate::BREAK_GLASS
        )))
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await?;
        let refused_here = match &row {
            Some(p) => self.refuses_here(&from, p.tenant_id, p.user_id, &p.kind).await,
            None => false,
        };
        if refused_here {
            if let Some(p) = &row {
                crate::gate::failure(&mut tx, p.tenant_id, p.user_id, "refused", &from, "passkey").await?;
            }
            tx.commit().await?;
            return Err(refused("it is not known here"));
        }
        let Some(p) = row else { return Err(refused("it is not known here")) };
        if p.rp_id != rp_id || tenant.is_some_and(|t| t != p.tenant_id) {
            return Err(refused("it belongs to another address"));
        }
        self.refuse_if_moving(p.tenant_id).await?;
        if let Some(handle) = res["userHandle"].as_str().filter(|h| !h.is_empty())
            && B64.decode(handle.trim_end_matches('=')).ok().as_deref() != Some(p.user_id.as_bytes().as_slice())
        {
            return Err(refused("it belongs to someone else"));
        }
        if !verify(i64::from(p.alg), &p.public_key, &[&auth[..], &sha256(&client)].concat(), &sig) {
            return Err(refused("the signature does not match"));
        }
        // A device that counts counts up; one that went back may be a copy of the key.
        if (data.sign_count != 0 || p.sign_count != 0) && i64::from(data.sign_count) <= p.sign_count {
            return Err(refused("its counter went back, which a copied key would do"));
        }
        if !p.is_active || !p.tenant_active {
            return Err(Error::Unauthorized("this account cannot sign in".into()));
        }
        if p.sso_only {
            return Err(Error::Forbidden("this workspace signs in through single sign-on; use its button on the sign-in page".into()));
        }
        if p.break_glass {
            crate::gate::break_glass(&mut tx, p.tenant_id, p.user_id, &from, "passkey").await?;
        }
        sqlx::query("update passkey set sign_count = $3, last_used_at = now() where tenant_id = $1 and id = $2")
            .bind(p.tenant_id)
            .bind(p.id)
            .bind(i64::from(data.sign_count))
            .execute(&mut *tx)
            .await?;
        sqlx::query("update app_user set last_login_at = now(), failed_logins = 0, locked_until = null where tenant_id = $1 and id = $2")
            .bind(p.tenant_id)
            .bind(p.user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Session { tenant: p.tenant_id, user: p.user_id, token_version: p.token_version })
    }

    /// The caller's passkeys.
    pub async fn passkeys(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', id, 'name', name, 'rp_id', rp_id, 'transports', transports, 'created_at', created_at, 'last_used_at', last_used_at) \
             from passkey where tenant_id = $1 and user_id = $2 order by created_at",
        )
        .bind(ctx.tenant)
        .bind(ctx.user)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Renames (`Some`) or removes (`None`) one of the caller's passkeys.
    pub async fn change_passkey(&self, ctx: &Ctx, id: Uuid, name: Option<&str>) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let done = match name {
            Some(n) => {
                let n: String = n.trim().chars().take(100).collect();
                if n.is_empty() {
                    return Err(Error::field("name", "is required"));
                }
                sqlx::query("update passkey set name = $4 where tenant_id = $1 and user_id = $2 and id = $3")
                    .bind(ctx.tenant)
                    .bind(ctx.user)
                    .bind(id)
                    .bind(n)
                    .execute(&mut *tx)
                    .await?
            }
            None => {
                sqlx::query("delete from passkey where tenant_id = $1 and user_id = $2 and id = $3")
                    .bind(ctx.tenant)
                    .bind(ctx.user)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?
            }
        };
        if done.rows_affected() == 0 {
            return Err(Error::NotFound("passkey".into()));
        }
        crate::audit::record_admin(&mut tx, ctx, "Passkey", id, if name.is_some() { "rename" } else { "remove" }, None, json!({})).await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair};

    fn cbor(v: &Cbor) -> Vec<u8> {
        let mut out = Vec::new();
        ciborium::ser::into_writer(v, &mut out).unwrap();
        out
    }

    fn i(n: i64) -> Cbor {
        Cbor::Integer(n.into())
    }

    /// The authenticator data a device makes: the site's hash, flags, counter, a credential.
    fn auth_data(rp: &str, flags: u8, count: u32, credential: Option<(&[u8], &[u8])>) -> Vec<u8> {
        let mut b = sha256(rp.as_bytes()).to_vec();
        b.push(flags);
        b.extend(count.to_be_bytes());
        if let Some((id, key)) = credential {
            b.extend([0u8; 16]);
            b.extend(u16::try_from(id.len()).unwrap().to_be_bytes());
            b.extend(id);
            b.extend(key);
        }
        b
    }

    #[test]
    fn a_p256_key_signs_what_it_should_and_nothing_else() {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        let point = pair.public_key().as_ref();
        let cose = cbor(&Cbor::Map(vec![
            (i(1), i(2)),
            (i(3), i(ES256)),
            (i(-1), i(1)),
            (i(-2), Cbor::Bytes(point[1..33].to_vec())),
            (i(-3), Cbor::Bytes(point[33..].to_vec())),
        ]));
        let (alg, key) = cose_key(&cose).unwrap();
        assert_eq!((alg, key.as_slice()), (ES256, point));
        // Registering: the credential comes back out of the authenticator data, extensions after it ignored.
        let id = [7u8; 32];
        let data = [auth_data("erp.example.com", UP | UV | AT, 0, Some((&id, &cose))), cbor(&Cbor::Map(vec![]))].concat();
        let parsed = parse_auth_data(&data).unwrap();
        assert_eq!((parsed.flags & UV, parsed.credential.clone()), (UV, Some((id.to_vec(), cose.clone()))));
        // Signing in: the signature over the authenticator data and the client data's hash.
        let client = br#"{"type":"webauthn.get","challenge":"AQID","origin":"https://erp.example.com"}"#;
        let auth = auth_data("erp.example.com", UP | UV, 5, None);
        let message = [&auth[..], &sha256(client)].concat();
        let sig = pair.sign(&rng, &message).unwrap();
        assert!(verify(ES256, &key, &message, sig.as_ref()));
        assert!(!verify(ES256, &key, &[&auth[..], &sha256(b"other")].concat(), sig.as_ref()), "another message");
        assert!(!verify(EDDSA, &key, &message, sig.as_ref()), "another algorithm");
        check_client_data(client, "webauthn.get", &[1, 2, 3], &["https://erp.example.com".into()]).unwrap();
        assert!(check_client_data(client, "webauthn.create", &[1, 2, 3], &["https://erp.example.com".into()]).is_err(), "another ceremony");
        assert!(check_client_data(client, "webauthn.get", &[9], &["https://erp.example.com".into()]).is_err(), "another challenge");
        assert!(check_client_data(client, "webauthn.get", &[1, 2, 3], &["https://evil.example".into()]).is_err(), "another site");
        let framed = br#"{"type":"webauthn.get","challenge":"AQID","origin":"https://erp.example.com","crossOrigin":true}"#;
        assert!(check_client_data(framed, "webauthn.get", &[1, 2, 3], &["https://erp.example.com".into()]).is_err(), "inside another site's frame");
    }

    #[test]
    fn a_relying_party_covers_its_own_names_only() {
        assert!(host_may_use_rp("acme.com", "acme.com") && host_may_use_rp("erp.acme.com", "acme.com") && host_may_use_rp("a.b.Acme.com.", "acme.com"));
        for (host, rp) in [("evilacme.com", "acme.com"), ("acme.com.evil.com", "acme.com"), ("acme.com", "erp.acme.com"), (".acme.com", "acme.com"), ("x", "")]
        {
            assert!(!host_may_use_rp(host, rp), "{host} under {rp}");
        }
        assert_eq!(rp_for(Some("acme.com"), "erp.acme.com"), "acme.com");
        assert_eq!(rp_for(Some("acme.com"), "acme.com"), "acme.com");
        assert_eq!(rp_for(Some("acme.com"), "evilacme.com"), "evilacme.com", "a host outside it keeps its own");
        assert_eq!(rp_for(Some("acme.com"), "shop.example.org"), "shop.example.org");
        assert_eq!(rp_for(None, "erp.acme.com"), "erp.acme.com", "as before");
        assert!(origins_may_use(&["https://erp.acme.com".into()], "acme.com").is_ok());
        assert!(origins_may_use(&["https://evilacme.com".into()], "acme.com").is_err());
        assert!(origins_may_use(&["https://erp.acme.com".into(), "https://evilacme.com".into()], "acme.com").is_err(), "every origin");
        assert!(origins_may_use(&[], "acme.com").is_err());
        assert!(origins_may_use(&["not an origin".into()], "acme.com").is_err());
    }

    #[test]
    fn ed25519_keys_and_refusals() {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let cose = cbor(&Cbor::Map(vec![(i(1), i(1)), (i(3), i(EDDSA)), (i(-1), i(6)), (i(-2), Cbor::Bytes(pair.public_key().as_ref().to_vec()))]));
        let (alg, key) = cose_key(&cose).unwrap();
        let sig = pair.sign(b"hello");
        assert!(alg == EDDSA && verify(EDDSA, &key, b"hello", sig.as_ref()) && !verify(EDDSA, &key, b"hullo", sig.as_ref()));
        // Keys this server does not take, and short data.
        let p384 = cbor(&Cbor::Map(vec![(i(1), i(2)), (i(3), i(-35)), (i(-1), i(2))]));
        assert!(cose_key(&p384).is_err());
        let small_rsa = cbor(&Cbor::Map(vec![(i(1), i(3)), (i(3), i(RS256)), (i(-1), Cbor::Bytes(vec![0xab; 128])), (i(-2), Cbor::Bytes(vec![1, 0, 1]))]));
        assert!(cose_key(&small_rsa).is_err(), "1024-bit RSA");
        assert!(parse_auth_data(&[0u8; 20]).is_err());
        assert!(parse_auth_data(&auth_data("x.example", UP | AT, 0, None)).is_err(), "says a credential follows, has none");
        assert!(!verify(RS256, &[0, 1], b"m", b"s"));
    }
}
