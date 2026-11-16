//! Secrets: what each owner keeps in the vault, under a rule, versioned, each version sealed with a version of its
//! owner's key. Code uses them (`sign`, `verify`) and has the raw value only where the rule lets it (`reveal`); people
//! set, rotate and remove them with a right for the owner and a confirmed identity, and are never shown a value except
//! one the vault made for them, once. Every `reveal`, `sign`, `set`, `rotate` and `remove` is on the audit trail
//! (owner, name, verb, workspace; never a value), the uses under a cap per hour, as sign-in failures are; the day a
//! secret was last used is kept on it.

use super::seal::{self, secret_place};
use super::{Scope, Vault};
use crate::controls::Proof;
use crate::ctx::Ctx;
use crate::db::{Db, Tx, with_retry};
use crate::error::{Error, Result};
use crate::jobs::{Job, JobHandler};
use crate::{Kernel, audit};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;
use zeroize::Zeroizing;

/// The longest a rotation's overlap may be.
pub const MAX_OVERLAP_SECS: i64 = 30 * 86_400;
/// How many uses of one secret by one verb in an hour are recorded one by one; the rest are counted and recorded as
/// one summary once the hour is over (as sign-in failures are, `gate::FAILURES_PER_HOUR`).
pub const USES_PER_HOUR: i32 = 100;

/// What may be done with a secret, kept with it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Rule {
    /// Whether its owner's code may have the raw value ([`Vault::reveal`]). If not, it is only used: to sign, to check
    /// a signature, as a credential the platform adds to an outbound request.
    pub reveal: bool,
    /// The hosts it may be sent to (`api.example.com`; `*.example.com` for its subdomains), when the platform adds it
    /// to an outbound request.
    pub hosts: Vec<String>,
    /// What a sandbox made of its workspace has of it.
    pub sandbox: Sandbox,
    /// For how long, after a rotation, the version before still verifies (a signing key's), in seconds.
    pub overlap_secs: i64,
    /// When it stops being usable.
    pub expires_at: Option<DateTime<Utc>>,
}

/// What a sandbox has of a secret: nothing (`blank`, the default), a value of its own (`regenerate`), or this one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sandbox {
    #[default]
    Blank,
    Regenerate,
    Keep,
}

fn host_ok(host: &str) -> bool {
    let h = host.strip_prefix("*.").unwrap_or(host);
    (1..=253).contains(&h.len())
        && h.split('.').all(|l| {
            (1..=63).contains(&l.len())
                && l.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
}

impl Rule {
    fn check(&self) -> Result<()> {
        if self.hosts.len() > 20 {
            return Err(Error::field("hosts", "at most 20"));
        }
        if let Some(h) = self.hosts.iter().find(|h| !host_ok(h)) {
            return Err(Error::field("hosts", format!("{h:?} is not a host's name in lowercase (api.example.com, or *.example.com for its subdomains)")));
        }
        if !(0..=MAX_OVERLAP_SECS).contains(&self.overlap_secs) {
            return Err(Error::field("overlap_secs", "between 0 and 30 days, in seconds"));
        }
        if self.expires_at.is_some_and(|t| t <= Utc::now()) {
            return Err(Error::field("expires_at", "a time to come"));
        }
        Ok(())
    }
}

/// What a secret holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A value: a password, a token, a key; signs and verifies as an HMAC-SHA256 key.
    Value,
    /// An Ed25519 key pair the vault made: its private half never leaves unless the rule reveals it.
    Ed25519,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Value => "value",
            Kind::Ed25519 => "ed25519",
        }
    }

    fn parse(s: &str) -> Kind {
        if s == "ed25519" { Kind::Ed25519 } else { Kind::Value }
    }
}

/// A secret's new value.
pub enum New {
    /// This value, at most [`crate::secret::MAX_SECRET`] bytes.
    Value(Zeroizing<Vec<u8>>),
    /// A random value of this many bytes (16 to 64), made by the vault as text (base64url): shown once to the person
    /// who asked for it.
    Random(usize),
    /// An Ed25519 key pair made by the vault: its public half is described, its private half kept.
    Ed25519,
}

impl std::fmt::Debug for New {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            New::Value(_) => f.write_str("New::Value(..)"),
            New::Random(n) => write!(f, "New::Random({n})"),
            New::Ed25519 => f.write_str("New::Ed25519"),
        }
    }
}

/// A secret as anyone may read of it: never its value.
#[derive(Debug, Clone, Serialize)]
pub struct Described {
    pub owner: String,
    /// None: the installation's own.
    pub workspace: Option<Uuid>,
    pub name: String,
    pub kind: Kind,
    pub version: i32,
    pub rule: Rule,
    /// When this version was set, and by whom (none: by code).
    pub set_at: DateTime<Utc>,
    pub set_by: Option<Uuid>,
    /// The day it was last used.
    pub last_used: Option<NaiveDate>,
    /// An Ed25519 key's public half, base64url.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    /// Until when the version before this one still verifies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_until: Option<DateTime<Utc>>,
    /// The app that manages it itself ([`Vault::manage`]): people's routes leave it to that app. None: people's to change.
    pub managed_by: Option<String>,
}

/// Who changes a secret, for the trail.
struct Who<'a> {
    actor: Option<Uuid>,
    request: &'a str,
    /// What acted when no person did, said in the record: `operator`, `environment`, `sandbox`.
    by: Option<&'a str>,
    /// The service whose render made, used or changed it (crate::companion), said in the record.
    service: Option<&'a str>,
}

/// A secret's name in its shape: lowercase letters, digits, and _ . -, up to 100.
fn valid_name(name: &str) -> Result<()> {
    let b = name.as_bytes();
    let ok = (1..=100).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.-".contains(c));
    if ok { Ok(()) } else { Err(Error::field("name", "lowercase letters, digits, and _ . -, up to 100")) }
}

/// Whether the verbs take `name` as a secret's name ([`check_name`]): what a service declares the vault makes for it.
pub(crate) fn secret_name_ok(name: &str) -> bool {
    check_name(name).is_ok()
}

/// A name the verbs take: in its shape, and not one of a value the kernel's stores keep.
fn check_name(name: &str) -> Result<()> {
    valid_name(name)?;
    // What the kernel's own stores keep (kept.rs) changes where it is set, never through these verbs.
    if super::kept::Store::ALL.iter().any(|s| name.strip_prefix(s.kind()).is_some_and(|rest| rest.starts_with('.'))) {
        return Err(Error::field("name", "this name is one the kernel's stores keep: change the value where it is set"));
    }
    Ok(())
}

/// A version of a secret as stored.
pub(super) struct Row {
    pub(super) version: i32,
    key_version: i32,
    kind: Kind,
    sealed: Vec<u8>,
    public_key: Option<Vec<u8>>,
    pub(super) rule: Rule,
    pub(super) valid_until: Option<DateTime<Utc>>,
    set_at: DateTime<Utc>,
    set_by: Option<Uuid>,
    last_used: Option<NaiveDate>,
}

type RowTuple = (i32, i32, String, Vec<u8>, Option<Vec<u8>>, Value, Option<DateTime<Utc>>, DateTime<Utc>, Option<Uuid>, Option<NaiveDate>);

/// The current version of a secret first, then the version before it while it still verifies.
pub(super) async fn rows(tx: &mut Tx, scope: Scope, owner: &str, name: &str) -> Result<Vec<Row>> {
    let rows: Vec<RowTuple> = sqlx::query_as(
        "select version, key_version, kind, sealed, public_key, rule, valid_until, created_at, created_by, last_used from vault.secret \
         where scope = $1 and owner = $2 and name = $3 and (valid_until is null or valid_until > now()) order by valid_until is not null, version desc",
    )
    .bind(scope.id())
    .bind(owner)
    .bind(name)
    .fetch_all(&mut **tx)
    .await?;
    rows.into_iter()
        .map(|(version, key_version, kind, sealed, public_key, rule, valid_until, set_at, set_by, last_used)| {
            let rule = serde_json::from_value(rule).map_err(|e| Error::Internal(format!("{owner}'s secret {name} has a rule that cannot be read: {e}")))?;
            Ok(Row { version, key_version, kind: Kind::parse(&kind), sealed, public_key, rule, valid_until, set_at, set_by, last_used })
        })
        .collect()
}

fn described(scope: Scope, owner: &str, name: &str, rows: &[Row]) -> Option<Described> {
    let current = rows.iter().find(|r| r.valid_until.is_none())?;
    Some(Described {
        owner: owner.to_string(),
        workspace: scope.tenant(),
        name: name.to_string(),
        kind: current.kind,
        version: current.version,
        rule: current.rule.clone(),
        set_at: current.set_at,
        set_by: current.set_by,
        last_used: current.last_used,
        public_key: current.public_key.as_ref().map(|k| B64URL.encode(k)),
        previous_until: rows.iter().filter_map(|r| r.valid_until).max(),
        managed_by: None,
    })
}

/// `d` with its app named where that app manages it ([`Vault::manage`]).
async fn with_managed(tx: &mut Tx, scope: Scope, mut d: Described) -> Result<Described> {
    let managed: bool = sqlx::query_scalar("select exists (select 1 from vault.managed where scope = $1 and owner = $2 and name = $3)")
        .bind(scope.id())
        .bind(&d.owner)
        .bind(&d.name)
        .fetch_one(&mut **tx)
        .await?;
    d.managed_by = managed.then(|| d.owner.clone());
    Ok(d)
}

/// Whether `owner` manages `name` in `scope` itself, in `tx`.
async fn is_managed(tx: &mut Tx, scope: Scope, owner: &str, name: &str) -> Result<bool> {
    Ok(sqlx::query_scalar("select exists (select 1 from vault.managed where scope = $1 and owner = $2 and name = $3)")
        .bind(scope.id())
        .bind(owner)
        .bind(name)
        .fetch_one(&mut **tx)
        .await?)
}

/// Whether `owner` manages `name` in `scope` itself.
async fn managed(k: &Kernel, scope: Scope, owner: &str, name: &str) -> Result<bool> {
    let mut tx = k.db.vault_tx(scope.tenant()).await?;
    let managed: bool = sqlx::query_scalar("select exists (select 1 from vault.managed where scope = $1 and owner = $2 and name = $3)")
        .bind(scope.id())
        .bind(owner)
        .bind(name)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(managed)
}

/// Refused, naming the app, when `owner` manages `name` itself: what people's routes and an operator's regenerate ask.
async fn not_managed(k: &Kernel, scope: Scope, owner: &str, name: &str) -> Result<()> {
    if managed(k, scope, owner, name).await? {
        return Err(managed_by(owner, name));
    }
    Ok(())
}

/// The refusal of a change to a secret its app manages: `409`, code [`MANAGED_BY_APP`], the app's id as its own field (a
/// screen names the app in the person's language), and a sentence for every other client.
fn managed_by(owner: &str, name: &str) -> Error {
    Error::Refused {
        code: MANAGED_BY_APP,
        status: 409,
        message: format!("{name} is managed by its app, {owner}: it is changed there, not here"),
        pointer: Some(owner.to_string()),
    }
}

/// The code of [`managed_by`]'s refusal; the server answers with the app's id beside it (`app`).
pub const MANAGED_BY_APP: &str = "managed_by_app";

/// The current version, or refused: not there, or expired.
fn current<'a>(rows: &'a [Row], name: &str) -> Result<&'a Row> {
    let row = rows.iter().find(|r| r.valid_until.is_none()).ok_or_else(|| Error::NotFound("secret".into()))?;
    if let Some(at) = row.rule.expires_at.filter(|at| *at <= Utc::now()) {
        return Err(Error::Forbidden(format!("{name} expired on {}", at.format("%Y-%m-%d %H:%M UTC"))));
    }
    Ok(row)
}

/// What a version holds, opened with the version of the owner key that sealed it.
pub(super) async fn opened(db: &Db, tx: &mut Tx, scope: Scope, owner: &str, name: &str, row: &Row) -> Result<Zeroizing<Vec<u8>>> {
    let key = db.vault.owner_key(db, tx, scope.id(), owner, row.key_version).await?;
    key.open(&secret_place(scope.id(), owner, name, row.version, row.key_version), &row.sealed).ok_or_else(|| {
        Error::Internal(format!(
            "{owner}'s secret {name} (version {}) does not open: its row was changed or damaged; restore the vault's tables from a backup",
            row.version
        ))
    })
}

/// Whether this transaction may write. A fenced database's are read only (db.rs): a secret is still used there, and
/// its use is not recorded.
async fn writable(tx: &mut Tx) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, String>("select current_setting('transaction_read_only')").fetch_one(&mut **tx).await? == "off")
}

/// Keeps, coarsely, the day a secret was last used: one write a day at most.
async fn used(tx: &mut Tx, scope: Scope, owner: &str, name: &str, row: &Row) -> Result<()> {
    if row.last_used != Some(Utc::now().date_naive()) && writable(tx).await? {
        sqlx::query(
            "update vault.secret set last_used = (now() at time zone 'utc')::date where scope = $1 and owner = $2 and name = $3 and version = $4 \
             and last_used is distinct from (now() at time zone 'utc')::date",
        )
        .bind(scope.id())
        .bind(owner)
        .bind(name)
        .bind(row.version)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Records an event of a scope: a workspace's on its own audit trail; the installation's in the installation's own
/// record of its vault (`vault.event`), which no workspace sees.
async fn event(tx: &mut Tx, scope: Scope, actor: Option<Uuid>, request: &str, category: &'static str, verb: &str, diff: Value) -> Result<()> {
    match scope {
        Scope::Workspace(tenant) => audit::record_as(tx, tenant, actor, request, category, "Vault", Uuid::now_v7(), verb, None, diff).await,
        Scope::Installation => {
            let (owner, name) = (diff["owner"].as_str().unwrap_or_default().to_string(), diff["name"].as_str().unwrap_or_default().to_string());
            super::record(tx, verb, &owner, &name, diff, actor, request).await
        }
    }
}

/// Records a use or a change of a secret. A use (`capped`) is counted for its hour: past [`USES_PER_HOUR`] it is only
/// counted, and the hours that are over get their summary ([`flush_uses`]).
async fn audited(tx: &mut Tx, scope: Scope, owner: &str, name: &str, verb: &'static str, who: &Who<'_>, version: i32, capped: bool) -> Result<()> {
    if capped {
        if !writable(tx).await? {
            return Ok(());
        }
        flush_uses(tx, Some((scope, owner, name, verb))).await?;
        // ponytail: one counter row per secret, verb and hour, so uses of one secret take turns on it; count in memory
        // per replica, flushed now and then, if one secret is ever used thousands of times a second.
        let n: i32 = sqlx::query_scalar(
            "insert into vault.use_count (scope, owner, name, verb, hour, n) values ($1, $2, $3, $4, date_trunc('hour', now()), 1) \
             on conflict (scope, owner, name, verb, hour) do update set n = vault.use_count.n + 1 returning n",
        )
        .bind(scope.id())
        .bind(owner)
        .bind(name)
        .bind(verb)
        .fetch_one(&mut **tx)
        .await?;
        if n > USES_PER_HOUR {
            return Ok(());
        }
    }
    let category = if capped { audit::ACCESS } else { audit::ADMIN };
    let mut diff = json!({"owner": owner, "name": name, "version": version});
    if let Some(by) = who.by {
        diff["by"] = json!(by);
    }
    if let Some(service) = who.service {
        diff["service"] = json!(service);
    }
    event(tx, scope, who.actor, who.request, category, verb, diff).await
}

/// Records a reveal the kernel made of `name` for its owner, in `tx` (the vault's tables seen): as [`Vault::reveal`]
/// records its own (kept.rs, a secret field's value read by its app).
pub(super) async fn revealed(tx: &mut Tx, scope: Scope, owner: &str, name: &str) -> Result<()> {
    let rows = rows(tx, scope, owner, name).await?;
    let Some(row) = rows.iter().find(|r| r.valid_until.is_none()) else { return Ok(()) };
    audited(tx, scope, owner, name, "reveal", &CODE, row.version, true).await?;
    used(tx, scope, owner, name, row).await
}

/// Hours that are over, of one secret's verb or of all: a summary of each that held more uses than were recorded.
pub(crate) async fn flush_uses(tx: &mut Tx, which: Option<(Scope, &str, &str, &str)>) -> Result<u64> {
    let done: Vec<(Uuid, String, String, String, DateTime<Utc>, i32)> = sqlx::query_as(
        "delete from vault.use_count where hour < date_trunc('hour', now()) \
         and ($1::uuid is null or (scope = $1 and owner = $2 and name = $3 and verb = $4)) returning scope, owner, name, verb, hour, n",
    )
    .bind(which.map(|w| w.0.id()))
    .bind(which.map(|w| w.1))
    .bind(which.map(|w| w.2))
    .bind(which.map(|w| w.3))
    .fetch_all(&mut **tx)
    .await?;
    let mut n = 0;
    for (scope, owner, name, verb, hour, count) in done.into_iter().filter(|d| d.5 > USES_PER_HOUR) {
        let scope = if scope.is_nil() { Scope::Installation } else { Scope::Workspace(scope) };
        let diff = json!({"owner": owner, "name": name, "hour": hour, "count": count - USES_PER_HOUR});
        event(tx, scope, None, "", audit::ACCESS, &format!("{verb}_summary"), diff).await?;
        n += 1;
    }
    Ok(n)
}

/// The value a new version holds, its kind, its public half, and what is shown once.
type Made = (Kind, Zeroizing<Vec<u8>>, Option<Vec<u8>>, Option<Zeroizing<String>>);

fn make(value: New) -> Result<Made> {
    match value {
        New::Value(v) if v.is_empty() || v.len() > crate::secret::MAX_SECRET => {
            Err(Error::field("value", format!("between 1 and {} bytes", crate::secret::MAX_SECRET)))
        }
        New::Value(v) => Ok((Kind::Value, v, None, None)),
        New::Random(n) if !(16..=64).contains(&n) => Err(Error::field("value", "a random value is 16 to 64 bytes")),
        New::Random(n) => {
            let text = Zeroizing::new(B64URL.encode(&*seal::random(n)?));
            Ok((Kind::Value, Zeroizing::new(text.as_bytes().to_vec()), None, Some(text)))
        }
        New::Ed25519 => {
            let (private, public) = seal::ed25519_new()?;
            Ok((Kind::Ed25519, private, Some(public), None))
        }
    }
}

/// Sets (with a rule) or rotates (without) `name`: a new version, sealed with the owner key's current version. The
/// version it replaces still verifies for the rule's overlap; any older one goes. One change of a secret at a time,
/// across replicas.
async fn write(
    k: &Kernel,
    scope: Scope,
    owner: &str,
    name: &str,
    value: New,
    rule: Option<Rule>,
    who: &Who<'_>,
) -> Result<(Described, Option<Zeroizing<String>>)> {
    check_name(name)?;
    let made = make(value)?;
    write_made(&k.db, scope, owner, name, made, rule, who, None).await.map(|(d, shown, _)| (d, shown))
}

/// What a write does where the secret has a current version already, when it does not replace it.
#[derive(Clone, Copy)]
struct OnlyNew {
    /// What a write is recorded as: `adopt` (what the environment or a sandbox's source holds), `make` (a value the vault
    /// makes for a service, crate::companion).
    verb: &'static str,
    /// Marked as its owner's to manage as it is made ([`Vault::manage`]); and one there without that mark (a person's)
    /// is refused rather than left.
    managed: bool,
}

/// The `adopt` of [`OnlyNew`]: what is there is left as it is.
const ADOPT: Option<OnlyNew> = Some(OnlyNew { verb: "adopt", managed: false });

/// [`write`] of a value already made; with `only_new`, only where the secret has no current version (what is there is
/// left as it is: `false`). Under the secret's lock, so that replicas doing it at once write it once.
#[allow(clippy::too_many_arguments)]
async fn write_made(
    db: &Db,
    scope: Scope,
    owner: &str,
    name: &str,
    (kind, plain, public, shown): Made,
    rule: Option<Rule>,
    who: &Who<'_>,
    only_new: Option<OnlyNew>,
) -> Result<(Described, Option<Zeroizing<String>>, bool)> {
    // The shape alone: a sandbox makes the stores' values by their rules here too (the verbs check the rest first).
    valid_name(name)?;
    if let Some(r) = &rule {
        r.check()?;
    }
    let (described, wrote) = with_retry(|| async {
        let mut tx = db.vault_tx(scope.tenant()).await?;
        sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("ferrum.vault.secret {}/{owner}/{name}", scope.id()))
            .execute(&mut *tx)
            .await?;
        let before = rows(&mut tx, scope, owner, name).await?;
        let replaced = before.iter().find(|r| r.valid_until.is_none());
        if let (Some(only), Some(_)) = (only_new, replaced) {
            if only.managed && !is_managed(&mut tx, scope, owner, name).await? {
                return Err(Error::Conflict(format!(
                    "{name} was set by a person: the vault makes it for a service only where there is none (remove it to have it made)"
                )));
            }
            tx.commit().await?;
            return described(scope, owner, name, &before).map(|d| (d, false)).ok_or_else(|| Error::Internal("a secret there is not".into()));
        }
        let rule = match (&rule, replaced) {
            (Some(r), _) => r.clone(),
            (None, Some(r)) => r.rule.clone(),
            (None, None) => return Err(Error::NotFound("secret".into())),
        };
        if let Some(r) = replaced.filter(|r| r.kind != kind) {
            return Err(Error::Invalid(format!("{name} holds {}: remove it first to keep another kind of secret under its name", r.kind.as_str())));
        }
        let (key_version, _, key) = db.vault.current_owner_key(db, &mut tx, scope.id(), owner).await?;
        let version = sqlx::query_scalar::<_, Option<i32>>("select max(version) from vault.secret where scope = $1 and owner = $2 and name = $3")
            .bind(scope.id())
            .bind(owner)
            .bind(name)
            .fetch_one(&mut *tx)
            .await?
            .unwrap_or(0)
            + 1;
        let sealed = key.seal(&secret_place(scope.id(), owner, name, version, key_version), &plain)?;
        if let Some(r) = replaced {
            // The versions before it go; the one replaced verifies until its overlap is over.
            sqlx::query("delete from vault.secret where scope = $1 and owner = $2 and name = $3 and (valid_until is not null or ($4 = 0 and version = $5))")
                .bind(scope.id())
                .bind(owner)
                .bind(name)
                .bind(rule.overlap_secs)
                .bind(r.version)
                .execute(&mut *tx)
                .await?;
            sqlx::query("update vault.secret set valid_until = now() + make_interval(secs => $5) where scope = $1 and owner = $2 and name = $3 and version = $4")
                .bind(scope.id())
                .bind(owner)
                .bind(name)
                .bind(r.version)
                .bind(rule.overlap_secs as f64)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(
            "insert into vault.secret (scope, owner, name, version, key_version, kind, sealed, public_key, rule, created_by) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(scope.id())
        .bind(owner)
        .bind(name)
        .bind(version)
        .bind(key_version)
        .bind(kind.as_str())
        .bind(&sealed)
        .bind(&public)
        .bind(serde_json::to_value(&rule).map_err(Error::internal)?)
        .bind(who.actor)
        .execute(&mut *tx)
        .await
        .map_err(super::keys::raced)?;
        if only_new.is_some_and(|o| o.managed) {
            sqlx::query("insert into vault.managed (scope, owner, name) values ($1, $2, $3) on conflict do nothing")
                .bind(scope.id())
                .bind(owner)
                .bind(name)
                .execute(&mut *tx)
                .await?;
        }
        let verb = match only_new {
            Some(o) => o.verb,
            None if replaced.is_some() => "rotate",
            None => "set",
        };
        audited(&mut tx, scope, owner, name, verb, who, version, false).await?;
        let after = rows(&mut tx, scope, owner, name).await?;
        tx.commit().await?;
        described(scope, owner, name, &after).map(|d| (d, true)).ok_or_else(|| Error::Internal("a secret just set is not there".into()))
    })
    .await?;
    Ok((described, if wrote { shown } else { None }, wrote))
}

async fn remove(k: &Kernel, scope: Scope, owner: &str, name: &str, who: &Who<'_>) -> Result<bool> {
    check_name(name)?;
    let mut tx = k.db.vault_tx(scope.tenant()).await?;
    sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("ferrum.vault.secret {}/{owner}/{name}", scope.id()))
        .execute(&mut *tx)
        .await?;
    sqlx::query("delete from vault.managed where scope = $1 and owner = $2 and name = $3").bind(scope.id()).bind(owner).bind(name).execute(&mut *tx).await?;
    let gone: Vec<i32> = sqlx::query_scalar("delete from vault.secret where scope = $1 and owner = $2 and name = $3 returning version")
        .bind(scope.id())
        .bind(owner)
        .bind(name)
        .fetch_all(&mut *tx)
        .await?;
    if let Some(v) = gone.iter().max() {
        audited(&mut tx, scope, owner, name, "remove", who, *v, false).await?;
    }
    tx.commit().await?;
    Ok(!gone.is_empty())
}

/// Changes only the rule of `name`'s current version (its value and version stay as they are), under the secret's lock;
/// recorded as `rule`, with who did it. Not found when there is no such secret.
async fn set_rule(k: &Kernel, scope: Scope, owner: &str, name: &str, rule: Rule, who: &Who<'_>) -> Result<Described> {
    check_name(name)?;
    rule.check()?;
    let mut tx = k.db.vault_tx(scope.tenant()).await?;
    sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("ferrum.vault.secret {}/{owner}/{name}", scope.id()))
        .execute(&mut *tx)
        .await?;
    let version: Option<i32> =
        sqlx::query_scalar("update vault.secret set rule = $4 where scope = $1 and owner = $2 and name = $3 and valid_until is null returning version")
            .bind(scope.id())
            .bind(owner)
            .bind(name)
            .bind(serde_json::to_value(&rule).map_err(|e| Error::Internal(e.to_string()))?)
            .fetch_optional(&mut *tx)
            .await?;
    let version = version.ok_or_else(|| Error::NotFound("secret".into()))?;
    audited(&mut tx, scope, owner, name, "rule", who, version, false).await?;
    let rows = rows(&mut tx, scope, owner, name).await?;
    tx.commit().await?;
    described(scope, owner, name, &rows).ok_or_else(|| Error::NotFound("secret".into()))
}

async fn list(k: &Kernel, scope: Scope, owner: Option<&str>) -> Result<Vec<Described>> {
    let mut tx = k.db.vault_tx(scope.tenant()).await?;
    let names: Vec<(String, String)> = sqlx::query_as(
        "select distinct owner, name from vault.secret where scope = $1 and ($2::text is null or owner = $2) and valid_until is null order by 1, 2",
    )
    .bind(scope.id())
    .bind(owner)
    .fetch_all(&mut *tx)
    .await?;
    let mut out = Vec::new();
    for (owner, name) in names {
        if let Some(d) = described(scope, &owner, &name, &rows(&mut tx, scope, &owner, &name).await?) {
            out.push(with_managed(&mut tx, scope, d).await?);
        }
    }
    tx.commit().await?;
    Ok(out)
}

/// Code's own changes and uses: nobody behind them but the owner.
const CODE: Who<'static> = Who { actor: None, request: "", by: None, service: None };

impl Vault {
    /// Sets `name` in `scope` to `value` under `rule`: a new secret, or a new version of one (its rule replaced).
    /// Recorded on the audit trail, never the value.
    pub async fn set(&self, k: &Kernel, scope: Scope, name: &str, value: New, rule: Rule) -> Result<Described> {
        let owner = self.bound(k)?;
        let mut d = write(k, scope, owner, name, value, Some(rule), &CODE).await?.0;
        d.managed_by = managed(k, scope, owner, name).await?.then(|| owner.to_string());
        Ok(d)
    }

    /// Sets `name` only where it has no current version: what a replica found in its environment, copied once, however
    /// many replicas copy it at once (the secret's lock). Whether it was written; what was there is left as it was.
    /// Recorded as `adopt` with `by` (`environment`), never the value.
    pub async fn adopt(&self, k: &Kernel, scope: Scope, name: &str, value: New, rule: Rule, by: &str) -> Result<bool> {
        let owner = self.bound(k)?;
        check_name(name)?;
        let request = crate::db::Trace::operation().unwrap_or_default();
        let who = Who { actor: None, request: &request, by: Some(by), service: None };
        write_made(&k.db, scope, owner, name, make(value)?, Some(rule), &who, ADOPT).await.map(|(_, _, wrote)| wrote)
    }

    /// A new version of `name`, under its rule: what it signs from now on is signed with it, and the version before
    /// still verifies for the rule's overlap. Not found when there is no such secret.
    pub async fn rotate(&self, k: &Kernel, scope: Scope, name: &str, value: New) -> Result<Described> {
        let owner = self.bound(k)?;
        let mut d = write(k, scope, owner, name, value, None, &CODE).await?.0;
        d.managed_by = managed(k, scope, owner, name).await?.then(|| owner.to_string());
        Ok(d)
    }

    /// Removes `name`, every version of it; whether there was one.
    pub async fn remove(&self, k: &Kernel, scope: Scope, name: &str) -> Result<bool> {
        let owner = self.bound(k)?;
        remove(k, scope, owner, name, &CODE).await
    }

    /// What may be read of `name`: never its value.
    pub async fn describe(&self, k: &Kernel, scope: Scope, name: &str) -> Result<Option<Described>> {
        let owner = self.bound(k)?;
        check_name(name)?;
        if scope == Scope::Installation && self.renewing.contains(&name) {
            return Ok(None);
        }
        let mut tx = k.db.vault_tx(scope.tenant()).await?;
        let rows = rows(&mut tx, scope, owner, name).await?;
        let d = match described(scope, owner, name, &rows) {
            Some(d) => Some(with_managed(&mut tx, scope, d).await?),
            None => None,
        };
        tx.commit().await?;
        Ok(d)
    }

    /// Marks `name`, a secret of this handle's owner in `scope`, as one its app manages itself (`managed`), or as
    /// people's to change again: while marked, the administrator's and operator's routes (set, rotate, remove, its rule)
    /// and an operator's regenerate refuse it, naming the app; the handle itself goes on using and changing it.
    /// Recorded as `manage` or `unmanage`. Not found when there is no such secret.
    pub async fn manage(&self, k: &Kernel, scope: Scope, name: &str, managed: bool) -> Result<()> {
        let owner = self.bound(k)?;
        check_name(name)?;
        let mut tx = k.db.vault_tx(scope.tenant()).await?;
        let version = current(&rows(&mut tx, scope, owner, name).await?, name)?.version;
        let sql = if managed {
            "insert into vault.managed (scope, owner, name) values ($1, $2, $3) on conflict do nothing"
        } else {
            "delete from vault.managed where scope = $1 and owner = $2 and name = $3"
        };
        sqlx::query(sql).bind(scope.id()).bind(owner).bind(name).execute(&mut *tx).await?;
        audited(&mut tx, scope, owner, name, if managed { "manage" } else { "unmanage" }, &CODE, version, false).await?;
        tx.commit().await?;
        Ok(())
    }

    /// This owner's secrets in `scope`, described.
    pub async fn list(&self, k: &Kernel, scope: Scope) -> Result<Vec<Described>> {
        let owner = self.bound(k)?;
        list(k, scope, Some(owner)).await
    }

    /// The raw value of `name`, only when its rule lets it out (for a library that must have it), and never to a
    /// plugin's handle; recorded each time.
    pub async fn reveal(&self, k: &Kernel, scope: Scope, name: &str) -> Result<Zeroizing<Vec<u8>>> {
        let owner = self.bound(k)?;
        if owner.starts_with("plugin:") {
            return Err(Error::Forbidden("a plugin's secrets are used, never revealed: the host adds them to its requests".into()));
        }
        check_name(name)?;
        with_retry(|| async {
            let mut tx = k.db.vault_tx(scope.tenant()).await?;
            let rows = rows(&mut tx, scope, owner, name).await?;
            let row = current(&rows, name)?;
            if !row.rule.reveal {
                return Err(Error::Forbidden(format!("{name}'s rule keeps its value in the vault: it is used, never revealed")));
            }
            let value = opened(&k.db, &mut tx, scope, owner, name, row).await?;
            audited(&mut tx, scope, owner, name, "reveal", &CODE, row.version, true).await?;
            used(&mut tx, scope, owner, name, row).await?;
            tx.commit().await?;
            Ok(value)
        })
        .await
    }

    /// `message` signed with `name`'s current version: HMAC-SHA256 under a value, Ed25519 with a key pair. Recorded.
    pub async fn sign(&self, k: &Kernel, scope: Scope, name: &str, message: &[u8]) -> Result<Vec<u8>> {
        let owner = self.bound(k)?;
        check_name(name)?;
        with_retry(|| async {
            let mut tx = k.db.vault_tx(scope.tenant()).await?;
            let rows = rows(&mut tx, scope, owner, name).await?;
            let row = current(&rows, name)?;
            let value = opened(&k.db, &mut tx, scope, owner, name, row).await?;
            let signature = match row.kind {
                Kind::Value => seal::hmac_sha256(&value, message),
                Kind::Ed25519 => seal::ed25519_sign(&value, message)?,
            };
            audited(&mut tx, scope, owner, name, "sign", &CODE, row.version, true).await?;
            used(&mut tx, scope, owner, name, row).await?;
            tx.commit().await?;
            Ok(signature)
        })
        .await
    }

    /// `message` signed with `name`'s version `version` (none: the current one), and the version that signed it: what
    /// a caller names beside a signature, so that whoever checks it knows which public half to use. A version is one
    /// still kept: the current, or the one before while the rule's overlap lasts. Recorded as [`Vault::sign`] is.
    pub async fn sign_at(&self, k: &Kernel, scope: Scope, name: &str, message: &[u8], version: Option<i32>) -> Result<(i32, Vec<u8>)> {
        let owner = self.bound(k)?;
        check_name(name)?;
        with_retry(|| async {
            let mut tx = k.db.vault_tx(scope.tenant()).await?;
            let rows = rows(&mut tx, scope, owner, name).await?;
            let current = current(&rows, name)?;
            let row = match version {
                None => current,
                Some(v) => rows.iter().find(|r| r.version == v).ok_or_else(|| Error::NotFound(format!("{name}, version {v}")))?,
            };
            let value = opened(&k.db, &mut tx, scope, owner, name, row).await?;
            let signature = match row.kind {
                Kind::Value => seal::hmac_sha256(&value, message),
                Kind::Ed25519 => seal::ed25519_sign(&value, message)?,
            };
            audited(&mut tx, scope, owner, name, "sign", &CODE, row.version, true).await?;
            used(&mut tx, scope, owner, name, current).await?;
            tx.commit().await?;
            Ok((row.version, signature))
        })
        .await
    }

    /// The public half (base64url) of `name`'s Ed25519 version `version` (none: the current one), while that version
    /// is kept; none for a value, which has no public half.
    pub async fn public_key(&self, k: &Kernel, scope: Scope, name: &str, version: Option<i32>) -> Result<Option<String>> {
        let owner = self.bound(k)?;
        check_name(name)?;
        let mut tx = k.db.vault_tx(scope.tenant()).await?;
        let rows = rows(&mut tx, scope, owner, name).await?;
        tx.commit().await?;
        let current = current(&rows, name)?;
        let row = match version {
            None => current,
            Some(v) => rows.iter().find(|r| r.version == v).ok_or_else(|| Error::NotFound(format!("{name}, version {v}")))?,
        };
        Ok(row.public_key.as_ref().map(|k| B64URL.encode(k)))
    }

    /// Whether `signature` is `message` signed with `name`: its current version, or the one before it while the
    /// rule's overlap lasts.
    pub async fn verify(&self, k: &Kernel, scope: Scope, name: &str, message: &[u8], signature: &[u8]) -> Result<bool> {
        let owner = self.bound(k)?;
        check_name(name)?;
        with_retry(|| async {
            let mut tx = k.db.vault_tx(scope.tenant()).await?;
            let rows = rows(&mut tx, scope, owner, name).await?;
            let row = current(&rows, name)?;
            let mut good = false;
            for r in &rows {
                good |= match (r.kind, &r.public_key) {
                    (Kind::Ed25519, Some(public)) => seal::ed25519_verify(public, message, signature),
                    _ => seal::hmac_verify(&opened(&k.db, &mut tx, scope, owner, name, r).await?, message, signature),
                };
                if good {
                    break;
                }
            }
            used(&mut tx, scope, owner, name, row).await?;
            tx.commit().await?;
            Ok(good)
        })
        .await
    }

    /// The header that carries `name` to `url`'s host as `how` says ([`carried`]): only to a host the secret's rule
    /// names, and only a secret of this handle's owner. Recorded as a use (`send`), never the value. What a plugin's
    /// handle does in place of revealing: the value goes on the request and to nobody else.
    pub async fn carry(&self, k: &Kernel, scope: Scope, name: &str, url: &reqwest::Url, how: &Carry) -> Result<(String, Zeroizing<String>)> {
        let owner = self.bound(k)?;
        check_name(name)?;
        let host = url.host_str().ok_or_else(|| Error::invalid("the address has no host"))?;
        with_retry(|| async {
            let mut tx = k.db.vault_tx(scope.tenant()).await?;
            let rows = rows(&mut tx, scope, owner, name).await?;
            let row = current(&rows, name)?;
            let value = opened(&k.db, &mut tx, scope, owner, name, row).await?;
            let text = std::str::from_utf8(&value).map_err(|_| Error::invalid(format!("{name} is not text: it goes on no request")))?;
            let header = carried(&row.rule.hosts, host, how, text)?;
            audited(&mut tx, scope, owner, name, "send", &CODE, row.version, true).await?;
            used(&mut tx, scope, owner, name, row).await?;
            tx.commit().await?;
            Ok(header)
        })
        .await
    }

    /// [`Vault::carry`] with `name`'s version `version` (none: the current one), and the version carried: what a client
    /// of a program that holds two versions at once sends while a rotation's overlap lasts (the version before, which a
    /// program started before the rotation knows too), so that neither refuses the other until the program has started
    /// again (docs/operations.md, "Files for the programs beside the server"). A version is one still kept.
    pub async fn carry_at(
        &self,
        k: &Kernel,
        scope: Scope,
        name: &str,
        url: &reqwest::Url,
        how: &Carry,
        version: Option<i32>,
    ) -> Result<(i32, (String, Zeroizing<String>))> {
        let owner = self.bound(k)?;
        check_name(name)?;
        let host = url.host_str().ok_or_else(|| Error::invalid("the address has no host"))?;
        with_retry(|| async {
            let mut tx = k.db.vault_tx(scope.tenant()).await?;
            let rows = rows(&mut tx, scope, owner, name).await?;
            let current = current(&rows, name)?;
            let row = match version {
                None => current,
                Some(v) => rows.iter().find(|r| r.version == v).ok_or_else(|| Error::NotFound(format!("{name}, version {v}")))?,
            };
            let value = opened(&k.db, &mut tx, scope, owner, name, row).await?;
            let text = std::str::from_utf8(&value).map_err(|_| Error::invalid(format!("{name} is not text: it goes on no request")))?;
            let header = carried(&row.rule.hosts, host, how, text)?;
            audited(&mut tx, scope, owner, name, "send", &CODE, row.version, true).await?;
            used(&mut tx, scope, owner, name, current).await?;
            tx.commit().await?;
            Ok((row.version, header))
        })
        .await
    }

    /// The platform's secret `name` of the installation, with its rule, as [`Vault::reveal`] gives it, before the
    /// kernel boots: what the server's start makes its mail sender and its audit sink with. Only the server's own handle
    /// ([`crate::HookRegistry::platform_vault`]) asks, of the registry that gave it out. `None`: the vault is not open
    /// (closed, or opened only once the kernel has migrated the database), or holds no such secret.
    pub async fn reveal_at_start(&self, db: &Db, hooks: &crate::HookRegistry, name: &str) -> Result<Option<(Zeroizing<Vec<u8>>, Rule)>> {
        let ours = self.owner() == "platform" && self.mint != 0 && hooks.vault_mint("platform") == Some(self.mint);
        if !ours {
            return Err(Error::Forbidden("only the server's own handle reads the platform's secrets as it starts".into()));
        }
        check_name(name)?;
        if db.vault.installation().is_err() {
            return Ok(None);
        }
        let (scope, owner) = (Scope::Installation, "platform");
        with_retry(|| async {
            let mut tx = db.vault_tx(None).await?;
            let rows = rows(&mut tx, scope, owner, name).await?;
            let row = match current(&rows, name) {
                Err(Error::NotFound(_)) => return Ok(None),
                other => other?,
            };
            if !row.rule.reveal {
                return Err(Error::Forbidden(format!("{name}'s rule keeps its value in the vault: it is used, never revealed")));
            }
            let value = opened(db, &mut tx, scope, owner, name, row).await?;
            audited(&mut tx, scope, owner, name, "reveal", &CODE, row.version, true).await?;
            used(&mut tx, scope, owner, name, row).await?;
            tx.commit().await?;
            Ok(Some((value, row.rule.clone())))
        })
        .await
    }
}

// Services: the programs beside the server whose files their owners render (crate::companion). A render is given its
// owner's secrets of the installation's scope for those files, recorded as `render` with the service's name.

/// How the current version of a secret came to be, as the installation's record of its vault says ([`Vault::origin`]):
/// never its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    /// What wrote it: `adopt` (copied from what something else made), `make` (made for a service), `set` or `rotate`.
    pub verb: String,
    /// What acted when no person did: `environment`, `operator`, the program that handed it over (`object store`), …
    pub by: Option<String>,
    /// The service it was made or rotated for.
    pub service: Option<String>,
}

impl Vault {
    /// How the current version of `name`, a secret of this handle's owner in the installation's scope, was written, as
    /// the installation's record of its vault says; `None` where there is no current version, or no record of it.
    pub async fn origin(&self, k: &Kernel, name: &str) -> Result<Option<Origin>> {
        let owner = self.bound(k)?;
        check_name(name)?;
        let mut tx = k.db.vault_tx(None).await?;
        let rows = rows(&mut tx, Scope::Installation, owner, name).await?;
        let Some(current) = rows.iter().find(|r| r.valid_until.is_none()) else {
            tx.commit().await?;
            return Ok(None);
        };
        let found: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "select action, details->>'by', details->>'service' from vault.event where owner = $1 and name = $2 \
             and action in ('adopt', 'make', 'set', 'rotate') and details->>'version' = $3 order by id desc limit 1",
        )
        .bind(owner)
        .bind(name)
        .bind(current.version.to_string())
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(found.map(|(verb, by, service)| Origin { verb, by, service }))
    }

    /// Marks `name`, a secret of this handle's owner at the installation's scope that is there already (something other
    /// than the vault made it), as its owner's to manage, kept for the service `service` under `rule` (its value and
    /// version as they are): from then on people's routes leave it to the owner, and `vault_rotate level:service` rotates
    /// it under that rule. Under the secret's lock, so that renders at once mark it once; recorded as `rule` and `manage`,
    /// naming the service, when it marks it. Nothing is made: not found where there is no current version. Whether it
    /// marked it.
    pub(crate) async fn keep_for(&self, k: &Kernel, name: &str, rule: Rule, service: &str) -> Result<bool> {
        let owner = self.bound(k)?;
        check_name(name)?;
        rule.check()?;
        let rule = serde_json::to_value(&rule).map_err(Error::internal)?;
        let scope = Scope::Installation;
        let request = crate::db::Trace::operation().unwrap_or_default();
        let who = Who { actor: None, request: &request, by: None, service: Some(service) };
        with_retry(|| async {
            let mut tx = k.db.vault_tx(None).await?;
            sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(format!("ferrum.vault.secret {}/{owner}/{name}", scope.id()))
                .execute(&mut *tx)
                .await?;
            let version = current(&rows(&mut tx, scope, owner, name).await?, name)?.version;
            let marked = sqlx::query("insert into vault.managed (scope, owner, name) values ($1, $2, $3) on conflict do nothing")
                .bind(scope.id())
                .bind(owner)
                .bind(name)
                .execute(&mut *tx)
                .await?
                .rows_affected()
                > 0;
            if marked {
                sqlx::query("update vault.secret set rule = $4 where scope = $1 and owner = $2 and name = $3 and valid_until is null")
                    .bind(scope.id())
                    .bind(owner)
                    .bind(name)
                    .bind(&rule)
                    .execute(&mut *tx)
                    .await?;
                audited(&mut tx, scope, owner, name, "rule", &who, version, false).await?;
                audited(&mut tx, scope, owner, name, "manage", &who, version, false).await?;
            }
            tx.commit().await?;
            Ok(marked)
        })
        .await
    }

    /// Makes `name` of this handle's owner at the installation's scope for the service `service`, where it has none: a
    /// random value of 32 bytes as text (43 characters of base64url) under `rule`, marked as its owner's to manage, in one
    /// transaction under the secret's lock, so that renders and replicas at once make one. One a person set there (not so
    /// marked) is refused. Recorded as `make`, naming the service. Whether it made one.
    pub(crate) async fn make_for(&self, k: &Kernel, name: &str, rule: Rule, service: &str) -> Result<bool> {
        let owner = self.bound(k)?;
        check_name(name)?;
        let request = crate::db::Trace::operation().unwrap_or_default();
        let who = Who { actor: None, request: &request, by: None, service: Some(service) };
        let made = OnlyNew { verb: "make", managed: true };
        write_made(&k.db, Scope::Installation, owner, name, make(New::Random(32))?, Some(rule), &who, Some(made)).await.map(|(_, _, wrote)| wrote)
    }

    /// A new value of `name`, a secret the vault made for the service `service` (marked as its owner's to manage), for the
    /// operator: of the same kind (a random value of 32 bytes, or an Ed25519 pair), under `rule` (the one the service
    /// declares now; none: its rule kept), so that the version before still verifies, under its own rule, and is given to
    /// the service, for the overlap. Recorded as `rotate` by `operator`, naming the service. Refused for a secret the vault
    /// did not make (a person's), and not found when there is none.
    pub(crate) async fn rotate_made(&self, k: &Kernel, name: &str, service: &str, rule: Option<Rule>) -> Result<Described> {
        let owner = self.bound(k)?;
        check_name(name)?;
        let scope = Scope::Installation;
        let mut tx = k.db.vault_tx(None).await?;
        let kind = described(scope, owner, name, &rows(&mut tx, scope, owner, name).await?).map(|d| d.kind);
        let ours = is_managed(&mut tx, scope, owner, name).await?;
        tx.commit().await?;
        let new = match kind {
            None => return Err(Error::NotFound(format!("{name} of {owner} (the service {service} has not been rendered yet)"))),
            Some(_) if !ours => {
                return Err(Error::Conflict(format!(
                    "{name} is not a secret the vault keeps for the service {service}: render the service first, or change it where it was set"
                )));
            }
            Some(Kind::Value) => New::Random(32),
            Some(Kind::Ed25519) => New::Ed25519,
        };
        let request = crate::db::Trace::operation().unwrap_or_default();
        let who = Who { actor: None, request: &request, by: Some("operator"), service: Some(service) };
        let mut d = write(k, scope, owner, name, new, rule, &who).await?.0;
        d.managed_by = Some(owner.to_string());
        Ok(d)
    }

    /// The values of this handle's secret `name` of the installation, for the files of the service `service`: the
    /// current version, and with `previous` the one before it while its overlap lasts, newest first. Only where each
    /// version's rule lets its owner read it (a secret that is only used never leaves the vault, into a file or
    /// otherwise), never a plugin's. Each version given is recorded as `render`, naming the service, under the cap on uses.
    pub(crate) async fn rendered(&self, k: &Kernel, name: &str, service: &str, previous: bool) -> Result<Vec<(i32, Zeroizing<Vec<u8>>)>> {
        let owner = self.bound(k)?;
        if owner.starts_with("plugin:") {
            return Err(Error::Forbidden("a plugin's secrets are used, never revealed: the host adds them to its requests".into()));
        }
        check_name(name)?;
        let scope = Scope::Installation;
        let request = crate::db::Trace::operation().unwrap_or_default();
        let who = Who { actor: None, request: &request, by: None, service: Some(service) };
        with_retry(|| async {
            let mut tx = k.db.vault_tx(None).await?;
            let rows = rows(&mut tx, scope, owner, name).await?;
            let current = current(&rows, name)?;
            let mut out = Vec::new();
            for row in rows.iter().filter(|r| r.version == current.version || (previous && r.valid_until.is_some())) {
                if !row.rule.reveal {
                    return Err(Error::Forbidden(format!("{name}'s rule keeps its value in the vault: it is used, never written to a file")));
                }
                out.push((row.version, opened(&k.db, &mut tx, scope, owner, name, row).await?));
            }
            for (version, _) in &out {
                audited(&mut tx, scope, owner, name, "render", &who, *version, true).await?;
            }
            used(&mut tx, scope, owner, name, current).await?;
            tx.commit().await?;
            Ok(out)
        })
        .await
    }
}

/// How a credential goes on an outbound request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Carry {
    /// `Authorization: Bearer <secret>`.
    Bearer,
    /// `Authorization: Basic <base64 of user:secret>`, with this user.
    Basic(String),
    /// `<name>: <prefix><secret>`.
    Header { name: String, prefix: String },
}

/// The one way a credential goes on an outbound request, wherever it is kept (the vault's [`Vault::carry`], a
/// plugin's credentials, a connection's): the header that carries `secret` to `host` as `how` says, and only when
/// `hosts` names that host (`api.example.com`, or `*.example.com` for its subdomains). Zeroed when dropped.
pub fn carried(hosts: &[String], host: &str, how: &Carry, secret: &str) -> Result<(String, Zeroizing<String>)> {
    let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
    let named =
        hosts.iter().map(|h| h.trim().to_ascii_lowercase()).any(|h| h == host || h.strip_prefix("*.").is_some_and(|d| host.ends_with(&format!(".{d}"))));
    if !named {
        return Err(Error::Forbidden(format!("this credential may not be sent to {host}")));
    }
    if secret.is_empty() || secret.contains(['\r', '\n']) {
        return Err(Error::invalid("a credential is text on one line"));
    }
    Ok(match how {
        Carry::Bearer => ("authorization".into(), Zeroizing::new(format!("Bearer {secret}"))),
        Carry::Basic(user) => {
            let pair = Zeroizing::new(format!("{user}:{secret}"));
            ("authorization".into(), Zeroizing::new(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(pair.as_bytes()))))
        }
        Carry::Header { name, prefix } => (name.clone(), Zeroizing::new(format!("{prefix}{secret}"))),
    })
}

// Rotation of an owner key, in the background: what an older version sealed is sealed again with the newest, a batch
// to a transaction, rows another replica is sealing skipped, so that it is safe beside traffic, on several replicas,
// and to stop at any point: the next run starts from what is left. An older version goes once nothing names it.

/// The job that seals again, in a workspace, what an older version of an owner's key sealed.
pub(crate) const RESEAL: &str = "platform.vault_reseal";
/// Secrets sealed again in one transaction.
const RESEAL_BATCH: i64 = 200;

pub(crate) fn handler(kind: &str) -> Option<Arc<dyn JobHandler>> {
    (kind == RESEAL).then(|| Arc::new(Reseal) as Arc<dyn JobHandler>)
}

struct Reseal;

#[async_trait]
impl JobHandler for Reseal {
    fn kind(&self) -> &'static str {
        RESEAL
    }

    fn title(&self) -> &'static str {
        "Sealing secrets again with a new key"
    }

    async fn run(&self, k: &Kernel, job: &Job) -> Result<Value> {
        let owner = job.payload["owner"].as_str().ok_or_else(|| Error::Invalid("a re-seal names its owner".into()))?;
        let scope = Scope::Workspace(job.tenant_id);
        loop {
            let (done, left) = reseal_some(k, scope, owner).await?;
            // Done, or what is left is another worker's (the upkeep comes back for what anyone leaves).
            if left == 0 || done == 0 || !job.progress(k, json!({"left": left})).await? {
                return Ok(json!({"left": left}));
            }
        }
    }
}

/// Seals again up to [`RESEAL_BATCH`] secrets of (`scope`, `owner`) that an older key version sealed: how many it did,
/// and how many are left. With none left, the older versions go.
async fn reseal_some(k: &Kernel, scope: Scope, owner: &str) -> Result<(usize, i64)> {
    let root = &k.db.vault;
    let (done, left) = with_retry(|| async {
        let mut tx = k.db.vault_tx(scope.tenant()).await?;
        let current: Option<i32> =
            sqlx::query_scalar("select max(version) from vault.key where scope = $1 and owner = $2").bind(scope.id()).bind(owner).fetch_one(&mut *tx).await?;
        let Some(current) = current else { return Ok((0, 0)) };
        let rows: Vec<(String, i32, i32, Vec<u8>)> = sqlx::query_as(
            "select name, version, key_version, sealed from vault.secret where scope = $1 and owner = $2 and key_version < $3 \
             order by name, version limit $4 for update skip locked",
        )
        .bind(scope.id())
        .bind(owner)
        .bind(current)
        .bind(RESEAL_BATCH)
        .fetch_all(&mut *tx)
        .await?;
        if !rows.is_empty() {
            let new = root.owner_key(&k.db, &mut tx, scope.id(), owner, current).await?;
            for (name, version, key_version, sealed) in &rows {
                let old = root.owner_key(&k.db, &mut tx, scope.id(), owner, *key_version).await?;
                let plain = old.open(&secret_place(scope.id(), owner, name, *version, *key_version), sealed).ok_or_else(|| {
                    Error::Internal(format!(
                        "{owner}'s secret {name} (version {version}) does not open: its row was changed or damaged; restore the vault's tables from a backup"
                    ))
                })?;
                sqlx::query("update vault.secret set sealed = $5, key_version = $6 where scope = $1 and owner = $2 and name = $3 and version = $4")
                    .bind(scope.id())
                    .bind(owner)
                    .bind(name)
                    .bind(version)
                    .bind(new.seal(&secret_place(scope.id(), owner, name, *version, current), &plain)?)
                    .bind(current)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        let left: i64 = sqlx::query_scalar("select count(*) from vault.secret where scope = $1 and owner = $2 and key_version < $3")
            .bind(scope.id())
            .bind(owner)
            .bind(current)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok((rows.len(), left))
    })
    .await?;
    if left == 0 {
        retire(k, scope, owner).await?;
    }
    Ok((done, left))
}

/// The older versions of an owner key that no secret names and that sealed or derived nothing kept outside: gone.
async fn retire(k: &Kernel, scope: Scope, owner: &str) -> Result<u64> {
    let mut tx = k.db.vault_tx(scope.tenant()).await?;
    let gone = sqlx::query(
        "delete from vault.key k where scope = $1 and owner = $2 and not outside \
         and version < (select max(version) from vault.key x where x.scope = $1 and x.owner = $2) \
         and not exists (select 1 from vault.secret s where s.scope = k.scope and s.owner = k.owner and s.key_version = k.version)",
    )
    .bind(scope.id())
    .bind(owner)
    .execute(&mut *tx)
    .await;
    let gone = match gone {
        Ok(r) => r.rows_affected(),
        // Something was sealed with it meanwhile, by a replica that had not yet seen the newer version: the next round.
        Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("23503") => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    if gone > 0 {
        super::root::notify(&mut tx, &format!("{}/{owner}", scope.id())).await?;
    }
    tx.commit().await?;
    if gone > 0 {
        k.db.vault.cache.forget(Some(scope.id()), Some(owner));
    }
    Ok(gone)
}

/// Records a new version of a key: a workspace key (owner `""`) or an owner key.
pub(crate) async fn changed_key(tx: &mut Tx, scope: Scope, owner: &str, version: i32) -> Result<()> {
    let what = if owner.is_empty() { "workspace_key_rotated" } else { "owner_key_rotated" };
    let request = crate::db::Trace::operation().unwrap_or_default();
    event(tx, scope, None, &request, audit::ADMIN, what, json!({"owner": owner, "version": version})).await
}

impl Kernel {
    /// Seals again, now, all that older versions of `owner`'s key in `scope` sealed (what the background does): how
    /// many are left, which another replica is sealing at this moment.
    pub async fn vault_reseal(&self, scope: Scope, owner: &str) -> Result<i64> {
        self.db.vault.installation()?;
        loop {
            let (done, left) = reseal_some(self, scope, owner).await?;
            if left == 0 || done == 0 {
                return Ok(left);
            }
        }
    }
}

/// The vault's part of the hourly upkeep (`Kernel::maintenance`): the summaries of hours of uses that are over, the
/// versions whose overlap is over, what older owner keys still seal, sealed again (the installation's scope has no job
/// queue; in a workspace this finds what a job left), and the old stores' values moved in (kept.rs).
pub(crate) async fn upkeep(k: &Kernel) -> Result<()> {
    if k.db.vault.installation().is_err() {
        return Ok(());
    }
    let mut tx = k.db.vault_tx(None).await?;
    flush_uses(&mut tx, None).await?;
    sqlx::query("delete from vault.secret where valid_until <= now()").execute(&mut *tx).await?;
    let pending: Vec<(Uuid, String)> = sqlx::query_as(
        "select distinct s.scope, s.owner from vault.secret s \
         where s.key_version < (select max(version) from vault.key k where k.scope = s.scope and k.owner = s.owner) \
         union select scope, owner from vault.key where owner <> '' group by scope, owner having count(*) > 1",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    for (scope, owner) in pending {
        let scope = if scope.is_nil() { Scope::Installation } else { Scope::Workspace(scope) };
        if let Err(e) = k.vault_reseal(scope, &owner).await {
            tracing::warn!(%owner, "sealing secrets again with a newer key: {e}");
        }
    }
    // What the kernel's stores no longer hold (kept.rs): gone from the vault.
    match super::kept::forget_gone(k).await {
        Ok(0) => {}
        Ok(gone) => tracing::info!(gone, "values the kernel's stores no longer hold went from the vault"),
        Err(e) => tracing::warn!("removing from the vault what the kernel's stores no longer hold: {e}"),
    }
    Ok(())
}

// People: an administrator of a workspace sets, rotates and removes its owners' secrets, having just confirmed it is
// them. The routes that call these come with a later slice; nothing here returns a value but one the vault made, once.

/// What setting or rotating a secret gave the person who did it.
pub struct Changed {
    pub secret: Described,
    /// A value the vault made: shown to them this once, and never again.
    pub shown: Option<Zeroizing<String>>,
}

impl std::fmt::Debug for Changed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Changed").field("secret", &self.secret).field("shown", &self.shown.as_ref().map(|_| "..")).finish()
    }
}

/// Whether `ctx` may manage the workspace's secrets: an administrator of it (not someone from outside); to change one,
/// also a staff member who has just confirmed it is them (the step-up), in a workspace that may change.
fn may_manage(ctx: &Ctx, change: bool) -> Result<()> {
    if ctx.outside || !ctx.is_admin {
        return Err(Error::Forbidden("a workspace's secrets are its administrators' to manage".into()));
    }
    if !change {
        return Ok(());
    }
    if ctx.read_only {
        return Err(Error::Forbidden("this workspace's account is restricted: nothing in it changes".into()));
    }
    let secs = if ctx.schema.controls.step_up_secs > 0 { ctx.schema.controls.step_up_secs } else { crate::shell::STEP_UP_SECS };
    match ctx.proof {
        Proof::Person { staff: true, .. } if ctx.proof.mark(secs).is_empty() => Ok(()),
        Proof::Person { staff: true, .. } => Err(crate::controls::needed()),
        _ => Err(Error::Forbidden("setting, rotating or removing a secret needs a person who has just confirmed it is them".into())),
    }
}

/// The handle of an owner a person names: the platform, an app this server registered, or a plugin. Kept in here (and
/// given to a service of that owner's as it renders, companion.rs).
pub(crate) fn of_owner(k: &Kernel, owner: &str) -> Result<Vault> {
    if owner == "platform" || owner.strip_prefix("plugin:").is_some_and(super::plugin_name_ok) {
        return Ok(Vault::kernel(owner));
    }
    match k.hooks.vault_mint(owner) {
        Some(mint) => Ok(Vault::app(owner, mint)),
        None => Err(Error::NotFound(format!("owner {owner}"))),
    }
}

impl Kernel {
    /// The workspace's secrets, every owner's, as its administrators see them: names, rules and dates, never a value.
    pub async fn vault_secrets(&self, ctx: &Ctx) -> Result<Vec<Described>> {
        may_manage(ctx, false)?;
        list(self, Scope::Workspace(ctx.tenant), None).await
    }

    /// The names of the workspace's people who set its secrets (`Described::set_by`), for its administrators, who may see
    /// them: by id, the ones still there.
    pub async fn vault_setters(&self, ctx: &Ctx, ids: &[Uuid]) -> Result<std::collections::BTreeMap<Uuid, String>> {
        may_manage(ctx, false)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<(Uuid, String)> =
            sqlx::query_as("select id, full_name from app_user where tenant_id = $1 and id = any($2)").bind(ctx.tenant).bind(ids).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok(rows.into_iter().collect())
    }

    /// Sets `owner`'s secret `name` in the workspace, for a person: an administrator who has just confirmed it is them.
    /// A value the vault made is returned to show them once.
    pub async fn vault_set(&self, ctx: &Ctx, owner: &str, name: &str, value: New, rule: Rule) -> Result<Changed> {
        may_manage(ctx, true)?;
        let v = of_owner(self, owner)?;
        not_managed(self, Scope::Workspace(ctx.tenant), v.bound(self)?, name).await?;
        let who = Who { actor: Some(ctx.user), request: &ctx.request_id, by: None, service: None };
        let (secret, shown) = write(self, Scope::Workspace(ctx.tenant), v.bound(self)?, name, value, Some(rule), &who).await?;
        Ok(Changed { secret, shown })
    }

    /// Rotates `owner`'s secret `name` in the workspace, for a person, as [`Kernel::vault_set`].
    pub async fn vault_rotate(&self, ctx: &Ctx, owner: &str, name: &str, value: New) -> Result<Changed> {
        may_manage(ctx, true)?;
        let v = of_owner(self, owner)?;
        not_managed(self, Scope::Workspace(ctx.tenant), v.bound(self)?, name).await?;
        let who = Who { actor: Some(ctx.user), request: &ctx.request_id, by: None, service: None };
        let (secret, shown) = write(self, Scope::Workspace(ctx.tenant), v.bound(self)?, name, value, None, &who).await?;
        Ok(Changed { secret, shown })
    }

    /// A new value of `owner`'s secret `name` in `scope`, made by the vault as the one before was (a random value of 32
    /// bytes, or a new Ed25519 pair) and never shown: an operator's rotation of a key that code uses and nobody types.
    pub async fn vault_regenerate(&self, scope: Scope, owner: &str, name: &str) -> Result<Described> {
        let v = of_owner(self, owner)?;
        let owner = v.bound(self)?;
        check_name(name)?;
        not_managed(self, scope, owner, name).await?;
        let mut tx = self.db.vault_tx(scope.tenant()).await?;
        let kind = described(scope, owner, name, &rows(&mut tx, scope, owner, name).await?).map(|d| d.kind);
        tx.commit().await?;
        let new = match kind.ok_or_else(|| Error::NotFound("secret".into()))? {
            Kind::Value => New::Random(32),
            Kind::Ed25519 => New::Ed25519,
        };
        write(self, scope, owner, name, new, None, &CODE).await.map(|(d, _)| d)
    }

    /// Changes only the rule of `owner`'s secret `name` in the workspace, for a person, as [`Kernel::vault_set`]: the
    /// same right, a confirmed identity for every change (stricter or not), on the audit trail as `rule`.
    pub async fn vault_rule(&self, ctx: &Ctx, owner: &str, name: &str, rule: Rule) -> Result<Described> {
        may_manage(ctx, true)?;
        let v = of_owner(self, owner)?;
        not_managed(self, Scope::Workspace(ctx.tenant), v.bound(self)?, name).await?;
        set_rule(
            self,
            Scope::Workspace(ctx.tenant),
            v.bound(self)?,
            name,
            rule,
            &Who { actor: Some(ctx.user), request: &ctx.request_id, by: None, service: None },
        )
        .await
    }

    /// Removes `owner`'s secret `name` from the workspace, for a person, as [`Kernel::vault_set`].
    pub async fn vault_remove(&self, ctx: &Ctx, owner: &str, name: &str) -> Result<bool> {
        may_manage(ctx, true)?;
        let v = of_owner(self, owner)?;
        not_managed(self, Scope::Workspace(ctx.tenant), v.bound(self)?, name).await?;
        remove(self, Scope::Workspace(ctx.tenant), v.bound(self)?, name, &Who { actor: Some(ctx.user), request: &ctx.request_id, by: None, service: None })
            .await
    }
}

// Operators: the installation's own secrets (every owner's), changed behind the operator's token. Only the platform's
// handle does it, which the server alone holds: an app's handle, or a plugin's, is refused.

impl Vault {
    /// The owner `owner`'s handle, for an operator acting through the platform's handle (refused for any other).
    fn for_operator(&self, k: &Kernel, owner: &str) -> Result<Vault> {
        if self.bound(k)? != "platform" {
            return Err(Error::Forbidden("only the platform's handle acts for the operator".into()));
        }
        of_owner(k, owner)
    }

    /// The installation's secrets, every owner's, as its operator sees them: names, rules and dates, never a value.
    pub async fn installation_secrets(&self, k: &Kernel) -> Result<Vec<Described>> {
        self.for_operator(k, "platform")?;
        list(k, Scope::Installation, None).await
    }

    /// Sets `owner`'s secret `name` of the installation for the operator; recorded as the operator's. A value the vault
    /// made is returned to show once.
    pub async fn operator_set(&self, k: &Kernel, owner: &str, name: &str, value: New, rule: Rule) -> Result<Changed> {
        let v = self.for_operator(k, owner)?;
        not_managed(k, Scope::Installation, v.bound(k)?, name).await?;
        let request = crate::db::Trace::operation().unwrap_or_default();
        let who = Who { actor: None, request: &request, by: Some("operator"), service: None };
        let (secret, shown) = write(k, Scope::Installation, v.bound(k)?, name, value, Some(rule), &who).await?;
        Ok(Changed { secret, shown })
    }

    /// Rotates `owner`'s secret `name` of the installation for the operator, as [`Vault::operator_set`].
    pub async fn operator_rotate(&self, k: &Kernel, owner: &str, name: &str, value: New) -> Result<Changed> {
        let v = self.for_operator(k, owner)?;
        not_managed(k, Scope::Installation, v.bound(k)?, name).await?;
        let request = crate::db::Trace::operation().unwrap_or_default();
        let who = Who { actor: None, request: &request, by: Some("operator"), service: None };
        let (secret, shown) = write(k, Scope::Installation, v.bound(k)?, name, value, None, &who).await?;
        Ok(Changed { secret, shown })
    }

    /// Changes only the rule of `owner`'s secret `name` of the installation for the operator; recorded as the operator's.
    pub async fn operator_rule(&self, k: &Kernel, owner: &str, name: &str, rule: Rule) -> Result<Described> {
        let v = self.for_operator(k, owner)?;
        not_managed(k, Scope::Installation, v.bound(k)?, name).await?;
        let request = crate::db::Trace::operation().unwrap_or_default();
        set_rule(k, Scope::Installation, v.bound(k)?, name, rule, &Who { actor: None, request: &request, by: Some("operator"), service: None }).await
    }

    /// Removes `owner`'s secret `name` of the installation for the operator; whether there was one.
    pub async fn operator_remove(&self, k: &Kernel, owner: &str, name: &str) -> Result<bool> {
        let v = self.for_operator(k, owner)?;
        not_managed(k, Scope::Installation, v.bound(k)?, name).await?;
        let request = crate::db::Trace::operation().unwrap_or_default();
        remove(k, Scope::Installation, v.bound(k)?, name, &Who { actor: None, request: &request, by: Some("operator"), service: None }).await
    }
}

/// A sandbox's secrets (sandbox.rs): each current secret of the workspace `from`, in this installation, made in the
/// sandbox `to` as its rule says: `blank` (none), `regenerate` (a value of its own of the same kind, made here and shown
/// to nobody) or `keep` (the same value). Each is sealed with the sandbox's own keys, made for it here, never the live
/// workspace's. Run again, it makes only what is not there yet.
pub(crate) async fn sandbox(k: &Kernel, from: Uuid, to: Uuid) -> Result<Value> {
    if k.db.vault.installation().is_err() {
        return Ok(json!({"kept": 0, "regenerated": 0, "blank": 0, "closed": true}));
    }
    let (source, sandbox) = (Scope::Workspace(from), Scope::Workspace(to));
    let mut tx = k.db.vault_tx(Some(from)).await?;
    let names: Vec<(String, String)> = sqlx::query_as("select distinct owner, name from vault.secret where scope = $1 and valid_until is null order by 1, 2")
        .bind(from)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let who = Who { actor: None, request: "", by: Some("sandbox"), service: None };
    let (mut kept, mut made, mut blank) = (0, 0, 0);
    for (owner, name) in names {
        let mut tx = k.db.vault_tx(Some(from)).await?;
        let rows = rows(&mut tx, source, &owner, &name).await?;
        let Some(row) = rows.iter().find(|r| r.valid_until.is_none()) else { continue };
        let expired = row.rule.expires_at.is_some_and(|at| at <= Utc::now());
        let value = match row.rule.sandbox {
            _ if expired => None,
            super::Sandbox::Blank => None,
            super::Sandbox::Keep => Some((row.kind, opened(&k.db, &mut tx, source, &owner, &name, row).await?, row.public_key.clone(), None)),
            super::Sandbox::Regenerate => Some(make(if row.kind == Kind::Ed25519 { New::Ed25519 } else { New::Random(32) })?),
        };
        tx.commit().await?;
        let Some(value) = value else {
            blank += 1;
            continue;
        };
        let keep = row.rule.sandbox == super::Sandbox::Keep;
        write_made(&k.db, sandbox, &owner, &name, value, Some(row.rule.clone()), &who, ADOPT).await?;
        if keep { kept += 1 } else { made += 1 }
    }
    // What its app manages in the workspace it manages in the sandbox too, for each secret the sandbox has.
    let mut tx = k.db.vault_tx(Some(to)).await?;
    sqlx::query(
        "insert into vault.managed (scope, owner, name) select $2, m.owner, m.name from vault.managed m where m.scope = $1 \
         and exists (select 1 from vault.secret s where s.scope = $2 and s.owner = m.owner and s.name = m.name) on conflict do nothing",
    )
    .bind(from)
    .bind(to)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(json!({"kept": kept, "regenerated": made, "blank": blank}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rule_says_what_it_allows_and_refuses_what_is_not_one() {
        assert_eq!(
            serde_json::to_value(Rule::default()).unwrap(),
            json!({"reveal": false, "hosts": [], "sandbox": "blank", "overlap_secs": 0, "expires_at": null})
        );
        assert!(serde_json::from_value::<Rule>(json!({"reveal": true, "colour": "red"})).is_err(), "nothing it does not know");
        let ok = Rule { hosts: vec!["api.stripe.com".into(), "*.example.com".into()], overlap_secs: 3600, ..Default::default() };
        assert!(ok.check().is_ok());
        for bad in ["API.stripe.com", "https://api.stripe.com", "a..b", "-a.com", "", "*.", "a b"] {
            assert!(Rule { hosts: vec![bad.into()], ..Default::default() }.check().is_err(), "{bad}");
        }
        assert!(Rule { overlap_secs: MAX_OVERLAP_SECS + 1, ..Default::default() }.check().is_err());
        assert!(Rule { overlap_secs: -1, ..Default::default() }.check().is_err());
        assert!(Rule { expires_at: Some(Utc::now() - chrono::Duration::minutes(1)), ..Default::default() }.check().is_err());
        for (name, ok) in [("stripe.api_key", true), ("x", true), ("Stripe", false), ("a/b", false), ("", false), ("_x", false)] {
            assert_eq!(check_name(name).is_ok(), ok, "{name}");
        }
        assert_eq!(format!("{:?}", New::Value(Zeroizing::new(b"hunter2".to_vec()))), "New::Value(..)");
    }

    #[test]
    fn what_the_vault_makes_is_what_it_shows() {
        let (kind, value, public, shown) = make(New::Random(32)).unwrap();
        assert_eq!((kind, public.is_none()), (Kind::Value, true));
        assert_eq!(shown.as_ref().map(|s| s.as_bytes()), Some(value.as_slice()), "the text shown is the value kept");
        assert_eq!(value.len(), 43, "32 bytes as base64url");
        assert!(make(New::Random(8)).is_err() && make(New::Value(Zeroizing::new(Vec::new()))).is_err());
        let (kind, private, public, shown) = make(New::Ed25519).unwrap();
        assert_eq!((kind, public.as_ref().map(Vec::len), shown.is_none()), (Kind::Ed25519, Some(32), true));
        let sig = seal::ed25519_sign(&private, b"m").unwrap();
        assert!(seal::ed25519_verify(&public.unwrap(), b"m", &sig));
    }
}
