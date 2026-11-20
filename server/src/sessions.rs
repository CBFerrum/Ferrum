//! The keys that sign sessions and API tokens (docs/operations.md, "The key that signs sessions"; docs/plan/stateless.md,
//! D5). Through this release `JWT_SECRET`, where the environment (or the file, in development and tests) sets it, signs
//! as before, and tokens it signed name no key, so that an older build still reads them. Without it, the vault's keys
//! sign: each a secret of the platform, `session.<n>`, and a token names the one that signed it (`kid` = n). Only this
//! server verifies these tokens, so a secret key with an id does what a key pair would, with nothing to publish.
//!
//! A key verifies until the longest token it signed has run out: a rotation (`ferrum task secret_set name:jwt_secret`)
//! makes the next key and gives each key before it an end, its rule's `expires_at`, as far ahead as the longest life
//! this server gives a token, and five minutes more. Every replica reads the keys again every 30 seconds, and at once (at
//! most once a second) when a token names a key it does not hold: so a replica signs with the new key within half a
//! minute, and verifies what another signed with it at once. The environment's secret, once copied into the vault
//! (`jwt_secret`), verifies what it signed after the environment stops setting it, until a rotation gives it an end too.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use chrono::{DateTime, Utc};
use ferrum_kernel::vault::{New, Rule, Scope, Vault, Zeroizing};
use ferrum_kernel::{Error, Kernel};
use loco_rs::auth::jwt::{JWT, UserClaims};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// The vault's name of the environment's secret, copied (crate::secrets): it verifies what carries no key id.
pub const COPY: &str = "jwt_secret";

/// The vault's name of the key `n`.
fn name_of(n: u32) -> String {
    format!("session.{n}")
}

/// One key as this replica holds it.
struct Held {
    /// The secret's version in the vault: a key is read again only when it changes.
    version: i32,
    key: Zeroizing<Vec<u8>>,
    /// Until when it verifies; `None` while it may sign.
    until: Option<DateTime<Utc>>,
}

/// The vault's keys this replica holds: by number, 0 for the copy of the environment's secret.
#[derive(Default)]
struct Ring(BTreeMap<u32, Held>);

impl Ring {
    /// The key that signs: the newest of those with no end.
    fn signing(&self) -> Option<(u32, &Held)> {
        self.0.iter().filter(|(n, h)| **n > 0 && h.until.is_none()).max_by_key(|(n, _)| **n).map(|(n, h)| (*n, h))
    }

    /// Key `n`, while it verifies.
    fn verifying(&self, n: u32) -> Option<&Held> {
        self.0.get(&n).filter(|h| h.until.is_none_or(|t| t > Utc::now()))
    }
}

pub struct Sessions {
    /// `JWT_SECRET` as configured: it signs, and verifies what names no key. `None`: nothing sets it.
    configured: Option<Zeroizing<String>>,
    held: RwLock<Arc<Ring>>,
    /// The kernel and the platform's handle, to read the keys again; `None` without the vault.
    vault: Option<(Arc<Kernel>, Vault)>,
    /// The vault read again on tokens' account in the current second ([`Reads`]).
    reads: tokio::sync::Mutex<Reads>,
}

/// The most different key ids the vault is read again for in one second: a token naming a made-up id forces no read per
/// request, and a key another replica has just made is still read for at once.
const UNKNOWN_IDS_A_SECOND: usize = 4;

/// The vault read again for tokens naming keys this replica does not hold, in the current second.
#[derive(Default)]
struct Reads {
    since: Option<Instant>,
    /// The key ids read for since then (`None`: a token naming none, with no secret here to check it).
    ids: Vec<Option<u32>>,
}

impl Reads {
    /// Whether a token naming `kid`, which is not held, has the vault read again at `now`: at once for an id not read for
    /// in this second, while fewer than [`UNKNOWN_IDS_A_SECOND`] ids were; counted before any read.
    fn admit(&mut self, kid: Option<u32>, now: Instant) -> bool {
        if self.since.is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_secs(1)) {
            *self = Reads { since: Some(now), ids: Vec::new() };
        }
        if self.ids.contains(&kid) || self.ids.len() >= UNKNOWN_IDS_A_SECOND {
            return false;
        }
        self.ids.push(kid);
        true
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Sessions {
    /// The keys as this replica starts: `configured` (`JWT_SECRET`, empty: none) and, with the vault open, the vault's.
    /// With nothing configured, the vault's keys sign: its first is made now, once however many replicas start together.
    /// Refused when nothing could sign.
    pub async fn start(configured: &str, vault: Option<(Arc<Kernel>, Vault)>) -> Result<Sessions, String> {
        let s = Sessions {
            configured: Some(Zeroizing::new(configured.trim().to_string())).filter(|c| !c.is_empty()),
            held: RwLock::default(),
            vault,
            reads: Default::default(),
        };
        if let Some((k, v)) = &s.vault {
            s.reload().await.map_err(|e| format!("the keys that sign sessions cannot be read from the vault: {e}"))?;
            if s.configured.is_none() && s.ring().signing().is_none() {
                let first = Rule { reveal: true, ..Default::default() };
                v.adopt(k, Scope::Installation, &name_of(1), New::Random(48), first, "start")
                    .await
                    .map_err(|e| format!("making the key that signs sessions: {e}"))?;
                s.reload().await.map_err(|e| format!("the keys that sign sessions cannot be read from the vault: {e}"))?;
            }
        }
        if s.configured.is_none() && s.ring().signing().is_none() {
            return Err("JWT_SECRET is not set, and the vault is closed: give it `openssl rand -base64 48`, or give the vault a root (docs/operations.md, \"The vault\")".into());
        }
        Ok(s)
    }

    fn ring(&self) -> Arc<Ring> {
        self.held.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// `JWT_SECRET` as configured, for the doctor's look at it; empty when nothing sets it.
    pub fn configured(&self) -> &str {
        self.configured.as_deref().map_or("", |c| c.as_str())
    }

    /// The vault's key that signs, when the configuration sets none: its number.
    pub fn vault_signs(&self) -> Option<u32> {
        if self.configured.is_some() { None } else { self.ring().signing().map(|(n, _)| n) }
    }

    /// Reads the vault's keys again: each one that changed since (or new), and those that are over left out. What is
    /// held stays when the vault cannot be read.
    pub async fn reload(&self) -> ferrum_kernel::Result<()> {
        let Some((k, vault)) = &self.vault else { return Ok(()) };
        let held = self.ring();
        let mut ring = Ring::default();
        for d in vault.list(k, Scope::Installation).await? {
            let n = match d.name.as_str() {
                COPY => 0,
                name => match name.strip_prefix("session.").and_then(|n| n.parse::<u32>().ok()).filter(|n| *n > 0) {
                    Some(n) => n,
                    None => continue,
                },
            };
            let until = d.rule.expires_at;
            if until.is_some_and(|t| t <= Utc::now()) {
                continue;
            }
            let key = match held.0.get(&n).filter(|h| h.version == d.version) {
                Some(h) => h.key.clone(),
                None => vault.reveal(k, Scope::Installation, &d.name).await?,
            };
            ring.0.insert(n, Held { version: d.version, key, until });
        }
        *self.held.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(ring);
        Ok(())
    }

    /// A token for `pid` with `claims`, lasting `ttl` seconds: signed by `JWT_SECRET` where it is set (no key id, as an
    /// older build signs and reads them), else by the vault's key, which it names.
    pub fn sign(&self, ttl: u64, pid: String, claims: Map<String, Value>) -> Result<String, Error> {
        if let Some(secret) = &self.configured {
            return JWT::new(secret).generate_token(ttl, pid, claims).map_err(|e| Error::Internal(format!("token: {e}")));
        }
        let ring = self.ring();
        let (n, held) = ring.signing().ok_or_else(|| Error::Unavailable("no key signs sessions here".into()))?;
        let mut body = claims;
        body.insert("pid".into(), json!(pid));
        body.insert("exp".into(), json!(now().saturating_add(ttl)));
        let header = json!({"typ": "JWT", "alg": "HS512", "kid": n.to_string()});
        let mut token = format!("{}.{}", B64URL.encode(header.to_string()), B64URL.encode(Value::Object(body).to_string()));
        let signature = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA512, &held.key), token.as_bytes());
        token.push('.');
        token.push_str(&B64URL.encode(signature.as_ref()));
        Ok(token)
    }

    /// What `token` says, when a key this server holds signed it and it has not run out. A token that names a key this
    /// replica does not hold has the vault read again first (once a second for each id, a few ids a second at most:
    /// [`Reads`]), so that one signed a moment ago by another replica's new key is taken at once.
    pub async fn validate(&self, token: &str) -> Option<UserClaims> {
        match self.check(token) {
            Checked::Unknown => {
                self.read_again(key_id(token).flatten()).await;
                match self.check(token) {
                    Checked::Good(c) => Some(c),
                    _ => None,
                }
            }
            Checked::Good(c) => Some(c),
            Checked::Bad => None,
        }
    }

    /// [`Sessions::validate`] with the keys held now, without reading the vault (where nothing may wait for it).
    pub fn validate_held(&self, token: &str) -> Option<UserClaims> {
        match self.check(token) {
            Checked::Good(c) => Some(c),
            _ => None,
        }
    }

    async fn read_again(&self, kid: Option<u32>) {
        let mut reads = self.reads.lock().await;
        if !reads.admit(kid, Instant::now()) {
            return;
        }
        if let Err(e) = self.reload().await {
            tracing::warn!("reading the keys that sign sessions again: {e}");
        }
    }

    fn check(&self, token: &str) -> Checked {
        let Some(kid) = key_id(token) else { return Checked::Bad };
        let ring = self.ring();
        match kid {
            // No key named: the configured secret, else its copy in the vault (read again when none is held), as an older
            // build reads it.
            None => {
                let copy = ring.verifying(0).and_then(|h| std::str::from_utf8(&h.key).ok());
                match self.configured.as_deref().map(|c| c.as_str()).or(copy) {
                    Some(secret) => JWT::new(secret).validate(token).map_or(Checked::Bad, |d| Checked::Good(d.claims)),
                    None => Checked::Unknown,
                }
            }
            Some(n) => match ring.verifying(n) {
                Some(held) => verified(&held.key, token).map_or(Checked::Bad, Checked::Good),
                None if n > 0 => Checked::Unknown,
                None => Checked::Bad,
            },
        }
    }
}

enum Checked {
    Good(UserClaims),
    Bad,
    /// It names a key this replica does not hold (yet).
    Unknown,
}

/// The key a token names: `Some(None)` for none, `Some(Some(n))` for `n`, `None` when its header is not one of ours
/// (not HS512, or a key id that is no number).
fn key_id(token: &str) -> Option<Option<u32>> {
    let header = token.split('.').next()?;
    let header: Value = serde_json::from_slice(&B64URL.decode(header).ok()?).ok()?;
    if header.get("alg")?.as_str()? != "HS512" {
        return None;
    }
    match header.get("kid") {
        None => Some(None),
        Some(kid) => Some(Some(kid.as_str()?.parse::<u32>().ok().filter(|n| *n > 0)?)),
    }
}

/// The claims of `token` when `key` signed it (HMAC-SHA512, compared in constant time) and it has not run out.
fn verified(key: &[u8], token: &str) -> Option<UserClaims> {
    let (signed, signature) = token.rsplit_once('.')?;
    let signature = B64URL.decode(signature).ok()?;
    ring::hmac::verify(&ring::hmac::Key::new(ring::hmac::HMAC_SHA512, key), signed.as_bytes(), &signature).ok()?;
    let claims: Value = serde_json::from_slice(&B64URL.decode(signed.split_once('.')?.1).ok()?).ok()?;
    if claims.get("exp")?.as_u64()? < now() {
        return None;
    }
    serde_json::from_value(claims).ok()
}

/// How long the longest token this server gives lasts, with five minutes for replicas still signing with a key that
/// was just replaced: what a replaced key verifies for.
pub fn horizon(session_days: u32, jwt_expiration: u64) -> Duration {
    Duration::from_secs(u64::from(session_days.clamp(1, 400)) * 86_400).max(Duration::from_secs(jwt_expiration)).max(Duration::from_secs(3600))
        + Duration::from_secs(300)
}

/// `ferrum task secret_set name:jwt_secret` (and the operator's route): a new key signs from now on, and every key
/// before it (with the environment's copy, `retire_copy`, once no replica's environment sets `JWT_SECRET`) verifies
/// for `horizon` more. Two at once make one new key. Its number.
pub async fn rotate(k: &Kernel, vault: &Vault, horizon: Duration, retire_copy: bool) -> ferrum_kernel::Result<u32> {
    let listed = vault.list(k, Scope::Installation).await?;
    let numbered = |name: &str| name.strip_prefix("session.").and_then(|n| n.parse::<u32>().ok()).filter(|n| *n > 0);
    let next = listed.iter().filter_map(|d| numbered(&d.name)).max().unwrap_or(0) + 1;
    let until = Utc::now() + chrono::Duration::from_std(horizon).map_err(Error::internal)?;
    vault.adopt(k, Scope::Installation, &name_of(next), New::Random(48), Rule { reveal: true, ..Default::default() }, "rotation").await?;
    for d in &listed {
        let retire = numbered(&d.name).is_some_and(|n| n < next) || (retire_copy && d.name == COPY);
        if !retire {
            continue;
        }
        match d.rule.expires_at {
            // Over: gone.
            Some(at) if at <= Utc::now() => {
                vault.remove(k, Scope::Installation, &d.name).await?;
            }
            Some(_) => {}
            None => {
                let key = vault.reveal(k, Scope::Installation, &d.name).await?;
                vault.set(k, Scope::Installation, &d.name, New::Value(key), Rule { reveal: true, expires_at: Some(until), ..Default::default() }).await?;
            }
        }
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(keys: &[(u32, &[u8], Option<DateTime<Utc>>)]) -> Sessions {
        let ring = Ring(keys.iter().map(|(n, k, until)| (*n, Held { version: 1, key: Zeroizing::new(k.to_vec()), until: *until })).collect());
        Sessions { configured: None, held: RwLock::new(Arc::new(ring)), vault: None, reads: Default::default() }
    }

    fn claims() -> Map<String, Value> {
        json!({"tenant": "t"}).as_object().cloned().unwrap()
    }

    /// session-key-race: each key id not held has the vault read at once, once a second; a made-up id forces no read per
    /// request, and only a few different ids are read for in one second.
    #[test]
    fn an_unknown_key_is_read_for_at_once_and_a_made_up_one_cannot_force_reads() {
        let mut reads = Reads::default();
        let t = Instant::now();
        assert!(reads.admit(Some(4000), t), "a first unknown id");
        assert!(!reads.admit(Some(4000), t), "the same id again, in the same second");
        assert!(reads.admit(Some(4), t + Duration::from_millis(10)), "a new key another replica just made: read at once");
        assert!(reads.admit(Some(5), t) && reads.admit(None, t), "up to the cap of different ids");
        let read = (6..100).filter(|n| reads.admit(Some(*n), t + Duration::from_millis(500))).count();
        assert_eq!(read, 0, "past the cap, no read in that second, however many ids are made up");
        assert!(reads.admit(Some(4000), t + Duration::from_secs(1)), "the next second, again");
    }

    #[test]
    fn a_token_names_its_key_and_only_that_key_verifies_it() {
        let gone = Utc::now() - chrono::Duration::seconds(1);
        let soon = Utc::now() + chrono::Duration::hours(1);
        let s = ring(&[(1, b"first key of enough length....", Some(soon)), (2, b"second key of enough length...", None), (3, b"over", Some(gone))]);
        let token = s.sign(60, "pid".into(), claims()).unwrap();
        assert_eq!(key_id(&token), Some(Some(2)), "the newest key with no end signs");
        assert_eq!(s.validate_held(&token).unwrap().pid, "pid");
        // Another key's id on the same claims and signature: refused, as is a key that is over or nobody's.
        let forge = |kid: &str| {
            let (_, rest) = token.split_once('.').unwrap();
            format!("{}.{rest}", B64URL.encode(json!({"typ": "JWT", "alg": "HS512", "kid": kid}).to_string()))
        };
        for kid in ["1", "3", "9", "0", "two", "-1"] {
            assert!(s.validate_held(&forge(kid)).is_none(), "kid {kid}");
        }
        let none = format!("{}.{}", B64URL.encode(json!({"typ": "JWT", "alg": "none", "kid": "2"}).to_string()), token.split_once('.').unwrap().1);
        assert!(s.validate_held(&none).is_none(), "alg none");
        let mut cut = token.clone();
        cut.pop();
        assert!(s.validate_held(&cut).is_none(), "a signature changed");
        let old = ring(&[(1, b"first key of enough length....", Some(soon))]);
        let by_first = Sessions { configured: None, ..old }.sign(60, "p".into(), claims());
        assert!(by_first.is_err(), "a key with an end signs nothing");
        // The first key still verifies what it signed until its end.
        let first = ring(&[(1, b"first key of enough length....", None)]).sign(60, "p".into(), claims()).unwrap();
        assert!(s.validate_held(&first).is_some(), "within its end");
        let s_after = ring(&[(1, b"first key of enough length....", Some(gone)), (2, b"second key of enough length...", None)]);
        assert!(s_after.validate_held(&first).is_none(), "past its end");
        // Run out: refused.
        let expired = ring(&[(2, b"second key of enough length...", None)]).sign(0, "p".into(), claims()).unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        assert!(s.validate_held(&expired).is_none());
    }

    #[test]
    fn what_the_environments_secret_signed_needs_no_key_id() {
        let secret = "c2Vzc2lvbnMtdGVzdC1zZWNyZXQtMDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";
        let legacy = JWT::new(secret).generate_token(60, "pid".into(), claims()).unwrap();
        assert_eq!(key_id(&legacy), Some(None));
        let copy = ring(&[(0, secret.as_bytes(), None), (1, b"a key of the vault of length..", None)]);
        assert!(copy.validate_held(&legacy).is_some(), "its copy in the vault verifies it");
        assert!(ring(&[(1, b"a key of the vault of length..", None)]).validate_held(&legacy).is_none(), "nothing else does");
        let configured = Sessions { configured: Some(Zeroizing::new(secret.into())), ..ring(&[]) };
        let token = configured.sign(60, "pid".into(), claims()).unwrap();
        assert_eq!(key_id(&token), Some(None), "where it is set it signs, naming no key");
        assert!(JWT::new(secret).validate(&token).is_ok(), "as an older build reads it");
        assert_eq!(horizon(14, 28_800), Duration::from_secs(14 * 86_400 + 300));
        assert_eq!(horizon(0, 60), Duration::from_secs(86_400 + 300), "a day at least");
    }
}
