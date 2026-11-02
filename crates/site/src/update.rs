//! Updating the runtime, and coming back from a bad update on its own. A release is believed only if
//! a key this site was installed with signed it, it has not expired, and it is newer than what is
//! running, so an old build cannot be replayed. The new binary replaces the old one with the old kept
//! beside it. A new one that keeps dying before it has spoken to the platform is rolled back.
//!
//! This is the gateway's own software. Robot, PLC and safety firmware stay with their makers.

use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use ferrum_ops_contract::edge::{Release, newer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Starts a new build gets to prove itself.
const MAX_TRIES: u32 = 3;
/// The exit code that asks the service manager for a restart into the new build.
pub const RESTART: i32 = 75;

/// Why a release is not for this site. Nothing here has touched the disk.
pub fn judge(release: &Release, running: &str, keys: &[String], now: DateTime<Utc>, refused: Option<&str>) -> Result<(), String> {
    if !newer(&release.version, running) {
        return Err(format!("{} is not newer than {running}", release.version));
    }
    if refused == Some(release.version.as_str()) {
        return Err(format!("{} was rolled back here", release.version));
    }
    if now >= release.expires_at {
        return Err(format!("{} has expired", release.version));
    }
    let signature: [u8; 64] = hex::decode(&release.signature).ok().and_then(|b| b.try_into().ok()).ok_or("the signature is not one")?;
    let trusted = keys.iter().filter_map(|k| hex::decode(k).ok()?.try_into().ok()).filter_map(|raw: [u8; 32]| VerifyingKey::from_bytes(&raw).ok());
    for key in trusted {
        if key.verify(&release.message(), &Signature::from_bytes(&signature)).is_ok() {
            return Ok(());
        }
    }
    Err("no key this site trusts signed it".into())
}

/// A new build on its way in: what to restore, and how many times it has started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub version: String,
    pub tries: u32,
}

fn previous(exe: &Path) -> PathBuf {
    exe.with_extension("previous")
}

/// Checks the file against what was signed and puts it in place. The running program keeps its
/// inode, so this is safe under it, and a crash at any point leaves one whole binary at `exe`.
pub fn install(exe: &Path, download: &Path, release: &Release) -> Result<(), String> {
    let bytes = std::fs::read(download).map_err(|e| e.to_string())?;
    if !hex::encode(Sha256::digest(&bytes)).eq_ignore_ascii_case(&release.sha256) {
        let _ = std::fs::remove_file(download);
        return Err("the file is not what was signed".into());
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(download, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    std::fs::copy(exe, previous(exe)).map_err(|e| format!("keeping the old build: {e}"))?;
    std::fs::rename(download, exe).map_err(|e| format!("{}: {e}", exe.display()))
}

/// What to do at start, given what was pending. `Err` is the version to refuse from now on.
pub fn at_start(exe: &Path, pending: Option<Pending>, running: &str) -> Result<Option<Pending>, String> {
    let Some(p) = pending else { return Ok(None) };
    if p.version != running {
        // The new build never took, or something else put this one here. Either way it is over.
        return Ok(None);
    }
    if p.tries + 1 < MAX_TRIES {
        return Ok(Some(Pending { tries: p.tries + 1, ..p }));
    }
    let old = previous(exe);
    if old.exists() {
        std::fs::rename(&old, exe).map_err(|e| format!("rolling back: {e}"))?;
    }
    Err(p.version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use ed25519_dalek::{Signer, SigningKey};

    fn signed(key: &SigningKey, version: &str, hours: i64) -> Release {
        let mut r = Release {
            version: version.into(),
            url: "https://x".into(),
            sha256: "ab".into(),
            signature: String::new(),
            expires_at: Utc::now() + Duration::hours(hours),
        };
        r.signature = hex::encode(key.sign(&r.message()).to_bytes());
        r
    }

    #[test]
    fn a_release_is_believed_only_if_everything_holds() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let other = SigningKey::from_bytes(&[8; 32]);
        let keys = vec![hex::encode(key.verifying_key().to_bytes())];
        let now = Utc::now();
        assert!(judge(&signed(&key, "0.2.0", 1), "0.1.0", &keys, now, None).is_ok());
        assert!(judge(&signed(&other, "0.2.0", 1), "0.1.0", &keys, now, None).is_err_and(|e| e.contains("no key")), "a stranger's signature");
        assert!(judge(&signed(&key, "0.1.0", 1), "0.1.0", &keys, now, None).is_err_and(|e| e.contains("not newer")), "an old build replayed");
        assert!(judge(&signed(&key, "0.2.0", -1), "0.1.0", &keys, now, None).is_err_and(|e| e.contains("expired")));
        assert!(judge(&signed(&key, "0.2.0", 1), "0.1.0", &[], now, None).is_err(), "no keys, no updates");
        assert!(judge(&signed(&key, "0.2.0", 1), "0.1.0", &keys, now, Some("0.2.0")).is_err_and(|e| e.contains("rolled back")), "not twice");
        let mut tampered = signed(&key, "0.2.0", 1);
        tampered.url = "https://evil".into();
        assert!(judge(&tampered, "0.1.0", &keys, now, None).is_err(), "the signature covers where it comes from");
    }

    #[test]
    fn a_bad_file_never_gets_in_and_a_good_one_keeps_the_old_beside_it() {
        let dir = std::env::temp_dir().join(format!("ferrum-update-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let (exe, download) = (dir.join("ferrum-site"), dir.join("download"));
        std::fs::write(&exe, b"old").unwrap();
        std::fs::write(&download, b"tampered").unwrap();
        let release = Release {
            version: "0.2.0".into(),
            url: String::new(),
            sha256: hex::encode(Sha256::digest(b"new")),
            signature: String::new(),
            expires_at: Utc::now(),
        };
        assert!(install(&exe, &download, &release).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        assert!(!download.exists(), "and it does not stay");
        std::fs::write(&download, b"new").unwrap();
        install(&exe, &download, &release).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert_eq!(std::fs::read(previous(&exe)).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_build_that_keeps_dying_is_rolled_back() {
        let dir = std::env::temp_dir().join(format!("ferrum-update-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("ferrum-site");
        std::fs::write(&exe, b"new").unwrap();
        std::fs::write(previous(&exe), b"old").unwrap();
        let pending = |tries| Some(Pending { version: "0.2.0".into(), tries });
        assert_eq!(at_start(&exe, None, "0.2.0"), Ok(None));
        assert_eq!(at_start(&exe, pending(0), "0.2.0"), Ok(pending(1)));
        assert_eq!(at_start(&exe, pending(1), "0.2.0"), Ok(pending(2)));
        assert_eq!(std::fs::read(&exe).unwrap(), b"new", "not yet");
        assert_eq!(at_start(&exe, pending(2), "0.2.0"), Err("0.2.0".to_string()));
        assert_eq!(std::fs::read(&exe).unwrap(), b"old", "back to what worked");
        assert_eq!(at_start(&exe, pending(5), "0.1.0"), Ok(None), "it is not running that build");
        let _ = std::fs::remove_dir_all(dir);
    }
}
