//! The platform's own keys for what it keys or seals outside the vault ([`Keyed`]): each derived from the platform's
//! key in the vault for one purpose, in the workspace's scope where the thing belongs to a workspace, and in the
//! installation's for processes' payloads, whose sealing is not told which run, so which workspace, a payload is for.
//! Every version of the platform's key that derived one is kept (`outside`), so what a version made is still checked or
//! opened after a rotation; what is made after it uses the new one.
//!
//! With the vault closed, nothing here falls back to anything else: what needs the vault's keys is refused, saying why.

use super::{Scope, Vault};
use crate::Kernel;
use crate::db::with_retry;
use crate::error::{Error, Result};
use crate::secret::SecretKey;
use dashmap::DashMap;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// What the platform keys or seals with a key of its own from the vault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Keyed {
    /// A promotional message's opt-out link (a workspace's): checked under every version kept, so a link outlives a
    /// rotation.
    OptOut,
    /// What an erased person's opt-out is kept as (a workspace's): checked under every version kept, and never made
    /// again (the address is gone).
    ErasedOptOut,
    /// A sign-in to an account: its `state` and its verifier while it waits (a workspace's; ten minutes).
    SignInState,
    /// A public address of a file (a workspace's): checked under every version kept, until it ends.
    FileAddress,
    /// A connector's account, known by a fingerprint (a workspace's): connections saved after a rotation are told apart
    /// from those saved before, until those are saved again.
    ConnectorAccount,
    /// A receipt's digest of a secret parameter (a workspace's): a request repeated across a rotation is told apart
    /// from the first, and answered as a new one.
    Receipt,
    /// A message's secret values while it waits to go (a workspace's).
    Message,
    /// A durable process's payloads (the installation's): each names the version that sealed it.
    Process,
}

impl Keyed {
    fn purpose(self) -> &'static str {
        match self {
            Keyed::OptOut => "opt-out",
            Keyed::ErasedOptOut => "erased-opt-out",
            Keyed::SignInState => "sign-in-state",
            Keyed::FileAddress => "file-address",
            Keyed::ConnectorAccount => "connector-account",
            Keyed::Receipt => "receipt",
            Keyed::Message => "message",
            Keyed::Process => "process",
        }
    }

    /// What needs it, in a refusal.
    fn what(self) -> &'static str {
        match self {
            Keyed::OptOut => "a promotional message's opt-out link",
            Keyed::ErasedOptOut => "erasing someone who opted out",
            Keyed::SignInState => "signing in to an account",
            Keyed::FileAddress => "a public address of a file",
            Keyed::ConnectorAccount => "a connector's account",
            Keyed::Receipt => "a secret parameter's receipt",
            Keyed::Message => "a message with secret values",
            Keyed::Process => "a process's payloads",
        }
    }
}

/// How long a replica keeps the platform key's current version as it last read it: a rotation reaches it within this.
const CURRENT_FOR: Duration = Duration::from_secs(30);

/// The platform's derived keys this process holds, by scope, purpose and version (a version's key never changes), and
/// each scope's current version as last read.
#[derive(Default)]
pub(crate) struct Ring {
    keys: DashMap<(Uuid, Keyed, i32), SecretKey>,
    current: DashMap<Uuid, (i32, Instant)>,
}

fn key_of(d: &super::Derived) -> Result<SecretKey> {
    let bytes: [u8; 32] = d.bytes().try_into().map_err(|_| Error::Internal("a derived key is not 32 bytes".into()))?;
    Ok(SecretKey::new(bytes))
}

impl Kernel {
    /// Refused, saying why, with the vault closed: what `what` needs is made with the vault's keys and nothing else.
    pub(crate) fn keyed_ready(&self, what: Keyed) -> Result<()> {
        self.vault_open(what.what())
    }

    /// The platform's key for `what` in `scope` at `version`.
    async fn key_at(&self, scope: Scope, what: Keyed, version: i32) -> Result<SecretKey> {
        if let Some(key) = self.keyed.keys.get(&(scope.id(), what, version)) {
            return Ok(key.clone());
        }
        let key = key_of(&Vault::kernel("platform").derive_at(self, scope, what.purpose(), version).await?)?;
        self.keyed.keys.insert((scope.id(), what, version), key.clone());
        Ok(key)
    }

    /// The key `what` is made with in `scope`, and the version of the platform's key it came from.
    pub(crate) async fn making_key(&self, scope: Scope, what: Keyed) -> Result<(i32, SecretKey)> {
        self.keyed_ready(what)?;
        if let Some((version, at)) = self.keyed.current.get(&scope.id()).map(|c| *c)
            && at.elapsed() < CURRENT_FOR
        {
            return Ok((version, self.key_at(scope, what, version).await?));
        }
        let derived = Vault::kernel("platform").derive(self, scope, what.purpose()).await?;
        let key = key_of(&derived)?;
        self.keyed.current.insert(scope.id(), (derived.version(), Instant::now()));
        self.keyed.keys.insert((scope.id(), what, derived.version()), key.clone());
        Ok((derived.version(), key))
    }

    /// Every key that may have made `what` in `scope`, to check or open it: the platform's, a version of its key each,
    /// newest first.
    pub(crate) async fn checking_keys(&self, scope: Scope, what: Keyed) -> Result<Vec<(i32, SecretKey)>> {
        self.keyed_ready(what)?;
        let versions: Vec<i32> = with_retry(|| async {
            let mut tx = self.db.vault_tx(scope.tenant()).await?;
            let v = sqlx::query_scalar("select version from vault.key where scope = $1 and owner = 'platform' order by version desc")
                .bind(scope.id())
                .fetch_all(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok(v)
        })
        .await?;
        let mut out = Vec::new();
        for version in versions {
            out.push((version, self.key_at(scope, what, version).await?));
        }
        Ok(out)
    }
}

// What the platform seals outside the vault for a while (a message's secret values while it waits, a sign-in's
// verifier): under the platform's key in its scope ([`Vault::seal`], which names the version that sealed it, so it opens
// after a rotation).

impl Kernel {
    /// `plain` sealed in `scope` for `context` under the platform's key. Refused with the vault closed.
    pub(crate) async fn seal_for_platform(&self, scope: Scope, context: &str, plain: &str, what: Keyed) -> Result<String> {
        self.keyed_ready(what)?;
        Vault::kernel("platform").seal(self, scope, context, plain.as_bytes()).await
    }

    /// What [`Kernel::seal_for_platform`] sealed.
    pub(crate) async fn open_for_platform(&self, scope: Scope, context: &str, stored: &str, what: Keyed) -> Result<String> {
        self.keyed_ready(what)?;
        let plain = Vault::kernel("platform").open(self, scope, context, stored).await?;
        String::from_utf8(plain.to_vec()).map_err(|_| Error::Internal("what the vault sealed is not text".into()))
    }
}

// Plugins' settings: the plugin host (platform code, holding the platform's handle) keeps each sealed under the plugin's
// own key (`plugin:<name>`, data under the owner key: no secret of the vault is revealed).

/// How the plugin host keeps a setting's value: `{"$vault": …}`.
pub(crate) const PLUGIN_VAULT: &str = "$vault";

/// What a plugin's setting is sealed for: its name, or a digest of a name that cannot be a context.
fn setting_context(key: &str) -> String {
    if (1..=150).contains(&key.len()) && !key.chars().any(char::is_control) {
        format!("plugin-setting/{key}")
    } else {
        use sha2::Digest;
        format!("plugin-setting#{}", hex::encode(sha2::Sha256::digest(key.as_bytes())))
    }
}

impl Kernel {
    /// A plugin's setting `key` holding `value`, as its host stores it. Refused with the vault closed: a setting is
    /// never stored plain.
    async fn keep_setting(&self, tenant: Uuid, plugin: &str, key: &str, value: &Value) -> Result<Value> {
        self.vault_open("a plugin's setting")?;
        let sealed =
            Vault::kernel(&format!("plugin:{plugin}")).seal(self, Scope::Workspace(tenant), &setting_context(key), value.to_string().as_bytes()).await?;
        Ok(json!({ PLUGIN_VAULT: sealed }))
    }

    /// A plugin's setting as its host reads it, from what is stored. `None`: it does not open here (moved to another
    /// setting, or not kept as the host keeps settings), which the host leaves out.
    async fn setting(&self, tenant: Uuid, plugin: &str, key: &str, stored: &Value) -> Result<Option<Value>> {
        let Some(sealed) = stored.get(PLUGIN_VAULT).and_then(Value::as_str) else { return Ok(None) };
        Ok(match Vault::kernel(&format!("plugin:{plugin}")).open(self, Scope::Workspace(tenant), &setting_context(key), sealed).await {
            Ok(plain) => serde_json::from_slice(&plain).ok(),
            Err(Error::Invalid(_)) => None,
            Err(e) => return Err(e),
        })
    }
}

impl Vault {
    /// The platform's handle only (what the server alone holds): the verbs it reaches check the vault where they need it.
    fn platform_handle(&self, k: &Kernel) -> Result<()> {
        if self.given(k)? != "platform" {
            return Err(Error::Forbidden("only the platform's handle does this".into()));
        }
        Ok(())
    }

    /// Plugin `plugin`'s setting `key` holding `value`, as the plugin host stores it (in `config`): sealed under the
    /// plugin's own key. Only the platform's handle (the host's).
    pub async fn keep_plugin_setting(&self, k: &Kernel, tenant: Uuid, plugin: &str, key: &str, value: &Value) -> Result<Value> {
        self.platform_handle(k)?;
        k.keep_setting(tenant, plugin, key, value).await
    }

    /// Plugin `plugin`'s setting `key` as the host gives it to the plugin, from what `config` stores; `None` when it
    /// does not open here. Only the platform's handle (the host's).
    pub async fn plugin_setting(&self, k: &Kernel, tenant: Uuid, plugin: &str, key: &str, stored: &Value) -> Result<Option<Value>> {
        self.platform_handle(k)?;
        k.setting(tenant, plugin, key, stored).await
    }

    /// A durable process's payload sealed for `aad` (crates/process), and the version of the platform's key of the
    /// installation that sealed it. Only the platform's handle.
    pub async fn seal_process(&self, k: &Kernel, aad: &str, plain: &str) -> Result<(i32, String)> {
        self.platform_handle(k)?;
        let (version, key) = k.making_key(Scope::Installation, Keyed::Process).await?;
        Ok((version, key.seal(aad, plain)?))
    }

    /// What [`Vault::seal_process`] sealed with `version`. Only the platform's handle.
    pub async fn open_process(&self, k: &Kernel, aad: &str, version: i32, sealed: &str) -> Result<String> {
        self.platform_handle(k)?;
        k.keyed_ready(Keyed::Process)?;
        k.key_at(Scope::Installation, Keyed::Process, version).await?.open(aad, sealed)
    }
}

impl Kernel {
    /// Whether a message's secret values can be sealed here while they wait (the vault is open): what an app asks before
    /// it makes something it sends that way.
    pub fn seals_secret_values(&self) -> bool {
        self.db.vault.installation().is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_purpose_is_its_own() {
        let all = [
            Keyed::OptOut,
            Keyed::ErasedOptOut,
            Keyed::SignInState,
            Keyed::FileAddress,
            Keyed::ConnectorAccount,
            Keyed::Receipt,
            Keyed::Message,
            Keyed::Process,
        ];
        let purposes: std::collections::BTreeSet<&str> = all.iter().map(|k| k.purpose()).collect();
        assert_eq!(purposes.len(), all.len());
        assert_eq!(setting_context("api_key"), "plugin-setting/api_key");
        assert!(setting_context(&"x".repeat(200)).starts_with("plugin-setting#") && setting_context("a\nb").starts_with("plugin-setting#"));
    }
}
