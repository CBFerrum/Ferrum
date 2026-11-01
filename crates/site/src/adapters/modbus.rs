//! Modbus TCP, for what speaks nothing else. Registers mean what their vendor says, so every one is
//! named in the site's configuration: what to read, and for each operation exactly which registers
//! to write and which to watch. Nothing else can be written. Modbus has no authentication worth the
//! name: this belongs on a segmented network, behind the site.
//!
//! An operation is a handshake with a program already in the controller: it writes an approved job
//! reference and watches a status register. It never generates controller logic.

use crate::adapter::{Adapter, Dispatched, Failure, Observation, Reading};
use async_trait::async_trait;
use ferrum_ops_contract::telemetry::Class;
use ferrum_ops_contract::{Command, sync::SiteDevice};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::sync::Mutex;
use tokio_modbus::client::{Context, Reader, Writer, tcp};
use tokio_modbus::slave::{Slave, SlaveContext};

fn one() -> f64 {
    1.0
}

fn unit() -> u8 {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModbusConfig {
    pub address: SocketAddr,
    #[serde(default = "unit")]
    pub unit: u8,
    #[serde(default)]
    pub registers: Vec<Register>,
    /// The operations this device may be asked, each a handshake.
    #[serde(default)]
    pub operations: HashMap<String, Handshake>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Register {
    pub device: String,
    pub metric: String,
    pub address: u16,
    #[serde(default = "one")]
    pub scale: f64,
    #[serde(default)]
    pub unit: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Handshake {
    /// Where the job number goes, read from the command's `job` field.
    pub job_register: u16,
    pub start_register: u16,
    pub status_register: u16,
    /// The controller's values for each state of the status register.
    pub running: u16,
    pub done: u16,
    pub fault: u16,
    /// A value meaning it finished nothing and is ready, to detect work that never began.
    #[serde(default)]
    pub idle: u16,
}

pub struct Modbus {
    cfg: ModbusConfig,
    conn: Mutex<Option<Context>>,
}

impl Modbus {
    pub fn new(cfg: ModbusConfig) -> Modbus {
        Modbus { cfg, conn: Mutex::new(None) }
    }

    async fn connected<'a>(&self, conn: &'a mut Option<Context>) -> Result<&'a mut Context, String> {
        if conn.is_none() {
            let mut c = tcp::connect(self.cfg.address).await.map_err(|e| e.to_string())?;
            c.set_slave(Slave(self.cfg.unit));
            *conn = Some(c);
        }
        conn.as_mut().ok_or_else(|| "not connected".to_string())
    }

    async fn read_one(&self, address: u16) -> Result<u16, String> {
        let mut guard = self.conn.lock().await;
        let c = self.connected(&mut guard).await?;
        let read = c.read_holding_registers(address, 1).await;
        match read {
            Ok(Ok(words)) => words.first().copied().ok_or_else(|| "an empty answer".to_string()),
            Ok(Err(code)) => Err(format!("the device refused: {code:?}")),
            Err(e) => {
                // Drop the connection: the next call makes a fresh one.
                *guard = None;
                Err(e.to_string())
            }
        }
    }
}

/// How a status register reads against a handshake.
pub fn judge(h: &Handshake, status: u16) -> Observation {
    match status {
        s if s == h.done => Observation::Completed(json!({"status_register": s})),
        s if s == h.fault => Observation::Failed(json!({"status_register": s})),
        s if s == h.running => Observation::Running,
        _ => Observation::Waiting,
    }
}

#[async_trait]
impl Adapter for Modbus {
    async fn dispatch(&self, c: &Command, _device: &SiteDevice) -> Result<Dispatched, Failure> {
        let Some(h) = self.cfg.operations.get(&c.operation) else {
            return Ok(Dispatched::Rejected(format!("{} is not mapped to registers at this site", c.operation)));
        };
        let Some(job) = c.payload["job"].as_u64().filter(|j| *j <= u64::from(u16::MAX)) else {
            return Ok(Dispatched::Rejected("a job number between 0 and 65535 is required".into()));
        };
        let mut guard = self.conn.lock().await;
        let conn = self.connected(&mut guard).await.map_err(Failure::NotSent)?;
        // The job first: until the start register is written nothing has happened.
        match conn.write_single_register(h.job_register, job as u16).await {
            Ok(Ok(())) => {}
            Ok(Err(code)) => return Ok(Dispatched::Rejected(format!("the device refused the job: {code:?}"))),
            Err(e) => {
                *guard = None;
                return Err(Failure::NotSent(e.to_string()));
            }
        }
        // From here an unanswered write may have reached it.
        let Some(conn) = guard.as_mut() else { return Err(Failure::NotSent("not connected".into())) };
        let started = conn.write_single_register(h.start_register, 1).await;
        match started {
            Ok(Ok(())) => Ok(Dispatched::Accepted { external: None }),
            Ok(Err(code)) => Ok(Dispatched::Rejected(format!("the device refused to start: {code:?}"))),
            Err(e) => {
                *guard = None;
                Err(Failure::Unknown(e.to_string()))
            }
        }
    }

    async fn observe(&self, c: &Command, _external: Option<&str>) -> Result<Observation, Failure> {
        let Some(h) = self.cfg.operations.get(&c.operation) else { return Ok(Observation::NotFound) };
        let status = self.read_one(h.status_register).await.map_err(Failure::Unknown)?;
        Ok(judge(h, status))
    }

    async fn cancel(&self, _c: &Command, _external: Option<&str>) -> Result<(), Failure> {
        // Stopping a machine belongs to its controller and its operators: asking over Modbus would
        // look like a stop without being one.
        Err(Failure::NotSent("stop this at the machine".into()))
    }

    async fn read(&self) -> Result<Vec<Reading>, Failure> {
        let mut out = Vec::with_capacity(self.cfg.registers.len());
        for r in &self.cfg.registers {
            let word = self.read_one(r.address).await.map_err(Failure::Unknown)?;
            out.push(Reading::Value {
                device: r.device.clone(),
                metric: r.metric.clone(),
                value: f64::from(word) * r.scale,
                unit: r.unit.clone(),
                class: Class::Sample,
                source_time: None,
            });
        }
        Ok(out)
    }

    fn health(&self) -> String {
        if self.conn.try_lock().is_ok_and(|c| c.is_some()) { "connected".into() } else { "not connected".into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h() -> Handshake {
        Handshake { job_register: 100, start_register: 101, status_register: 102, running: 1, done: 2, fault: 3, idle: 0 }
    }

    #[test]
    fn the_status_register_is_read_by_what_the_site_says_it_means() {
        assert_eq!(judge(&h(), 1), Observation::Running);
        assert!(matches!(judge(&h(), 2), Observation::Completed(_)));
        assert!(matches!(judge(&h(), 3), Observation::Failed(_)));
        assert_eq!(judge(&h(), 0), Observation::Waiting, "not started yet");
        assert_eq!(judge(&h(), 77), Observation::Waiting, "an unmapped value proves nothing");
    }
}
