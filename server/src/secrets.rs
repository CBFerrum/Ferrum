//! The platform's own secrets (docs/operations.md, "The platform's secrets"; docs/plan/stateless.md, D5). Each the
//! registry marks `vault:` belongs to the owner `platform` at the installation's scope, under its name there. Through
//! this release the environment, when it sets one, still wins, so that a rollback behaves; otherwise the vault decides;
//! with the vault closed everything is as before. What a replica's environment sets and the vault lacks is stored there
//! once, as the replica starts to serve (`adopt`). Three kinds:
//!
//! * `session`: `JWT_SECRET` (crate::sessions);
//! * `compared`: tokens the server only compares (bootstrap, metrics, operator). The vault keeps a digest of each, which
//!   is not the token; a caller's token is checked against it there (HMAC, in constant time), its version before too
//!   while a rotation's overlap lasts. They apply at once.
//! * `library`: what the server hands a library (the mail server's password, the object store's keys, the model
//!   provider's key, the audit sink's key). Only the platform reveals them, at start; a change applies at the next start.
//!   One that is sent to an address (`sends:` in the registry) is bound to the host it was set for, and given to no
//!   other.

use crate::settings::{Def, Held, Snapshot, registry};
use crate::state::AppState;
use base64::Engine as _;
use ferrum_kernel::doctor::Finding;
use ferrum_kernel::vault::{Described, New, Rule, Scope, Vault, Zeroizing};
use ferrum_kernel::{Db, Error, HookRegistry, Result};
use loco_rs::config::Config;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The registry's secrets that the vault holds for the platform.
pub fn declared() -> impl Iterator<Item = &'static Def> {
    registry().iter().filter(|d| d.vault.is_some())
}

/// The platform secret called `name` (its key or its variable).
pub fn find(name: &str) -> Option<&'static Def> {
    crate::settings::find(name).filter(|d| d.vault.is_some())
}

/// What the configuration sets for `def` (the environment's value, or the file's in development and tests), when it
/// is something.
pub fn configured(def: &Def, config: &Config) -> Option<String> {
    let text = match def.path.as_deref() {
        Some("auth.jwt.secret") => config.auth.as_ref()?.jwt.as_ref()?.secret.clone(),
        Some("mailer.smtp.auth.password") => config.mailer.as_ref()?.smtp.as_ref()?.auth.as_ref()?.password.clone(),
        Some(p) => config.settings.as_ref()?.pointer(&format!("/{}", p.strip_prefix("settings.")?.replace('.', "/")))?.as_str()?.to_string(),
        None => std::env::var(&def.env).ok()?,
    };
    Some(text).filter(|t| !t.trim().is_empty())
}

/// The host a secret is sent to, as `snapshot`'s settings say (the address marked `sends:` it in the registry): the mail
/// server's for its password, the model provider's for its key. `None`: it goes to no address of a setting.
pub fn sent_to(key: &str, snapshot: &Snapshot) -> Option<String> {
    let setting = |k: &str| snapshot.get(k).1.map(str::to_string).or_else(|| crate::settings::find(k).map(|d| d.default.clone())).unwrap_or_default();
    match key {
        "mailer_password" => Some(setting("mailer_host").trim().to_ascii_lowercase()).filter(|h| !h.is_empty()),
        "assistant_api_key" => reqwest::Url::parse(setting("assistant_url").trim()).ok()?.host_str().map(str::to_ascii_lowercase),
        _ => None,
    }
}

/// What the vault gave the start for the libraries that read no configuration of ours: the object store's keys and the
/// model provider's key, where the environment sets none. Never shown.
#[derive(Clone, Default)]
pub struct Revealed {
    values: BTreeMap<String, Zeroizing<String>>,
    /// Whether the mail password came from the vault (the mail sender is made again with it).
    pub mail: bool,
}

impl std::fmt::Debug for Revealed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Revealed").field("names", &self.values.keys().collect::<Vec<_>>()).field("mail", &self.mail).finish()
    }
}

impl Revealed {
    /// The vault's value of the setting `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(|v| v.as_str())
    }
}

/// The platform's library secrets the configuration does not set, read from the vault as the server starts, before
/// its kernel boots: the mail password and the audit sink's key put where the environment's would be in `config`; the
/// rest returned. One bound to a host other than the one `snapshot` sends it to is left out, saying why. Nothing while
/// the vault is not open.
pub async fn at_start(db: &Db, hooks: &HookRegistry, platform: &Vault, config: &mut Config, snapshot: &Snapshot) -> std::result::Result<Revealed, String> {
    let mut out = Revealed::default();
    for def in declared().filter(|d| d.vault == Some(Held::Library)) {
        if configured(def, config).is_some() {
            continue;
        }
        let found = platform.reveal_at_start(db, hooks, &def.key).await.map_err(|e| format!("{} in the vault: {e}", def.key))?;
        let Some((value, rule)) = found else { continue };
        let text = Zeroizing::new(String::from_utf8(value.to_vec()).map_err(|_| format!("{} in the vault is not text", def.key))?);
        if let Some(host) = sent_to(&def.key, snapshot)
            && !rule.hosts.contains(&host)
        {
            tracing::error!(
                secret = %def.key,
                "{} in the vault was set for {}, and is sent to {host} now: it is not given to {host}. Set it again for this host (ferrum task secret_set name:{})",
                def.key,
                rule.hosts.join(", "),
                def.key
            );
            continue;
        }
        match def.key.as_str() {
            "mailer_password" => {
                if let Some(smtp) = config.mailer.as_mut().and_then(|m| m.smtp.as_mut()) {
                    let auth = smtp.auth.get_or_insert_with(|| loco_rs::config::MailerAuth { user: String::new(), password: String::new() });
                    auth.password = text.to_string();
                    out.mail = true;
                }
            }
            "audit_sink_secret" => {
                if let Some(s) = config.settings.as_mut().and_then(Value::as_object_mut) {
                    s.insert("audit_sink_secret".into(), json!(text.as_str()));
                }
            }
            _ => {
                out.values.insert(def.key.clone(), text);
            }
        }
    }
    Ok(out)
}

/// The form a secret is kept in, and its rule: a token's digest, never the token; anything else as it is, revealed to
/// the platform alone, bound to the host it is sent to.
fn kept(def: &Def, value: &str, snapshot: &Snapshot) -> (New, Rule) {
    match def.vault {
        Some(Held::Compared) => (New::Value(digest(value)), Rule::default()),
        _ => {
            let hosts = sent_to(&def.key, snapshot).into_iter().collect();
            (New::Value(Zeroizing::new(value.as_bytes().to_vec())), Rule { reveal: true, hosts, ..Default::default() })
        }
    }
}

/// R1: each platform secret `env` sets and the vault does not hold yet, stored there once, as the environment's
/// (replicas starting together store it once: the vault's lock on each secret), recorded without its value. The keys
/// stored.
pub async fn adopt(st: &AppState, env: &(dyn Fn(&str) -> Option<String> + Sync)) -> Vec<String> {
    if !st.vault_open {
        return Vec::new();
    }
    let snapshot = &st.live_settings.start.snapshot;
    let mut stored = Vec::new();
    for def in declared() {
        let Some(text) = env(&def.env).map(Zeroizing::new).filter(|v| !v.trim().is_empty()) else { continue };
        let (value, rule) = kept(def, text.trim(), snapshot);
        match st.vault.adopt(&st.kernel, Scope::Installation, &def.key, value, rule, "environment").await {
            Ok(true) => stored.push(def.key.clone()),
            Ok(false) => {}
            Err(e) => tracing::warn!(secret = %def.key, "storing the environment's secret in the vault (the next start tries again): {e}"),
        }
    }
    stored
}

/// What a token is kept as: its SHA-256, which is not the token.
fn digest(token: &str) -> Zeroizing<Vec<u8>> {
    Zeroizing::new(ring::digest::digest(&ring::digest::SHA256, token.as_bytes()).as_ref().to_vec())
}

/// What the vault checks a given token by: an HMAC of a fixed message under the token's digest, which matches the one it
/// keeps only for the same token.
const PROOF: &[u8] = b"ferrum: a token the server compares";

fn proof(given: &str) -> Vec<u8> {
    ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &digest(given)), PROOF).as_ref().to_vec()
}

/// What a caller's token is to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    /// Neither the configuration nor the vault holds one: the route answers as it does without one.
    NotSet,
    Good,
    Bad,
}

/// Whether `given` is the platform's token `key` (`bootstrap_token`, `metrics_token`, `operator_token`): the
/// configuration's, when it sets one, compared in constant time; else the vault's, its current version or the one before
/// while a rotation's overlap lasts. A vault that cannot be asked refuses.
pub async fn token(st: &AppState, key: &str, configured: &str, given: &str) -> Token {
    if !configured.is_empty() {
        return if crate::controllers::system::same_secret(given, configured) { Token::Good } else { Token::Bad };
    }
    if !st.vault_open {
        return Token::NotSet;
    }
    match st.vault.verify(&st.kernel, Scope::Installation, key, PROOF, &proof(given)).await {
        Ok(true) => Token::Good,
        Ok(false) => Token::Bad,
        Err(Error::NotFound(_)) => Token::NotSet,
        Err(e) => {
            tracing::warn!(token = key, "checking a token against the vault: {e}");
            Token::Bad
        }
    }
}

/// Where each of the platform's secrets comes from now: `environment`, `vault` or `default` (none), with the vault's
/// description of it.
pub async fn sources(st: &AppState) -> BTreeMap<String, (&'static str, Option<Described>)> {
    let held: BTreeMap<String, Described> = if st.vault_open {
        st.vault.list(&st.kernel, Scope::Installation).await.unwrap_or_default().into_iter().map(|d| (d.name.clone(), d)).collect()
    } else {
        BTreeMap::new()
    };
    let config = st.live_settings.config();
    declared()
        .map(|d| {
            let source = match (configured(d, config).is_some(), d.vault == Some(Held::Session) && st.sessions.vault_signs().is_some(), held.get(&d.key)) {
                (true, _, _) => "environment",
                (false, true, _) | (false, false, Some(_)) => "vault",
                _ => "default",
            };
            (d.key.clone(), (source, held.get(&d.key).cloned()))
        })
        .collect()
}

/// What the doctor says of the platform's secrets: which the environment still decides, and that the vault is closed;
/// and, in `out`, the settings' own findings of the secrets the vault holds (what signs sessions, the metrics and
/// bootstrap tokens) as the vault has them.
pub async fn findings(st: &AppState, out: &mut Vec<Finding>) {
    if !st.vault_open {
        out.push(Finding::warning(
            "secrets.vault",
            "the vault is closed: the platform's secrets come from the environment alone, and none is kept in the vault",
            "Give the vault a root (a password in DATABASE_URL, a key file, or a key-management service); docs/operations.md, \"The vault\".",
        ));
        return;
    }
    let sources = sources(st).await;
    let from_vault = |key: &str| sources.get(key).is_some_and(|(s, _)| *s == "vault");
    let mut replace = |check: &str, f: Finding| match out.iter_mut().find(|x| x.check == check) {
        Some(x) => *x = f,
        None => out.push(f),
    };
    if let Some(n) = st.sessions.vault_signs() {
        replace("secrets.jwt", Finding::ok("secrets.jwt", format!("sessions are signed with the vault's key {n}; JWT_SECRET is not set")));
    }
    if from_vault("metrics_token") {
        replace("secrets.metrics", Finding::ok("secrets.metrics", "/metrics needs a token (the vault's)"));
    }
    if from_vault("bootstrap_token") {
        replace(
            "secrets.bootstrap",
            Finding::warning(
                "secrets.bootstrap",
                "anyone with the bootstrap token can create workspaces",
                "Remove it (ferrum task secret_remove name:bootstrap_token) unless you are creating workspaces over HTTP.",
            ),
        );
    }
    let decided: Vec<&str> = sources.iter().filter(|(_, (s, _))| *s == "environment").map(|(k, _)| k.as_str()).collect();
    out.push(if decided.is_empty() {
        Finding::ok("secrets.environment", "the vault decides every one of the platform's secrets that is set")
    } else {
        Finding::ok(
            "secrets.environment",
            format!(
                "the environment still decides {} of the platform's secrets: {}. Each is in the vault too once a replica has started with it; take it out of the environment then (docs/operations.md, \"The platform's secrets\")",
                decided.len(),
                decided.join(", ")
            ),
        )
    });
}
