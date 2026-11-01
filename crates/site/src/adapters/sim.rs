//! A fleet that exists only in a file, for rehearsals and tests. Nothing physical happens. The file
//! stands for the equipment's own memory, which outlives this process as a real fleet manager does,
//! so a crash here can be tested honestly: one line per mission means one mission.

use crate::adapter::{Adapter, Dispatched, Failure, Observation, Reading};
use async_trait::async_trait;
use chrono::Utc;
use ferrum_ops_contract::telemetry::Class;
use ferrum_ops_contract::{Command, sync::SiteDevice};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SimConfig {
    /// How long a mission takes.
    #[serde(default)]
    pub mission_millis: i64,
    /// Operations it declines.
    #[serde(default)]
    pub rejects: Vec<String>,
    /// Operations that fail part way.
    #[serde(default)]
    pub fails: Vec<String>,
    /// While true it answers nothing: the link to the fleet is down.
    #[serde(default)]
    pub silent_file: Option<PathBuf>,
    /// It has no way to look up what it was sent, as many controllers do not.
    #[serde(default)]
    pub cannot_find: bool,
}

pub struct Sim {
    cfg: SimConfig,
    file: PathBuf,
    devices: Vec<String>,
    write: Mutex<()>,
}

impl Sim {
    pub fn new(cfg: SimConfig, file: PathBuf, devices: Vec<String>) -> Sim {
        Sim { cfg, file, devices, write: Mutex::new(()) }
    }

    fn lines(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.file).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }

    fn append(&self, line: Value) -> Result<(), Failure> {
        let _guard = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let write = || -> std::io::Result<()> {
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.file)?;
            writeln!(f, "{line}")?;
            f.sync_all()
        };
        write().map_err(|e| Failure::NotSent(e.to_string()))
    }

    fn silent(&self) -> bool {
        self.cfg.silent_file.as_ref().is_some_and(|p| p.exists())
    }

    fn state_of(&self, id: &str, operation: &str) -> Observation {
        let lines = self.lines();
        let Some(start) = lines.iter().find(|l| l["mission"] == json!(id)) else { return Observation::NotFound };
        if lines.iter().any(|l| l["cancel"] == json!(id)) {
            return Observation::Aborted(json!({"simulated": true, "stopped": true}));
        }
        let age = Utc::now().timestamp_millis() - start["at"].as_i64().unwrap_or(0);
        if age < self.cfg.mission_millis {
            return Observation::Running;
        }
        if self.cfg.fails.iter().any(|o| o == operation) {
            return Observation::Failed(json!({"simulated": true, "fault": "stopped part way"}));
        }
        Observation::Completed(json!({"simulated": true, "mission": id}))
    }
}

#[async_trait]
impl Adapter for Sim {
    async fn dispatch(&self, c: &Command, _device: &SiteDevice) -> Result<Dispatched, Failure> {
        if self.silent() {
            return Err(Failure::Unknown("no answer from the fleet".into()));
        }
        if self.cfg.rejects.contains(&c.operation) {
            return Ok(Dispatched::Rejected(format!("the fleet declined {}", c.operation)));
        }
        let id = c.id.to_string();
        self.append(json!({"mission": id, "at": Utc::now().timestamp_millis(), "operation": c.operation}))?;
        Ok(Dispatched::Accepted { external: Some(id) })
    }

    async fn observe(&self, c: &Command, _external: Option<&str>) -> Result<Observation, Failure> {
        if self.silent() {
            return Err(Failure::Unknown("no answer from the fleet".into()));
        }
        Ok(self.state_of(&c.id.to_string(), &c.operation))
    }

    async fn find(&self, c: &Command) -> Result<Option<Observation>, Failure> {
        if self.cfg.cannot_find {
            return Ok(None);
        }
        if self.silent() {
            return Err(Failure::Unknown("no answer from the fleet".into()));
        }
        Ok(Some(self.state_of(&c.id.to_string(), &c.operation)))
    }

    async fn cancel(&self, c: &Command, _external: Option<&str>) -> Result<(), Failure> {
        self.append(json!({"cancel": c.id.to_string()}))
    }

    async fn read(&self) -> Result<Vec<Reading>, Failure> {
        let t = Utc::now().timestamp() as f64;
        Ok(self
            .devices
            .iter()
            .map(|d| Reading::Value {
                device: d.clone(),
                metric: "battery.pct".into(),
                value: 70.0 + (t / 60.0).sin() * 20.0,
                unit: Some("%".into()),
                class: Class::Sample,
                source_time: None,
            })
            .collect())
    }

    fn health(&self) -> String {
        if self.silent() { "no answer".into() } else { "simulated".into() }
    }
}
