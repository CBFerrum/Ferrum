//! The vault's root: the installation key and the slots that open it (docs/operations.md, "The vault").
//!
//! The installation key is random and is stored only wrapped, once per slot; any slot opens it:
//! * `file`: the key in `/run/ferrum/root.key` (base64 of 32 bytes), mounted where the server runs;
//! * `kms`: a key the key-management service holds, reached as `kms.rs` reaches it: a command
//!   (`FERRUM_SECRET_KEY_COMMAND`) or OpenBao's or Vault's Transit engine (`FERRUM_KMS_TRANSIT`);
//! * `credential`: a key derived from the database password in `DATABASE_URL` (Argon2id, the slot's own salt);
//! * `recovery`: a key printed once, for its owner to keep offline: only `ferrum task vault_rewrap` opens with it.
//!
//! A server opens the vault at start, trying `file`, then `kms`, then `credential`: the first that opens it wins, and
//! each that did not is said. A database without slots gets an installation key and the best slot available here,
//! made once under a lock when replicas start together. Nothing falls back silently: a vault that does not open is
//! refused with the reason of each slot, and what a slot holds is checked before it is used, so that a wrong key (a
//! changed password) is told from a damaged row.

use super::seal::{
    Kdf, Key, check_place, key_base64, key_place, password_key, print_recovery, random, read_key_file, read_recovery, recovery_slot_key, root_place,
};
use crate::db::{Db, Tx};
use crate::error::{Error, Result};
use crate::kms;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;
use zeroize::Zeroizing;

/// Where the file slot's key is mounted (the boot contract's fixed path).
pub const ROOT_FILE: &str = "/run/ferrum/root.key";

/// The channel a change of the vault's keys is told on: `*` for the root (a slot or the installation key), the scope's
/// id for its workspace key, `{scope}/{owner}` for an owner key. Each replica drops what it holds of what changed.
pub(crate) const CHANNEL: &str = "ferrum_vault";

/// A kind of slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotKind {
    File,
    Kms,
    Credential,
    Recovery,
}

impl SlotKind {
    /// The slots a server opens with, in the order it tries them.
    pub const AT_START: [SlotKind; 3] = [SlotKind::File, SlotKind::Kms, SlotKind::Credential];

    pub fn as_str(self) -> &'static str {
        match self {
            SlotKind::File => "file",
            SlotKind::Kms => "kms",
            SlotKind::Credential => "credential",
            SlotKind::Recovery => "recovery",
        }
    }

    pub fn parse(s: &str) -> Option<SlotKind> {
        [SlotKind::File, SlotKind::Kms, SlotKind::Credential, SlotKind::Recovery].into_iter().find(|k| k.as_str() == s)
    }
}

impl std::fmt::Display for SlotKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What this process has of each slot's key. A server makes it from its environment, as it is read today:
/// `DATABASE_URL`, the fixed file path, `FERRUM_KMS_TRANSIT` and `FERRUM_SECRET_KEY_COMMAND`.
#[derive(Clone, Default)]
pub struct Opener {
    /// The credential slot's key is derived from this address's password.
    pub database_url: String,
    /// The file slot's key ([`ROOT_FILE`] in a server).
    pub file: PathBuf,
    /// The kms slot through Transit.
    pub transit: Option<kms::Transit>,
    /// The kms slot through a command (asked first, as for the secret key).
    pub command: Option<String>,
    /// Argon2id's costs for a credential slot made here (a slot keeps its own).
    pub kdf: Kdf,
}

impl std::fmt::Debug for Opener {
    /// Which ways it has, never what they hold (the address carries a password).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opener")
            .field("password", &password_of(&self.database_url).is_some())
            .field("file", &self.file)
            .field("transit", &self.transit.is_some())
            .field("command", &self.command.is_some())
            .finish()
    }
}

/// How [`open`] went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    /// Open, with this slot.
    With(SlotKind),
    /// The database is not migrated as far as the vault yet: it opens once the kernel has migrated it (`Kernel::boot`).
    Later,
    /// The vault has no slot yet, and nothing here can make its first one: no password in `DATABASE_URL`, no key
    /// file, no key-management service. It stays closed, saying so, and everything that asks it is refused; a later
    /// start with one of them makes it. In this release nothing depends on the vault, so the server starts all the same.
    Closed,
}

/// The installation key this process holds.
pub(crate) struct Installation {
    pub(crate) version: i32,
    pub(crate) key: Key,
    pub(crate) made_at: DateTime<Utc>,
}

enum State {
    Closed {
        why: String,
        opener: Option<Opener>,
    },
    /// No slot yet, and none possible here ([`Opened::Closed`]).
    Rootless {
        opener: Opener,
    },
    Pending {
        opener: Opener,
        tolerate: bool,
    },
    Open {
        installation: Arc<Installation>,
        opener: Opener,
        by: SlotKind,
    },
}

/// The vault of one database, as this process holds it (`Db::vault`): the installation key once opened, and the keys
/// below it unwrapped so far.
pub(crate) struct Root {
    state: std::sync::RwLock<State>,
    /// One opening at a time: at start, and again once another process has rotated the installation key.
    opening: tokio::sync::Mutex<()>,
    pub(crate) cache: super::keys::Cache,
}

impl Default for Root {
    fn default() -> Self {
        Root {
            state: std::sync::RwLock::new(State::Closed { why: "it was not opened (a server opens it as it starts)".into(), opener: None }),
            opening: tokio::sync::Mutex::new(()),
            cache: Default::default(),
        }
    }
}

/// Why a vault with no slot and nothing to make one with is closed, and the three ways to give it a root.
fn rootless(opener: &Opener) -> String {
    format!(
        "nothing here can make its first slot: DATABASE_URL has no password, there is no key file at {}, and no key-management service is set \
         (FERRUM_KMS_TRANSIT or FERRUM_SECRET_KEY_COMMAND). Give it one of these, and the next start makes it (docs/operations.md, \"The vault\")",
        opener.file.display()
    )
}

fn poisoned() -> Error {
    Error::Internal("the vault's state is unreadable (a panic held its lock)".into())
}

impl Root {
    /// The installation key, or why the vault is closed.
    pub(crate) fn installation(&self) -> Result<Arc<Installation>> {
        match &*self.state.read().map_err(|_| poisoned())? {
            State::Open { installation, .. } => Ok(installation.clone()),
            State::Closed { why, .. } => Err(Error::Unavailable(format!("the vault is closed: {why}"))),
            State::Rootless { opener } => Err(Error::Unavailable(format!("the vault is closed: {}", rootless(opener)))),
            State::Pending { .. } => Err(Error::Unavailable("the vault opens once the database is migrated".into())),
        }
    }

    /// Whether it is closed only because nothing here can make its first slot ([`Opened::Closed`]).
    pub(crate) fn rootless(&self) -> bool {
        self.state.read().is_ok_and(|s| matches!(*s, State::Rootless { .. }))
    }

    /// How this process opens the vault, whatever it holds now.
    pub(crate) fn opener(&self) -> Option<Opener> {
        match &*self.state.read().ok()? {
            State::Open { opener, .. } | State::Pending { opener, .. } | State::Rootless { opener } => Some(opener.clone()),
            State::Closed { opener, .. } => opener.clone(),
        }
    }

    /// The slot it was opened with; or why it is closed.
    pub(crate) fn opened(&self) -> std::result::Result<SlotKind, String> {
        match &*self.state.read().map_err(|_| "its state is unreadable".to_string())? {
            State::Open { by, .. } => Ok(*by),
            State::Closed { why, .. } => Err(why.clone()),
            State::Rootless { opener } => Err(rootless(opener)),
            State::Pending { .. } => Err("it opens once the database is migrated".into()),
        }
    }

    fn set(&self, state: State) {
        if let Ok(mut s) = self.state.write() {
            *s = state;
        }
    }

    fn set_open(&self, installation: Arc<Installation>, opener: Opener, by: SlotKind) {
        self.cache.forget(None, None);
        self.set(State::Open { installation, opener, by });
    }

    /// Opens again with the slot it opened with, after another process changed the root: what it held keeps working
    /// until then, and an opening that fails leaves it as it was.
    pub(crate) async fn reopen(&self, db: &Db) -> Result<Arc<Installation>> {
        let before = self.installation()?;
        let _one = self.opening.lock().await;
        let now = self.installation()?;
        if now.version != before.version {
            // Another task here opened it again meanwhile.
            return Ok(now);
        }
        let opener = self.opener().ok_or_else(|| Error::Unavailable("the vault is closed".into()))?;
        let slots = read_slots(db).await?;
        let (installation, by) = unlock_any(&slots, &opener).await?;
        if installation.version != now.version {
            tracing::info!(version = installation.version, slot = by.as_str(), "the vault's installation key changed: opened again");
        }
        self.set_open(installation.clone(), opener, by);
        Ok(installation)
    }
}

/// Opens the vault of `db`, as a server does at start and a task before it runs (`server/src/tasks.rs`, `open_vault`).
/// A database not migrated as far as the vault yet is opened once the kernel has migrated it ([`Opened::Later`]); one
/// without slots gets its installation key and the best slot here. An error says why each slot did not open, and
/// leaves the vault closed with that reason: whatever then asks it is refused with it. `tolerate`: a task that
/// reports or repairs the vault, which runs with it closed (only for an opening left to [`finish`]).
pub async fn open(db: &Db, opener: Opener, tolerate: bool) -> Result<Opened> {
    let root = &db.vault;
    let _one = root.opening.lock().await;
    if !migrated(db).await? {
        root.set(State::Pending { opener, tolerate });
        return Ok(Opened::Later);
    }
    match open_now(db, &opener).await {
        Ok(Some((installation, by))) => {
            root.set_open(installation, opener, by);
            Ok(Opened::With(by))
        }
        Ok(None) => {
            tracing::warn!("the vault is closed: {}", rootless(&opener));
            root.set(State::Rootless { opener });
            Ok(Opened::Closed)
        }
        Err(e) => {
            root.set(State::Closed { why: e.to_string(), opener: Some(opener) });
            Err(e)
        }
    }
}

/// An opening [`open`] left until the database was migrated, done now (`Kernel::boot`, once it has migrated).
pub(crate) async fn finish(db: &Db) -> Result<()> {
    let pending = match &*db.vault.state.read().map_err(|_| poisoned())? {
        State::Pending { opener, tolerate } => Some((opener.clone(), *tolerate)),
        _ => None,
    };
    let Some((opener, tolerate)) = pending else { return Ok(()) };
    let failed = match open(db, opener.clone(), tolerate).await {
        Ok(Opened::With(by)) => {
            tracing::info!(slot = by.as_str(), "the vault is open");
            return Ok(());
        }
        Ok(Opened::Closed) => return Ok(()),
        Ok(Opened::Later) => {
            let e = Error::Unavailable("the database is not migrated as far as the vault: run `ferrum task migrate` with this build".into());
            db.vault.set(State::Closed { why: e.to_string(), opener: Some(opener) });
            e
        }
        Err(e) => e,
    };
    if tolerate {
        tracing::error!("the vault is closed: {failed}");
        Ok(())
    } else {
        Err(failed)
    }
}

/// The kernel's migration that makes the vault.
const MIGRATION: i64 = 20261119000201;

/// Whether this database is migrated as far as the vault: its migration recorded, and with it every table it and the
/// vault's module use (the workspaces', the audit trail's, the jobs'). A database not yet migrated (a new one, an
/// upgrade, a test's reset of the public schema) has its vault opened once the kernel has migrated it.
async fn migrated(db: &Db) -> Result<bool> {
    let (recorded, tables): (bool, bool) =
        sqlx::query_as("select to_regclass('public._sqlx_migrations') is not null, to_regclass('vault.root_slot') is not null").fetch_one(&db.side).await?;
    if !recorded {
        return Ok(false);
    }
    let made: bool =
        sqlx::query_scalar("select exists (select 1 from public._sqlx_migrations where version = $1 and success)").bind(MIGRATION).fetch_one(&db.side).await?;
    if made && !tables {
        return Err(Error::Unavailable(
            "the vault's tables are gone, although the database's migrations made them: restore the vault's schema from the backup taken with the rest".into(),
        ));
    }
    Ok(made)
}

/// The vault's transaction-level lock on its root: taken shared by whatever wraps a key with the installation key,
/// exclusive by whatever changes a slot or the installation key, so neither meets the other half done.
pub(crate) async fn lock(tx: &mut Tx, exclusive: bool) -> Result<()> {
    let sql = if exclusive {
        "select pg_advisory_xact_lock(hashtextextended('ferrum.vault.root', 0))"
    } else {
        "select pg_advisory_xact_lock_shared(hashtextextended('ferrum.vault.root', 0))"
    };
    sqlx::query(sql).execute(&mut **tx).await?;
    Ok(())
}

/// The installation key's version the slots wrap now (none: no slot).
pub(crate) async fn current_version(tx: &mut Tx) -> Result<Option<i32>> {
    Ok(sqlx::query_scalar("select max(installation_version) from vault.root_slot").fetch_one(&mut **tx).await?)
}

pub(crate) async fn notify(tx: &mut Tx, what: &str) -> Result<()> {
    sqlx::query("select pg_notify($1, $2)").bind(CHANNEL).bind(what).execute(&mut **tx).await?;
    Ok(())
}

/// A slot as it is stored.
#[derive(Clone)]
struct SlotRow {
    kind: SlotKind,
    version: i32,
    made_at: DateTime<Utc>,
    wrapped: Vec<u8>,
    check: Vec<u8>,
    salt: Option<Vec<u8>>,
    params: Value,
    kms_wrapped: Option<String>,
    created_at: DateTime<Utc>,
}

type SlotTuple = (String, i32, DateTime<Utc>, Vec<u8>, Vec<u8>, Option<Vec<u8>>, Value, Option<String>, DateTime<Utc>);

async fn read_slots(db: &Db) -> Result<Vec<SlotRow>> {
    let mut tx = db.vault_tx(None).await?;
    let rows: Vec<SlotTuple> = sqlx::query_as(
        "select kind, installation_version, installation_made_at, wrapped, check_value, salt, params, kms_wrapped, created_at from vault.root_slot order by kind",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    rows.into_iter()
        .map(|(kind, version, made_at, wrapped, check, salt, params, kms_wrapped, created_at)| {
            let kind = SlotKind::parse(&kind).ok_or_else(|| Error::Internal(format!("the vault has a slot of no known kind ({kind})")))?;
            Ok(SlotRow { kind, version, made_at, wrapped, check, salt, params, kms_wrapped, created_at })
        })
        .collect()
}

/// Why a slot did not open, in the words its refusal uses.
enum Miss {
    /// What it needs is not here: no key file, no service set, no password.
    Absent(String),
    /// Its key is not the one the slot was made with.
    Wrong(String),
    /// Its key is right, and what it holds does not open, or its row is not whole.
    Damaged(String),
    /// The service that holds its key did not answer.
    Unreachable(String),
}

impl std::fmt::Display for Miss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Miss::Absent(s) | Miss::Wrong(s) | Miss::Unreachable(s) => f.write_str(s),
            Miss::Damaged(s) => write!(f, "it is damaged ({s}): restore the vault's tables from a backup"),
        }
    }
}

fn wrong_key(kind: SlotKind, opener: &Opener) -> String {
    match kind {
        SlotKind::Credential => "the database password in DATABASE_URL is not the one it was made with (if the password changed, run \
                                 `ferrum task vault_rewrap` with the new DATABASE_URL and give the old password once)"
            .into(),
        SlotKind::File => format!("the key in {} is not the one it was made with", opener.file.display()),
        SlotKind::Kms => "the key-management service gave back another key than the one it was made with".into(),
        SlotKind::Recovery => "that is not this vault's recovery key".into(),
    }
}

/// The password in a database address (`postgres://user:password@…`, or `?password=`, as sqlx reads them); none when
/// it has none (a password from elsewhere, such as a password file, is not the address's).
pub(crate) fn password_of(url: &str) -> Option<Zeroizing<String>> {
    let parsed = reqwest::Url::parse(url.trim()).ok()?;
    if let Some(p) = parsed.password() {
        let decoded = Zeroizing::new(percent_decode(p)?);
        return String::from_utf8(decoded.to_vec()).ok().filter(|p| !p.is_empty()).map(Zeroizing::new);
    }
    parsed.query_pairs().find(|(k, _)| k == "password").map(|(_, v)| Zeroizing::new(v.into_owned())).filter(|p| !p.is_empty())
}

fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| Error::Internal(format!("deriving a key: {e}")))?
}

fn kms_key(k: crate::secret::SecretKey) -> Key {
    Key::from_slice(k.key_bytes()).expect("a secret key is 32 bytes")
}

fn file_key(opener: &Opener) -> std::result::Result<Key, Miss> {
    let path = opener.file.display();
    match std::fs::read_to_string(&opener.file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Miss::Absent(format!("there is no key file at {path}"))),
        Err(e) => Err(Miss::Absent(format!("the key file {path} cannot be read: {e}"))),
        Ok(text) => {
            let text = Zeroizing::new(text);
            read_key_file(&text).ok_or_else(|| Miss::Wrong(format!("{path} does not hold base64 of 32 bytes (openssl rand -base64 32)")))
        }
    }
}

/// The key of `slot`, from what this process has.
async fn slot_key(slot: &SlotRow, opener: &Opener) -> std::result::Result<Key, Miss> {
    match slot.kind {
        SlotKind::File => file_key(opener),
        SlotKind::Kms => match slot.params["how"].as_str() {
            Some("command") => {
                let Some(command) = &opener.command else {
                    return Err(Miss::Absent("it was made through FERRUM_SECRET_KEY_COMMAND, which is not set here".into()));
                };
                match kms::from_command(command).await {
                    Ok(k) => Ok(kms_key(k)),
                    Err(Error::Invalid(e)) => Err(Miss::Wrong(e)),
                    Err(e) => Err(Miss::Unreachable(e.to_string())),
                }
            }
            Some("transit") => {
                let Some(transit) = &opener.transit else {
                    return Err(Miss::Absent("it was made through the key-management service's Transit key, and FERRUM_KMS_TRANSIT is not set here".into()));
                };
                let Some(wrapped) = &slot.kms_wrapped else { return Err(Miss::Damaged("its row holds no Transit ciphertext".into())) };
                match kms::resolve(wrapped, Some(transit)).await {
                    Ok(k) => Ok(kms_key(k)),
                    Err(Error::Invalid(e)) => Err(Miss::Wrong(e)),
                    Err(e) => Err(Miss::Unreachable(e.to_string())),
                }
            }
            _ => Err(Miss::Damaged("its row does not say how the service is asked".into())),
        },
        SlotKind::Credential => {
            let Some(password) = password_of(&opener.database_url) else {
                return Err(Miss::Absent("it is opened with the database password, and DATABASE_URL has none".into()));
            };
            let salt = slot.salt.clone().ok_or_else(|| Miss::Damaged("its row holds no salt".into()))?;
            let kdf: Kdf = serde_json::from_value(slot.params["kdf"].clone()).map_err(|_| Miss::Damaged("its row does not say Argon2id's costs".into()))?;
            blocking(move || password_key(password.as_bytes(), &salt, kdf)).await.map_err(|e| Miss::Damaged(e.to_string()))
        }
        SlotKind::Recovery => Err(Miss::Absent("its key is kept offline: `ferrum task vault_rewrap with:recovery` opens with it".into())),
    }
}

/// The installation key out of `slot`, with its key checked first.
async fn unlock(slot: &SlotRow, opener: &Opener) -> std::result::Result<Key, Miss> {
    let kek = slot_key(slot, opener).await?;
    open_slot(slot, &kek, opener)
}

fn open_slot(slot: &SlotRow, kek: &Key, opener: &Opener) -> std::result::Result<Key, Miss> {
    if !kek.checks(&check_place(slot.kind.as_str()), &slot.check) {
        // The key opens what the slot holds all the same: its check value is what is damaged (still refused).
        if kek.unwrap(&root_place(slot.kind.as_str(), slot.version), &slot.wrapped).is_some() {
            return Err(Miss::Damaged("its check value does not match, although its key opens it".into()));
        }
        return Err(Miss::Wrong(wrong_key(slot.kind, opener)));
    }
    kek.unwrap(&root_place(slot.kind.as_str(), slot.version), &slot.wrapped)
        .ok_or_else(|| Miss::Damaged("its key is right and what it holds does not open".into()))
}

/// The installation key, opened with the first slot (in [`SlotKind::AT_START`]'s order) that opens it.
async fn unlock_any(slots: &[SlotRow], opener: &Opener) -> Result<(Arc<Installation>, SlotKind)> {
    let versions: std::collections::BTreeSet<i32> = slots.iter().map(|s| s.version).collect();
    if versions.len() > 1 {
        return Err(Error::Unavailable(format!(
            "the vault's slots wrap different versions of its installation key ({versions:?}): a slot was restored without the others; restore the vault's tables together"
        )));
    }
    let mut missed: Vec<(SlotKind, Miss)> = Vec::new();
    for kind in SlotKind::AT_START {
        let Some(slot) = slots.iter().find(|s| s.kind == kind) else { continue };
        match unlock(slot, opener).await {
            Ok(key) => {
                for (other, why) in &missed {
                    tracing::warn!(slot = other.as_str(), "the vault's {other} slot did not open ({why}); it opened with its {kind} slot");
                }
                return Ok((Arc::new(Installation { version: slot.version, key, made_at: slot.made_at }), kind));
            }
            Err(miss) => missed.push((kind, miss)),
        }
    }
    let mut said: Vec<String> = missed.iter().map(|(k, m)| format!("the {k} slot: {m}")).collect();
    if slots.iter().any(|s| s.kind == SlotKind::Recovery) {
        said.push("its recovery slot opens it only with the recovery key, through `ferrum task vault_rewrap with:recovery`".into());
    }
    Err(Error::Unavailable(format!("no slot opens the vault: {}", said.join("; "))))
}

/// A slot ready to be written: its key, and what makes that key again.
struct Made {
    kek: Key,
    salt: Option<Vec<u8>>,
    params: Value,
    kms_wrapped: Option<String>,
    /// A recovery key, to print once.
    printed: Option<Zeroizing<String>>,
}

impl Made {
    fn plain(kek: Key) -> Made {
        Made { kek, salt: None, params: json!({}), kms_wrapped: None, printed: None }
    }

    /// The row of a slot of `kind` that wraps `installation`.
    fn row(&self, kind: SlotKind, installation: &Installation) -> Result<SlotRow> {
        Ok(SlotRow {
            kind,
            version: installation.version,
            made_at: installation.made_at,
            wrapped: self.kek.wrap(&root_place(kind.as_str(), installation.version), &installation.key)?,
            check: self.kek.check_value(&check_place(kind.as_str())),
            salt: self.salt.clone(),
            params: self.params.clone(),
            kms_wrapped: self.kms_wrapped.clone(),
            created_at: Utc::now(),
        })
    }
}

async fn file_slot(opener: &Opener) -> Result<Made> {
    file_key(opener).map(Made::plain).map_err(|m| Error::Unavailable(format!("the file slot: {m}")))
}

/// A kms slot, when a key-management service is set here: the command's key, or a new key Transit wraps (and is
/// asked to open again at once, so that no slot is made that its service cannot open).
async fn kms_slot(opener: &Opener) -> Result<Option<Made>> {
    if let Some(command) = &opener.command {
        let key = kms_key(kms::from_command(command).await?);
        return Ok(Some(Made { params: json!({"how": "command"}), ..Made::plain(key) }));
    }
    if let Some(transit) = &opener.transit {
        let kek = Key::random()?;
        let wrapped = transit.wrap(&key_base64(&kek)).await?;
        let back = kms_key(kms::resolve(&wrapped, Some(transit)).await?);
        if back.bytes() != kek.bytes() {
            return Err(Error::Unavailable("the key-management service gave back another key than the one it was given".into()));
        }
        let params = json!({"how": "transit", "key": format!("{}/{}", transit.mount, transit.key)});
        return Ok(Some(Made { kek, salt: None, params, kms_wrapped: Some(wrapped), printed: None }));
    }
    Ok(None)
}

/// A credential slot for this process's database password, with a new salt; none without a password.
async fn credential_slot(opener: &Opener) -> Result<Option<Made>> {
    let Some(password) = password_of(&opener.database_url) else { return Ok(None) };
    let salt = random(16)?.to_vec();
    let kdf = opener.kdf;
    let s = salt.clone();
    let kek = blocking(move || password_key(password.as_bytes(), &s, kdf)).await?;
    Ok(Some(Made { kek, salt: Some(salt), params: json!({"kdf": kdf}), kms_wrapped: None, printed: None }))
}

/// A recovery slot with a new recovery key, printed once.
fn recovery_slot() -> Result<Made> {
    let recovery = Key::random()?;
    let salt = random(16)?.to_vec();
    let kek = recovery_slot_key(&recovery, &salt);
    Ok(Made { kek, salt: Some(salt), params: json!({}), kms_wrapped: None, printed: Some(print_recovery(&recovery)) })
}

async fn make_slot(kind: SlotKind, opener: &Opener) -> Result<Made> {
    match kind {
        SlotKind::File => file_slot(opener).await,
        SlotKind::Kms => kms_slot(opener)
            .await?
            .ok_or_else(|| Error::Invalid("no key-management service is set here (FERRUM_KMS_TRANSIT or FERRUM_SECRET_KEY_COMMAND)".into())),
        SlotKind::Credential => {
            credential_slot(opener).await?.ok_or_else(|| Error::Invalid("DATABASE_URL has no password, and the credential slot is made from it".into()))
        }
        SlotKind::Recovery => recovery_slot(),
    }
}

/// The best slot available here for a new vault: a key file, then a key-management service, then the database password;
/// none when there is none of them.
async fn best_slot(opener: &Opener) -> Result<Option<(SlotKind, Made)>> {
    // A key file that is there but cannot be used stops it: the operator meant it to be the root.
    if opener.file.exists() {
        return Ok(Some((SlotKind::File, file_slot(opener).await?)));
    }
    if let Some(made) = kms_slot(opener).await? {
        return Ok(Some((SlotKind::Kms, made)));
    }
    Ok(credential_slot(opener).await?.map(|made| (SlotKind::Credential, made)))
}

async fn write_slot(tx: &mut Tx, row: &SlotRow) -> Result<()> {
    sqlx::query(
        "insert into vault.root_slot (kind, installation_version, installation_made_at, wrapped, check_value, salt, params, kms_wrapped) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) on conflict (kind) do update set installation_version = excluded.installation_version, \
         installation_made_at = excluded.installation_made_at, wrapped = excluded.wrapped, check_value = excluded.check_value, salt = excluded.salt, \
         params = excluded.params, kms_wrapped = excluded.kms_wrapped, created_at = now()",
    )
    .bind(row.kind.as_str())
    .bind(row.version)
    .bind(row.made_at)
    .bind(&row.wrapped)
    .bind(&row.check)
    .bind(&row.salt)
    .bind(&row.params)
    .bind(&row.kms_wrapped)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// What making the vault's first slot came to.
enum Init {
    Made(Arc<Installation>, SlotKind),
    /// Another replica made it meanwhile.
    Exists,
    /// Nothing here can make a slot.
    Impossible,
}

/// Opens the vault with its slots, or makes it when it has none; none when it has none and nothing here can make one.
async fn open_now(db: &Db, opener: &Opener) -> Result<Option<(Arc<Installation>, SlotKind)>> {
    for _ in 0..2 {
        let slots = read_slots(db).await?;
        if !slots.is_empty() {
            return unlock_any(&slots, opener).await.map(Some);
        }
        match initialize(db, opener).await? {
            Init::Made(installation, by) => return Ok(Some((installation, by))),
            Init::Impossible => return Ok(None),
            Init::Exists => {}
        }
    }
    Err(Error::Unavailable("the vault's slots changed while it was being opened; start again".into()))
}

/// A new installation key with the best slot here, once: replicas starting together wait on the root's lock, and all
/// but the first find its slot and open with it (`None`).
async fn initialize(db: &Db, opener: &Opener) -> Result<Init> {
    // Keys without a slot are a vault whose slots were lost, never one to make again (nor to leave closed quietly).
    if held_keys(db).await? {
        return Err(lost_slots());
    }
    // Made before the lock: a key-management service may take its time.
    let Some((kind, made)) = best_slot(opener).await? else { return Ok(Init::Impossible) };
    let installation = Arc::new(Installation { version: 1, key: Key::random()?, made_at: Utc::now() });
    let row = made.row(kind, &installation)?;
    let mut tx = db.vault_tx(None).await?;
    lock(&mut tx, true).await?;
    let (slots, keys): (i64, bool) =
        sqlx::query_as("select (select count(*) from vault.root_slot), exists (select 1 from vault.key)").fetch_one(&mut *tx).await?;
    if slots > 0 {
        tx.rollback().await?;
        return Ok(Init::Exists);
    }
    if keys {
        return Err(lost_slots());
    }
    write_slot(&mut tx, &row).await?;
    super::audit_root(&mut tx, "made", json!({"slot": kind.as_str(), "installation_version": 1})).await?;
    tx.commit().await?;
    tracing::warn!(slot = kind.as_str(), "the vault was made: its {kind} slot opens it. Keep a recovery key as well: `ferrum task vault_recovery`");
    Ok(Init::Made(installation, kind))
}

fn lost_slots() -> Error {
    Error::Unavailable(
        "the vault holds keys and no slot to open them: restore vault.root_slot from the backup taken with them (a new installation key would open none of them)".into(),
    )
}

async fn held_keys(db: &Db) -> Result<bool> {
    let mut tx = db.vault_tx(None).await?;
    let keys: bool = sqlx::query_scalar("select exists (select 1 from vault.key)").fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(keys)
}

/// What the task that adds or replaces a slot prints: a recovery key, once.
pub type Printed = Option<Zeroizing<String>>;

/// Adds a slot of `kind`, or replaces the one there with a new key (rotating a slot): `file` from the key file here,
/// `kms` from the service set here, `credential` from this process's database password, `recovery` with a new key,
/// returned to be printed once. The installation key stays as it is; one row changes.
pub async fn put_slot(db: &Db, kind: SlotKind) -> Result<Printed> {
    let root = &db.vault;
    let installation = root.installation()?;
    let opener = root.opener().ok_or_else(|| Error::Unavailable("the vault is closed".into()))?;
    if kind == SlotKind::Credential
        && let Some(slot) = read_slots(db).await?.into_iter().find(|s| s.kind == SlotKind::Credential)
    {
        // A credential slot is made from this process's password: it must be the one the servers' address carries.
        match unlock(&slot, &opener).await {
            Ok(_) => {}
            Err(Miss::Wrong(_) | Miss::Absent(_)) => {
                return Err(Error::Invalid(
                    "this task's DATABASE_URL does not open the credential slot: run it with the servers' DATABASE_URL, or run `ferrum task vault_rewrap` if the database password changed".into(),
                ));
            }
            Err(m) => return Err(Error::Unavailable(format!("the credential slot: {m}"))),
        }
    }
    let made = make_slot(kind, &opener).await?;
    let row = made.row(kind, &installation)?;
    let mut tx = db.vault_tx(None).await?;
    lock(&mut tx, true).await?;
    if current_version(&mut tx).await? != Some(installation.version) {
        return Err(Error::Conflict("the installation key changed since this task opened the vault: run it again".into()));
    }
    let had: bool = sqlx::query_scalar("select exists (select 1 from vault.root_slot where kind = $1)").bind(kind.as_str()).fetch_one(&mut *tx).await?;
    write_slot(&mut tx, &row).await?;
    notify(&mut tx, "*").await?;
    super::audit_root(&mut tx, if had { "slot_replaced" } else { "slot_added" }, json!({"slot": kind.as_str()})).await?;
    tx.commit().await?;
    Ok(made.printed)
}

/// Removes the slot of `kind`; never the last one a server opens the vault with (file, kms or credential).
pub async fn remove_slot(db: &Db, kind: SlotKind) -> Result<()> {
    db.vault.installation()?;
    let mut tx = db.vault_tx(None).await?;
    lock(&mut tx, true).await?;
    let kinds: Vec<String> = sqlx::query_scalar("select kind from vault.root_slot").fetch_all(&mut *tx).await?;
    if !kinds.iter().any(|k| k == kind.as_str()) {
        return Err(Error::NotFound(format!("{kind} slot")));
    }
    let left = kinds.iter().filter(|k| *k != kind.as_str()).filter_map(|k| SlotKind::parse(k)).filter(|k| SlotKind::AT_START.contains(k)).count();
    if left == 0 {
        return Err(Error::Conflict(format!("the {kind} slot is the last one a server opens the vault with: add another first (file, kms or credential)")));
    }
    sqlx::query("delete from vault.root_slot where kind = $1").bind(kind.as_str()).execute(&mut *tx).await?;
    notify(&mut tx, "*").await?;
    super::audit_root(&mut tx, "slot_removed", json!({"slot": kind.as_str()})).await?;
    tx.commit().await?;
    Ok(())
}

/// What becomes of the recovery slot when the installation key is rotated.
pub enum RecoveryKey {
    /// It is wrapped again with its key, given once.
    Keep(Zeroizing<String>),
    /// It is replaced by a new one, printed once.
    New,
}

/// A new installation key: every slot is wrapped again with it (each slot's key had here, or nothing changes), and
/// every workspace key wrapped again under it, in one transaction. Other replicas hear of it and open the vault again;
/// what they hold keeps working meanwhile. A recovery slot needs `recovery`; a new recovery key is returned to print.
pub async fn rotate_installation_key(db: &Db, recovery: Option<RecoveryKey>) -> Result<Printed> {
    let root = &db.vault;
    let held = root.installation()?;
    let opener = root.opener().ok_or_else(|| Error::Unavailable("the vault is closed".into()))?;
    let slots = read_slots(db).await?;
    let mut made: Vec<(SlotKind, Made)> = Vec::new();
    let mut printed = None;
    for slot in &slots {
        let m = match (slot.kind, &recovery) {
            (SlotKind::Recovery, None) => {
                return Err(Error::Invalid(
                    "the recovery slot is wrapped again only with its key: give it (recovery:keep, then the key on standard input), or have a new one printed (recovery:new)".into(),
                ));
            }
            (SlotKind::Recovery, Some(RecoveryKey::New)) => {
                let m = recovery_slot()?;
                printed = m.printed.clone();
                m
            }
            (SlotKind::Recovery, Some(RecoveryKey::Keep(typed))) => {
                let typed = read_recovery(typed).ok_or_else(|| Error::Invalid("that is not a recovery key".into()))?;
                let salt = slot.salt.clone().ok_or_else(|| Error::Unavailable("the recovery slot is damaged: its row holds no salt".into()))?;
                let kek = recovery_slot_key(&typed, &salt);
                open_slot(slot, &kek, &opener).map_err(|m| Error::Invalid(m.to_string()))?;
                Made { kek, salt: Some(salt), params: slot.params.clone(), kms_wrapped: None, printed: None }
            }
            (kind, _) => {
                let kek = slot_key(slot, &opener).await.map_err(|m| Error::Unavailable(format!("the {kind} slot cannot be wrapped again here: {m}")))?;
                open_slot(slot, &kek, &opener).map_err(|m| Error::Unavailable(format!("the {kind} slot: {m}")))?;
                Made { kek, salt: slot.salt.clone(), params: slot.params.clone(), kms_wrapped: slot.kms_wrapped.clone(), printed: None }
            }
        };
        made.push((slot.kind, m));
    }
    let new = Arc::new(Installation { version: held.version + 1, key: Key::random()?, made_at: Utc::now() });
    let mut tx = db.vault_tx(None).await?;
    lock(&mut tx, true).await?;
    let kinds: Vec<String> = sqlx::query_scalar("select kind from vault.root_slot order by kind").fetch_all(&mut *tx).await?;
    if current_version(&mut tx).await? != Some(held.version) || kinds != slots.iter().map(|s| s.kind.as_str().to_string()).collect::<Vec<_>>() {
        return Err(Error::Conflict("the vault's slots changed since this task read them: run it again".into()));
    }
    for (kind, m) in &made {
        write_slot(&mut tx, &m.row(*kind, &new)?).await?;
    }
    let rewrapped = rewrap_workspace_keys(&mut tx, &held, &new).await?;
    notify(&mut tx, "*").await?;
    super::audit_root(&mut tx, "installation_key_rotated", json!({"installation_version": new.version, "workspace_keys": rewrapped})).await?;
    tx.commit().await?;
    let by = root.opened().unwrap_or(SlotKind::Credential);
    root.set_open(new, opener, by);
    Ok(printed)
}

/// Every workspace key, wrapped by `new` instead of `held` (in the rotation's transaction).
async fn rewrap_workspace_keys(tx: &mut Tx, held: &Installation, new: &Installation) -> Result<usize> {
    let rows: Vec<(Uuid, i32, i32, Vec<u8>)> =
        sqlx::query_as("select scope, version, parent_version, wrapped from vault.key where owner = '' order by scope, version for update")
            .fetch_all(&mut **tx)
            .await?;
    for chunk in rows.chunks(1000) {
        let (mut scopes, mut versions, mut wrapped) = (Vec::new(), Vec::new(), Vec::new());
        for (scope, version, parent, w) in chunk {
            if *parent != held.version {
                return Err(Error::Internal(format!(
                    "the workspace key of {scope} is wrapped by installation key version {parent}, not the current {}",
                    held.version
                )));
            }
            let key = held
                .key
                .unwrap(&key_place(*scope, "", *version, *parent), w)
                .ok_or_else(|| Error::Internal(format!("the workspace key of {scope} (version {version}) does not open: its row was changed or damaged")))?;
            scopes.push(*scope);
            versions.push(*version);
            wrapped.push(new.key.wrap(&key_place(*scope, "", *version, new.version), &key)?);
        }
        sqlx::query(
            "update vault.key k set wrapped = u.w, parent_version = $1 from unnest($2::uuid[], $3::int[], $4::bytea[]) u(s, v, w) \
             where k.scope = u.s and k.owner = '' and k.version = u.v",
        )
        .bind(new.version)
        .bind(&scopes)
        .bind(&versions)
        .bind(&wrapped)
        .execute(&mut **tx)
        .await?;
    }
    Ok(rows.len())
}

/// How the credential slot is opened once more, after the database password changed: with the old password or the
/// recovery key, each given once.
pub enum Rewrap {
    OldPassword(Zeroizing<String>),
    Recovery(Zeroizing<String>),
}

/// Makes the credential slot again for this process's database password, opening the vault with the old password or
/// the recovery key: what to do when the password changed or was lost. Leaves the vault open here.
pub async fn rewrap(db: &Db, with: Rewrap) -> Result<()> {
    let root = &db.vault;
    let opener = root.opener().ok_or_else(|| Error::Unavailable("the vault was not opened by this process".into()))?;
    let slots = read_slots(db).await?;
    let find = |kind: SlotKind| slots.iter().find(|s| s.kind == kind).ok_or_else(|| Error::NotFound(format!("{kind} slot")));
    let (slot, kek) = match with {
        Rewrap::OldPassword(old) => {
            let slot = find(SlotKind::Credential)?;
            let salt = slot.salt.clone().ok_or_else(|| Error::Unavailable("the credential slot is damaged: its row holds no salt".into()))?;
            let kdf: Kdf = serde_json::from_value(slot.params["kdf"].clone())
                .map_err(|_| Error::Unavailable("the credential slot is damaged: its row does not say Argon2id's costs".into()))?;
            (slot, blocking(move || password_key(old.as_bytes(), &salt, kdf)).await?)
        }
        Rewrap::Recovery(typed) => {
            let slot = find(SlotKind::Recovery)?;
            let typed = read_recovery(&typed).ok_or_else(|| Error::Invalid("that is not a recovery key".into()))?;
            let salt = slot.salt.clone().ok_or_else(|| Error::Unavailable("the recovery slot is damaged: its row holds no salt".into()))?;
            (slot, recovery_slot_key(&typed, &salt))
        }
    };
    let key = match open_slot(slot, &kek, &opener) {
        Ok(k) => k,
        Err(Miss::Wrong(_)) if slot.kind == SlotKind::Credential => {
            return Err(Error::Invalid("that is not the password the credential slot was made with".into()));
        }
        Err(m) => return Err(Error::Invalid(m.to_string())),
    };
    let installation = Arc::new(Installation { version: slot.version, key, made_at: slot.made_at });
    let made = credential_slot(&opener).await?.ok_or_else(|| Error::Invalid("DATABASE_URL has no password, and the credential slot is made from it".into()))?;
    let row = made.row(SlotKind::Credential, &installation)?;
    let mut tx = db.vault_tx(None).await?;
    lock(&mut tx, true).await?;
    if current_version(&mut tx).await? != Some(installation.version) {
        return Err(Error::Conflict("the installation key changed meanwhile: run it again".into()));
    }
    write_slot(&mut tx, &row).await?;
    notify(&mut tx, "*").await?;
    super::audit_root(&mut tx, "credential_rewrapped", json!({"with": if slot.kind == SlotKind::Recovery { "recovery" } else { "old_password" }})).await?;
    tx.commit().await?;
    root.set_open(installation, opener, SlotKind::Credential);
    Ok(())
}

/// A slot, as the status shows it.
#[derive(Debug, Clone, Serialize)]
pub struct SlotStatus {
    pub kind: SlotKind,
    pub made_at: DateTime<Utc>,
    /// For a kms slot: how the service is asked, and which Transit key wraps it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub how: Option<String>,
}

/// The vault as an operator reads it (`ferrum task vault_status`, the doctor): never a key.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    pub open: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_because: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opened_with: Option<SlotKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installation_version: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installation_made_at: Option<DateTime<Utc>>,
    pub slots: Vec<SlotStatus>,
    pub workspace_keys: i64,
    pub owner_keys: i64,
    pub secrets: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_workspace_key: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_owner_key: Option<DateTime<Utc>>,
    /// Owner keys with secrets still sealed by an older version: (workspace, or none for the installation; owner;
    /// how many; since when the newest version is there).
    pub resealing: Vec<Resealing>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Resealing {
    pub workspace: Option<Uuid>,
    pub owner: String,
    pub secrets: i64,
    pub since: DateTime<Utc>,
}

/// What the vault of `db` holds, counted, and how this process holds it.
pub async fn status(db: &Db) -> Result<Status> {
    let mut st = Status::default();
    match db.vault.opened() {
        Ok(by) => {
            st.open = true;
            st.opened_with = Some(by);
        }
        Err(why) => st.closed_because = Some(why),
    }
    if !migrated(db).await? {
        return Ok(st);
    }
    let slots = read_slots(db).await?;
    st.installation_version = slots.first().map(|s| s.version);
    st.installation_made_at = slots.first().map(|s| s.made_at);
    st.slots = slots
        .iter()
        .map(|s| SlotStatus {
            kind: s.kind,
            made_at: s.created_at,
            how: (s.kind == SlotKind::Kms).then(|| match (s.params["how"].as_str(), s.params["key"].as_str()) {
                (Some(how), Some(key)) => format!("{how} {key}"),
                (Some(how), None) => how.to_string(),
                _ => "?".into(),
            }),
        })
        .collect();
    let mut tx = db.vault_tx(None).await?;
    let counts: (i64, i64, i64, Option<DateTime<Utc>>, Option<DateTime<Utc>>) = sqlx::query_as(
        "select (select count(*) from vault.key where owner = ''), (select count(*) from vault.key where owner <> ''), \
         (select count(*) from vault.secret where valid_until is null), \
         (select min(created_at) from vault.key k where owner = '' and version = (select max(version) from vault.key x where x.scope = k.scope and x.owner = '')), \
         (select min(created_at) from vault.key k where owner <> '' and version = (select max(version) from vault.key x where x.scope = k.scope and x.owner = k.owner))",
    )
    .fetch_one(&mut *tx)
    .await?;
    (st.workspace_keys, st.owner_keys, st.secrets, st.oldest_workspace_key, st.oldest_owner_key) = counts;
    let resealing: Vec<(Uuid, String, i64, DateTime<Utc>)> = sqlx::query_as(
        "select s.scope, s.owner, count(*), max(k.created_at) from vault.secret s \
         join vault.key k on k.scope = s.scope and k.owner = s.owner and k.version = (select max(version) from vault.key x where x.scope = s.scope and x.owner = s.owner) \
         where s.key_version < k.version group by s.scope, s.owner order by 1, 2",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    st.resealing = resealing
        .into_iter()
        .map(|(scope, owner, secrets, since)| Resealing { workspace: (!scope.is_nil()).then_some(scope), owner, secrets, since })
        .collect();
    Ok(st)
}

/// How each slot fares with what this process has (the doctor): `Ok` for a slot that opens the vault here, else why
/// not. The recovery slot is never tried: its key is offline.
pub(crate) async fn try_slots(db: &Db) -> Result<Vec<(SlotKind, std::result::Result<(), String>)>> {
    let Some(opener) = db.vault.opener() else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for slot in read_slots(db).await? {
        if slot.kind == SlotKind::Recovery {
            continue;
        }
        out.push((slot.kind, unlock(&slot, &opener).await.map(|_| ()).map_err(|m| m.to_string())));
    }
    Ok(out)
}

/// The doctor's name for a slot's check.
fn slot_check(kind: SlotKind) -> &'static str {
    match kind {
        SlotKind::File => "vault.slot.file",
        SlotKind::Kms => "vault.slot.kms",
        SlotKind::Credential => "vault.slot.credential",
        SlotKind::Recovery => "vault.slot.recovery",
    }
}

impl crate::Kernel {
    /// What the doctor says of the vault (`ferrum task doctor`): whether it is open and with which slot, which slots
    /// there are and whether each opens it here, whether there is a recovery key, how old its keys are, and what waits
    /// to be sealed again. Never a key.
    pub async fn vault_findings(&self) -> Result<Vec<crate::doctor::Finding>> {
        use crate::doctor::Finding;
        let st = status(&self.db).await?;
        let mut out = Vec::new();
        match (&st.opened_with, &st.closed_because) {
            (Some(by), _) => out.push(Finding::ok("vault", format!("open: its {by} slot opened it"))),
            (None, Some(why)) if self.db.vault.rootless() => {
                out.push(Finding::warning(
                    "vault",
                    format!("the vault is closed: {why}"),
                    "Give it a root: a password in DATABASE_URL, a key file at /run/ferrum/root.key, or a key-management service; the next start makes it.",
                ));
                return Ok(out);
            }
            (None, _) if self.db.vault.opener().is_none() => {
                out.push(Finding::off("vault", "this process did not open the vault"));
                return Ok(out);
            }
            (None, why) => {
                out.push(Finding::blocker(
                    "vault",
                    format!("the vault is closed: {}", why.as_deref().unwrap_or("it was not opened")),
                    "Each slot's reason says what it needs; docs/operations.md, \"The vault\".",
                ));
                return Ok(out);
            }
        }
        let kinds: Vec<&str> = st.slots.iter().map(|s| s.kind.as_str()).collect();
        out.push(Finding::ok("vault.slots", format!("slots: {}", kinds.join(", "))));
        out.push(match st.slots.iter().find(|s| s.kind == SlotKind::Recovery) {
            Some(s) => Finding::ok("vault.recovery", format!("a recovery key made {} opens it when nothing else does", s.made_at.format("%Y-%m-%d"))),
            None => Finding::warning(
                "vault.recovery",
                "the vault has no recovery key: if its slots' keys are lost (the database password reset, the key file gone), every secret is lost",
                "Run ferrum task vault_recovery and keep what it prints offline.",
            ),
        });
        for (kind, opens) in try_slots(&self.db).await? {
            out.push(match opens {
                Ok(()) => Finding::ok(slot_check(kind), format!("the {kind} slot opens the vault here")),
                Err(why) => Finding::warning(
                    slot_check(kind),
                    format!("the {kind} slot does not open the vault here: {why}"),
                    "A replica that cannot use one slot opens with another; make this one again (ferrum task vault_add_slot) or remove it.",
                ),
            });
        }
        let day = |t: Option<DateTime<Utc>>| t.map_or("-".to_string(), |t| t.format("%Y-%m-%d").to_string());
        out.push(Finding::ok(
            "vault.keys",
            format!(
                "installation key version {} made {}; {} workspace keys, the oldest in use made {}; {} owner keys, the oldest in use made {}; {} secrets",
                st.installation_version.unwrap_or(0),
                day(st.installation_made_at),
                st.workspace_keys,
                day(st.oldest_workspace_key),
                st.owner_keys,
                day(st.oldest_owner_key),
                st.secrets
            ),
        ));
        let late: Vec<String> = st
            .resealing
            .iter()
            .filter(|r| Utc::now() - r.since > chrono::Duration::days(1))
            .map(|r| format!("{} of {} in {}", r.secrets, r.owner, r.workspace.map_or("the installation".into(), |w| w.to_string())))
            .collect();
        out.push(if late.is_empty() {
            Finding::ok("vault.resealing", format!("{} owner keys have secrets being sealed again", st.resealing.len()))
        } else {
            Finding::warning(
                "vault.resealing",
                format!("secrets still sealed with an older key a day after it was rotated: {}", late.join("; ")),
                "The job platform.vault_reseal and the hourly upkeep seal them again; ferrum task vault_rotate level:owner … wait:yes does it now.",
            )
        });
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_password_is_read_from_the_address_as_sqlx_reads_it() {
        let pw = |u: &str| password_of(u).map(|p| p.to_string());
        assert_eq!(pw("postgres://ferrum:s3cret@db:5432/ferrum").as_deref(), Some("s3cret"));
        assert_eq!(pw("postgres://ferrum:p%40ss%2Fw0rd@db/ferrum").as_deref(), Some("p@ss/w0rd"), "percent-encoded");
        assert_eq!(pw("postgres://db/ferrum?user=ferrum&password=q%26r").as_deref(), Some("q&r"), "as a parameter");
        assert_eq!(pw("postgres://ferrum@db/ferrum"), None);
        assert_eq!(pw("postgres://ferrum:@db/ferrum"), None, "empty is none");
        assert_eq!(pw("not an address"), None);
        assert_eq!(pw("postgres://ferrum:%zz@db/ferrum"), None, "a broken escape is no password");
        let o = Opener { database_url: "postgres://ferrum:s3cret@db/ferrum".into(), ..Default::default() };
        assert!(!format!("{o:?}").contains("s3cret"), "an opener never shows the address: {o:?}");
    }

    #[test]
    fn slot_kinds_read_back() {
        for k in [SlotKind::File, SlotKind::Kms, SlotKind::Credential, SlotKind::Recovery] {
            assert_eq!(SlotKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(SlotKind::parse("password"), None);
        assert!(!SlotKind::AT_START.contains(&SlotKind::Recovery), "a server never opens with the recovery key");
    }
}
