//! Connections that sign a token (TP-1): a JWT under a private key the connection keeps as a secret, ES256 or RS256,
//! reused for a while; or given to an operation that answers an access token, kept as a session's is.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use chrono::Utc;
use ring::signature::{self, EcdsaKeyPair, RsaKeyPair};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{Error, Result};

/// The longest a signed token is reused, in seconds: under the hour the services that take one allow.
pub const MAX_REUSE: u32 = 3300;
/// How much longer than its reuse a token is valid, in seconds: a call made at the end of the reuse still arrives in time.
const SPARE: i64 = 300;

pub(crate) fn default_reuse() -> u32 {
    3000
}

/// How a token is signed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Alg {
    /// ECDSA on P-256 with SHA-256.
    #[serde(rename = "ES256")]
    Es256,
    /// RSA PKCS#1 v1.5 with SHA-256.
    #[serde(rename = "RS256")]
    Rs256,
}

enum Key {
    Ec(EcdsaKeyPair),
    Rsa(RsaKeyPair),
}

/// The private key `pem` holds (PKCS#8, PEM) for `alg`, or why it holds none. Never the key's text in the reason.
fn key(alg: Alg, pem: &str) -> std::result::Result<Key, String> {
    let name = match alg {
        Alg::Es256 => "ES256 (a P-256 key)",
        Alg::Rs256 => "RS256 (an RSA key of 2048 bits or more)",
    };
    let wrong = || format!("the key is not a private key for {name}, as PKCS#8 in PEM");
    let body: String = pem.lines().map(str::trim).filter(|l| !l.starts_with("-----")).collect();
    let der = STANDARD.decode(body).map_err(|_| wrong())?;
    match alg {
        Alg::Es256 => EcdsaKeyPair::from_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, &der, &ring::rand::SystemRandom::new()).map(Key::Ec),
        Alg::Rs256 => RsaKeyPair::from_pkcs8(&der).map(Key::Rsa),
    }
    .map_err(|_| wrong())
}

/// Why `pem` cannot sign as `alg`, for saving a connection.
pub(crate) fn key_problem(alg: Alg, pem: &str) -> Option<String> {
    key(alg, pem).err()
}

/// A token signed now: the alg and `kid` in its header, `claims` with `iat` and `exp` (`life` seconds on).
fn sign(alg: Alg, pem: &str, kid: Option<&str>, claims: &Map<String, Value>, life: i64) -> Result<String> {
    let key = key(alg, pem).map_err(Error::invalid)?;
    let mut header = json!({"alg": alg, "typ": "JWT"});
    if let Some(kid) = kid {
        header["kid"] = json!(kid);
    }
    let now = Utc::now().timestamp();
    let mut claims = claims.clone();
    claims.insert("iat".into(), json!(now));
    claims.insert("exp".into(), json!(now + life));
    let signing = format!("{}.{}", URL_SAFE_NO_PAD.encode(header.to_string()), URL_SAFE_NO_PAD.encode(Value::Object(claims).to_string()));
    let rng = ring::rand::SystemRandom::new();
    let unsigned = || Error::internal("a token's signature");
    let sig = match key {
        Key::Ec(k) => k.sign(&rng, signing.as_bytes()).map_err(|_| unsigned())?.as_ref().to_vec(),
        Key::Rsa(k) => {
            let mut sig = vec![0; k.public().modulus_len()];
            k.sign(&signature::RSA_PKCS1_SHA256, &rng, signing.as_bytes(), &mut sig).map_err(|_| unsigned())?;
            sig
        }
    };
    Ok(format!("{signing}.{}", URL_SAFE_NO_PAD.encode(sig)))
}

/// Tokens signed and reused, by connection: a digest of what signed them, the token, when it was signed.
// ponytail: per server process; each process signs its own, which the services allow.
static SIGNED: LazyLock<Mutex<HashMap<Uuid, (String, String, i64)>>> = LazyLock::new(Default::default);

/// Connection `conn`'s token: the one signed under the same key, kid and claims less than `reuse` seconds ago, else
/// one signed now.
pub(crate) fn token(conn: Uuid, alg: Alg, pem: &str, kid: Option<&str>, claims: &Map<String, Value>, reuse: u32) -> Result<String> {
    let what = hex::encode(Sha256::digest(format!("{alg:?}\0{pem}\0{kid:?}\0{}", Value::Object(claims.clone())).as_bytes()));
    let now = Utc::now().timestamp();
    let mut signed = SIGNED.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((was, token, at)) = signed.get(&conn)
        && *was == what
        && now - at < i64::from(reuse)
    {
        return Ok(token.clone());
    }
    let token = sign(alg, pem, kid, claims, i64::from(reuse) + SPARE)?;
    signed.insert(conn, (what, token.clone(), now));
    Ok(token)
}
