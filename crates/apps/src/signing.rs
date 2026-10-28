//! Ed25519 signatures over bundle digests: keys, signing and verification.

use crate::{AppError, Bundle, MAX_SIGNATURES};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::str::FromStr;

/// Standard alphabet; padding optional when decoding (keys are often pasted without it).
const LENIENT: GeneralPurpose =
    GeneralPurpose::new(&base64::alphabet::STANDARD, GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent));

/// Who vouches for a release: its publisher, or the store after review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Publisher,
    Store,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Role::Publisher => "publisher",
            Role::Store => "store",
        })
    }
}

impl FromStr for Role {
    type Err = AppError;

    fn from_str(s: &str) -> Result<Role, AppError> {
        match s {
            "publisher" => Ok(Role::Publisher),
            "store" => Ok(Role::Store),
            other => Err(AppError::Key(format!("role must be publisher or store, not '{other}'"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignatureEntry {
    /// [`key_id`] of the signing key.
    pub key_id: String,
    pub role: Role,
    /// Standard base64 of the Ed25519 signature over the 32 raw digest bytes.
    pub sig: String,
}

/// `SIGNATURES.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signatures {
    /// The signed digest (lower-case hex).
    pub digest: String,
    pub signatures: Vec<SignatureEntry>,
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn bad<T>(msg: impl Into<String>) -> Result<T, AppError> {
    Err(AppError::Signature(msg.into()))
}

impl Signatures {
    pub(crate) fn parse(raw: &[u8]) -> Result<Signatures, AppError> {
        let s: Signatures = serde_json::from_slice(raw).map_err(|e| AppError::Signature(format!("SIGNATURES.json is not valid: {e}")))?;
        if !is_hex(&s.digest, 64) {
            return bad("SIGNATURES.json: digest must be 64 lower-case hex characters");
        }
        if s.signatures.len() > MAX_SIGNATURES {
            return bad(format!("SIGNATURES.json: at most {MAX_SIGNATURES} signatures"));
        }
        for e in &s.signatures {
            if !is_hex(&e.key_id, 16) {
                return bad(format!("SIGNATURES.json: key_id '{}' must be 16 lower-case hex characters", e.key_id));
            }
            if STANDARD.decode(&e.sig).map(|s| s.len()) != Ok(64) {
                return bad(format!("SIGNATURES.json: the signature by key {} is not a base64 Ed25519 signature", e.key_id));
            }
        }
        Ok(s)
    }
}

/// Lower-case hex of the first 8 bytes of SHA-256(public key).
pub fn key_id(public_key: &[u8]) -> String {
    hex::encode(&Sha256::digest(public_key)[..8])
}

/// An Ed25519 private key that signs bundles.
pub struct Signer {
    pair: Ed25519KeyPair,
}

impl fmt::Debug for Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Signer").field("key_id", &self.key_id()).finish_non_exhaustive()
    }
}

impl Signer {
    /// A new private key as a PKCS#8 (v2) DER document; keep it secret.
    pub fn generate_pkcs8() -> Result<Vec<u8>, AppError> {
        Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map(|doc| doc.as_ref().to_vec())
            .map_err(|_| AppError::Key("cannot generate a key: the system random number generator failed".into()))
    }

    /// Loads a PKCS#8 DER Ed25519 private key (v2 as generated here, or v1 as `openssl genpkey`
    /// writes).
    pub fn from_pkcs8(der: &[u8]) -> Result<Signer, AppError> {
        Ed25519KeyPair::from_pkcs8_maybe_unchecked(der)
            .map(|pair| Signer { pair })
            .map_err(|e| AppError::Key(format!("not an Ed25519 private key in PKCS#8 DER form ({e})")))
    }

    /// The 32-byte public key.
    pub fn public_key(&self) -> Vec<u8> {
        self.pair.public_key().as_ref().to_vec()
    }

    pub fn key_id(&self) -> String {
        key_id(self.pair.public_key().as_ref())
    }
}

/// A public key this cluster (or store) trusts for one role. Written `store:<base64>` or
/// `publisher:<base64>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedKey {
    pub key_id: String,
    pub public_key: Vec<u8>,
    pub role: Role,
}

impl TrustedKey {
    pub fn parse(s: &str) -> Result<TrustedKey, AppError> {
        let (role, key) =
            s.trim().split_once(':').ok_or_else(|| AppError::Key(format!("'{s}': expected store:<base64 public key> or publisher:<base64 public key>")))?;
        let role: Role = role.trim().parse()?;
        let public_key = LENIENT.decode(key.trim()).map_err(|e| AppError::Key(format!("the public key is not base64: {e}")))?;
        if public_key.len() != 32 {
            return Err(AppError::Key(format!("an Ed25519 public key has 32 bytes, this one has {}", public_key.len())));
        }
        Ok(TrustedKey { key_id: key_id(&public_key), public_key, role })
    }
}

impl fmt::Display for TrustedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.role, STANDARD.encode(&self.public_key))
    }
}

impl Bundle {
    /// Adds (or renews) `signer`'s signature in `role`. Refuses when the signatures already
    /// present were made for different content.
    pub fn sign(&mut self, signer: &Signer, role: Role) -> Result<(), AppError> {
        let digest = self.digest();
        let raw = hex::decode(&digest).map_err(|e| AppError::Signature(e.to_string()))?;
        let entry = SignatureEntry { key_id: signer.key_id(), role, sig: STANDARD.encode(signer.pair.sign(&raw).as_ref()) };
        let sigs = self.signatures.get_or_insert_with(|| Signatures { digest: digest.clone(), signatures: Vec::new() });
        if sigs.digest != digest {
            return bad(format!(
                "the bundle's signatures are for other content (signed digest {}, content digest {digest}); remove them before signing",
                sigs.digest
            ));
        }
        match sigs.signatures.iter().position(|e| e.key_id == entry.key_id && e.role == role) {
            Some(i) => sigs.signatures[i] = entry,
            None if sigs.signatures.len() >= MAX_SIGNATURES => return bad(format!("a bundle carries at most {MAX_SIGNATURES} signatures")),
            None => sigs.signatures.push(entry),
        }
        Ok(())
    }

    /// Checks that the signed digest is this content's and every signature by a trusted key (same
    /// key id and role); returns those `(key_id, role)`. Fails when the bundle is unsigned, was
    /// changed after signing, carries a bad signature by a trusted key, or no trusted key signed
    /// it. Signatures by other keys are ignored.
    pub fn verify(&self, trusted: &[TrustedKey]) -> Result<Vec<(String, Role)>, AppError> {
        let Some(sigs) = &self.signatures else { return bad("the bundle is not signed") };
        let digest = self.digest();
        if sigs.digest != digest {
            return bad(format!("the bundle was changed after signing: its content digest is {digest}, the signed digest {}", sigs.digest));
        }
        let raw = hex::decode(&digest).map_err(|e| AppError::Signature(e.to_string()))?;
        let mut verified: Vec<(String, Role)> = Vec::new();
        for entry in &sigs.signatures {
            for key in trusted.iter().filter(|k| k.key_id == entry.key_id && k.role == entry.role) {
                let sig = STANDARD
                    .decode(&entry.sig)
                    .map_err(|_| AppError::Signature(format!("the {} signature by key {} is not base64", entry.role, entry.key_id)))?;
                UnparsedPublicKey::new(&ED25519, &key.public_key)
                    .verify(&raw, &sig)
                    .map_err(|_| AppError::Signature(format!("the {} signature by trusted key {} does not match the content", entry.role, entry.key_id)))?;
                if !verified.iter().any(|(id, role)| id == &entry.key_id && *role == entry.role) {
                    verified.push((entry.key_id.clone(), entry.role));
                }
            }
        }
        if verified.is_empty() {
            let by: Vec<String> = sigs.signatures.iter().map(|e| format!("{} {}", e.role, e.key_id)).collect();
            let by = if by.is_empty() { "nobody".to_string() } else { by.join(", ") };
            return bad(format!("no trusted key signed this bundle (signed by: {by})"));
        }
        Ok(verified)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use std::collections::BTreeMap;

    fn trusted(s: &Signer, role: Role) -> TrustedKey {
        TrustedKey::parse(&format!("{role}:{}", STANDARD.encode(s.public_key()))).unwrap()
    }

    fn signed(signers: &[(&Signer, Role)]) -> Bundle {
        let mut b = Bundle::from_files(wasm_app()).unwrap();
        for (s, role) in signers {
            b.sign(s, *role).unwrap();
        }
        b
    }

    /// The signed bundle with one file changed, as an attacker would repack it.
    fn tampered(b: &Bundle, path: &str, content: &[u8]) -> Bundle {
        let mut files: BTreeMap<String, Vec<u8>> = b.paths().map(|p| (p.to_string(), b.file(p).unwrap().to_vec())).collect();
        files.insert(path.into(), content.to_vec());
        files.insert("SIGNATURES.json".into(), serde_json::to_vec(b.signatures.as_ref().unwrap()).unwrap());
        Bundle::from_files(files).unwrap()
    }

    #[test]
    fn keys() {
        let s = signer();
        assert_eq!(s.public_key().len(), 32);
        assert_eq!(s.key_id(), hex::encode(&Sha256::digest(s.public_key())[..8]));
        assert_eq!(s.key_id().len(), 16);
        let t = trusted(&s, Role::Store);
        assert_eq!((t.key_id.as_str(), t.role), (s.key_id().as_str(), Role::Store));
        assert_eq!(TrustedKey::parse(&t.to_string()).unwrap(), t);
        let unpadded = t.to_string().trim_end_matches('=').to_string();
        assert_eq!(TrustedKey::parse(&format!("  {unpadded}\n")).unwrap(), t);
        assert!(TrustedKey::parse("admin:AAAA").unwrap_err().to_string().contains("publisher or store"));
        assert!(TrustedKey::parse("store").unwrap_err().to_string().contains("expected store:"));
        assert!(TrustedKey::parse("store:not base64!").unwrap_err().to_string().contains("not base64"));
        assert!(TrustedKey::parse("store:AAAA").unwrap_err().to_string().contains("32 bytes"));
        assert!(Signer::from_pkcs8(b"not a key").is_err());
        assert!(format!("{s:?}").contains(&s.key_id()));
        // A PKCS#8 v1 key (no public key inside, as openssl writes) loads too.
        let v1_prefix = hex::decode("302e020100300506032b657004220420").unwrap();
        let seed = [7u8; 32];
        let v1 = Signer::from_pkcs8(&[v1_prefix, seed.to_vec()].concat()).unwrap();
        assert_eq!(v1.public_key().len(), 32);
    }

    #[test]
    fn publisher_and_store_signatures_verify() {
        let (publisher, store) = (signer(), signer());
        let b = signed(&[(&publisher, Role::Publisher), (&store, Role::Store)]);
        let sigs = b.signatures.as_ref().unwrap();
        assert_eq!(sigs.digest, b.digest());
        assert_eq!(sigs.signatures.len(), 2);
        assert_eq!(b.verify(&[trusted(&store, Role::Store)]).unwrap(), vec![(store.key_id(), Role::Store)]);
        assert_eq!(
            b.verify(&[trusted(&store, Role::Store), trusted(&publisher, Role::Publisher)]).unwrap(),
            vec![(publisher.key_id(), Role::Publisher), (store.key_id(), Role::Store)]
        );
        // Survives writing and reading; the signature is over the raw digest bytes.
        let back = Bundle::read(&b.write().unwrap()).unwrap();
        assert_eq!(back.verify(&[trusted(&store, Role::Store)]).unwrap().len(), 1);
        let raw = hex::decode(b.digest()).unwrap();
        let sig = STANDARD.decode(&sigs.signatures[1].sig).unwrap();
        UnparsedPublicKey::new(&ED25519, store.public_key()).verify(&raw, &sig).unwrap();
        // Signing again with the same key and role replaces the entry.
        let mut again = b.clone();
        again.sign(&store, Role::Store).unwrap();
        assert_eq!(again.signatures.as_ref().unwrap().signatures.len(), 2);
    }

    #[test]
    fn tampering_is_detected() {
        let store = signer();
        let b = signed(&[(&store, Role::Store)]);
        let keys = [trusted(&store, Role::Store)];
        // A changed file: the content no longer has the signed digest.
        let changed = tampered(&b, "README.md", b"# Totally legitimate\n");
        assert!(changed.verify(&keys).unwrap_err().to_string().contains("changed after signing"));
        // The attacker also rewrites the digest: the trusted signature no longer matches.
        let mut redigested = changed.clone();
        redigested.signatures.as_mut().unwrap().digest = redigested.digest();
        assert!(redigested.verify(&keys).unwrap_err().to_string().contains("does not match the content"));
        // A signature swapped for another valid-looking one.
        let mut forged = b.clone();
        forged.signatures.as_mut().unwrap().signatures[0].sig = STANDARD.encode([0u8; 64]);
        assert!(forged.verify(&keys).unwrap_err().to_string().contains("does not match"));
        // Signing on top of signatures for other content is refused.
        let mut stale = changed.clone();
        assert!(stale.sign(&store, Role::Store).unwrap_err().to_string().contains("other content"));
    }

    #[test]
    fn untrusted_and_wrong_keys() {
        let (store, stranger, publisher) = (signer(), signer(), signer());
        // Only an untrusted key signed: refused.
        let b = signed(&[(&stranger, Role::Store)]);
        let err = b.verify(&[trusted(&store, Role::Store)]).unwrap_err().to_string();
        assert!(err.contains("no trusted key") && err.contains(&stranger.key_id()), "{err}");
        // Untrusted signatures, even broken ones, are ignored next to a trusted one.
        let mut b = signed(&[(&stranger, Role::Store), (&store, Role::Store)]);
        b.signatures.as_mut().unwrap().signatures[0].sig = STANDARD.encode([1u8; 64]);
        assert_eq!(b.verify(&[trusted(&store, Role::Store)]).unwrap(), vec![(store.key_id(), Role::Store)]);
        // A key counts only in the role it is trusted for.
        let b = signed(&[(&publisher, Role::Store)]);
        assert!(b.verify(&[trusted(&publisher, Role::Publisher)]).is_err());
        assert!(b.verify(&[trusted(&publisher, Role::Store)]).is_ok());
        // Unsigned and no trusted keys at all.
        assert!(Bundle::from_files(wasm_app()).unwrap().verify(&[trusted(&store, Role::Store)]).unwrap_err().to_string().contains("not signed"));
        assert!(signed(&[(&store, Role::Store)]).verify(&[]).is_err());
    }

    #[test]
    fn signature_file_limits() {
        let mut b = Bundle::from_files(wasm_app()).unwrap();
        for _ in 0..MAX_SIGNATURES {
            b.sign(&signer(), Role::Publisher).unwrap();
        }
        assert!(b.sign(&signer(), Role::Publisher).unwrap_err().to_string().contains("at most"));
        let mut too_many = b.signatures.clone().unwrap();
        too_many.signatures.push(too_many.signatures[0].clone());
        assert!(Signatures::parse(&serde_json::to_vec(&too_many).unwrap()).is_err());
        let mut bad_id = b.signatures.clone().unwrap();
        bad_id.signatures[0].key_id = "XYZ".into();
        assert!(Signatures::parse(&serde_json::to_vec(&bad_id).unwrap()).unwrap_err().to_string().contains("key_id"));
        assert!(Signatures::parse(br#"{"digest": "00", "signatures": [], "extra": 1}"#).is_err());
    }
}
