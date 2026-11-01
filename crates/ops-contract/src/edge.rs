//! What a gateway is told to be: its definitions, its devices, its rules, where else its data goes,
//! and which release it should run. Fetched from the platform when it changes and kept in the
//! journal, so that a gateway restarted with the link down is still what it was.
//!
//! A rule watches a number here, at the edge, so an alarm is raised with the link down. It says only
//! that something is wrong. Rules raise alarms and do not move equipment.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceLink {
    pub code: String,
    /// The definition's name.
    pub definition: String,
    /// Overrides the definition's address.
    #[serde(default)]
    pub connect: Option<String>,
    #[serde(default)]
    pub settings: Value,
    /// Commands it may run at once.
    #[serde(default = "one")]
    pub concurrency: u32,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Comparison {
    Above,
    Below,
    /// Nothing has been heard for `hold_secs`.
    Silent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: Uuid,
    pub name: String,
    /// Device codes. Empty means every device with this metric.
    #[serde(default)]
    pub devices: Vec<String>,
    pub metric: String,
    pub when: Comparison,
    #[serde(default)]
    pub value: f64,
    /// How long it must hold before anyone is told, so that one spike is not an alarm.
    #[serde(default)]
    pub hold_secs: i64,
    /// Where it stops being true, when that is not `value`. Without a gap, a reading sitting on the
    /// limit would raise and clear for ever.
    #[serde(default)]
    pub clear: Option<f64>,
    #[serde(default = "warning")]
    pub severity: String,
}

fn warning() -> String {
    "Warning".into()
}

/// Where else a site's data goes: a local historian, another broker, someone else's cloud.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Export {
    pub name: String,
    /// `http(s)://…` takes batches by POST. `mqtt(s)://…` takes a message per measurement.
    pub to: String,
    /// `{device}` and `{metric}` work here. MQTT only.
    #[serde(default)]
    pub topic: Option<String>,
    /// Only these metrics, with `*` for the rest of a name. Empty means all.
    #[serde(default)]
    pub only: Vec<String>,
    /// Signs each HTTP batch, by the name of a secret held at the site.
    #[serde(default)]
    pub sign_with: Option<String>,
}

/// A build of the site runtime, and how to know it is the real one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Release {
    pub version: String,
    pub url: String,
    /// Of the file, in hexadecimal.
    pub sha256: String,
    /// Ed25519 over [`Release::message`], in hexadecimal, by a key the site was installed with.
    pub signature: String,
    /// Refused after this, so an old release cannot be replayed for ever.
    pub expires_at: DateTime<Utc>,
}

impl Release {
    /// What is signed: everything that decides what runs.
    pub fn message(&self) -> Vec<u8> {
        format!("ferrum-site-release\n{}\n{}\n{}\n{}", self.version, self.url, self.sha256, self.expires_at.to_rfc3339()).into_bytes()
    }
}

/// Dotted numbers compared as numbers. Anything else is older than everything: a build cannot be
/// made newer by naming it oddly.
pub fn newer(candidate: &str, current: &str) -> bool {
    let parts = |v: &str| v.split('.').map(|p| p.parse::<u64>()).collect::<Result<Vec<_>, _>>();
    match (parts(candidate), parts(current)) {
        (Ok(a), Ok(b)) => a > b,
        _ => false,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EdgeConfig {
    pub version: String,
    #[serde(default)]
    pub definitions: BTreeMap<String, String>,
    #[serde(default)]
    pub devices: Vec<DeviceLink>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub exports: Vec<Export>,
    #[serde(default)]
    pub release: Option<Release>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigRequest {
    /// The version held, so an unchanged configuration costs one short answer.
    #[serde(default)]
    pub have: String,
    /// What this gateway heard and does not know, for a person to claim.
    #[serde(default)]
    pub strangers: Vec<String>,
    /// Why what it was given could not all start.
    #[serde(default)]
    pub problems: Vec<String>,
    /// Each link's state, as it is.
    #[serde(default)]
    pub links: Value,
    /// The release it is running.
    #[serde(default)]
    pub runtime: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigResponse {
    /// Absent when `have` is current.
    #[serde(default)]
    pub config: Option<EdgeConfig>,
}

/// A change in an alarm, in a fact of kind `edge.alarm`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlarmChange {
    pub rule: Uuid,
    pub device: Uuid,
    pub raised: bool,
    #[serde(default)]
    pub value: Option<f64>,
}

/// Where one rule stands on one device.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Watch {
    since: Option<DateTime<Utc>>,
    raised: bool,
    last_heard: Option<DateTime<Utc>>,
}

impl Watch {
    fn breached(&self, rule: &Rule, value: f64) -> bool {
        // Once raised, it clears at `clear`: the gap stops a reading on the limit from chattering.
        let limit = if self.raised { rule.clear.unwrap_or(rule.value) } else { rule.value };
        match rule.when {
            Comparison::Above => value > limit,
            Comparison::Below => value < limit,
            Comparison::Silent => false,
        }
    }

    /// Takes a reading. `Some` when the alarm should change.
    pub fn feed(&mut self, rule: &Rule, value: f64, at: DateTime<Utc>) -> Option<bool> {
        self.last_heard = Some(at);
        if rule.when == Comparison::Silent {
            self.since = None;
            return self.raised.then(|| {
                self.raised = false;
                false
            });
        }
        if !self.breached(rule, value) {
            self.since = None;
            return self.raised.then(|| {
                self.raised = false;
                false
            });
        }
        let since = *self.since.get_or_insert(at);
        if !self.raised && at - since >= Duration::seconds(rule.hold_secs) {
            self.raised = true;
            return Some(true);
        }
        None
    }

    /// Time passing, for what silence means. Only after something has been heard: a device nobody
    /// has seen yet is not a device that went quiet.
    pub fn tick(&mut self, rule: &Rule, now: DateTime<Utc>) -> Option<bool> {
        if rule.when != Comparison::Silent || self.raised {
            return None;
        }
        let heard = self.last_heard?;
        (now - heard >= Duration::seconds(rule.hold_secs)).then(|| {
            self.raised = true;
            true
        })
    }

    pub fn raised(&self) -> bool {
        self.raised
    }

    /// One that was raised before a restart: forgetting would leave it raised for ever on the
    /// platform, with nothing here left to clear it.
    pub fn still_raised() -> Watch {
        Watch { raised: true, ..Default::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(when: Comparison, value: f64, hold: i64, clear: Option<f64>) -> Rule {
        Rule { id: Uuid::nil(), name: "r".into(), devices: vec![], metric: "m".into(), when, value, hold_secs: hold, clear, severity: "Warning".into() }
    }

    fn at(s: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_000_000 + s, 0).unwrap()
    }

    #[test]
    fn one_spike_is_not_an_alarm() {
        let r = rule(Comparison::Above, 80.0, 30, None);
        let mut w = Watch::default();
        assert_eq!(w.feed(&r, 90.0, at(0)), None);
        assert_eq!(w.feed(&r, 70.0, at(10)), None, "it came back in time");
        assert_eq!(w.feed(&r, 90.0, at(20)), None);
        assert_eq!(w.feed(&r, 90.0, at(49)), None);
        assert_eq!(w.feed(&r, 90.0, at(50)), Some(true), "it held for thirty seconds");
        assert_eq!(w.feed(&r, 95.0, at(60)), None, "raised once");
        assert_eq!(w.feed(&r, 70.0, at(70)), Some(false));
        assert_eq!(w.feed(&r, 70.0, at(80)), None, "cleared once");
    }

    #[test]
    fn a_reading_on_the_limit_does_not_chatter() {
        let r = rule(Comparison::Above, 80.0, 0, Some(75.0));
        let mut w = Watch::default();
        assert_eq!(w.feed(&r, 81.0, at(0)), Some(true));
        assert_eq!(w.feed(&r, 79.0, at(1)), None, "still above where it clears");
        assert_eq!(w.feed(&r, 81.0, at(2)), None);
        assert_eq!(w.feed(&r, 74.0, at(3)), Some(false));
        assert_eq!(w.feed(&r, 79.0, at(4)), None, "and below where it raises");
    }

    #[test]
    fn below_is_the_same_the_other_way() {
        let r = rule(Comparison::Below, 20.0, 0, Some(25.0));
        let mut w = Watch::default();
        assert_eq!(w.feed(&r, 15.0, at(0)), Some(true));
        assert_eq!(w.feed(&r, 22.0, at(1)), None);
        assert_eq!(w.feed(&r, 26.0, at(2)), Some(false));
    }

    #[test]
    fn silence_counts_only_after_something_was_heard() {
        let r = rule(Comparison::Silent, 0.0, 60, None);
        let mut w = Watch::default();
        assert_eq!(w.tick(&r, at(1000)), None, "never heard is not gone quiet");
        assert_eq!(w.feed(&r, 1.0, at(0)), None);
        assert_eq!(w.tick(&r, at(59)), None);
        assert_eq!(w.tick(&r, at(60)), Some(true));
        assert_eq!(w.tick(&r, at(120)), None, "raised once");
        assert_eq!(w.feed(&r, 1.0, at(121)), Some(false), "it is back");
    }

    #[test]
    fn a_release_is_newer_by_its_numbers() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("0.10.0", "0.9.0"), "as numbers, not as text");
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.0.9", "0.1.0"));
        assert!(!newer("9.9.9-evil", "0.1.0"), "a name that is not numbers is not newer");
    }

    #[test]
    fn what_is_signed_covers_what_runs() {
        let r = Release { version: "0.2.0".into(), url: "https://x".into(), sha256: "ab".into(), signature: String::new(), expires_at: at(0) };
        let mut other = r.clone();
        other.sha256 = "cd".into();
        assert_ne!(r.message(), other.message());
    }
}
