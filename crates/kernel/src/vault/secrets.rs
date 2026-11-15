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
use crate::db::{Tx, with_retry};
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
}

/// Who changes a secret, for the trail.
struct Who<'a> {
    actor: Option<Uuid>,
    request: &'a str,
}

fn check_name(name: &str) -> Result<()> {
    let b = name.as_bytes();
    let ok = (1..=100).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.-".contains(c));
    if ok { Ok(()) } else { Err(Error::field("name", "lowercase letters, digits, and _ . -, up to 100")) }
}

/// A version of a secret as stored.
struct Row {
    version: i32,
    key_version: i32,
    kind: Kind,
    sealed: Vec<u8>,
    public_key: Option<Vec<u8>>,
    rule: Rule,
    valid_until: Option<DateTime<Utc>>,
    set_at: DateTime<Utc>,
    set_by: Option<Uuid>,
    last_used: Option<NaiveDate>,
}

type RowTuple = (i32, i32, String, Vec<u8>, Option<Vec<u8>>, Value, Option<DateTime<Utc>>, DateTime<Utc>, Option<Uuid>, Option<NaiveDate>);

/// The current version of a secret first, then the version before it while it still verifies.
async fn rows(tx: &mut Tx, scope: Scope, owner: &str, name: &str) -> Result<Vec<Row>> {
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
    })
}

/// The current version, or refused: not there, or expired.
fn current<'a>(rows: &'a [Row], name: &str) -> Result<&'a Row> {
    let row = rows.iter().find(|r| r.valid_until.is_none()).ok_or_else(|| Error::NotFound("secret".into()))?;
    if let Some(at) = row.rule.expires_at.filter(|at| *at <= Utc::now()) {
        return Err(Error::Forbidden(format!("{name} expired on {}", at.format("%Y-%m-%d %H:%M UTC"))));
    }
    Ok(row)
}

/// What a version holds, opened with the version of the owner key that sealed it.
async fn opened(k: &Kernel, tx: &mut Tx, scope: Scope, owner: &str, name: &str, row: &Row) -> Result<Zeroizing<Vec<u8>>> {
    let key = k.db.vault.owner_key(&k.db, tx, scope.id(), owner, row.key_version).await?;
    key.open(&secret_place(scope.id(), owner, name, row.version, row.key_version), &row.sealed).ok_or_else(|| {
        Error::Internal(format!(
            "{owner}'s secret {name} (version {}) does not open: its row was changed or damaged; restore the vault's tables from a backup",
            row.version
        ))
    })
}

/// Keeps, coarsely, the day a secret was last used: one write a day at most.
async fn used(tx: &mut Tx, scope: Scope, owner: &str, name: &str, row: &Row) -> Result<()> {
    if row.last_used != Some(Utc::now().date_naive()) {
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

/// The workspace whose trail a scope's events go into: its own, or for the installation the first workspace's, as the
/// platform's own events are (none before there is one).
async fn trail(tx: &mut Tx, scope: Scope) -> Result<Option<Uuid>> {
    match scope {
        Scope::Workspace(t) => Ok(Some(t)),
        Scope::Installation => Ok(sqlx::query_scalar("select id from tenant order by created_at limit 1").fetch_optional(&mut **tx).await?),
    }
}

async fn event(tx: &mut Tx, scope: Scope, actor: Option<Uuid>, request: &str, category: &'static str, verb: &str, mut diff: Value) -> Result<()> {
    if scope == Scope::Installation {
        diff["scope"] = json!("installation");
    }
    match trail(tx, scope).await? {
        Some(tenant) => audit::record_as(tx, tenant, actor, request, category, "Vault", Uuid::now_v7(), verb, None, diff).await,
        None => {
            tracing::info!(%diff, "the vault: {verb} (no workspace's trail yet to record it in)");
            Ok(())
        }
    }
}

/// Records a use or a change of a secret. A use (`capped`) is counted for its hour: past [`USES_PER_HOUR`] it is only
/// counted, and the hours that are over get their summary ([`flush_uses`]).
async fn audited(tx: &mut Tx, scope: Scope, owner: &str, name: &str, verb: &'static str, who: &Who<'_>, version: i32, capped: bool) -> Result<()> {
    if capped {
        flush_uses(tx, Some((scope, owner, name, verb))).await?;
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
    event(tx, scope, who.actor, who.request, category, verb, json!({"owner": owner, "name": name, "version": version})).await
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
    if let Some(r) = &rule {
        r.check()?;
    }
    let (kind, plain, public, shown) = make(value)?;
    let described = with_retry(|| async {
        let mut tx = k.db.vault_tx(scope.tenant()).await?;
        sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("ferrum.vault.secret {}/{owner}/{name}", scope.id()))
            .execute(&mut *tx)
            .await?;
        let before = rows(&mut tx, scope, owner, name).await?;
        let replaced = before.iter().find(|r| r.valid_until.is_none());
        let rule = match (&rule, replaced) {
            (Some(r), _) => r.clone(),
            (None, Some(r)) => r.rule.clone(),
            (None, None) => return Err(Error::NotFound("secret".into())),
        };
        if let Some(r) = replaced.filter(|r| r.kind != kind) {
            return Err(Error::Invalid(format!("{name} holds {}: remove it first to keep another kind of secret under its name", r.kind.as_str())));
        }
        let (key_version, _, key) = k.db.vault.current_owner_key(&k.db, &mut tx, scope.id(), owner).await?;
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
        audited(&mut tx, scope, owner, name, if replaced.is_some() { "rotate" } else { "set" }, who, version, false).await?;
        let after = rows(&mut tx, scope, owner, name).await?;
        tx.commit().await?;
        described(scope, owner, name, &after).ok_or_else(|| Error::Internal("a secret just set is not there".into()))
    })
    .await?;
    Ok((described, shown))
}

async fn remove(k: &Kernel, scope: Scope, owner: &str, name: &str, who: &Who<'_>) -> Result<bool> {
    check_name(name)?;
    let mut tx = k.db.vault_tx(scope.tenant()).await?;
    sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("ferrum.vault.secret {}/{owner}/{name}", scope.id()))
        .execute(&mut *tx)
        .await?;
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
        out.extend(described(scope, &owner, &name, &rows(&mut tx, scope, &owner, &name).await?));
    }
    tx.commit().await?;
    Ok(out)
}

/// Code's own changes and uses: nobody behind them but the owner.
const CODE: Who<'static> = Who { actor: None, request: "" };

impl Vault {
    /// Sets `name` in `scope` to `value` under `rule`: a new secret, or a new version of one (its rule replaced).
    /// Recorded on the audit trail, never the value.
    pub async fn set(&self, k: &Kernel, scope: Scope, name: &str, value: New, rule: Rule) -> Result<Described> {
        let owner = self.bound(k)?;
        write(k, scope, owner, name, value, Some(rule), &CODE).await.map(|(d, _)| d)
    }

    /// A new version of `name`, under its rule: what it signs from now on is signed with it, and the version before
    /// still verifies for the rule's overlap. Not found when there is no such secret.
    pub async fn rotate(&self, k: &Kernel, scope: Scope, name: &str, value: New) -> Result<Described> {
        let owner = self.bound(k)?;
        write(k, scope, owner, name, value, None, &CODE).await.map(|(d, _)| d)
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
        let mut tx = k.db.vault_tx(scope.tenant()).await?;
        let rows = rows(&mut tx, scope, owner, name).await?;
        tx.commit().await?;
        Ok(described(scope, owner, name, &rows))
    }

    /// This owner's secrets in `scope`, described.
    pub async fn list(&self, k: &Kernel, scope: Scope) -> Result<Vec<Described>> {
        let owner = self.bound(k)?;
        list(k, scope, Some(owner)).await
    }

    /// The raw value of `name`, only when its rule lets it out (for a library that must have it); recorded each time.
    pub async fn reveal(&self, k: &Kernel, scope: Scope, name: &str) -> Result<Zeroizing<Vec<u8>>> {
        let owner = self.bound(k)?;
        check_name(name)?;
        with_retry(|| async {
            let mut tx = k.db.vault_tx(scope.tenant()).await?;
            let rows = rows(&mut tx, scope, owner, name).await?;
            let row = current(&rows, name)?;
            if !row.rule.reveal {
                return Err(Error::Forbidden(format!("{name}'s rule keeps its value in the vault: it is used, never revealed")));
            }
            let value = opened(k, &mut tx, scope, owner, name, row).await?;
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
            let value = opened(k, &mut tx, scope, owner, name, row).await?;
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
                    _ => seal::hmac_verify(&opened(k, &mut tx, scope, owner, name, r).await?, message, signature),
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
        loop {
            let (done, left) = reseal_some(self, scope, owner).await?;
            if left == 0 || done == 0 {
                return Ok(left);
            }
        }
    }
}

/// The vault's part of the hourly upkeep (`Kernel::maintenance`): the summaries of hours of uses that are over, the
/// versions whose overlap is over, and what older owner keys still seal, sealed again (the installation's scope has no
/// job queue; in a workspace this finds what a job left).
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

/// The handle of an owner a person names: the platform, an app this server registered, or a plugin. Kept in here.
fn of_owner(k: &Kernel, owner: &str) -> Result<Vault> {
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

    /// Sets `owner`'s secret `name` in the workspace, for a person: an administrator who has just confirmed it is them.
    /// A value the vault made is returned to show them once.
    pub async fn vault_set(&self, ctx: &Ctx, owner: &str, name: &str, value: New, rule: Rule) -> Result<Changed> {
        may_manage(ctx, true)?;
        let v = of_owner(self, owner)?;
        let who = Who { actor: Some(ctx.user), request: &ctx.request_id };
        let (secret, shown) = write(self, Scope::Workspace(ctx.tenant), v.bound(self)?, name, value, Some(rule), &who).await?;
        Ok(Changed { secret, shown })
    }

    /// Rotates `owner`'s secret `name` in the workspace, for a person, as [`Kernel::vault_set`].
    pub async fn vault_rotate(&self, ctx: &Ctx, owner: &str, name: &str, value: New) -> Result<Changed> {
        may_manage(ctx, true)?;
        let v = of_owner(self, owner)?;
        let who = Who { actor: Some(ctx.user), request: &ctx.request_id };
        let (secret, shown) = write(self, Scope::Workspace(ctx.tenant), v.bound(self)?, name, value, None, &who).await?;
        Ok(Changed { secret, shown })
    }

    /// Removes `owner`'s secret `name` from the workspace, for a person, as [`Kernel::vault_set`].
    pub async fn vault_remove(&self, ctx: &Ctx, owner: &str, name: &str) -> Result<bool> {
        may_manage(ctx, true)?;
        let v = of_owner(self, owner)?;
        remove(self, Scope::Workspace(ctx.tenant), v.bound(self)?, name, &Who { actor: Some(ctx.user), request: &ctx.request_id }).await
    }
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
