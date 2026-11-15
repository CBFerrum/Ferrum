//! The server's settings (docs/plan/stateless.md, D1 and D2). Every setting is declared once, in `settings.yaml`.
//! A replica reads them as it starts, over a pool of two connections of its own, before anything else connects: the
//! environment's value when the environment sets one (through R1 it still wins, so that a rollback behaves), else the
//! `setting` table's, else the default. They land where they always have, in Loco's configuration and in the two
//! settings structs built from it, so that everything downstream reads them as before.

use loco_rs::config::Config;
use loco_rs::mailer::EmailSender;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::Duration;

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
    /// `def`'s row for a replica of `pool`: the installation's.
    fn row(&self, def: &Def, _pool: &str) -> Option<&Row> {
        self.rows.iter().find(|r| r.key == def.key && r.scope == "installation")
    }
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
/// copied. Returns the keys written.
pub async fn adopt(db: &ferrum_kernel::Db, pool: &str, env: &(dyn Fn(&str) -> Option<String> + Sync)) -> ferrum_kernel::Result<Vec<String>> {
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
            written.push(def.key.clone());
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
    fn cors_origins_read_as_the_file_reads_them() {
        let mut c = config();
        c.server.middlewares.cors.get_or_insert_with(|| serde_json::from_value(json!({})).unwrap());
        loco_put(&mut c, "cors_origins", r#""https://a.example", "https://b.example""#).unwrap();
        assert_eq!(c.server.middlewares.cors.as_ref().unwrap().allow_origins, ["https://a.example", "https://b.example"]);
        assert_eq!(loco_get(&c, "cors_origins").as_deref(), Some(r#""https://a.example", "https://b.example""#));
    }
}
