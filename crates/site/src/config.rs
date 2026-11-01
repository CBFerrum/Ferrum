//! A site's own configuration: what only the site knows, such as addresses on its own network and
//! their secrets. What each device may be asked comes from the platform.

use serde::Deserialize;
use std::path::{Path, PathBuf};

fn five() -> u64 {
    5
}

fn quarter_second() -> u64 {
    250
}

fn ten() -> u64 {
    10
}

fn five_minutes() -> i64 {
    300
}

fn one_gigabyte() -> i64 {
    1024
}

fn quarter_gigabyte() -> i64 {
    256
}

fn hold() -> i64 {
    2000
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// The platform, `https://…`.
    pub server: String,
    /// The workspace's slug.
    pub tenant: String,
    /// Where the journal and key live. Back this up.
    pub state_dir: PathBuf,
    #[serde(default = "five")]
    pub sync_secs: u64,
    #[serde(default = "quarter_second")]
    pub work_millis: u64,
    #[serde(default = "five")]
    pub read_secs: u64,
    /// How long an adapter may take to answer one call.
    #[serde(default = "ten")]
    pub adapter_timeout_secs: u64,
    /// With no answer from the equipment for this long, a running command's outcome is unknown.
    #[serde(default = "five_minutes")]
    pub unknown_after_secs: i64,
    /// Unsent measurements kept, at most. Commands and evidence are never counted in this.
    #[serde(default = "one_gigabyte")]
    pub telemetry_budget_mb: i64,
    /// With less free disk than this the site starts no new work, so that what is running can
    /// still be recorded.
    #[serde(default = "quarter_gigabyte")]
    pub reserve_mb: i64,
    /// Commands the journal may hold waiting.
    #[serde(default = "hold")]
    pub max_queued: i64,
    /// The operator console. Listen on the site's own network only; it has no TLS of its own.
    #[serde(default)]
    pub console: Option<String>,
    #[serde(default)]
    pub adapters: Vec<AdapterConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdapterConfig {
    pub name: String,
    /// Device codes this adapter reaches.
    pub devices: Vec<String>,
    /// Commands one device may run at once.
    #[serde(default = "one")]
    pub concurrency: u32,
    #[serde(flatten)]
    pub kind: AdapterKind,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AdapterKind {
    /// A fleet that exists only in a file, for rehearsals and tests. Nothing physical happens.
    Sim(crate::adapters::sim::SimConfig),
    /// Work a person does at the console.
    Operator,
    #[cfg(feature = "mqtt")]
    Mqtt(crate::adapters::mqtt::MqttConfig),
    #[cfg(feature = "mqtt")]
    Vda5050(crate::adapters::vda5050::VdaConfig),
    #[cfg(feature = "modbus")]
    Modbus(crate::adapters::modbus::ModbusConfig),
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let cfg: Config = serde_yaml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        cfg.check()?;
        Ok(cfg)
    }

    fn check(&self) -> Result<(), String> {
        let local = self.server.starts_with("http://127.") || self.server.starts_with("http://localhost");
        if !self.server.starts_with("https://") && !local {
            return Err("server must be https, or plain http only to this machine".into());
        }
        if self.sync_secs == 0 || self.work_millis == 0 || self.read_secs == 0 || self.adapter_timeout_secs == 0 {
            return Err("intervals must be above zero".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for a in &self.adapters {
            if !seen.insert(&a.name) {
                return Err(format!("two adapters are called {}", a.name));
            }
        }
        let mut owned = std::collections::BTreeSet::new();
        for d in self.adapters.iter().flat_map(|a| &a.devices) {
            if !owned.insert(d) {
                return Err(format!("{d} is listed under two adapters: one thing dispatches to a device"));
            }
        }
        Ok(())
    }

    pub fn journal_path(&self) -> PathBuf {
        self.state_dir.join("journal.db")
    }

    pub fn key_path(&self) -> PathBuf {
        self.state_dir.join("site.key")
    }
}
