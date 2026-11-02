//! An operational command: an intent to cause a physical effect, and what is known about it.
//!
//! Accepted is not completed. Cancellation requested is not cancelled. A timeout is not proof that
//! nothing happened. A message acknowledged is not a load moved. The states below say only what was
//! established, and [`step`] is the one place that decides how a report moves a command.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum State {
    /// Written, waiting for a person's approval.
    Proposed,
    /// Approval refused. It never left.
    Refused,
    /// Allowed, not yet handed to a queue.
    Authorized,
    /// Waiting for its site, or on it.
    Queued,
    /// Its deadline passed before anything started. It never reached equipment.
    Expired,
    /// The equipment or its manager took it. Nothing has moved yet, as far as anyone knows.
    Accepted,
    /// The equipment or its manager declined it. Nothing happened.
    Rejected,
    /// The equipment says it has started.
    Executing,
    Completed,
    /// The equipment says it stopped without finishing. What it did before that is in the evidence.
    Failed,
    /// Stopped on request.
    Aborted,
    /// Nobody can say what happened. Not retried: a person or the equipment has to settle it.
    OutcomeUnknown,
}

impl State {
    pub const ALL: [State; 12] = [
        State::Proposed,
        State::Refused,
        State::Authorized,
        State::Queued,
        State::Expired,
        State::Accepted,
        State::Rejected,
        State::Executing,
        State::Completed,
        State::Failed,
        State::Aborted,
        State::OutcomeUnknown,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            State::Proposed => "Proposed",
            State::Refused => "Refused",
            State::Authorized => "Authorized",
            State::Queued => "Queued",
            State::Expired => "Expired",
            State::Accepted => "Accepted",
            State::Rejected => "Rejected",
            State::Executing => "Executing",
            State::Completed => "Completed",
            State::Failed => "Failed",
            State::Aborted => "Aborted",
            State::OutcomeUnknown => "OutcomeUnknown",
        }
    }

    pub fn parse(s: &str) -> Option<State> {
        State::ALL.into_iter().find(|v| v.as_str() == s)
    }

    /// Finished for good. `OutcomeUnknown` is not: it is waiting for evidence.
    pub fn is_final(self) -> bool {
        matches!(self, State::Refused | State::Expired | State::Rejected | State::Completed | State::Failed | State::Aborted)
    }

    /// Something may have happened or may yet happen on the floor: the resources it holds cannot
    /// be given to other work.
    pub fn holds_resources(self) -> bool {
        matches!(self, State::Accepted | State::Executing | State::OutcomeUnknown)
    }

    fn rank(self) -> u8 {
        match self {
            State::Proposed => 0,
            State::Authorized => 1,
            State::Queued => 2,
            State::Accepted => 3,
            State::Executing => 4,
            State::OutcomeUnknown => 5,
            _ => 6,
        }
    }
}

/// What a report does to a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The command takes the reported state.
    Move,
    /// Late or repeated: kept in the history, the state stands.
    Keep,
    /// A second, different ending. The first stands, and a person has to look.
    Conflict,
    /// This report cannot apply to this command.
    Refuse,
}

/// How a report of `to` applies to a command in `from`. `evidence` says the report carries what
/// settles an unknown outcome: the equipment was asked, or a person looked.
///
/// Reports may arrive out of order, so a command may skip states (a completion before the
/// acceptance), but it never goes backwards.
pub fn step(from: State, to: State, evidence: bool) -> Step {
    use State::*;
    match (from, to) {
        // What only the platform decides.
        (Proposed, Authorized | Refused | Expired) | (Authorized, Queued | Expired) | (Queued, Expired) => Step::Move,
        (_, Proposed | Authorized | Queued | Refused | Expired) => Step::Refuse,
        // Never handed to anyone, so nothing can be reported.
        (Proposed | Authorized | Refused | Expired, _) => Step::Refuse,
        (a, b) if a == b => Step::Keep,
        // Only evidence ends an unknown; a late "started" says nothing about how it ended.
        (OutcomeUnknown, Completed | Failed | Aborted) => {
            if evidence {
                Step::Move
            } else {
                Step::Keep
            }
        }
        (OutcomeUnknown, _) => Step::Keep,
        (a, _) if a.is_final() => {
            if matches!(to, Completed | Failed | Aborted | Rejected) {
                Step::Conflict
            } else {
                Step::Keep
            }
        }
        (a, b) if b.rank() > a.rank() => Step::Move,
        _ => Step::Keep,
    }
}

/// A command as the site receives it. The hash travels with the payload and is checked before it
/// reaches an adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Command {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub site_id: Uuid,
    /// The authority domain whose holder may dispatch it.
    pub domain: String,
    /// The device, robot, fleet or operator station it goes to.
    pub target_id: Uuid,
    pub operation: String,
    pub schema_version: i32,
    pub issued_by: Option<Uuid>,
    pub approval_ref: Option<String>,
    /// The epoch it was issued under. A holder on another epoch must not dispatch it.
    pub epoch: i64,
    /// What must be true of the target before it starts, for the adapter to check.
    #[serde(default)]
    pub expected_state: Value,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub payload: Value,
    pub payload_hash: String,
    pub workflow_id: Option<String>,
    pub step_id: Option<String>,
    pub correlation_id: Option<String>,
    pub config_version: Option<String>,
}

/// The newest form of a command this build knows the meaning of. A new field that can be ignored does not
/// change this, and an older build ignores it. A new meaning does, and an older build then refuses.
pub const COMMAND_SCHEMA: i32 = 1;

impl Command {
    /// The payload is what was authorized.
    pub fn intact(&self) -> bool {
        payload_hash(&self.payload) == self.payload_hash
    }

    /// Fit to start, at `now`, by a holder on `epoch`. The reason is for the operator.
    pub fn startable(&self, now: DateTime<Utc>, epoch: i64) -> Result<(), &'static str> {
        if self.schema_version > COMMAND_SCHEMA {
            return Err("it was written for a newer version than this one, which would be guessing at what it means");
        }
        if !self.intact() {
            return Err("its payload does not match what was authorized");
        }
        if self.epoch != epoch {
            return Err("it was issued under another authority");
        }
        if now >= self.expires_at {
            return Err("its deadline has passed");
        }
        Ok(())
    }
}

/// What a site establishes about a command. Idempotent by `report_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub report_id: Uuid,
    pub command_id: Uuid,
    pub state: State,
    /// When the site observed it, on the site's clock.
    pub at_source: DateTime<Utc>,
    /// The epoch the reporter holds. A report from an older one is refused.
    pub epoch: i64,
    #[serde(default)]
    pub detail: Value,
    /// What settles an unknown outcome: the equipment's own answer, or a person's finding.
    #[serde(default)]
    pub evidence: Option<Value>,
}

/// JSON with object keys in order, so one value has one hash on any build of either side.
pub fn canonical(v: &Value) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                write_canonical(&m[k], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(x, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

pub fn payload_hash(v: &Value) -> String {
    hex::encode(Sha256::digest(canonical(v).as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;
    use std::collections::HashSet;

    fn any_state() -> impl Strategy<Value = State> {
        (0..State::ALL.len()).prop_map(|i| State::ALL[i])
    }

    fn apply(from: State, reports: &[(State, bool)]) -> (State, Vec<State>) {
        let mut s = from;
        let mut seen = vec![s];
        for &(to, evidence) in reports {
            if step(s, to, evidence) == Step::Move {
                s = to;
                seen.push(s);
            }
        }
        (s, seen)
    }

    proptest! {
        // Whatever arrives, in whatever order, a command never goes backwards and an ending stands.
        #[test]
        fn reports_never_undo_anything(start in any_state(), reports in proptest::collection::vec((any_state(), any::<bool>()), 0..24)) {
            let (_, seen) = apply(start, &reports);
            for pair in seen.windows(2) {
                prop_assert!(pair[1].rank() >= pair[0].rank(), "{:?} went back to {:?}", pair[0], pair[1]);
                prop_assert!(!pair[0].is_final(), "{:?} was final and became {:?}", pair[0], pair[1]);
            }
        }

        // Delivering the same reports twice changes nothing more than delivering them once.
        #[test]
        fn a_replay_changes_nothing(reports in proptest::collection::vec((any_state(), any::<bool>()), 0..24)) {
            let (once, _) = apply(State::Queued, &reports);
            let twice: Vec<_> = reports.iter().chain(reports.iter()).copied().collect();
            prop_assert_eq!(once, apply(State::Queued, &twice).0);
        }

        // The ending does not depend on the order reports of one execution arrive in.
        #[test]
        fn order_does_not_change_a_clean_ending(mut order in Just(vec![State::Accepted, State::Executing, State::Completed]).prop_shuffle()) {
            order.push(State::Executing);
            let reports: Vec<_> = order.iter().map(|s| (*s, false)).collect();
            prop_assert_eq!(apply(State::Queued, &reports).0, State::Completed);
        }

        #[test]
        fn hashes_ignore_key_order(a in any::<i64>(), b in ".*") {
            let one: Value = serde_json::from_str(&format!(r#"{{"a":{a},"b":{}}}"#, Value::String(b.clone()))).unwrap();
            let two: Value = serde_json::from_str(&format!(r#"{{"b":{},"a":{a}}}"#, Value::String(b))).unwrap();
            prop_assert_eq!(payload_hash(&one), payload_hash(&two));
        }
    }

    #[test]
    fn nothing_but_evidence_ends_an_unknown() {
        for end in [State::Completed, State::Failed, State::Aborted] {
            assert_eq!(step(State::OutcomeUnknown, end, false), Step::Keep);
            assert_eq!(step(State::OutcomeUnknown, end, true), Step::Move);
        }
        assert_eq!(step(State::OutcomeUnknown, State::Executing, true), Step::Keep);
        assert_eq!(step(State::OutcomeUnknown, State::Rejected, true), Step::Keep, "something may have happened");
    }

    #[test]
    fn a_different_ending_is_a_conflict_not_a_correction() {
        assert_eq!(step(State::Completed, State::Failed, false), Step::Conflict);
        assert_eq!(step(State::Failed, State::Completed, true), Step::Conflict);
        assert_eq!(step(State::Completed, State::Completed, false), Step::Keep);
        assert_eq!(step(State::Completed, State::Executing, false), Step::Keep);
    }

    #[test]
    fn a_site_cannot_authorize_and_nothing_unqueued_can_be_reported() {
        for from in State::ALL {
            for to in [State::Proposed, State::Authorized, State::Refused] {
                if !(from == State::Proposed && matches!(to, State::Authorized | State::Refused)) {
                    assert_eq!(step(from, to, false), Step::Refuse, "{from:?} to {to:?}");
                }
            }
        }
        for from in [State::Proposed, State::Authorized, State::Refused, State::Expired] {
            for to in [State::Accepted, State::Executing, State::Completed, State::Failed] {
                assert_eq!(step(from, to, true), Step::Refuse, "{from:?} to {to:?}");
            }
        }
    }

    #[test]
    fn every_state_has_one_name() {
        let names: HashSet<&str> = State::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(names.len(), State::ALL.len());
        for s in State::ALL {
            assert_eq!(State::parse(s.as_str()), Some(s));
            assert_eq!(serde_json::to_value(s).unwrap(), json!(s.as_str()), "the wire name is the name");
        }
    }

    fn command(now: DateTime<Utc>) -> Command {
        let payload = json!({"tote": "T-481", "to": "PACK-1"});
        Command {
            id: Uuid::now_v7(),
            tenant_id: Uuid::now_v7(),
            site_id: Uuid::now_v7(),
            domain: "north".into(),
            target_id: Uuid::now_v7(),
            operation: "warehouse.move".into(),
            schema_version: 1,
            issued_by: None,
            approval_ref: None,
            epoch: 4,
            expected_state: Value::Null,
            issued_at: now,
            expires_at: now + chrono::Duration::minutes(5),
            payload_hash: payload_hash(&payload),
            payload,
            workflow_id: None,
            step_id: None,
            correlation_id: None,
            config_version: None,
        }
    }

    #[test]
    fn a_command_starts_only_as_authorized_in_time_and_under_its_authority() {
        let now = Utc::now();
        let c = command(now);
        assert_eq!(c.startable(now, 4), Ok(()));
        assert!(c.startable(now, 5).is_err(), "an old command under a new holder");
        assert!(c.startable(now + chrono::Duration::minutes(5), 4).is_err(), "the deadline itself is too late");
        let mut tampered = c.clone();
        tampered.payload["to"] = json!("DOCK-9");
        assert!(tampered.startable(now, 4).is_err());
    }

    /// AT-MSG-008.
    #[test]
    fn an_old_build_ignores_what_it_may_and_refuses_what_it_must() {
        let c = command(Utc::now());
        let mut sent = serde_json::to_value(&c).unwrap();
        sent["priority"] = json!("high");
        let read: Command = serde_json::from_value(sent).expect("a field it does not know does no harm");
        assert_eq!(read.startable(read.issued_at, read.epoch), Ok(()));
        let newer = Command { schema_version: COMMAND_SCHEMA + 1, ..c };
        assert!(newer.startable(newer.issued_at, newer.epoch).unwrap_err().contains("newer version"));
    }
}
