//! The one call between a site and the platform. A link that comes and goes gets one idempotent
//! exchange per round: everything in a request may be sent again, and nothing is forgotten on the
//! site until the response says it arrived.

use crate::command::{Command, Report, State};
use crate::telemetry::{Limits, Measurement};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

/// A business fact recorded at the site, such as a scan or a count, to be applied once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub fact_id: Uuid,
    /// `wms.task.completed`, `mes.job.started`: letters, digits and dots.
    pub kind: String,
    pub at_source: DateTime<Utc>,
    /// Who did it, as the site knew them.
    #[serde(default)]
    pub actor: Option<String>,
    pub body: Value,
}

/// How a site is doing, for the people who look after it. No business records.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Health {
    pub version: String,
    pub started_at: Option<DateTime<Utc>>,
    /// Oldest unsent report, in seconds.
    #[serde(default)]
    pub backlog_secs: i64,
    #[serde(default)]
    pub queued_commands: i64,
    #[serde(default)]
    pub unknown_outcomes: i64,
    #[serde(default)]
    pub telemetry_bytes: i64,
    #[serde(default)]
    pub telemetry_shed: i64,
    #[serde(default)]
    pub disk_free_bytes: i64,
    /// Why this site has stopped taking new work, when it has.
    #[serde(default)]
    pub stopped: Option<String>,
    #[serde(default)]
    pub adapters: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncRequest {
    pub contract: u32,
    /// The epoch this site believes it holds.
    pub epoch: i64,
    pub boot: Uuid,
    #[serde(default)]
    pub reports: Vec<Report>,
    #[serde(default)]
    pub measurements: Vec<Measurement>,
    #[serde(default)]
    pub facts: Vec<Fact>,
    #[serde(default)]
    pub health: Health,
    /// Commands its journal now holds, so they stop being sent.
    #[serde(default)]
    pub held: Vec<Uuid>,
    /// Commands nobody here can say the outcome of, so that a person's finding reaches this site.
    #[serde(default)]
    pub in_doubt: Vec<Uuid>,
    /// How many more commands it can hold.
    pub want: u32,
    /// The versions of what it was last told, so that only a change is sent.
    #[serde(default)]
    pub config_version: String,
}

/// How long a site may go on without hearing from the platform. A disconnected site cannot learn
/// that a person was revoked: this is the limit on how long that can go unnoticed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lease {
    pub epoch: i64,
    /// Past this, the site starts nothing new. Running work is followed to its end.
    pub valid_until: DateTime<Utc>,
}

/// A device of this site as the site must know it. Connection details and their secrets stay in
/// the site's own configuration; the platform decides only what each device may be asked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SiteDevice {
    pub id: Uuid,
    pub code: String,
    pub kind: String,
    pub lifecycle: String,
    #[serde(default)]
    pub operations: Vec<String>,
    #[serde(default)]
    pub limits: BTreeMap<String, Limits>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncResponse {
    pub contract: u32,
    pub server_time: DateTime<Utc>,
    pub lease: Option<Lease>,
    /// This site no longer holds its domain: it must stop dispatching at once and say so locally.
    pub fenced: bool,
    #[serde(default)]
    pub commands: Vec<Command>,
    /// Stop these if you can. Not a statement that they stopped.
    #[serde(default)]
    pub cancel: Vec<Uuid>,
    /// What became of the commands it was in doubt about, now that a person has said.
    #[serde(default)]
    pub settled: Vec<(Uuid, State)>,
    #[serde(default)]
    pub reports_done: Vec<Uuid>,
    /// Reports the platform refused, with the reason, so they are not resent for ever.
    #[serde(default)]
    pub reports_refused: Vec<(Uuid, String)>,
    #[serde(default)]
    pub facts_done: Vec<Uuid>,
    pub measurements_stored: i64,
    /// This workspace has used today's allowance, so none of these were taken: keep them and send
    /// them again. Nothing else in the exchange is affected.
    #[serde(default)]
    pub measurements_held: bool,
    /// Present when it changed: compare with what is held.
    #[serde(default)]
    pub devices: Option<Vec<SiteDevice>>,
    #[serde(default)]
    pub config_version: String,
    /// People who may work at the console while the link is down, with their PIN hashes.
    #[serde(default)]
    pub operators: Option<Vec<Operator>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Operator {
    pub user: Uuid,
    pub name: String,
    /// `salt$sha256(salt + pin)`.
    pub pin_hash: String,
    /// What they may do here: `dispatch`, `recover`.
    pub may: Vec<String>,
}
