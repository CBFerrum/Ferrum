//! What every adapter promises. An adapter says only what it can prove: an accepted message is not
//! work done, and an error must say whether the work can have gone out, because the answer decides
//! whether a retry is safe.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrum_ops_contract::telemetry::Class;
use ferrum_ops_contract::{Command, sync::SiteDevice};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Dispatched {
    /// The equipment or its manager took it. `external` is its name for the work, if it has one.
    Accepted { external: Option<String> },
    /// It declined. Nothing happened.
    Rejected(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// It did not leave this machine: safe to try again.
    NotSent(String),
    /// It may have arrived. Never retried: the outcome is unknown.
    Unknown(String),
}

/// What the equipment says about work now. Only `Completed`, `Failed` and `Aborted` settle it, and
/// their value is the evidence.
#[derive(Debug, Clone, PartialEq)]
pub enum Observation {
    Waiting,
    Running,
    Completed(Value),
    Failed(Value),
    Aborted(Value),
    /// The equipment has no record of it.
    NotFound,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reading {
    Value {
        device: String,
        metric: String,
        value: f64,
        unit: Option<String>,
        class: Class,
        source_time: Option<DateTime<Utc>>,
    },
    /// `state`: Running, Idle, Stopped, Maintenance or Unknown.
    State {
        device: String,
        state: String,
        reason: Option<String>,
    },
}

#[async_trait]
pub trait Adapter: Send + Sync {
    /// Hands the work over, once. The runtime has already made its intent durable.
    async fn dispatch(&self, command: &Command, device: &SiteDevice) -> Result<Dispatched, Failure>;

    /// What the equipment says about work it took.
    async fn observe(&self, command: &Command, external: Option<&str>) -> Result<Observation, Failure>;

    /// After a crash between the intent and an answer: does the equipment know this command? `None`
    /// means it cannot say, and the outcome stays unknown. Only an adapter whose equipment keeps
    /// its own record may answer `NotFound`, because that makes a resend safe.
    async fn find(&self, _command: &Command) -> Result<Option<Observation>, Failure> {
        Ok(None)
    }

    /// Asks it to stop. Not a statement that it stopped: `observe` says that.
    async fn cancel(&self, command: &Command, external: Option<&str>) -> Result<(), Failure>;

    /// Readings since the last call.
    async fn read(&self) -> Result<Vec<Reading>, Failure> {
        Ok(vec![])
    }

    /// One line for the console and the platform's view of this site.
    fn health(&self) -> String {
        "ok".into()
    }
}
