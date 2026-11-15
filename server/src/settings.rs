//! The server's settings (docs/plan/stateless.md, D1 and D2). Every setting is declared once, in `settings.yaml`.
//! A replica reads them as it starts, over a pool of two connections of its own, before anything else connects: the
//! environment's value when the environment sets one (through R1 it still wins, so that a rollback behaves), else the
//! `setting` table's, else the default. They land where they always have, in Loco's configuration and in the two
//! settings structs built from it, so that everything downstream reads them as before.

use crate::state::{AppState, Settings};
use ferrum_kernel::KernelSettings;
use loco_rs::config::Config;
use loco_rs::mailer::EmailSender;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;
use tokio::sync::watch;

/// A setting's type: what its value may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Text,
    Bool,
    Int,
    Json,
    Choice,
}

/// Where a setting's row is: the whole installation's, or one pool of replicas' own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    Installation,
    Pool,
}

/// When a change takes effect: at a replica's next start, or within seconds on every replica.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Applies {
    #[default]
    Start,
    Live,
}

/// One setting of the registry (`settings.yaml`, whose head says what each field means).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Def {
    pub key: String,
    pub env: String,
    #[serde(default, rename = "type")]
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    #[serde(default)]
    pub default: String,
    #[serde(default)]
    pub scope: Scope,
    #[serde(default)]
    pub applies: Applies,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub secret: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sends: Option<String>,
    pub doc: String,
}

impl Def {
    /// Kept in the `setting` table: neither a secret nor read from the environment by someone else.
    pub fn stored(&self) -> bool {
        !self.secret && self.environment.is_none()
    }

    /// Whether `text` is a value of this setting, said in plain words when it is not.
    pub fn check(&self, text: &str) -> Result<(), String> {
        let t = text.trim();
        let ok = match self.kind {
            Kind::Text => true,
            Kind::Bool => t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("false"),
            Kind::Int => t.parse::<u64>().is_ok() || (t.is_empty() && self.default.is_empty()),
            Kind::Json => t.is_empty() || serde_json::from_str::<Value>(t).is_ok(),
            Kind::Choice => self.values.iter().any(|v| v == t),
        };
        if ok {
            return Ok(());
        }
        Err(match self.kind {
            Kind::Bool => format!("{} is true or false, not {t:?}", self.key),
            Kind::Int => format!("{} is a whole number, 0 or more, not {t:?}", self.key),
            Kind::Json => format!("{} is JSON, and {t:?} is not", self.key),
            Kind::Choice => format!("{} is one of {}, not {t:?}", self.key, self.values.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join(", ")),
            Kind::Text => unreachable!(),
        })
    }

    /// `text` as its place in the configuration file holds it once the file is read: a value checked by [`Def::check`].
    fn typed(&self, text: &str) -> Value {
        let t = text.trim();
        match self.kind {
            Kind::Bool => Value::Bool(t.eq_ignore_ascii_case("true")),
            Kind::Int => t.parse::<u64>().map(Value::from).unwrap_or(Value::Null),
            _ => Value::String(text.to_string()),
        }
    }
}

static REGISTRY: LazyLock<Vec<Def>> = LazyLock::new(|| serde_yaml::from_str(include_str!("settings.yaml")).expect("server/src/settings.yaml is the registry"));

/// Every setting, as `settings.yaml` declares it.
pub fn registry() -> &'static [Def] {
    &REGISTRY
}

/// The setting called `name`, by its key or its environment variable.
pub fn find(name: &str) -> Option<&'static Def> {
    let name = name.trim();
    REGISTRY.iter().find(|d| d.key == name || d.env == name)
}

/// Where a setting's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Environment,
    Database,
    Default,
}

/// A row of the `setting` table: its value as the variable would say it, `None` once unset.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub scope: String,
    pub scope_name: String,
    pub key: String,
    pub value: Option<String>,
    pub version: i64,
}

/// What the table holds, and its highest version (0 with no row, and on a database without the table yet).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stored {
    pub rows: Vec<Row>,
    pub version: i64,
}

impl Stored {
    /// `def`'s row for a replica of `pool`: its pool's for a pool's own setting, else the installation's.
    fn row(&self, def: &Def, pool: &str) -> Option<&Row> {
        let (scope, name) = place(def, pool);
        self.rows.iter().find(|r| r.key == def.key && r.scope == scope && r.scope_name == name)
    }
}

/// The pool of replicas whose address names none.
pub const DEFAULT_POOL: &str = "default";

/// The pool a replica is in (D1): `application_name=pool:<name>` in its database address names it; without one (or
/// with another application name), `default`. A name that is not lower-case letters, digits and dashes is refused.
pub fn pool_name(url: &str) -> Result<String, String> {
    let named = reqwest::Url::parse(url).ok().and_then(|u| u.query_pairs().find(|(k, _)| k == "application_name").map(|(_, v)| v.into_owned()));
    let Some(name) = named.as_deref().and_then(|a| a.strip_prefix("pool:")) else { return Ok(DEFAULT_POOL.into()) };
    plain_pool(name).map_err(|e| format!("DATABASE_URL names {e} (application_name=pool:...)"))
}

/// Reads the table over a pool of two connections of its own, closed again before the server opens its real pools.
/// A database without the table (this build's first start on it: the migrations make it) holds nothing.
pub async fn read(url: &str) -> Result<Stored, String> {
    let options = PgConnectOptions::from_str(url).map_err(|e| format!("DATABASE_URL is not a database address: {e}"))?;
    // As the main pool connects through a pooler in transaction mode: nothing a pooler refuses, nothing kept in the session.
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .min_connections(0)
        .acquire_timeout(Duration::from_secs(10))
        .connect_lazy_with(options.application_name("ferrum").extra_float_digits(None));
    let read = read_with(&pool).await;
    pool.close().await;
    read.map_err(|e| format!("the settings cannot be read from the database (DATABASE_URL): {e}"))
}

/// The table's rows, in a system transaction of `pool`.
pub async fn read_with(pool: &sqlx::PgPool) -> Result<Stored, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("select set_config('ferrum.system', 'on', true)").execute(&mut *tx).await?;
    let there: bool = sqlx::query_scalar("select to_regclass('setting') is not null").fetch_one(&mut *tx).await?;
    let rows: Vec<(String, String, String, Option<Value>, i64)> =
        if there { sqlx::query_as("select scope, scope_name, key, value, version from setting").fetch_all(&mut *tx).await? } else { Vec::new() };
    tx.commit().await?;
    let version = rows.iter().map(|r| r.4).max().unwrap_or(0);
    let rows =
        rows.into_iter().map(|(scope, scope_name, key, value, version)| Row { scope, scope_name, key, value: value.as_ref().map(text_of), version }).collect();
    Ok(Stored { rows, version })
}

/// A stored value as the variable would say it (a JSON string; anything else written there by hand, as JSON).
fn text_of(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_string)
}

/// The channel a change of the table is announced on: every replica listens (crates/kernel/src/registry.rs).
pub const CHANNEL: &str = ferrum_kernel::registry::SETTINGS_CHANNEL;

/// The lock every write of the table holds until it commits: writes one at a time, so that versions follow the order of
/// the commits and two replicas copying their environment at once copy each setting once.
async fn lock(tx: &mut ferrum_kernel::Tx) -> Result<(), sqlx::Error> {
    sqlx::query("select pg_advisory_xact_lock(hashtextextended('ferrum_settings', 0))").execute(&mut **tx).await?;
    Ok(())
}

/// Where `def`'s row is for a replica of `pool`: `(scope, scope_name)`.
fn place<'a>(def: &Def, pool: &'a str) -> (&'static str, &'a str) {
    match def.scope {
        Scope::Pool => ("pool", pool),
        Scope::Installation => ("installation", ""),
    }
}

/// Records a change of the table in the transaction that makes it: the value before and after (`None`: unset).
#[allow(clippy::too_many_arguments)]
async fn record(
    tx: &mut ferrum_kernel::Tx,
    (scope, name): (&str, &str),
    key: &str,
    old: Option<&str>,
    new: Option<&str>,
    version: i64,
    by: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "insert into setting_change (scope, scope_name, key, old_value, new_value, version, updated_by) \
         values ($1, $2, $3, to_jsonb($4::text), to_jsonb($5::text), $6, $7)",
    )
    .bind(scope)
    .bind(name)
    .bind(key)
    .bind(old)
    .bind(new)
    .bind(version)
    .bind(by)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// R1: each setting the environment (`env`) sets and the table does not hold yet is written there once, as the
/// environment's, so that an installation configured by its environment ends with its settings in the table. A row the
/// table holds, even one an operator unset, is left as it is; a variable set to nothing says the default, and is not
/// copied. Returns the keys written, each with the version it made.
pub async fn adopt(db: &ferrum_kernel::Db, pool: &str, env: &(dyn Fn(&str) -> Option<String> + Sync)) -> ferrum_kernel::Result<Vec<(String, i64)>> {
    let mut tx = db.system_tx().await?;
    lock(&mut tx).await?;
    let mut written = Vec::new();
    for def in registry().iter().filter(|d| d.stored()) {
        let Some(text) = env(&def.env).filter(|v| !v.trim().is_empty()) else { continue };
        let at = place(def, pool);
        let version: Option<i64> = sqlx::query_scalar(
            "insert into setting (scope, scope_name, key, value, updated_by) values ($1, $2, $3, to_jsonb($4::text), 'environment') \
             on conflict do nothing returning version",
        )
        .bind(at.0)
        .bind(at.1)
        .bind(&def.key)
        .bind(&text)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(version) = version {
            record(&mut tx, at, &def.key, None, Some(&text), version, "environment").await?;
            written.push((def.key.clone(), version));
        }
    }
    if !written.is_empty() {
        sqlx::query("select pg_notify($1, '')").bind(CHANNEL).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(written)
}

/// What every setting is for one replica, and where each comes from.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The replica's pool.
    pub pool: String,
    /// The table's version it was made from.
    pub version: i64,
    /// By key: the source, and the value as the variable would say it (`None` for the default).
    values: BTreeMap<String, (Source, Option<String>)>,
}

impl Snapshot {
    /// The environment (`env`, when it sets the variable at all, even to nothing) over the table over the default. A
    /// secret, or a setting someone else reads from the environment, comes from the environment alone; so does an
    /// address while the secret it is given is in the environment.
    pub fn resolve(stored: &Stored, pool: &str, env: &dyn Fn(&str) -> Option<String>) -> Snapshot {
        let mut values = BTreeMap::new();
        for def in registry() {
            let given_secret = def.sends.as_deref().is_some_and(|s| env(s).is_some_and(|v| !v.is_empty()));
            let entry = match env(&def.env) {
                Some(v) => (Source::Environment, Some(v)),
                None => match stored.row(def, pool).and_then(|r| r.value.clone()) {
                    Some(v) if def.stored() && !given_secret => (Source::Database, Some(v)),
                    _ => (Source::Default, None),
                },
            };
            values.insert(def.key.clone(), entry);
        }
        Snapshot { pool: pool.to_string(), version: stored.version, values }
    }

    /// Where `key`'s value comes from, and the value (`None`: the default).
    pub fn get(&self, key: &str) -> (Source, Option<&str>) {
        self.values.get(key).map_or((Source::Default, None), |(s, v)| (*s, v.as_deref()))
    }

    /// What the variable `name` (or the setting of that key) says, as `std::env::var(name).ok()` would: the
    /// environment's value, else the table's, else nothing.
    pub fn var(&self, name: &str) -> Option<String> {
        find(name).and_then(|d| self.get(&d.key).1.map(str::to_string))
    }

    /// Every variable's value, for [`ferrum_kernel::env`].
    pub fn vars(&self) -> HashMap<String, Option<String>> {
        registry().iter().map(|d| (d.env.clone(), self.get(&d.key).1.map(str::to_string))).collect()
    }

    /// Whether the table sets anything of the mail sender's.
    pub fn mailer_from_database(&self) -> bool {
        registry().iter().any(|d| d.key.starts_with("mailer_") && self.get(&d.key).0 == Source::Database)
    }
}

/// Where the configuration keeps its [`Base`].
const BASE: &str = "_base";

/// The configuration as its file and the environment made it, before any of the table's values: where a setting the
/// table no longer sets goes back to. It travels in the configuration (`settings._base`), so that the second of Loco's
/// two set-ups of `ferrum start`, which is handed the first one's configuration, starts from the same place.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Base {
    /// Stored settings that are Loco's own (`log_level`, `port`, …), as their variable would say them.
    loco: BTreeMap<String, String>,
    /// Stored settings under `settings:`, as the file holds them.
    settings: BTreeMap<String, Value>,
    /// Loco's own migrator's switch (`database.auto_migrate`), which follows `migrate_at_start`.
    auto_migrate: bool,
}

impl Base {
    /// `config`'s, or what it carries from a set-up before.
    pub fn of(config: &Config) -> Base {
        if let Some(base) = config.settings.as_ref().and_then(|s| s.get(BASE)).and_then(|b| serde_json::from_value(b.clone()).ok()) {
            return base;
        }
        let mut base = Base { auto_migrate: config.database.auto_migrate, ..Default::default() };
        let empty = json!({});
        let settings = config.settings.as_ref().unwrap_or(&empty);
        for def in registry().iter().filter(|d| d.stored()) {
            match def.path.as_deref().map(|p| p.strip_prefix("settings.").ok_or(p)) {
                Some(Ok(field)) => {
                    if let Some(v) = at(settings, field) {
                        base.settings.insert(def.key.clone(), v.clone());
                    }
                }
                Some(Err(_)) => {
                    if let Some(v) = loco_get(config, &def.key) {
                        base.loco.insert(def.key.clone(), v);
                    }
                }
                None => {}
            }
        }
        base
    }

    /// `config` as `snap` has it: the table's value wherever it gives one, the base's elsewhere.
    pub fn apply(&self, snap: &Snapshot, config: &mut Config) -> Result<(), String> {
        let mut settings = config.settings.take().unwrap_or_else(|| json!({}));
        if let Some(o) = settings.as_object_mut() {
            o.remove(BASE);
        }
        for def in registry().iter().filter(|d| d.stored()) {
            let Some(path) = def.path.as_deref() else { continue };
            let from_table = match snap.get(&def.key) {
                (Source::Database, Some(text)) => Some(text),
                _ => None,
            };
            match path.strip_prefix("settings.") {
                Some(field) => match from_table.map(|t| def.typed(t)).or_else(|| self.settings.get(&def.key).cloned()) {
                    Some(v) => put(&mut settings, field, v),
                    None => remove(&mut settings, field),
                },
                None => {
                    if let Some(text) = from_table.or(self.loco.get(&def.key).map(String::as_str)) {
                        loco_put(config, &def.key, text)?;
                    }
                }
            }
        }
        // Loco's own migrator follows the same setting (it has no migrations: the kernel owns the schema).
        config.database.auto_migrate = match snap.get("migrate_at_start") {
            (Source::Database, Some(text)) => text.trim().eq_ignore_ascii_case("true"),
            _ => self.auto_migrate,
        };
        settings[BASE] = serde_json::to_value(self).map_err(|e| e.to_string())?;
        config.settings = Some(settings);
        Ok(())
    }
}

/// The value at a dotted path of `v`.
fn at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(v, |v, part| v.get(part))
}

fn put(v: &mut Value, path: &str, value: Value) {
    let mut parts: Vec<&str> = path.split('.').collect();
    let last = parts.pop().unwrap_or_default();
    let mut v = v;
    for part in parts {
        if !v.get(part).is_some_and(Value::is_object) {
            v[part] = json!({});
        }
        v = &mut v[part];
    }
    v[last] = value;
}

fn remove(v: &mut Value, path: &str) {
    let (parent, last) = path.rsplit_once('.').map_or((None, path), |(p, l)| (Some(p), l));
    let parent = match parent {
        Some(p) => p.split('.').try_fold(&mut *v, |v, part| v.get_mut(part)),
        None => Some(v),
    };
    if let Some(o) = parent.and_then(Value::as_object_mut) {
        o.remove(last);
    }
}

/// A setting of Loco's own configuration, as its variable would say it.
fn loco_get(c: &Config, key: &str) -> Option<String> {
    let smtp = c.mailer.as_ref().and_then(|m| m.smtp.as_ref());
    let m = &c.server.middlewares;
    match key {
        "log_level" => serde_json::to_value(&c.logger.level).ok().and_then(|v| v.as_str().map(str::to_string)),
        "port" => Some(c.server.port.to_string()),
        "binding" => Some(c.server.binding.clone()),
        "public_url" => Some(c.server.host.clone()),
        "request_timeout_ms" => m.timeout_request.as_ref().map(|t| t.timeout.to_string()),
        "cors_enabled" => m.cors.as_ref().map(|x| x.enable.to_string()),
        "cors_origins" => m.cors.as_ref().map(|x| x.allow_origins.iter().map(|o| format!("{o:?}")).collect::<Vec<_>>().join(", ")),
        "mailer_enabled" => smtp.map(|s| s.enable.to_string()),
        "mailer_host" => smtp.map(|s| s.host.clone()),
        "mailer_port" => smtp.map(|s| s.port.to_string()),
        "mailer_secure" => smtp.map(|s| s.secure.to_string()),
        "mailer_user" => smtp.and_then(|s| s.auth.as_ref()).map(|a| a.user.clone()),
        "jwt_expiration_seconds" => c.auth.as_ref().and_then(|a| a.jwt.as_ref()).map(|j| j.expiration.to_string()),
        _ => None,
    }
}

/// Sets a setting of Loco's own configuration from `text`, where the configuration has that part at all.
fn loco_put(c: &mut Config, key: &str, text: &str) -> Result<(), String> {
    let t = text.trim();
    let number = |what: &str| format!("{key} is a whole number, not {t:?} ({what})");
    let truth = || t.eq_ignore_ascii_case("true");
    let smtp = c.mailer.as_mut().and_then(|m| m.smtp.as_mut());
    let m = &mut c.server.middlewares;
    match key {
        "log_level" => {
            c.logger.level = serde_json::from_value(json!(t)).map_err(|_| format!("log_level is off, trace, debug, info, warn or error, not {t:?}"))?
        }
        "port" => c.server.port = t.parse().map_err(|_| number("a port"))?,
        "binding" => c.server.binding = t.to_string(),
        "public_url" => c.server.host = text.to_string(),
        "request_timeout_ms" => {
            if let Some(x) = m.timeout_request.as_mut() {
                x.timeout = t.parse().map_err(|_| number("milliseconds"))?;
            }
        }
        "cors_enabled" => {
            if let Some(x) = m.cors.as_mut() {
                x.enable = truth();
            }
        }
        // As the file splices the variable into `allow_origins: [...]`.
        "cors_origins" => {
            if let Some(x) = m.cors.as_mut() {
                x.allow_origins = serde_yaml::from_str(&format!("[{text}]")).map_err(|e| format!("cors_origins: {e}"))?;
            }
        }
        "mailer_enabled" | "mailer_host" | "mailer_port" | "mailer_secure" | "mailer_user" => {
            let Some(s) = smtp else { return Ok(()) };
            match key {
                "mailer_enabled" => s.enable = truth(),
                "mailer_host" => s.host = t.to_string(),
                "mailer_port" => s.port = t.parse().map_err(|_| number("a port"))?,
                "mailer_secure" => s.secure = truth(),
                _ => {
                    if let Some(a) = s.auth.as_mut() {
                        a.user = text.to_string();
                    }
                }
            }
        }
        "jwt_expiration_seconds" => {
            if let Some(j) = c.auth.as_mut().and_then(|a| a.jwt.as_mut()) {
                j.expiration = t.parse().map_err(|_| number("seconds"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// How often a replica says it is alive and compares the table's version with its own, so that a change whose notice it
/// missed reaches it all the same (D2: within 30 seconds). Lowered by tests.
pub static EVERY_MS: AtomicU64 = AtomicU64::new(30_000);

/// The settings' values a replica's readers take while it runs: at its start, and again whenever one that applies at
/// once changes.
pub struct Now {
    /// What the table, the environment and the defaults said.
    pub snapshot: Snapshot,
    /// The start's settings, with those that apply at once as `snapshot` has them.
    pub settings: Settings,
    /// `request_timeout_ms`, when the configuration times requests out at all.
    pub timeout_ms: Option<u64>,
    /// `jwt_expiration_seconds`.
    pub jwt_expiration: u64,
}

/// One replica's settings while it runs (D2, live apply).
pub struct Live {
    /// The replica's pool (D1).
    pub pool: String,
    /// The replica's row in `replica`.
    pub replica: uuid::Uuid,
    /// The configuration the start made, and its base: a newer snapshot is applied to a copy of it.
    config: Config,
    base: Base,
    /// As the replica started.
    pub start: Arc<Now>,
    now: RwLock<Arc<Now>>,
    /// The pool's replicas seen in the last two minutes, this one among them.
    replicas: AtomicU32,
    /// The table's version this replica runs the settings that apply at start of (`replica.settings_version`): its
    /// start's, or a later one that only copied its own environment there.
    seen: std::sync::atomic::AtomicI64,
}

impl Live {
    /// The settings a replica started with: `config` as the start made it from `snapshot`, and `settings` from it.
    pub fn new(pool: &str, config: &Config, snapshot: Snapshot, settings: Settings) -> Live {
        let now = Arc::new(Now { snapshot, settings, timeout_ms: timeout_ms(config), jwt_expiration: jwt_expiration(config) });
        Live {
            pool: pool.to_string(),
            replica: uuid::Uuid::now_v7(),
            config: config.clone(),
            base: Base::of(config),
            start: now.clone(),
            seen: std::sync::atomic::AtomicI64::new(now.snapshot.version),
            now: RwLock::new(now),
            replicas: AtomicU32::new(1),
        }
    }

    /// The values the readers take now.
    pub fn now(&self) -> Arc<Now> {
        self.now.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// What this replica copied from its own environment into the table, at `version`: values it runs already.
    pub fn saw(&self, version: i64) {
        self.seen.fetch_max(version, Ordering::Relaxed);
    }

    /// The pool's live replicas, this one among them.
    pub fn replicas(&self) -> u32 {
        self.replicas.load(Ordering::Relaxed).max(1)
    }

    /// `snapshot`'s values for what applies at once, over the start's for the rest, and the kernel's: refused, saying
    /// why, where the start's checks would refuse them.
    pub fn next(&self, snapshot: &Snapshot) -> Result<(Now, KernelLive), String> {
        let mut config = self.config.clone();
        self.base.apply(snapshot, &mut config)?;
        let new: Settings = serde_json::from_value(config.settings.clone().unwrap_or_else(|| json!({}))).map_err(|e| format!("invalid settings: {e}"))?;
        let settings = live_view(&self.start.settings, &new);
        // What the kernel reads directly of these (plans, sign-up, connections) all applies at once.
        let kernel = kernel_live(&settings, &|name| snapshot.var(name))?;
        ferrum_kernel::zones::Zones::parse("", "", "", &settings.zone_mode)?;
        Ok((Now { snapshot: snapshot.clone(), settings, timeout_ms: timeout_ms(&config), jwt_expiration: jwt_expiration(&config) }, kernel))
    }

    /// Every variable as the readers take it now: those that apply at once from `now`, the rest as at the start.
    fn vars(&self, now: &Snapshot) -> HashMap<String, Option<String>> {
        registry()
            .iter()
            .map(|d| {
                let from = if d.applies == Applies::Live { now } else { &self.start.snapshot };
                (d.env.clone(), from.get(&d.key).1.map(str::to_string))
            })
            .collect()
    }
}

fn timeout_ms(config: &Config) -> Option<u64> {
    config.server.middlewares.timeout_request.as_ref().filter(|t| t.enable).map(|t| t.timeout)
}

fn jwt_expiration(config: &Config) -> u64 {
    config.auth.as_ref().and_then(|a| a.jwt.as_ref()).map_or(0, |j| j.expiration)
}

/// `start` with the settings that apply at once (`applies: live` in the registry) as `new` has them.
pub fn live_view(start: &Settings, new: &Settings) -> Settings {
    Settings {
        user_requests_per_minute: new.user_requests_per_minute,
        address_requests_per_minute: new.address_requests_per_minute,
        proxy_hops: new.proxy_hops,
        session_days: new.session_days,
        direct_upload_max_bytes: new.direct_upload_max_bytes,
        clamav_addr: new.clamav_addr.clone(),
        clamav_max_mb: new.clamav_max_mb,
        pdf_url: new.pdf_url.clone(),
        push_services: new.push_services.clone(),
        push_contact: new.push_contact.clone(),
        audit_sink_url: new.audit_sink_url.clone(),
        host_leaving_days: new.host_leaving_days,
        dns_resolvers: new.dns_resolvers.clone(),
        zone_mode: new.zone_mode.clone(),
        ..start.clone()
    }
}

/// The kernel's settings that apply at once, from the server's (`settings`) and the variables read directly (`var`):
/// the same at the start and on a change, refused as the start refuses them.
pub struct KernelLive {
    pub connection_hosts: Vec<String>,
    pub connection_upload_bytes: u64,
    pub clamd: Option<String>,
    pub clamd_max_bytes: u64,
    pub pdf_url: Option<String>,
    pub push_services: Vec<String>,
    pub push_contact: Option<String>,
    pub audit_sink: Option<ferrum_kernel::audit_sink::Sink>,
    pub plans: serde_json::Map<String, Value>,
    pub signup: ferrum_kernel::signup::SignupPolicy,
    pub host_leaving_days: i64,
    pub dns_resolvers: Vec<std::net::SocketAddr>,
}

pub fn kernel_live(settings: &Settings, var: &dyn Fn(&str) -> Option<String>) -> Result<KernelLive, String> {
    let (plans, signup) = crate::app::signup_settings(var, crate::shell::base_domain(settings).is_some())?;
    Ok(KernelLive {
        connection_hosts: crate::app::connection_hosts(var("FERRUM_CONNECTION_HOSTS")),
        connection_upload_bytes: crate::app::connection_upload_bytes(var("FERRUM_CONNECTION_UPLOAD_MB")),
        clamd: Some(settings.clamav_addr.trim().to_string()).filter(|a| !a.is_empty()),
        clamd_max_bytes: settings.clamav_max_mb << 20,
        pdf_url: Some(settings.pdf_url.trim().to_string()).filter(|u| !u.is_empty()),
        push_services: crate::app::push_services(&settings.push_services),
        push_contact: Some(settings.push_contact.trim().to_string()).filter(|c| c.starts_with("mailto:") || c.starts_with("https://")),
        audit_sink: ferrum_kernel::audit_sink::Sink::parse(&settings.audit_sink_url, &settings.audit_sink_secret)?,
        plans,
        signup,
        host_leaving_days: settings.host_leaving_days.min(365).into(),
        dns_resolvers: ferrum_kernel::shell::dns_resolvers(&settings.dns_resolvers)?,
    })
}

impl KernelLive {
    /// `settings` with these in place of its own.
    pub fn onto(self, settings: KernelSettings) -> KernelSettings {
        KernelSettings {
            connection_hosts: self.connection_hosts,
            connection_upload_bytes: self.connection_upload_bytes,
            clamd: self.clamd,
            clamd_max_bytes: self.clamd_max_bytes,
            pdf_url: self.pdf_url,
            push_services: self.push_services,
            push_contact: self.push_contact,
            audit_sink: self.audit_sink,
            plans: self.plans,
            signup: self.signup,
            host_leaving_days: self.host_leaving_days,
            dns_resolvers: self.dns_resolvers,
            ..settings
        }
    }
}

/// A per-replica budget's share: the pool's `limit` divided among its `replicas` (0 stays no limit).
pub fn share(limit: u32, replicas: u32) -> u32 {
    if limit == 0 { 0 } else { limit.div_ceil(replicas.max(1)).max(1) }
}

/// The replica's request budgets: each setting divided among the pool's live replicas.
fn budgets(st: &AppState) {
    let (now, n) = (st.live_settings.now(), st.live_settings.replicas());
    st.user_limiter.set_max(share(now.settings.user_requests_per_minute, n));
    st.address_limiter.set_max(share(now.settings.address_requests_per_minute, n));
}

/// Reads the table and, when it changed, takes the new values of the settings that apply at once. A value the start
/// would refuse is not taken: the replica keeps the ones it has, and says why.
pub async fn refresh(st: &AppState) {
    let stored = match read_with(&st.kernel.db.pool).await {
        Ok(s) => s,
        Err(e) => {
            return tracing::warn!("reading the settings from the database failed (tried again within {} s): {e}", EVERY_MS.load(Ordering::Relaxed) / 1000);
        }
    };
    let before = st.live_settings.now();
    if stored.version == before.snapshot.version {
        return;
    }
    let snapshot = Snapshot::resolve(&stored, &st.live_settings.pool, &|name| std::env::var(name).ok());
    let (next, kernel) = match st.live_settings.next(&snapshot) {
        Ok(n) => n,
        Err(e) => return tracing::error!(version = stored.version, "the settings in the database changed, and this replica keeps the ones it has: {e}"),
    };
    if let Err(e) = st.kernel.registry.catalog().closure(&kernel.signup.packages) {
        return tracing::error!(
            version = stored.version,
            "the settings in the database changed, and this replica keeps the ones it has: FERRUM_SIGNUP_PACKAGES: {e}"
        );
    }
    if kernel.signup.open && st.kernel.mailer.is_none() {
        return tracing::error!(version = stored.version, "the settings in the database changed, and this replica keeps the ones it has: sign-up needs mail");
    }
    let changed: Vec<&str> = registry().iter().filter(|d| snapshot.get(&d.key) != before.snapshot.get(&d.key)).map(|d| d.key.as_str()).collect();
    let (at_once, later): (Vec<&str>, Vec<&str>) = changed.iter().partition(|k| find(k).is_some_and(|d| d.applies == Applies::Live));
    st.kernel.set_live(kernel.onto(st.kernel.settings.clone()));
    ferrum_kernel::env::set(st.live_settings.vars(&snapshot));
    if let Ok(z) = ferrum_kernel::zones::Zones::parse("", "", "", &next.settings.zone_mode) {
        st.zones.set_mode(z.mode());
    }
    crate::extract::PROXY_HOPS.store(next.settings.proxy_hops, Ordering::Relaxed);
    st.shell.set_session_days(next.settings.session_days);
    *st.live_settings.now.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);
    budgets(st);
    tracing::info!(version = stored.version, now = %at_once.join(", "), at_next_start = %later.join(", "), "the settings changed in the database");
}

/// Says this replica is alive (`replica`, refreshed every [`EVERY_MS`]; rows not seen for two minutes go), counts its
/// pool's live replicas, and divides the request budgets among them.
async fn heartbeat(st: &AppState) -> Result<(), ferrum_kernel::Error> {
    let live = &st.live_settings;
    let mut tx = st.kernel.db.system_tx().await?;
    // A fenced database takes no writes: the replica is counted as it was.
    if !st.kernel.db.fenced() {
        sqlx::query(
            "insert into replica (id, pool, build_date, settings_version) values ($1, $2, $3, $4) \
             on conflict (id) do update set seen_at = now(), settings_version = excluded.settings_version",
        )
        .bind(live.replica)
        .bind(&live.pool)
        .bind(ferrum_kernel::window::build_date())
        .bind(live.seen.load(Ordering::Relaxed))
        .execute(&mut *tx)
        .await?;
        sqlx::query("delete from replica where seen_at < now() - interval '2 minutes'").execute(&mut *tx).await?;
    }
    let n: i64 = sqlx::query_scalar("select count(*) from replica where pool = $1 and seen_at > now() - interval '2 minutes'")
        .bind(&live.pool)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    live.replicas.store(u32::try_from(n).unwrap_or(u32::MAX).max(1), Ordering::Relaxed);
    budgets(st);
    Ok(())
}

/// The replica's settings loop, on every replica that serves: a change takes effect as soon as its notice comes (the
/// metadata listener hears `ferrum_settings`), and every [`EVERY_MS`] the replica says it is alive and compares the
/// table's version with its own. Its row goes when it stops.
pub fn spawn(st: AppState, mut stop: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut notices = st.kernel.registry.settings_notices();
        let mut tick = tokio::time::interval(Duration::from_millis(EVERY_MS.load(Ordering::Relaxed).max(10)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = stop.changed() => break,
                Ok(()) = notices.changed() => refresh(&st).await,
                _ = tick.tick() => {
                    if let Err(e) = heartbeat(&st).await {
                        tracing::warn!("this replica's row in the replica table: {e}");
                    }
                    refresh(&st).await;
                }
            }
        }
        if let Ok(mut tx) = st.kernel.db.system_tx().await
            && sqlx::query("delete from replica where id = $1").bind(st.live_settings.replica).execute(&mut *tx).await.is_ok()
        {
            let _ = tx.commit().await;
        }
    })
}

/// `timeout_request` (Loco's), reading `request_timeout_ms` as each request comes, so that a change applies at once.
pub struct LiveTimeout(pub Arc<Live>);

impl loco_rs::controller::middleware::MiddlewareLayer for LiveTimeout {
    fn name(&self) -> &'static str {
        "timeout_request"
    }

    fn is_enabled(&self) -> bool {
        true
    }

    fn config(&self) -> serde_json::Result<Value> {
        Ok(json!({"enable": true, "timeout": self.0.now().timeout_ms}))
    }

    fn apply(&self, app: axum::Router<loco_rs::app::AppContext>) -> loco_rs::Result<axum::Router<loco_rs::app::AppContext>> {
        let live = self.0.clone();
        Ok(app.layer(axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
            let live = live.clone();
            async move {
                use axum::response::IntoResponse;
                let Some(ms) = live.now().timeout_ms else { return next.run(req).await };
                // As tower-http's TimeoutLayer that Loco's middleware is: 408, empty.
                tokio::time::timeout(Duration::from_millis(ms), next.run(req)).await.unwrap_or_else(|_| axum::http::StatusCode::REQUEST_TIMEOUT.into_response())
            }
        })))
    }
}

/// A change of one setting: `value` `None` unsets it (back to the environment's value or the default).
#[derive(Debug, Clone, Default)]
pub struct Change {
    /// The setting's key, or its variable.
    pub name: String,
    /// The pool, for a pool's own setting.
    pub pool: Option<String>,
    pub value: Option<String>,
}

/// What a change did: the value before and after, and the table's version it made (`None`: it was so already).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Done {
    pub key: String,
    pub pool: Option<String>,
    pub old: Option<String>,
    pub new: Option<String>,
    pub version: Option<i64>,
}

impl Stored {
    /// The table as it would be with `value` for `def` (for a replica of `pool`).
    fn put(&mut self, def: &Def, pool: &str, value: Option<String>) {
        let (scope, name) = place(def, pool);
        match self.rows.iter_mut().find(|r| r.key == def.key && r.scope == scope && r.scope_name == name) {
            Some(r) => r.value = value,
            None => self.rows.push(Row { scope: scope.into(), scope_name: name.into(), key: def.key.clone(), value, version: 0 }),
        }
    }
}

/// A pool's name: lower-case letters, digits and dashes.
fn plain_pool(name: &str) -> Result<String, String> {
    let plain = (1..=63).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if plain { Ok(name.to_string()) } else { Err(format!("the pool {name:?}: a pool's name is lower-case letters, digits and dashes")) }
}

/// Checks `changes` by the registry, then all together as a replica's start would check them, and writes them in one
/// transaction under the table's lock, each recorded in `setting_change` with its value before and after and `by`,
/// announced to every replica. When one is refused nothing is written. A secret, or a setting someone else reads from
/// the environment, is never accepted.
pub async fn change(st: &AppState, changes: &[Change], by: &str) -> ferrum_kernel::Result<Vec<Done>> {
    use ferrum_kernel::Error;
    let mut planned: Vec<(&'static Def, Option<String>, Option<String>)> = Vec::new();
    for c in changes {
        let def = find(&c.name).ok_or_else(|| Error::NotFound(format!("no setting is called {:?}", c.name.trim())))?;
        if def.secret {
            return Err(Error::Invalid(format!(
                "{} is a secret: it stays in the environment ({}) until the vault holds it, and is never written to the database",
                def.key, def.env
            )));
        }
        if let Some(who) = &def.environment {
            return Err(Error::Invalid(format!("{} is read from the environment by {who}: set {} there", def.key, def.env)));
        }
        let pool = match (def.scope, c.pool.as_deref().map(str::trim).filter(|p| !p.is_empty())) {
            (Scope::Pool, Some(p)) => Some(plain_pool(p).map_err(Error::Invalid)?),
            (Scope::Pool, None) => {
                return Err(Error::Invalid(format!("{} is each pool's own: say which pool ({DEFAULT_POOL} for replicas whose address names none)", def.key)));
            }
            (Scope::Installation, Some(_)) => return Err(Error::Invalid(format!("{} is the installation's, not a pool's", def.key))),
            (Scope::Installation, None) => None,
        };
        let value = c.value.as_deref().map(str::trim).map(str::to_string);
        if let Some(v) = &value {
            def.check(v).map_err(Error::Invalid)?;
        }
        if planned.iter().any(|(d, p, _)| d.key == def.key && *p == pool) {
            return Err(Error::Invalid(format!("{} is named twice", def.key)));
        }
        planned.push((def, pool, value));
    }
    // Together, as the replicas of each pool they reach would start with them.
    let mut would = read_with(&st.kernel.db.pool).await?;
    for (def, pool, value) in &planned {
        would.put(def, pool.as_deref().unwrap_or(DEFAULT_POOL), value.clone());
    }
    let mut pools: Vec<&str> = planned.iter().filter_map(|(_, p, _)| p.as_deref()).collect();
    pools.push(DEFAULT_POOL);
    pools.sort();
    pools.dedup();
    for pool in pools {
        check(st, &Snapshot::resolve(&would, pool, &|name| std::env::var(name).ok())).await.map_err(Error::Invalid)?;
    }
    let mut tx = st.kernel.db.system_tx().await?;
    lock(&mut tx).await?;
    let mut done = Vec::new();
    for (def, pool, value) in planned {
        let at = place(def, pool.as_deref().unwrap_or(DEFAULT_POOL));
        let old: Option<Option<Value>> = sqlx::query_scalar("select value from setting where scope = $1 and scope_name = $2 and key = $3 for update")
            .bind(at.0)
            .bind(at.1)
            .bind(&def.key)
            .fetch_optional(&mut *tx)
            .await?;
        let old = old.flatten().as_ref().map(text_of);
        let version = if old == value {
            None
        } else {
            let version: i64 = sqlx::query_scalar(
                "insert into setting (scope, scope_name, key, value, updated_by) values ($1, $2, $3, to_jsonb($4::text), $5) \
                 on conflict (scope, scope_name, key) do update set value = excluded.value, version = nextval('setting_version'), \
                 updated_at = now(), updated_by = excluded.updated_by returning version",
            )
            .bind(at.0)
            .bind(at.1)
            .bind(&def.key)
            .bind(value.as_deref())
            .bind(by)
            .fetch_one(&mut *tx)
            .await?;
            record(&mut tx, at, &def.key, old.as_deref(), value.as_deref(), version, by).await?;
            Some(version)
        };
        done.push(Done { key: def.key.clone(), pool, old, new: value, version });
    }
    if done.iter().any(|d| d.version.is_some()) {
        sqlx::query("select pg_notify($1, '')").bind(CHANNEL).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(done)
}

/// What a replica's start checks of `snapshot`'s values, so that a change cannot write what would keep the next
/// start from serving: the values' types, the host names, the zones, the audit sink, plans and sign-up, how domains
/// are proven, the base domain, object storage and the mail server.
async fn check(st: &AppState, snapshot: &Snapshot) -> Result<(), String> {
    let mut config = st.live_settings.config.clone();
    st.live_settings.base.apply(snapshot, &mut config)?;
    let settings: Settings = serde_json::from_value(config.settings.clone().unwrap_or_else(|| json!({}))).map_err(|e| format!("invalid settings: {e}"))?;
    crate::shell::check_names(&settings, &config.server.host)?;
    let zones = ferrum_kernel::zones::Zones::parse(&settings.zones, &settings.zone, &settings.zone_default, &settings.zone_mode)?;
    zones.check_background(settings.run_background)?;
    crate::zone::resolve(zones, &st.kernel).await?;
    let kernel = kernel_live(&settings, &|name| snapshot.var(name))?;
    st.kernel.registry.catalog().closure(&kernel.signup.packages).map_err(|e| format!("FERRUM_SIGNUP_PACKAGES: {e}"))?;
    let base = crate::shell::base_domain(&settings);
    ferrum_kernel::shell::DomainProof::from_setting(&settings.domain_proof, base.is_some())?;
    if let Some(base) = base
        && ferrum_kernel::shell::normalize_host(&format!("{}.{base}", ferrum_kernel::identity::DIRECTORY)).is_none()
    {
        return Err(format!("settings.base_domain ({base}) is not a domain name"));
    }
    if settings.require_object_storage && settings.s3_bucket.is_empty() {
        return Err(
            "object storage is required (require_object_storage): set s3_bucket too, or require_object_storage false for a single-server install".into()
        );
    }
    if let Some(smtp) = config.mailer.as_ref().and_then(|m| m.smtp.as_ref()).filter(|s| s.enable) {
        crate::mailer::smtp_transport(smtp).map_err(|e| format!("the mail server: {e}"))?;
    }
    Ok(())
}

/// A whole document of settings (`ferrum task apply_settings file:`, which the chart's Job runs): `installation:` and
/// `pools: {name: ...}`, each a map of settings (by key or variable) to a value (`null` unsets it), and `others`:
/// `keep` (the default) leaves alone what the table holds and the document does not name; `unset` unsets it, in the
/// installation and in each pool the document names.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    #[serde(default)]
    pub installation: BTreeMap<String, serde_yaml::Value>,
    #[serde(default)]
    pub pools: BTreeMap<String, BTreeMap<String, serde_yaml::Value>>,
    #[serde(default)]
    pub others: Others,
}

/// What a [`Document`] does with the settings it leaves out.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Others {
    #[default]
    Keep,
    Unset,
}

impl Document {
    /// Its changes, against what the table holds now.
    pub fn changes(&self, stored: &Stored) -> Result<Vec<Change>, String> {
        let text = |name: &str, v: &serde_yaml::Value| match v {
            serde_yaml::Value::Null => Ok(None),
            serde_yaml::Value::String(s) => Ok(Some(s.clone())),
            serde_yaml::Value::Bool(b) => Ok(Some(b.to_string())),
            serde_yaml::Value::Number(n) => Ok(Some(n.to_string())),
            _ => Err(format!("{name}: a setting's value is text, a number, true or false, or null to unset it")),
        };
        let mut out = Vec::new();
        for (name, v) in &self.installation {
            out.push(Change { name: name.clone(), pool: None, value: text(name, v)? });
        }
        for (pool, settings) in &self.pools {
            for (name, v) in settings {
                out.push(Change { name: name.clone(), pool: Some(pool.clone()), value: text(name, v)? });
            }
        }
        if self.others == Others::Unset {
            let named: std::collections::BTreeSet<(Option<String>, &str)> =
                out.iter().filter_map(|c| find(&c.name).map(|d| (c.pool.clone(), d.key.as_str()))).collect();
            let mut left_out = Vec::new();
            for r in stored.rows.iter().filter(|r| r.value.is_some()) {
                let pool = match r.scope.as_str() {
                    "installation" => None,
                    "pool" if self.pools.contains_key(&r.scope_name) => Some(r.scope_name.clone()),
                    _ => continue,
                };
                // A row of a setting this build does not know (a newer build's), or one at a scope its setting does not
                // read, is left as it is.
                let read = find(&r.key).is_some_and(|d| d.stored() && (d.scope == Scope::Pool) == pool.is_some());
                if read && !named.contains(&(pool.clone(), r.key.as_str())) {
                    left_out.push(Change { name: r.key.clone(), pool, value: None });
                }
            }
            out.extend(left_out);
        }
        Ok(out)
    }
}

/// Every setting as this replica has it (the environment's, the table's or the default), the table's own rows, and
/// the replicas running, with the settings that apply at start each still runs an older value of: what
/// `ferrum task settings` prints and `GET /api/operator/settings` answers. A secret's value is never in it, only
/// whether the environment sets it.
pub async fn listing(st: &AppState) -> ferrum_kernel::Result<Value> {
    let stored = read_with(&st.kernel.db.pool).await?;
    let now = st.live_settings.now();
    let settings: Vec<Value> = registry()
        .iter()
        .map(|d| {
            let (source, value) = now.snapshot.get(&d.key);
            let rows: BTreeMap<&str, Option<&str>> =
                stored.rows.iter().filter(|r| r.key == d.key && r.scope == place(d, "").0).map(|r| (r.scope_name.as_str(), r.value.as_deref())).collect();
            json!({
                "key": d.key,
                "env": d.env,
                "type": d.kind,
                "values": d.values,
                "default": d.default,
                "scope": d.scope,
                "applies": d.applies,
                "secret": d.secret,
                "environment": d.environment,
                "sends": d.sends,
                "source": source,
                "value": if d.secret { None } else { value },
                "set": value.is_some(),
                "stored": if d.scope == Scope::Installation { json!(rows.get("").copied().flatten()) } else { json!(rows) },
            })
        })
        .collect();
    let mut tx = st.kernel.db.system_tx().await?;
    let rows: Vec<(uuid::Uuid, String, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, i64)> = sqlx::query_as(
        "select id, pool, build_date, started_at, seen_at, settings_version from replica where seen_at > now() - interval '2 minutes' order by pool, started_at",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let replicas: Vec<Value> = rows
        .into_iter()
        .map(|(id, pool, build, started, seen, version)| {
            let older: Vec<&str> = stored
                .rows
                .iter()
                .filter(|r| r.version > version && (r.scope == "installation" || (r.scope == "pool" && r.scope_name == pool)))
                .filter_map(|r| find(&r.key).filter(|d| d.applies == Applies::Start).map(|d| d.key.as_str()))
                .collect();
            json!({"id": id, "pool": pool, "build_date": build, "started_at": started, "seen_at": seen, "settings_version": version, "older": older})
        })
        .collect();
    Ok(json!({"version": stored.version, "pool": st.live_settings.pool, "settings": settings, "replicas": replicas}))
}

/// The mail sender, made from `config` as Loco makes it before any of our code runs (its `create_mailer`): made again
/// when the table sets any of the mail settings.
pub fn mailer(config: &Config) -> loco_rs::Result<Option<EmailSender>> {
    let Some(m) = config.mailer.as_ref() else { return Ok(None) };
    if m.stub {
        return Ok(Some(EmailSender::stub()));
    }
    m.smtp.as_ref().filter(|s| s.enable).map(EmailSender::smtp).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(rows: &[(&str, &str)]) -> Stored {
        let rows: Vec<Row> = rows
            .iter()
            .enumerate()
            .map(|(i, (k, v))| Row {
                scope: "installation".into(),
                scope_name: String::new(),
                key: k.to_string(),
                value: Some(v.to_string()),
                version: i as i64 + 1,
            })
            .collect();
        Stored { version: rows.len() as i64, rows }
    }

    #[test]
    fn the_registry_declares_each_setting_once_and_soundly() {
        let r = registry();
        assert!(r.len() > 100, "every setting the server reads");
        let mut keys = std::collections::BTreeSet::new();
        let mut envs = std::collections::BTreeSet::new();
        for d in r {
            assert!(keys.insert(d.key.as_str()), "{} twice", d.key);
            assert!(envs.insert(d.env.as_str()), "{} twice", d.env);
            assert!(d.key.len() <= 63 && d.key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'), "{}", d.key);
            let lower = d.env.to_ascii_lowercase();
            assert_eq!(d.key, lower.strip_prefix("ferrum_").unwrap_or(&lower), "the key is its variable's name in lower case without FERRUM_");
            assert!(!(d.secret && d.environment.is_some()), "{}: a secret or someone else's, not both", d.key);
            assert!(d.stored() || d.applies == Applies::Start, "{}: only what the table holds can apply at once", d.key);
            assert!(d.scope == Scope::Installation || d.stored(), "{}", d.key);
            assert!(!d.doc.trim().is_empty(), "{} says what it does", d.key);
            assert!(d.secret || d.default.is_empty() || d.check(&d.default).is_ok(), "{}: its default {:?} is not of its type", d.key, d.default);
            assert_eq!(d.kind == Kind::Choice, !d.values.is_empty(), "{}: values go with a choice", d.key);
            if let Some(s) = &d.sends {
                assert!(find(s).is_some_and(|x| x.secret), "{}: {s} is a secret of the registry", d.key);
            }
        }
        let pools: Vec<&str> = r.iter().filter(|d| d.scope == Scope::Pool).map(|d| d.key.as_str()).collect();
        assert_eq!(pools, ["db_max_connections", "run_background", "zone"], "a pool's own: the connection budget, background work, the zone pin");
        assert_eq!(find("FERRUM_SESSION_DAYS").map(|d| d.key.as_str()), Some("session_days"), "found by variable or key");
        assert!(find("DATABASE_URL").is_none(), "the boot contract is not a setting");
    }

    #[test]
    fn the_address_names_the_pool() {
        assert_eq!(pool_name("postgres://u:p@db:5432/ferrum").unwrap(), "default");
        assert_eq!(pool_name("postgres://u:p@db:5432/ferrum?sslmode=require&application_name=pool:web").unwrap(), "web");
        assert_eq!(pool_name("postgres://u:p@db/ferrum?application_name=pool%3Abackground").unwrap(), "background", "as the address encodes it");
        assert_eq!(pool_name("postgres://u:p@db/ferrum?application_name=main").unwrap(), "default", "another application name");
        for bad in ["pool:Web", "pool:", "pool:-a", "pool:a_b"] {
            assert!(pool_name(&format!("postgres://db/f?application_name={bad}")).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_pools_own_settings_come_from_its_row() {
        let rows = vec![
            Row { scope: "pool".into(), scope_name: "web".into(), key: "db_max_connections".into(), value: Some("40".into()), version: 1 },
            Row { scope: "installation".into(), scope_name: String::new(), key: "db_max_connections".into(), value: Some("9".into()), version: 2 },
            Row { scope: "pool".into(), scope_name: "web".into(), key: "session_days".into(), value: Some("2".into()), version: 3 },
        ];
        let table = Stored { rows, version: 3 };
        let web = Snapshot::resolve(&table, "web", &|_| None);
        assert_eq!(web.get("db_max_connections"), (Source::Database, Some("40")));
        assert_eq!(web.get("session_days"), (Source::Default, None), "the installation's setting is read from the installation's row only");
        assert_eq!(Snapshot::resolve(&table, "default", &|_| None).get("db_max_connections"), (Source::Default, None), "another pool's row");
        let env = |name: &str| (name == "FERRUM_DB_MAX_CONNECTIONS").then(|| "12".to_string());
        assert_eq!(Snapshot::resolve(&table, "web", &env).get("db_max_connections"), (Source::Environment, Some("12")), "the pool's environment still wins");
    }

    #[test]
    fn types_refuse_what_is_not_theirs() {
        let d = |k: &str| find(k).unwrap();
        assert!(d("session_days").check("7").is_ok() && d("session_days").check("seven").is_err() && d("session_days").check("-1").is_err());
        assert!(d("run_background").check("false").is_ok() && d("run_background").check("no").is_err());
        assert!(d("app_uploads").check("cluster").is_ok() && d("app_uploads").check("all").is_err());
        assert!(d("zone_mode").check("").is_ok(), "empty where the default is");
        assert!(d("zones").check(r#"{"public": {"staff": "none"}}"#).is_ok() && d("zones").check("{bad").is_err());
        assert!(d("tenant_requests").check("").is_ok(), "no default: empty is unset");
        assert!(d("public_url").check("anything").is_ok());
    }

    #[test]
    fn the_environment_wins_over_the_table_which_wins_over_the_default() {
        let table = stored(&[("session_days", "7"), ("pdf_url", "http://pdf:3000"), ("secret_key", "from-the-table"), ("office_url", "http://o")]);
        let env = |name: &str| match name {
            "FERRUM_SESSION_DAYS" => Some("3".to_string()),
            "FERRUM_SECRET_KEY" => Some("from-the-environment".to_string()),
            _ => None,
        };
        let s = Snapshot::resolve(&table, "default", &env);
        assert_eq!(s.get("session_days"), (Source::Environment, Some("3")));
        assert_eq!(s.get("pdf_url"), (Source::Database, Some("http://pdf:3000")));
        assert_eq!(s.get("host_leaving_days"), (Source::Default, None));
        assert_eq!(s.get("secret_key"), (Source::Environment, Some("from-the-environment")), "a secret is never the table's");
        assert_eq!(Snapshot::resolve(&table, "default", &|_| None).get("secret_key"), (Source::Default, None));
        assert_eq!(s.get("office_url"), (Source::Default, None), "Drive reads it from the environment: the table's would not reach it");
        assert_eq!(s.var("FERRUM_PDF_URL").as_deref(), Some("http://pdf:3000"));
        assert_eq!(s.var("FERRUM_HOST_LEAVING_DAYS"), None, "the reader's own default");
        let empty = |name: &str| (name == "FERRUM_PDF_URL").then(String::new);
        assert_eq!(Snapshot::resolve(&table, "default", &empty).get("pdf_url"), (Source::Environment, Some("")), "set to nothing is set");
        let host = stored(&[("mailer_host", "smtp.elsewhere.example")]);
        let with_password = |name: &str| (name == "MAILER_PASSWORD").then(|| "p".to_string());
        assert_eq!(
            Snapshot::resolve(&host, "default", &with_password).get("mailer_host"),
            (Source::Default, None),
            "never sends the environment's password elsewhere"
        );
        assert_eq!(Snapshot::resolve(&host, "default", &|_| None).get("mailer_host"), (Source::Database, Some("smtp.elsewhere.example")));
        let unset = Stored { rows: vec![Row { value: None, ..stored(&[("pdf_url", "x")]).rows[0].clone() }], version: 1 };
        assert_eq!(Snapshot::resolve(&unset, "default", &|_| None).get("pdf_url"), (Source::Default, None), "an unset row is the default");
    }

    fn config() -> Config {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config");
        loco_rs::environment::Environment::Test.load_from_folder(&dir).expect("config/test.yaml")
    }

    #[test]
    fn the_tables_values_land_where_the_file_puts_the_variables_and_leave_again() {
        let mut c = config();
        let before = c.clone();
        let base = Base::of(&c);
        let table = stored(&[
            ("session_days", "7"),
            ("app_uploads", "cluster"),
            ("zones", r#"{"public": {"staff": "none"}}"#),
            ("require_object_storage", "false"),
            ("tenant_requests", "4"),
            ("port", "6160"),
            ("log_level", "debug"),
            ("public_url", "https://erp.example.com"),
            ("jwt_expiration_seconds", "60"),
            ("migrate_at_start", "false"),
        ]);
        let s = Snapshot::resolve(&table, "default", &|_| None);
        base.apply(&s, &mut c).unwrap();
        let settings: crate::state::Settings = serde_json::from_value(c.settings.clone().unwrap()).unwrap();
        assert_eq!(settings.session_days, 7);
        assert_eq!(settings.apps.uploads, crate::state::Uploads::Cluster);
        assert_eq!(settings.zones, r#"{"public": {"staff": "none"}}"#);
        assert!(!settings.require_object_storage && !settings.migrate_at_start && !c.database.auto_migrate);
        assert_eq!(settings.tenant_requests, Some(4));
        assert_eq!((c.server.port, c.server.host.as_str()), (6160, "https://erp.example.com"));
        assert!(matches!(c.logger.level, loco_rs::logger::LogLevel::Debug));
        assert_eq!(c.auth.as_ref().unwrap().jwt.as_ref().unwrap().expiration, 60);
        assert_eq!(Base::of(&c), base, "the next set-up starts from the same base");
        // Nothing in the table any more: everything as the file and the environment said.
        Base::of(&c).apply(&Snapshot::resolve(&Stored::default(), "default", &|_| None), &mut c).unwrap();
        let back: crate::state::Settings = serde_json::from_value(c.settings.clone().unwrap()).unwrap();
        let first: crate::state::Settings = serde_json::from_value(before.settings.clone().unwrap()).unwrap();
        assert_eq!(format!("{back:?}"), format!("{first:?}"));
        assert_eq!(
            (c.server.port, c.server.host.clone(), c.database.auto_migrate),
            (before.server.port, before.server.host.clone(), before.database.auto_migrate)
        );
        assert_eq!(c.auth.as_ref().unwrap().jwt.as_ref().unwrap().expiration, before.auth.as_ref().unwrap().jwt.as_ref().unwrap().expiration);
    }

    #[test]
    fn what_applies_at_once_is_exactly_what_a_running_replica_takes() {
        let c = config();
        let start: Settings = serde_json::from_value(c.settings.clone().unwrap()).unwrap();
        for def in registry().iter().filter(|d| d.stored() && d.path.as_deref().is_some_and(|p| p.starts_with("settings."))) {
            let other = match def.kind {
                Kind::Bool if def.default == "true" => "false",
                Kind::Bool => "true",
                Kind::Int => "7",
                Kind::Choice => def.values.iter().find(|v| **v != def.default && !v.is_empty()).unwrap().as_str(),
                Kind::Text | Kind::Json => "x",
            };
            let mut changed = c.clone();
            Base::of(&c).apply(&Snapshot::resolve(&stored(&[(def.key.as_str(), other)]), "default", &|_| None), &mut changed).unwrap();
            let new: Settings = serde_json::from_value(changed.settings.clone().unwrap()).unwrap();
            let taken = format!("{:?}", live_view(&start, &new)) != format!("{start:?}");
            assert_eq!(taken, def.applies == Applies::Live && def.scope == Scope::Installation, "{}: applies {:?}", def.key, def.applies);
        }
    }

    #[test]
    fn a_budget_is_shared_by_the_pools_replicas() {
        assert_eq!((share(3000, 2), share(3000, 3), share(1, 3), share(0, 4), share(10, 0)), (1500, 1000, 1, 0, 10));
    }

    #[test]
    fn cors_origins_read_as_the_file_reads_them() {
        let mut c = config();
        c.server.middlewares.cors.get_or_insert_with(|| serde_json::from_value(json!({})).unwrap());
        loco_put(&mut c, "cors_origins", r#""https://a.example", "https://b.example""#).unwrap();
        assert_eq!(c.server.middlewares.cors.as_ref().unwrap().allow_origins, ["https://a.example", "https://b.example"]);
        assert_eq!(loco_get(&c, "cors_origins").as_deref(), Some(r#""https://a.example", "https://b.example""#));
    }
}
