//! The vault's cryptography in one place: keys that are zeroed when dropped, sealing with AES-256-GCM bound to where a
//! value is kept (its associated data names the place, so a row copied anywhere else does not open), the slots' key
//! derivations and check values, and the recovery key's printed form. Nothing here reaches the database or the
//! network, and nothing here formats, logs or serialises a key.

use crate::error::{Error, Result};
use base64::Engine as _;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use uuid::Uuid;
use zeroize::Zeroizing;

pub(crate) const KEY_LEN: usize = 32;
const TAG_LEN: usize = 16;
/// The salt every key derived from another starts from (the purpose and where it is from go into its info).
const DERIVE_SALT: &[u8] = b"ferrum-vault/derive";

/// A key: 32 bytes, zeroed when dropped, never shown.
pub(crate) struct Key(Zeroizing<[u8; KEY_LEN]>);

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

/// `n` random bytes, zeroed when dropped.
pub(crate) fn random(n: usize) -> Result<Zeroizing<Vec<u8>>> {
    let mut out = Zeroizing::new(vec![0u8; n]);
    SystemRandom::new().fill(&mut out).map_err(|_| Error::Internal("no randomness for the vault".into()))?;
    Ok(out)
}

impl Key {
    pub(crate) fn random() -> Result<Key> {
        let mut k = Zeroizing::new([0u8; KEY_LEN]);
        SystemRandom::new().fill(&mut *k).map_err(|_| Error::Internal("no randomness for a key".into()))?;
        Ok(Key(k))
    }

    /// From exactly 32 bytes.
    pub(crate) fn from_slice(bytes: &[u8]) -> Option<Key> {
        let mut k = Zeroizing::new([0u8; KEY_LEN]);
        if bytes.len() != KEY_LEN {
            return None;
        }
        k.copy_from_slice(bytes);
        Some(Key(k))
    }

    pub(crate) fn bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    fn aead(&self) -> LessSafeKey {
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &*self.0).expect("a 32-byte AES-256 key"))
    }

    /// `plain` sealed for `place`: a fresh nonce, the ciphertext and its tag. It opens with this key for this place only.
    pub(crate) fn seal(&self, place: &str, plain: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce).map_err(|_| Error::Internal("no randomness for sealing".into()))?;
        let mut out = Vec::with_capacity(NONCE_LEN + plain.len() + TAG_LEN);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(plain);
        let tag = self
            .aead()
            .seal_in_place_separate_tag(Nonce::assume_unique_for_key(nonce), Aad::from(place.as_bytes()), &mut out[NONCE_LEN..])
            .map_err(|_| Error::Internal("sealing failed".into()))?;
        out.extend_from_slice(tag.as_ref());
        Ok(out)
    }

    /// What [`Key::seal`] sealed for `place`; `None` when it does not open: another key, another place, or damage.
    pub(crate) fn open(&self, place: &str, sealed: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
        if sealed.len() < NONCE_LEN + TAG_LEN {
            return None;
        }
        let (nonce, body) = sealed.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
        let mut buf = Zeroizing::new(body.to_vec());
        let len = self.aead().open_in_place(nonce, Aad::from(place.as_bytes()), &mut buf).ok()?.len();
        buf.truncate(len);
        Some(buf)
    }

    /// `key` wrapped by this key for `place`.
    pub(crate) fn wrap(&self, place: &str, key: &Key) -> Result<Vec<u8>> {
        self.seal(place, key.bytes())
    }

    pub(crate) fn unwrap(&self, place: &str, wrapped: &[u8]) -> Option<Key> {
        self.open(place, wrapped).and_then(|k| Key::from_slice(&k))
    }

    fn hmac(&self) -> ring::hmac::Key {
        ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &*self.0)
    }

    /// An HMAC of `what` under this key: what a slot keeps to tell a wrong key from a damaged row.
    pub(crate) fn check_value(&self, what: &str) -> Vec<u8> {
        ring::hmac::sign(&self.hmac(), what.as_bytes()).as_ref().to_vec()
    }

    /// Whether `value` is [`Key::check_value`] of `what` under this key (compared in constant time).
    pub(crate) fn checks(&self, what: &str, value: &[u8]) -> bool {
        ring::hmac::verify(&self.hmac(), what.as_bytes(), value).is_ok()
    }

    /// A key for `info` from this one (HKDF-SHA256): the same key and info give the same key, and it says nothing of this one.
    pub(crate) fn derive(&self, info: &str) -> Key {
        derive_from(DERIVE_SALT, &*self.0, info)
    }
}

/// HKDF-SHA256 of `ikm` with `salt`, for `info`.
fn derive_from(salt: &[u8], ikm: &[u8], info: &str) -> Key {
    struct Len;
    impl ring::hkdf::KeyType for Len {
        fn len(&self) -> usize {
            KEY_LEN
        }
    }
    let mut out = Zeroizing::new([0u8; KEY_LEN]);
    let info = [info.as_bytes()];
    ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, salt)
        .extract(ikm)
        .expand(&info, Len)
        .and_then(|okm| okm.fill(&mut *out))
        .expect("32 bytes are within what HKDF-SHA256 gives");
    Key(out)
}

/// Argon2id's costs, as each credential slot keeps them: they can be raised for new slots without breaking old ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Kdf {
    /// Memory, in KiB.
    pub m_kib: u32,
    /// Passes.
    pub t: u32,
    /// Lanes.
    pub p: u32,
}

impl Default for Kdf {
    /// The costs people's passwords are hashed with (crate::auth): 19 MiB, two passes, one lane.
    fn default() -> Self {
        Kdf { m_kib: argon2::Params::DEFAULT_M_COST, t: argon2::Params::DEFAULT_T_COST, p: argon2::Params::DEFAULT_P_COST }
    }
}

/// The credential slot's key: Argon2id of the database password, with the slot's salt and costs.
pub(crate) fn password_key(password: &[u8], salt: &[u8], kdf: Kdf) -> Result<Key> {
    let params = argon2::Params::new(kdf.m_kib, kdf.t, kdf.p, Some(KEY_LEN)).map_err(|e| Error::Invalid(format!("Argon2id's costs: {e}")))?;
    let mut out = Zeroizing::new([0u8; KEY_LEN]);
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(password, salt, &mut *out)
        .map_err(|e| Error::Invalid(format!("deriving the credential slot's key: {e}")))?;
    Ok(Key(out))
}

/// The recovery slot's key: from the recovery key (32 random bytes, so no slow derivation is needed) and the slot's salt.
pub(crate) fn recovery_slot_key(recovery: &Key, salt: &[u8]) -> Key {
    derive_from(salt, recovery.bytes(), "ferrum-vault/recovery")
}

/// The recovery key as printed: its 32 bytes in base32, in groups of four (`ABCD-EFGH-…`).
pub(crate) fn print_recovery(key: &Key) -> Zeroizing<String> {
    let b32 = Zeroizing::new(data_encoding::BASE32_NOPAD.encode(key.bytes()));
    let groups: Vec<&str> = b32.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).expect("base32 is ASCII")).collect();
    Zeroizing::new(groups.join("-"))
}

/// The file slot's key, as `openssl rand -base64 32` writes it.
pub(crate) fn read_key_file(text: &str) -> Option<Key> {
    let bytes = Zeroizing::new(base64::engine::general_purpose::STANDARD.decode(text.trim()).ok()?);
    Key::from_slice(&bytes)
}

/// A key as base64 (what Transit takes and gives back), zeroed when dropped.
pub(crate) fn key_base64(key: &Key) -> Zeroizing<String> {
    Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(key.bytes()))
}

// Where each kind of value is kept, as its associated data says it: every part of the place is in it, so a value
// copied to another workspace, owner, name, version or slot, or under another version of the key above it, does not
// open. No part but the last (an app's own context) can hold a `/`.

/// The installation key, wrapped by a slot.
pub(crate) fn root_place(kind: &str, installation_version: i32) -> String {
    format!("ferrum-vault/root/{kind}/{installation_version}")
}

/// What a slot's check value is of.
pub(crate) fn check_place(kind: &str) -> String {
    format!("ferrum-vault/check/{kind}")
}

/// A workspace key (owner `""`) under the installation key, or an owner key under its workspace key.
pub(crate) fn key_place(scope: Uuid, owner: &str, version: i32, parent_version: i32) -> String {
    format!("ferrum-vault/key/{scope}/{owner}/{version}/{parent_version}")
}

/// Data an owner sealed to keep outside the vault, in the context it names (the one part that can hold a `/`, last).
pub(crate) fn sealed_place(scope: Uuid, owner: &str, key_version: i32, context: &str) -> String {
    format!("ferrum-vault/sealed/{scope}/{owner}/{key_version}/{context}")
}

/// A key an owner derived for a purpose.
pub(crate) fn derive_info(scope: Uuid, owner: &str, key_version: i32, purpose: &str) -> String {
    format!("ferrum-vault/derived/{scope}/{owner}/{key_version}/{purpose}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_value_opens_with_its_key_in_its_place_only() {
        let key = Key::random().unwrap();
        let sealed = key.seal("here", b"hunter2").unwrap();
        assert_ne!(sealed, key.seal("here", b"hunter2").unwrap(), "a fresh nonce each time");
        assert_eq!(&**key.open("here", &sealed).unwrap(), b"hunter2");
        assert!(key.open("there", &sealed).is_none(), "another place");
        assert!(Key::random().unwrap().open("here", &sealed).is_none(), "another key");
        for i in 0..sealed.len() {
            let mut changed = sealed.clone();
            changed[i] ^= 1;
            assert!(key.open("here", &changed).is_none(), "byte {i} changed");
        }
        assert!(key.open("here", &sealed[..sealed.len() - 1]).is_none(), "cut short");
        assert!(key.open("here", &[0u8; 27]).is_none(), "too short to be anything");
        let empty = key.seal("here", b"").unwrap();
        assert_eq!(key.open("here", &empty).unwrap().len(), 0);
    }

    #[test]
    fn keys_wrap_check_and_derive() {
        let (a, b) = (Key::random().unwrap(), Key::random().unwrap());
        let wrapped = a.wrap("p", &b).unwrap();
        assert_eq!(a.unwrap("p", &wrapped).unwrap().bytes(), b.bytes());
        assert!(a.unwrap("q", &wrapped).is_none());
        assert!(!wrapped.windows(KEY_LEN).any(|w| w == b.bytes()), "the wrapped key is not in what is stored");
        let check = a.check_value("credential");
        assert!(a.checks("credential", &check));
        assert!(!a.checks("file", &check), "a check value is of one thing");
        assert!(!b.checks("credential", &check), "and under one key");
        assert_eq!(a.derive("x").bytes(), a.derive("x").bytes());
        assert_ne!(a.derive("x").bytes(), a.derive("y").bytes());
        assert_ne!(a.derive("x").bytes(), b.derive("x").bytes());
        assert_ne!(a.derive("x").bytes(), a.bytes());
        assert_eq!(format!("{a:?}"), "Key(..)", "a key never shows itself");
    }

    #[test]
    fn slot_keys_come_back_from_what_made_them() {
        let fast = Kdf { m_kib: 64, t: 1, p: 1 };
        let k = password_key(b"pw", b"saltsalt", fast).unwrap();
        assert_eq!(k.bytes(), password_key(b"pw", b"saltsalt", fast).unwrap().bytes());
        assert_ne!(k.bytes(), password_key(b"pw2", b"saltsalt", fast).unwrap().bytes());
        assert_ne!(k.bytes(), password_key(b"pw", b"saltSALT", fast).unwrap().bytes());
        assert!(password_key(b"pw", b"salt", Kdf { m_kib: 1, t: 1, p: 1 }).is_err(), "costs Argon2id refuses");
        let recovery = Key::random().unwrap();
        let printed = print_recovery(&recovery);
        assert_eq!(printed.len(), 52 + 12, "52 characters in 13 groups");
        let salt = random(16).unwrap();
        assert_eq!(recovery_slot_key(&recovery, &salt).bytes(), recovery_slot_key(&recovery, &salt).bytes());
        assert_ne!(recovery_slot_key(&recovery, &salt).bytes(), recovery.bytes());
        let file = key_base64(&recovery);
        assert_eq!(read_key_file(&format!("{}\n", *file)).unwrap().bytes(), recovery.bytes());
        assert!(read_key_file("c2hvcnQ=").is_none(), "not 32 bytes");
    }

    #[test]
    fn every_part_of_a_place_is_in_it() {
        let (s, t) = (Uuid::now_v7(), Uuid::now_v7());
        let key = key_place(s, "crm", 1, 1);
        for other in [key_place(t, "crm", 1, 1), key_place(s, "", 1, 1), key_place(s, "hr", 1, 1), key_place(s, "crm", 2, 1), key_place(s, "crm", 1, 2)] {
            assert_ne!(key, other);
        }
        assert_ne!(root_place("file", 1), root_place("credential", 1));
        assert_ne!(root_place("file", 1), root_place("file", 2));
        assert_ne!(sealed_place(s, "crm", 1, "a/b"), sealed_place(s, "crm", 1, "a/c"));
        assert_ne!(sealed_place(s, "crm", 1, "a"), sealed_place(s, "crm", 2, "a"));
        assert_ne!(derive_info(s, "crm", 1, "links"), derive_info(s, "crm", 2, "links"));
        assert_ne!(derive_info(s, "crm", 1, "links"), derive_info(t, "crm", 1, "links"));
    }
}
