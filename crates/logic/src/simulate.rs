//! The simulator (FLC-UI-007, FLC-UI-010): a process run in the state machine the runtime drives, on a virtual
//! clock, with stand-ins for everything outside it. It reaches no record, provider or person. Every capability
//! answers from the scenario; one the scenario does not answer is reported as not modelled, and the step sees
//! an unknown outcome, never an invented success. A task without a decision, a wait without its event and a
//! delay run to their deadlines on the virtual clock, worked out on the scenario's calendars as the runtime
//! works them out on the workspace's.

use crate::calendar::{self, Calendar, Wait};
use crate::compile::Release;
use crate::process::{Command, Event, Invoked, Machine, Run, Status};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// The most events one simulation handles.
pub const MAX_EVENTS: usize = 2_000;

fn someone() -> String {
    "someone".into()
}

/// What a person does with a task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub approve: bool,
    #[serde(default = "someone")]
    pub by: String,
    #[serde(default)]
    pub values: Value,
    /// Seconds after the task opens; at once when left out. After the deadline, the deadline wins.
    #[serde(default)]
    pub after: u64,
}

/// An event a wait hears.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signal {
    pub payload: Value,
    /// Seconds after the wait starts; at once when left out. After the deadline, the deadline wins.
    #[serde(default)]
    pub after: u64,
}

/// One scenario: the trigger, when it starts, and what the world outside the process does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub process: String,
    pub trigger: Value,
    #[serde(default)]
    pub requester: Option<String>,
    pub start: DateTime<Utc>,
    /// Stand-in outcomes by activation (`snapshot`, `lines_each[A]/hold_line`), or by capability id for each
    /// activation without one of its own.
    #[serde(default)]
    pub results: BTreeMap<String, Invoked>,
    /// People's decisions by task activation.
    #[serde(default)]
    pub decisions: BTreeMap<String, Decision>,
    /// Events by wait activation.
    #[serde(default)]
    pub signals: BTreeMap<String, Signal>,
    /// Case tasks done, by case activation (task → values); the case closes after them.
    #[serde(default)]
    pub cases: BTreeMap<String, BTreeMap<String, Value>>,
    /// Recoveries of suspended steps by activation: `true` resumes, `false` ends it.
    #[serde(default)]
    pub recoveries: BTreeMap<String, bool>,
    #[serde(default)]
    pub calendars: Vec<Calendar>,
}

/// Where an event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The scenario's stand-in.
    StandIn,
    /// Nothing in the scenario answered: the step saw an unknown outcome.
    NotModelled,
    /// A deadline passed on the virtual clock.
    Deadline,
}

/// One line of the timeline: something the run asked for, or something that happened to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub at: DateTime<Utc>,
    /// `command` or `event`.
    pub kind: String,
    pub activation: Option<String>,
    pub what: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Simulation {
    pub entries: Vec<Entry>,
    pub status: Status,
    pub run: Run,
    /// Steps whose capability the scenario did not answer.
    pub not_modelled: Vec<String>,
    /// Why it stopped before finishing, when it did: a person's recovery, nothing left to happen, the limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
}

fn activation_of(v: &Value) -> Option<String> {
    v.get("activation").and_then(Value::as_str).map(str::to_string)
}

struct Sim<'a> {
    s: &'a Scenario,
    now: DateTime<Utc>,
    seq: u64,
    /// What will happen: when, in what order it was asked for, what, and where it came from.
    pending: Vec<(DateTime<Utc>, u64, Event, Source)>,
    entries: Vec<Entry>,
    not_modelled: Vec<String>,
    stopped: Option<String>,
}

impl Sim<'_> {
    fn at(&mut self, when: DateTime<Utc>, event: Event, source: Source) {
        self.seq += 1;
        self.pending.push((when, self.seq, event, source));
    }

    fn deadline(&self, wait: &Value) -> Result<DateTime<Utc>, String> {
        let w = Wait::parse(wait).ok_or_else(|| format!("{wait} is not a wait"))?;
        let cal = w.calendar().and_then(|name| self.s.calendars.iter().find(|c| c.name == name));
        Ok(calendar::deadline(&w, self.now, cal)?.at)
    }

    fn command(&mut self, c: Command) -> Result<(), String> {
        let what = serde_json::to_value(&c).unwrap_or(Value::Null);
        self.entries.push(Entry { at: self.now, kind: "command".into(), activation: activation_of(&what), what, source: None });
        match c {
            Command::Invoke { activation, capability, .. } => {
                let answer = self.s.results.get(&activation).or_else(|| self.s.results.get(&capability)).cloned();
                let (outcome, source) = match answer {
                    Some(o) => (o, Source::StandIn),
                    None => {
                        self.not_modelled.push(activation.clone());
                        (Invoked::Unknown { why: format!("{capability} is not modelled in this scenario") }, Source::NotModelled)
                    }
                };
                self.at(self.now, Event::Invoked { activation, outcome }, source);
            }
            Command::OpenTask { activation, timeout, .. } => {
                let due = self.deadline(&timeout)?;
                match self.s.decisions.get(&activation) {
                    Some(d) if self.now + Duration::seconds(d.after as i64) < due => {
                        let when = self.now + Duration::seconds(d.after as i64);
                        let event = Event::TaskDecided {
                            activation: activation.clone(),
                            approved: d.approve,
                            by: d.by.clone(),
                            decision_id: format!("simulated/{activation}"),
                            at: when.to_rfc3339(),
                            values: if d.values.is_null() { json!({}) } else { d.values.clone() },
                        };
                        self.at(when, event, Source::StandIn);
                    }
                    _ => self.at(due, Event::TaskTimedOut { activation }, Source::Deadline),
                }
            }
            Command::Wait { activation, timeout, .. } => {
                let due = self.deadline(&timeout)?;
                match self.s.signals.get(&activation) {
                    Some(sig) if self.now + Duration::seconds(sig.after as i64) < due => {
                        let when = self.now + Duration::seconds(sig.after as i64);
                        self.at(when, Event::Signal { activation, payload: sig.payload.clone() }, Source::StandIn);
                    }
                    _ => self.at(due, Event::WaitTimedOut { activation }, Source::Deadline),
                }
            }
            Command::Sleep { activation, wait } => {
                let due = self.deadline(&wait)?;
                self.at(due, Event::Woke { activation, at: due.to_rfc3339() }, Source::Deadline);
            }
            Command::OpenCase { activation, timeout, .. } => match self.s.cases.get(&activation) {
                Some(done) => {
                    let mut closed = Map::new();
                    for (task, values) in done {
                        let mut out = values.as_object().cloned().unwrap_or_default();
                        out.insert("decision".into(), json!("done"));
                        out.insert("decision_id".into(), json!(format!("simulated/{activation}/{task}")));
                        out.insert("by".into(), json!(someone()));
                        out.insert("at".into(), json!(self.now.to_rfc3339()));
                        closed.insert(task.clone(), Value::Object(out));
                        let event = Event::CaseTaskDone {
                            activation: activation.clone(),
                            task: task.clone(),
                            by: someone(),
                            decision_id: format!("simulated/{activation}/{task}"),
                            at: self.now.to_rfc3339(),
                            values: values.clone(),
                        };
                        self.at(self.now, event, Source::StandIn);
                    }
                    self.at(self.now, Event::CaseClosed { activation, done: Value::Object(closed) }, Source::StandIn);
                }
                None => {
                    let due = self.deadline(&timeout)?;
                    self.at(due, Event::CaseTimedOut { activation }, Source::Deadline);
                }
            },
            // A task, wait, delay or case asked to stop no longer happens; an effect in hand still settles.
            Command::Cancel { activation } => {
                self.pending.retain(|(_, _, e, _)| matches!(e, Event::Invoked { .. }) || event_activation(e) != Some(activation.as_str()))
            }
            Command::Suspend { activation, .. } => match self.s.recoveries.get(&activation) {
                Some(resume) => {
                    let event = Event::Recovered { activation, by: someone(), reason: "in the scenario".into(), resume: *resume };
                    self.at(self.now, event, Source::StandIn);
                }
                None => self.stopped = Some(format!("{activation} waits for a person's recovery")),
            },
            Command::Attention { .. } | Command::Finish { .. } => {}
        }
        Ok(())
    }
}

fn event_activation(e: &Event) -> Option<&str> {
    match e {
        Event::Invoked { activation, .. }
        | Event::TaskDecided { activation, .. }
        | Event::TaskTimedOut { activation }
        | Event::Signal { activation, .. }
        | Event::WaitTimedOut { activation }
        | Event::Woke { activation, .. }
        | Event::CaseTaskDone { activation, .. }
        | Event::CaseClosed { activation, .. }
        | Event::CaseTimedOut { activation }
        | Event::Recovered { activation, .. } => Some(activation),
        Event::CancelRequested { .. } | Event::Migrated { .. } => None,
    }
}

/// Runs a scenario against a release, on a virtual clock.
pub fn simulate(release: &Release, s: &Scenario) -> Result<Simulation, String> {
    let m = Machine::new(release);
    let (mut run, cmds) = m.start(&s.process, "simulation", s.trigger.clone(), s.requester.clone())?;
    let mut sim = Sim { s, now: s.start, seq: 0, pending: vec![], entries: vec![], not_modelled: vec![], stopped: None };
    for c in cmds {
        sim.command(c)?;
    }
    let mut handled = 0usize;
    while !matches!(run.status, Status::Finished { .. }) {
        // The earliest thing to happen, then the first asked for: the clock moves only forward.
        let Some(next) = sim.pending.iter().enumerate().min_by_key(|(_, (t, seq, _, _))| (*t, *seq)).map(|(i, _)| i) else {
            sim.stopped.get_or_insert_with(|| "nothing else would happen".into());
            break;
        };
        let (when, _, event, source) = sim.pending.remove(next);
        sim.now = sim.now.max(when);
        let what = serde_json::to_value(&event).unwrap_or(Value::Null);
        sim.entries.push(Entry { at: sim.now, kind: "event".into(), activation: activation_of(&what), what, source: Some(source) });
        for c in m.on(&mut run, event) {
            sim.command(c)?;
        }
        handled += 1;
        if handled >= MAX_EVENTS {
            sim.stopped = Some(format!("{MAX_EVENTS} events: the simulation stops there"));
            break;
        }
    }
    if matches!(run.status, Status::Finished { .. }) {
        sim.stopped = None;
    }
    Ok(Simulation { entries: sim.entries, status: run.status.clone(), run, not_modelled: sim.not_modelled, stopped: sim.stopped })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::RuntimeProfile;
    use crate::process::Finished;
    use crate::testkit::{compile_value, purchase};

    fn release() -> Release {
        compile_value(purchase(), RuntimeProfile::FullProcess).unwrap_or_else(|d| panic!("{d:#?}"))
    }

    fn start() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-06T10:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn scenario() -> Scenario {
        Scenario {
            process: "purchase_exception".into(),
            trigger: json!({"id": "MR-1"}),
            requester: Some("u-req".into()),
            start: start(),
            results: BTreeMap::from([
                (
                    "snapshot".into(),
                    Invoked::Done { output: json!({"total": {"amount": "150000.00", "currency": "INR"}, "supplier_email": "s@x.example", "version": 1}) },
                ),
                ("procurement.material_request.make_purchase_order".into(), Invoked::Done { output: json!({"order_id": "PO-1"}) }),
            ]),
            decisions: BTreeMap::new(),
            signals: BTreeMap::new(),
            cases: BTreeMap::new(),
            recoveries: BTreeMap::new(),
            calendars: vec![],
        }
    }

    #[test]
    fn a_person_decides_an_hour_later_and_what_is_not_modelled_is_said() {
        let mut s = scenario();
        s.decisions.insert("review".into(), Decision { approve: true, by: "u-rev".into(), values: Value::Null, after: 3600 });
        let sim = simulate(&release(), &s).unwrap();
        let decided = sim.entries.iter().find(|e| e.kind == "event" && e.activation.as_deref() == Some("review")).unwrap();
        assert_eq!(decided.at, start() + Duration::hours(1), "the clock moved to the decision");
        assert_eq!(sim.not_modelled, vec!["submit_supplier".to_string()], "the mail was not answered, so the run saw it as unknown");
        assert_eq!(sim.stopped.as_deref(), Some("needs_attention waits for a person's recovery"));
        assert!(sim.entries.iter().any(|e| e.source == Some(Source::NotModelled)));
        assert!(
            !sim.entries.iter().any(|e| e.what["outcome"]["result"] == json!("done") && e.activation.as_deref() == Some("submit_supplier")),
            "nothing invented"
        );
    }

    #[test]
    fn nobody_deciding_runs_to_the_deadline_on_the_virtual_clock_and_recovery_ends_it() {
        let mut s = scenario();
        s.recoveries.insert("needs_attention".into(), false);
        let sim = simulate(&release(), &s).unwrap();
        let timed_out = sim.entries.iter().find(|e| e.what["event"] == json!("task_timed_out")).unwrap();
        assert_eq!((timed_out.at, timed_out.source), (start() + Duration::days(2), Some(Source::Deadline)));
        assert!(matches!(sim.status, Status::Finished { outcome: Finished::Refused, .. }), "{:?}", sim.status);
        assert_eq!(sim.stopped, None);
    }

    #[test]
    fn the_whole_way_with_every_answer_given_completes() {
        let mut s = scenario();
        s.decisions.insert("review".into(), Decision { approve: true, by: "u-rev".into(), values: Value::Null, after: 0 });
        s.results.insert("ferrum.mail".into(), Invoked::Done { output: json!({}) });
        s.signals.insert("supplier_ack".into(), Signal { payload: json!({"id": "PO-1", "status": "Acknowledged"}), after: 7200 });
        let sim = simulate(&release(), &s).unwrap();
        assert!(matches!(sim.status, Status::Finished { outcome: Finished::Completed, .. }), "{:?}", sim.status);
        assert_eq!(sim.entries.last().unwrap().at, start() + Duration::hours(2));
        assert!(sim.not_modelled.is_empty());
        // An answer after the deadline loses to it.
        s.signals.insert("supplier_ack".into(), Signal { payload: json!({}), after: 90_000 });
        let late = simulate(&release(), &s).unwrap();
        assert!(late.entries.iter().any(|e| e.what["event"] == json!("wait_timed_out")));
    }
}
