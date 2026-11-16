//! The platform's own keys for what it keys or seals outside the vault, and the old master key's end (docs/plan/
//! stateless.md, D5, "The old key's last uses"; docs/operations.md, "Retiring the old key").
//!
//! What the old key (`FERRUM_SECRET_KEY`) keyed after the stores moved has a key of its own here ([`Keyed`]): derived
//! from the platform's key in the vault for one purpose, in the workspace's scope where the thing belongs to a
//! workspace, and in the installation's for processes' payloads, whose sealing is not told which run, so which
//! workspace, a payload is for. Every version of the platform's key that derived one is kept (`outside`), so what a
//! version made is still checked or opened after a rotation; what is made after it uses the new one.
//!
//! The previous build checks and opens these things with the old key alone. So while the old key is configured and
//! such a build may still start, what is made is made with the old key, as before, and an upgrade rolls back with
//! every link and sign-in it gave out still good. Once older builds are refused ([`OLD_KEY_FLOOR`]: `ferrum task
//! legacy_key retire:yes`, or the first thing made here without the old key), what is made is made with the vault's
//! keys. Whatever the old key made is checked or opened with it for good: the environment's while it is set, and the
//! copy adopted into the vault ([`at_boot`]) at the first start that has both the vault open and the key configured,
//! each key once and race-safe on replicas starting together (the retired keys too).
//!
//! With the vault closed, nothing here falls back to anything else: what needs the vault's keys is refused, saying why.

use super::secrets::{opened, rows};
use super::{New, Rule, Scope, Vault};
use crate::db::{Db, with_retry};
use crate::error::{Error, Result};
use crate::secret::SecretKey;
use crate::window::Floor;
use crate::{Kernel, KernelSettings};
use dashmap::DashMap;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Keeps out the builds that check what the platform makes with the old key only, once it makes it with the vault's.
pub const OLD_KEY_FLOOR: Floor = Floor {
    feature: "old-key-retired",
    since: "2026-11-15T20:49:40Z",
    what: "checks opt-out links, sign-ins to accounts, public addresses of files, receipts, waiting messages and processes' payloads \
           with FERRUM_SECRET_KEY only, and does not start without it",
    temporary: false,
};

/// How long a replica keeps the platform key's current version as it last read it: a rotation reaches it within this.
const CURRENT_FOR: Duration = Duration::from_secs(30);

/// The platform's derived keys this process holds, by scope, purpose and version (a version's key never changes), each
/// scope's current version as last read, and whether builds that check with the old key only are refused here.
#[derive(Default)]
pub(crate) struct Ring {
    keys: DashMap<(Uuid, Keyed, i32), SecretKey>,
    current: DashMap<Uuid, (i32, Instant)>,
    retired: AtomicBool,
}

impl Ring {
    pub(crate) fn new(retired: bool) -> Ring {
        Ring { retired: AtomicBool::new(retired), ..Default::default() }
    }
}

fn key_of(d: &super::Derived) -> Result<SecretKey> {
    let bytes: [u8; 32] = d.bytes().try_into().map_err(|_| Error::Internal("a derived key is not 32 bytes".into()))?;
    Ok(SecretKey::new(bytes))
}

impl Kernel {
    /// The environment's old key (`FERRUM_SECRET_KEY`), for what is still sealed with it beside the vault: what the
    /// previous build reads.
    pub(crate) fn env_key(&self) -> Option<&SecretKey> {
        self.env_key.as_ref()
    }

    /// The old key, while what an older build checks is still made with it: configured, and older builds not refused.
    fn old_making(&self) -> Option<&SecretKey> {
        self.env_key().filter(|_| !self.keyed.retired.load(Ordering::Relaxed))
    }

    /// Refused, saying so, with the vault closed: nothing falls back to the old key.
    fn keyed_open(&self, what: Keyed) -> Result<()> {
        self.db.vault.installation().map(|_| ()).map_err(|e| Error::Unavailable(format!("{} needs the vault, and {e}", what.what())))
    }

    /// From the first thing made with the vault's keys: no build that checks with the old key only starts again.
    async fn older_builds_out(&self) -> Result<()> {
        if !self.keyed.retired.load(Ordering::Relaxed) {
            self.require_floor(&OLD_KEY_FLOOR, "").await?;
            self.keyed.retired.store(true, Ordering::Relaxed);
            tracing::info!(
                "the platform makes links, sign-ins and receipts with the vault's keys from now on: builds that check with FERRUM_SECRET_KEY only are refused"
            );
        }
        Ok(())
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

    /// The key `what` is made with in `scope`, and the version of the platform's key it came from (none: the old key's).
    pub(crate) async fn making_key(&self, scope: Scope, what: Keyed) -> Result<(Option<i32>, SecretKey)> {
        if let Some(key) = self.old_making() {
            return Ok((None, key.clone()));
        }
        self.keyed_open(what)?;
        self.older_builds_out().await?;
        if let Some((version, at)) = self.keyed.current.get(&scope.id()).map(|c| *c)
            && at.elapsed() < CURRENT_FOR
        {
            return Ok((Some(version), self.key_at(scope, what, version).await?));
        }
        let derived = Vault::kernel("platform").derive(self, scope, what.purpose()).await?;
        let key = key_of(&derived)?;
        self.keyed.current.insert(scope.id(), (derived.version(), Instant::now()));
        self.keyed.keys.insert((scope.id(), what, derived.version()), key.clone());
        Ok((Some(derived.version()), key))
    }

    /// Every key that may have made `what` in `scope`, to check or open it: the platform's, a version of its key each,
    /// newest first; then the old keys (the environment's, and those adopted into the vault), with no version.
    pub(crate) async fn checking_keys(&self, scope: Scope, what: Keyed) -> Result<Vec<(Option<i32>, SecretKey)>> {
        let mut out = Vec::new();
        match self.db.vault.installation() {
            Ok(_) => {
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
                for version in versions {
                    out.push((Some(version), self.key_at(scope, what, version).await?));
                }
            }
            // While the old key makes them, the old keys check them as before.
            Err(_) if self.old_making().is_some() => {}
            Err(e) => return Err(Error::Unavailable(format!("{} needs the vault, and {e}", what.what()))),
        }
        out.extend(self.old_keys.iter().map(|k| (None, k.clone())));
        Ok(out)
    }
}

// The old key's end. Each key the environment configures is adopted into the vault once, as a secret of the platform's
// in the installation's scope (`legacy.key.<id>`), used and never revealed: what the old key made keeps being checked
// and opened with it after the environment no longer has it. The key the environment last had as its current one is
// noted (`legacy_key_use`, `current key`): the previous build gives its apps that still read the old key that one, from
// the vault's copy, once it is gone from the environment.

/// The adopted keys' names: `legacy.key.<id>`. The vault's verbs refuse them (secrets.rs, `check_name`).
pub(super) const ADOPTED: &str = "legacy.key.";

/// The old keys this process holds, worked out as the kernel boots ([`at_boot`]).
pub(crate) struct OldKeys {
    /// The environment's current key, for what is still sealed with it beside the vault for the previous build.
    pub(crate) env: Option<SecretKey>,
    /// Every old key there is: the environment's (current, then retired), then those adopted into the vault.
    pub(crate) all: Vec<SecretKey>,
    /// Builds that check with the old key only are refused here ([`OLD_KEY_FLOOR`], or `COMPATIBLE_FROM` past it).
    pub(crate) retired: bool,
}

/// Adopts `key` as `name`, once (replicas adopting it at once write it once: the secret's lock); whether this did.
async fn adopt(db: &Db, name: &str, key: &SecretKey) -> Result<bool> {
    let value = New::Value(zeroize::Zeroizing::new(key.key_bytes().to_vec()));
    let rule = Rule { reveal: false, ..Default::default() };
    super::secrets::adopt_at_boot(db, Scope::Installation, "platform", name, value, rule).await
}

/// The names of the keys adopted into the vault.
async fn adopted_names(tx: &mut crate::Tx) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "select distinct name from vault.secret where scope = $1 and owner = 'platform' and name like 'legacy.key.%' and valid_until is null order by name",
    )
    .bind(Uuid::nil())
    .fetch_all(&mut **tx)
    .await?)
}

/// The keys adopted into the vault, opened (never revealed: they stay inside the kernel).
async fn adopted(db: &Db) -> Result<Vec<SecretKey>> {
    with_retry(|| async {
        let mut tx = db.vault_tx(None).await?;
        let mut out = Vec::new();
        for name in adopted_names(&mut tx).await? {
            let rows = rows(&mut tx, Scope::Installation, "platform", &name).await?;
            let Some(row) = rows.iter().find(|r| r.valid_until.is_none()) else { continue };
            let value = opened(db, &mut tx, Scope::Installation, "platform", &name, row).await?;
            let bytes: [u8; 32] = value.as_slice().try_into().map_err(|_| Error::Internal(format!("the vault's {name} is not a key")))?;
            out.push(SecretKey::new(bytes));
        }
        tx.commit().await?;
        Ok(out)
    })
    .await
}

async fn note_current(db: &Db, key: &SecretKey) -> Result<()> {
    let mut tx = db.system_tx().await?;
    sqlx::query("insert into legacy_key_use (what, caller) values ('current key', $1) on conflict (what, caller) do update set last_at = now()")
        .bind(&key.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Whether builds that check with the old key only are refused here: [`OLD_KEY_FLOOR`] is in effect, or the oldest
/// build the database admits is later than it.
pub(crate) async fn older_builds_refused(db: &Db) -> Result<bool> {
    let mut conn = db.pool.acquire().await?;
    let floors = crate::window::floors(&mut conn).await?;
    let recorded = crate::window::recorded(&mut conn).await?;
    let since = chrono::DateTime::parse_from_rfc3339(OLD_KEY_FLOOR.since).map_err(Error::internal)?;
    Ok(floors.iter().any(|f| f.feature == OLD_KEY_FLOOR.feature) || recorded.is_some_and(|r| r.compatible_from >= since))
}

/// As the kernel boots: with the vault open, each key the environment configures adopted once and every adopted key
/// opened; and whether older builds are refused. With the vault closed: the environment's keys, as before.
pub(crate) async fn at_boot(db: &Db, settings: &KernelSettings) -> Result<OldKeys> {
    let env = settings.old_key.current.clone();
    let mut all: Vec<SecretKey> = env.iter().chain(&settings.old_key.retired).cloned().collect();
    let retired = older_builds_refused(db).await?;
    if db.vault.installation().is_err() {
        return Ok(OldKeys { env, all, retired });
    }
    for key in all.clone() {
        if adopt(db, &format!("{ADOPTED}{}", key.id), &key).await? {
            tracing::info!(key = %key.id, "an old key was adopted into the vault: what it made is checked with the vault's copy from now on");
        }
    }
    if let Some(key) = &env {
        note_current(db, key).await?;
    }
    for key in adopted(db).await? {
        if !all.iter().any(|a| a.id == key.id) {
            all.push(key);
        }
    }
    Ok(OldKeys { env, all, retired })
}

/// The ids of the keys adopted into the vault (`ferrum task legacy_key`).
pub(crate) async fn adopted_ids(db: &Db) -> Result<Vec<String>> {
    let mut tx = db.vault_tx(None).await?;
    let names = adopted_names(&mut tx).await?;
    tx.commit().await?;
    Ok(names.into_iter().filter_map(|n| n.strip_prefix(ADOPTED).map(str::to_string)).collect())
}

// What the platform seals outside the vault for a while (a message's secret values while it waits, a sign-in's
// verifier): under the platform's key in its scope (`Vault::seal`, which names the version that sealed it, so it opens
// after a rotation), or with the old key while it makes such things. What the old key sealed opens with it.

/// Whether `stored` is what [`Vault::seal`] makes (what the old key seals is standard base64, which has no `.`).
pub(crate) fn sealed_by_vault(stored: &str) -> bool {
    stored.starts_with(super::SEALED_PREFIX)
}

impl Kernel {
    /// `plain` sealed in `scope` for `context`: with the old key, as the old code sealed it for `label`, while what an
    /// older build opens is made with it; else under the platform's key. Refused with neither.
    pub(crate) async fn seal_for_platform(&self, scope: Scope, context: &str, label: &str, plain: &str, what: Keyed) -> Result<String> {
        if let Some(key) = self.old_making() {
            return key.seal(label, plain);
        }
        self.sealing_ready(what).await?;
        Vault::kernel("platform").seal(self, scope, context, plain.as_bytes()).await
    }

    /// Refused, saying why, where [`Kernel::seal_for_platform`] would be: asked before anything is done.
    pub(crate) async fn sealing_ready(&self, what: Keyed) -> Result<()> {
        if self.old_making().is_some() {
            return Ok(());
        }
        self.keyed_open(what)?;
        self.older_builds_out().await
    }

    /// What [`Kernel::seal_for_platform`] sealed.
    pub(crate) async fn open_for_platform(&self, scope: Scope, context: &str, label: &str, stored: &str, what: Keyed) -> Result<String> {
        if !sealed_by_vault(stored) {
            return self.open_sealed(label, stored);
        }
        self.keyed_open(what)?;
        let plain = Vault::kernel("platform").open(self, scope, context, stored).await?;
        String::from_utf8(plain.to_vec()).map_err(|_| Error::Internal("what the vault sealed is not text".into()))
    }
}

// Plugins' settings: the plugin host (platform code, holding the platform's handle) keeps each sealed under the plugin's
// own key (`plugin:<name>`, data under the owner key: no secret of the vault is revealed), and, while the old key is
// configured, sealed with it beside for the previous build. Settings from before are taken over by the move.

/// How the plugin host keeps a setting's value: `{"$vault": …}`, with `"$sealed"` and `"$key"` (the old key's) beside
/// while it is configured.
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

/// What the old key bound a plugin's setting to.
pub(crate) fn setting_label(tenant: Uuid, plugin: &str, key: &str) -> String {
    format!("{tenant}/plugin/{plugin}/{key}")
}

/// Whether `stored` is a setting as the host keeps it now (or nothing).
fn kept_now(stored: &Value) -> bool {
    stored.is_null() || stored.get(PLUGIN_VAULT).is_some_and(Value::is_string)
}

impl Kernel {
    /// A plugin's setting `key` holding `value`, as its host stores it. Refused with neither the vault open nor the old
    /// key configured: a setting is never stored plain.
    async fn keep_setting(&self, tenant: Uuid, plugin: &str, key: &str, value: &Value) -> Result<Value> {
        let text = value.to_string();
        let mut out = match self.env_key() {
            Some(k) => json!({"$sealed": k.seal(&setting_label(tenant, plugin, key), &text)?, "$key": k.id}),
            None => json!({}),
        };
        match self.db.vault.installation() {
            Ok(_) => {
                let sealed = Vault::kernel(&format!("plugin:{plugin}")).seal(self, Scope::Workspace(tenant), &setting_context(key), text.as_bytes()).await?;
                out[PLUGIN_VAULT] = Value::String(sealed);
            }
            Err(e) if self.env_key().is_none() => {
                return Err(Error::Unavailable(format!("a plugin's setting needs the vault (it is never stored plain), and {e}")));
            }
            Err(_) => {}
        }
        Ok(out)
    }

    /// A plugin's setting as its host reads it, from what is stored: what [`Kernel::keep_setting`] kept, what the old key
    /// sealed before, or a value stored plain from before settings were sealed. `None`: it does not open here (moved to
    /// another setting, or sealed with a key this server has not), which the host leaves out.
    async fn setting(&self, tenant: Uuid, plugin: &str, key: &str, stored: &Value) -> Result<Option<Value>> {
        if let Some(sealed) = stored.get(PLUGIN_VAULT).and_then(Value::as_str)
            && self.db.vault.installation().is_ok()
        {
            return Ok(match Vault::kernel(&format!("plugin:{plugin}")).open(self, Scope::Workspace(tenant), &setting_context(key), sealed).await {
                Ok(plain) => serde_json::from_slice(&plain).ok(),
                Err(Error::Invalid(_)) => None,
                Err(e) => return Err(e),
            });
        }
        Ok(match stored.get("$sealed").and_then(Value::as_str) {
            Some(sealed) => self.open_sealed(&setting_label(tenant, plugin, key), sealed).ok().and_then(|p| serde_json::from_str(&p).ok()),
            None if stored.get(PLUGIN_VAULT).is_some() => None,
            None => Some(stored.clone()),
        })
    }

    /// The move of plugins' settings (`vault_move`): each value not kept as the host keeps it now (sealed with the old
    /// key alone, or plain from before settings were sealed) opened and kept again, a plugin at a time, unless it changed
    /// meanwhile. (moved, left: values no key here opens, or plugins changed meanwhile).
    pub(crate) async fn move_plugin_settings(&self) -> Result<(u64, u64)> {
        let mut tx = self.db.system_tx().await?;
        let rows: Vec<(Uuid, Uuid, String, Value)> = sqlx::query_as(
            "select tenant_id, id, name, config from plugin where jsonb_typeof(config) = 'object' and exists (select 1 from jsonb_each(config) c \
               where c.value <> 'null'::jsonb and coalesce(jsonb_typeof(c.value->'$vault'), '') <> 'string')",
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let (mut moved, mut left) = (0, 0);
        for (tenant, id, name, config) in rows {
            let Value::Object(map) = &config else { continue };
            let mut now = map.clone();
            let mut took = 0;
            for (k, v) in map.iter().filter(|(_, v)| !kept_now(v)) {
                match self.setting(tenant, &name, k, v).await? {
                    Some(value) => {
                        now.insert(k.clone(), self.keep_setting(tenant, &name, k, &value).await?);
                        took += 1;
                    }
                    None => left += 1,
                }
            }
            if took == 0 {
                continue;
            }
            let mut tx = self.db.system_tx().await?;
            let n = sqlx::query("update plugin set config = $3 where tenant_id = $1 and id = $2 and config = $4")
                .bind(tenant)
                .bind(id)
                .bind(Value::Object(now))
                .bind(&config)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            tx.commit().await?;
            if n == 1 { moved += took } else { left += took }
        }
        Ok((moved, left))
    }

    /// How many plugins' settings wait for the move.
    pub(crate) async fn plugin_settings_waiting(&self, tx: &mut crate::Tx) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "select count(*) from plugin, jsonb_each(case when jsonb_typeof(config) = 'object' then config else '{}' end) c \
             where c.value <> 'null'::jsonb and coalesce(jsonb_typeof(c.value->'$vault'), '') <> 'string'",
        )
        .fetch_one(&mut **tx)
        .await?)
    }
}

impl Vault {
    /// The platform's handle only (what the server alone holds), the vault open or not: what works as before with the
    /// vault closed asks this, and the verbs it reaches check the vault where they need it.
    fn platform_handle(&self, k: &Kernel) -> Result<()> {
        if self.given(k)? != "platform" {
            return Err(Error::Forbidden("only the platform's handle does this".into()));
        }
        Ok(())
    }

    /// Plugin `plugin`'s setting `key` holding `value`, as the plugin host stores it (in `config`): sealed under the
    /// plugin's own key, and with the old key beside while it is configured. Only the platform's handle (the host's).
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
    /// installation that sealed it (none: the old key, while it seals them). Only the platform's handle.
    pub async fn seal_process(&self, k: &Kernel, aad: &str, plain: &str) -> Result<(Option<i32>, String)> {
        self.platform_handle(k)?;
        let (version, key) = k.making_key(Scope::Installation, Keyed::Process).await?;
        Ok((version, key.seal(aad, plain)?))
    }

    /// What [`Vault::seal_process`] sealed with `version`, or (none) what the old key sealed. Only the platform's handle.
    pub async fn open_process(&self, k: &Kernel, aad: &str, version: Option<i32>, sealed: &str) -> Result<String> {
        self.platform_handle(k)?;
        let refused = || Error::Unavailable("no key here opens this payload: the old key it was sealed with is neither configured nor adopted".into());
        match version {
            Some(v) => {
                k.keyed_open(Keyed::Process)?;
                k.key_at(Scope::Installation, Keyed::Process, v).await?.open(aad, sealed)
            }
            None => k.old_keys.iter().find_map(|key| key.open(aad, sealed).ok()).ok_or_else(refused),
        }
    }
}

// R1 only, and gone in R2 with the old key: what an app sealed or keyed with the old key itself (through
// `settings.secret_key`, while that was public) keeps working through the app's own handle, from the copies adopted into the vault once the key is gone
// from the environment, and never anything wider. Each names a context of the app's own ([`app_context`]); each use is
// noted for `ferrum task legacy_key` and the doctor, as the other readers past the vault are.

/// What may follow an app's id at the start of a context of its own.
const SEPARATORS: &[char] = &[' ', '-', '.', ':', '/', '|'];

/// Words the kernel's own labels for the old key begin with (`message opt-out`, `pulled-file`, `saml-sp|…`, `ferrum.…`,
/// …), and owners that are not apps: an app with one of these ids is refused these calls, so that no app can name what
/// the kernel sealed or keyed.
const KERNEL_WORDS: &[&str] = &["action", "conn", "connection", "erased", "ferrum", "message", "platform", "plugin", "pulled", "saml", "server", "sso", "totp"];

/// Whether app `id`, among the apps `all` registered here, may name `context`: it is `{id}`, then a separator, then more;
/// it is not another app's (one whose longer id it also begins with, then a separator: `cloud-ops-x` is app
/// `cloud-ops`'s, not `cloud`'s); it does not begin with a workspace's id (what the kernel seals for a workspace does);
/// and `id` is not a word the kernel's own labels begin with.
fn app_context<'a>(id: &str, mut all: impl Iterator<Item = &'a str>, context: &str) -> bool {
    let after = |o: &str| context.strip_prefix(o).and_then(|rest| rest.chars().next().map(|c| (SEPARATORS.contains(&c), rest.len())));
    (1..=200).contains(&context.len())
        && !context.chars().any(char::is_control)
        && matches!(after(id), Some((true, n)) if n > 1)
        && !all.any(|o| o != id && o.len() > id.len() && matches!(after(o), Some((true, _))))
        && context.get(..36).is_none_or(|s| s.parse::<Uuid>().is_err())
        && !KERNEL_WORDS.contains(&id)
}

impl Vault {
    /// This app's id, when `context` is one of its own ([`app_context`]); refused otherwise, and to any handle that is
    /// not an app's.
    fn own_context(&self, k: &Kernel, context: &str) -> Result<&str> {
        let id = self.given(k)?;
        if !matches!(self.owner, super::Owner::App(_)) || !app_context(id, k.hooks.vault_app_ids(), context) {
            return Err(Error::Forbidden(
                "an app names only contexts of its own: its id, then one of ' -.:/|', then more; never another app's or the platform's".into(),
            ));
        }
        Ok(id)
    }

    /// R1 only: what the old key sealed for `context`, a context of this app's own, opened with every old key (the
    /// environment's, and those adopted into the vault, so it opens once the key is gone). Goes in R2.
    pub fn open_old(&self, k: &Kernel, context: &str, sealed: &str) -> Result<zeroize::Zeroizing<String>> {
        let id = self.own_context(k, context)?;
        k.note_old_key_by("reader:Vault::open_old", id);
        k.old_keys
            .iter()
            .find_map(|key| key.open(context, sealed).ok())
            .map(zeroize::Zeroizing::new)
            .ok_or_else(|| Error::Invalid("this does not open with the old key: it was sealed for another context, or changed".into()))
    }

    /// R1 only: `plain` sealed with the old key for `context`, a context of this app's own, while the key is in the
    /// environment, for the column this app's previous release reads; `None` once it is gone (no release that reads it
    /// starts then). Goes in R2.
    pub fn seal_old(&self, k: &Kernel, context: &str, plain: &str) -> Result<Option<String>> {
        let id = self.own_context(k, context)?;
        k.note_old_key_by("reader:Vault::seal_old", id);
        k.env_key().map(|key| key.seal(context, plain)).transpose()
    }

    /// R1 only: `data`'s fingerprint for `context`, a context of this app's own, under each old key, as
    /// `SecretKey::fingerprint` made it: to check what this app issued with the old key until it lapses, never to
    /// issue anything new (its own keys do that: `derive`, `sign`). Goes in R2.
    pub fn old_fingerprints(&self, k: &Kernel, context: &str, data: &str) -> Result<Vec<String>> {
        let id = self.own_context(k, context)?;
        k.note_old_key_by("reader:Vault::old_fingerprints", id);
        Ok(k.old_keys.iter().map(|key| key.fingerprint(context, data)).collect())
    }
}

impl Kernel {
    /// Whether a message's secret values can be sealed here while they wait: what an app asks before it makes something
    /// it sends that way, in place of asking whether the old key is set (the kernel's alone).
    pub fn seals_secret_values(&self) -> bool {
        self.old_making().is_some() || self.db.vault.installation().is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_app_names_only_contexts_of_its_own() {
        let apps = ["cloud", "cloud-ops", "helpdesk", "pos", "message"];
        let ok = |id: &str, c: &str| app_context(id, apps.into_iter(), c);
        assert!(ok("cloud", "cloud-dns-key") && ok("helpdesk", "helpdesk mail token") && ok("pos", "pos snapshot") && ok("cloud", "cloud:x"));
        assert!(!ok("cloud", "cloud-ops-key"), "another app's: the longer id it begins with");
        assert!(ok("cloud-ops", "cloud-ops-key"));
        assert!(!ok("pos", "pos") && !ok("pos", "pos ") && !ok("pos", "posx snapshot") && !ok("pos", "helpdesk mail token"), "its id, a separator, then more");
        assert!(!ok("message", "message opt-out"), "the kernel's own");
        let workspace = format!("{}/plugin/x/y", Uuid::now_v7());
        assert!(!ok(&workspace[..8], &workspace), "a workspace's");
        assert!(!ok("pos", "pos \nx") && !ok("pos", &format!("pos {}", "x".repeat(200))));
    }

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
        assert!(sealed_by_vault("fv1.1.abc") && !sealed_by_vault("AAAA"));
        assert!(
            kept_now(&Value::Null) && kept_now(&json!({"$vault": "fv1.1.x", "$sealed": "y"})) && !kept_now(&json!({"$sealed": "y"})) && !kept_now(&json!("eu"))
        );
    }
}
