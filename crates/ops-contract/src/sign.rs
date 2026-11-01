//! What a site signs. Ed25519 over the request, with a timestamp that must increase per key: the
//! server stores public keys only, and a replay fails with no nonce table.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How far a request's time may be from the server's. A site whose clock is further off learns the
/// server's time from the refusal and corrects for it.
pub const MAX_CLOCK_SKEW_MS: i64 = 300_000;

pub const HEADER_KEY: &str = "x-ferrum-key";
pub const HEADER_TIME: &str = "x-ferrum-time";
pub const HEADER_SIGNATURE: &str = "x-ferrum-signature";

/// The bytes signed for a request: nothing in it can be changed or moved to another address.
pub fn message(method: &str, path: &str, time_ms: i64, body: &[u8]) -> Vec<u8> {
    format!("ferrum-site-v1\n{method}\n{path}\n{time_ms}\n{}", hex::encode(Sha256::digest(body))).into_bytes()
}

/// `POST /api/edge/{tenant}/enroll`: proves nothing but the code, which is used once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Enroll {
    pub code: String,
    /// Ed25519, hex. The private half never leaves the site.
    pub public_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Enrolled {
    pub key_id: uuid::Uuid,
    pub device_id: uuid::Uuid,
    pub site_id: uuid::Uuid,
    pub domain: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_in_the_request_changes_what_is_signed() {
        let base = message("POST", "/api/edge/acme/sync", 1000, b"{}");
        assert_ne!(base, message("GET", "/api/edge/acme/sync", 1000, b"{}"));
        assert_ne!(base, message("POST", "/api/edge/other/sync", 1000, b"{}"));
        assert_ne!(base, message("POST", "/api/edge/acme/sync", 1001, b"{}"));
        assert_ne!(base, message("POST", "/api/edge/acme/sync", 1000, b"{ }"));
    }
}
