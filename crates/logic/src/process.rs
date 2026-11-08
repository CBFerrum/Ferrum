//! A process run as a pure state machine (package MASTER_PLAN §8, §10). The durable runtime feeds it what
//! happened and carries out what it asks for; the builder's simulator does the same with a virtual clock and
//! stand-in capabilities, so what is simulated is what runs (decisions L-05).
//!
//! It reads no clock, store or network. Time arrives inside events, every effect leaves as a [`Command`] named
//! by its activation, and an activation's name is the same however often it is attempted: the step's id, under
//! the path of structured steps around it (`quotes.email/mail`, `lines_each[SKU-4]/hold_line`,
//! `review_loop#2/review`). Given the same release and the same events, it asks for the same things in the same
//! order, which is what replaying a durable history needs.
//!
//! * A block runs one step at a time; side by side only through a parallel step, a for-each or a subprocess.
//! * A parallel `any` asks its losing branches to stop and keeps what they had in flight: an effect that settles
//!   after its branch lost, or after the run was cancelled, is recorded and handed to a person, never dropped.
//! * Compensation undoes the run's effects newest first, each through its app's compensating action; one that
//!   fails stays an open obligation.
//! * A suspension waits for an authorized recovery: resume where the source allows, or end refused. Nothing
//!   turns needing attention into success.

use crate::canonical;
use crate::capability::digestible;
use crate::compile::{Locked, ProcessTypes, Release, parameter_type};
use crate::decision::{self, Outcome};
use crate::expr::{self, Env, Facts, Ty, Val};
use crate::solution::{CaseTask, EndOutcome, FormField, Join, Node, Process};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// The digest a person's task binds its decision to.
pub const MATERIAL_SCHEMA: &str = "ferrum.task.material/1";

/// What a run asks its runtime to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Command {
    /// Run a capability through the gate under this activation's effect key.
    Invoke {
        activation: String,
        capability: String,
        version: String,
        input: Value,
        /// Whether it changes anything (and so carries an effect key).
        changes: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        approval: Option<Approval>,
    },
    /// Open a person's task.
    OpenTask {
        activation: String,
        label: String,
        role: String,
        material: Value,
        digest: String,
        form: Vec<FormField>,
        approvals: u32,
        delegation: u32,
        /// Who may not decide it, whatever their roles: the requester.
        exclude: Vec<String>,
        timeout: Value,
    },
    /// Wait for an event that names this value.
    Wait {
        activation: String,
        event: String,
        correlation: Value,
        timeout: Value,
    },
    Sleep {
        activation: String,
        wait: Value,
    },
    OpenCase {
        activation: String,
        label: String,
        owner: String,
        tasks: Vec<CaseTask>,
        exclude: Vec<String>,
        timeout: Value,
    },
    /// Ask work in hand to stop: a losing branch's, a cancelled run's. It may still settle.
    Cancel {
        activation: String,
    },
    /// Something a person has to look at: an effect that settled after it was no longer wanted, a compensation
    /// that failed.
    Attention {
        activation: String,
        why: String,
        evidence: Value,
    },
    /// A step waits for an authorized recovery.
    Suspend {
        activation: String,
        reason: String,
        resumable: bool,
    },
    Finish {
        outcome: Finished,
        output: Value,
    },
}

/// What an effect relies on: a person's decision on exact material.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Approval {
    pub task: String,
    pub decision_id: String,
    pub digest: String,
    pub material: Value,
    /// The entity of the record the material is, when it is one: re-checked as the effect runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
}

/// How a capability's invocation came out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Invoked {
    Done {
        output: Value,
    },
    Refused {
        code: String,
        message: String,
    },
    /// It may or may not have happened: reconciled, never sent again blindly.
    Unknown {
        why: String,
    },
}

/// What happened, as the runtime tells it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Invoked {
        activation: String,
        outcome: Invoked,
    },
    /// A task's outcome: approved by as many people as it needs, or rejected by one.
    TaskDecided {
        activation: String,
        approved: bool,
        by: String,
        decision_id: String,
        at: String,
        values: Value,
    },
    TaskTimedOut {
        activation: String,
    },
    Signal {
        activation: String,
        payload: Value,
    },
    WaitTimedOut {
        activation: String,
    },
    Woke {
        activation: String,
        at: String,
    },
    CaseTaskDone {
        activation: String,
        task: String,
        by: String,
        decision_id: String,
        at: String,
        values: Value,
    },
    CaseClosed {
        activation: String,
        /// Each finished task's outputs as the record has them, so a close that overtakes a task's own delivery
        /// still closes.
        #[serde(default)]
        done: Value,
    },
    CaseTimedOut {
        activation: String,
    },
    CancelRequested {
        by: String,
        reason: String,
    },
    /// A person's recovery of a suspended step: resume where its source allows, or end it refused.
    Recovered {
        activation: String,
        by: String,
        reason: String,
        resume: bool,
    },
    /// A move onto another release of its solution, planned by one administrator and approved by another
    /// (FLC-MIG-004). Its runtime carries it out ([`Machine::migrate`], checked again where the business records
    /// are); the run's own state machine has nothing to do with it.
    Migrated {
        release: String,
        map: BTreeMap<String, String>,
        by: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Finished {
    Completed,
    Refused,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    Running,
    /// Some step waits for a person's recovery.
    NeedsAttention,
    /// Asked to stop; waiting for work in hand to settle.
    Cancelling,
    Finished {
        outcome: Finished,
        output: Value,
    },
}

/// An effect the run did, with what undoes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    pub activation: String,
    pub capability: String,
    pub input: Value,
    pub output: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensation: Option<String>,
    #[serde(default)]
    pub compensated: bool,
    /// Settled after its branch lost or its run was cancelled.
    #[serde(default)]
    pub unwanted: bool,
}

/// A block instance: a run's top, a branch, an item, a round, or a subprocess.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Frame {
    process: String,
    /// Where its block is from its process's top: structured step, block id, structured step, block id, …
    block: Vec<String>,
    at: String,
    /// The frame holding the structured step that started it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    /// A subprocess starts a scope of its own: its trigger is its input, and it reads nothing of its caller's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trigger: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    item: Option<Item>,
    #[serde(default)]
    cancelled: bool,
}

/// A for-each's item: its step, its key as the data gave it, and its value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Item {
    node: String,
    key: String,
    value: Value,
}

/// A key as a frame's path writes it: the characters that separate a path's pieces are escaped, so a key from
/// data cannot be read as structure.
fn path_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for c in key.chars() {
        match c {
            '%' | '[' | ']' | '/' | '.' | '#' | '>' => out.push_str(&format!("%{:02X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// What an activation waits for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "for", rename_all = "snake_case")]
enum Waiting {
    Invoke {
        frame: String,
        node: String,
        /// What was asked, for the effect's record and its compensation.
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        undoing: Option<usize>,
    },
    Task {
        frame: String,
        node: String,
        material: Value,
        digest: String,
    },
    Wait {
        frame: String,
        node: String,
    },
    Sleep {
        frame: String,
        node: String,
    },
    Case {
        frame: String,
        node: String,
        done: BTreeMap<String, Value>,
    },
    Suspended {
        frame: String,
        node: String,
    },
    Parallel {
        frame: String,
        node: String,
        join: Join,
        /// Each branch's outcome and output, once it ended.
        ended: BTreeMap<String, (Finished, Value)>,
        branches: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        winner: Option<String>,
    },
    ForEach {
        frame: String,
        node: String,
        keys: Vec<String>,
        items: Vec<Value>,
        next: usize,
        concurrency: usize,
        running: BTreeSet<String>,
        results: BTreeMap<String, (Finished, Value)>,
        failed: bool,
    },
    Repeat {
        frame: String,
        node: String,
        round: u32,
        max: u32,
    },
    Subprocess {
        frame: String,
        node: String,
    },
    Compensate {
        frame: String,
        node: String,
        queue: Vec<usize>,
        done: u32,
        failed: u32,
    },
}

impl Waiting {
    fn frame(&self) -> &str {
        match self {
            Waiting::Invoke { frame, .. }
            | Waiting::Task { frame, .. }
            | Waiting::Wait { frame, .. }
            | Waiting::Sleep { frame, .. }
            | Waiting::Case { frame, .. }
            | Waiting::Suspended { frame, .. }
            | Waiting::Parallel { frame, .. }
            | Waiting::ForEach { frame, .. }
            | Waiting::Repeat { frame, .. }
            | Waiting::Subprocess { frame, .. }
            | Waiting::Compensate { frame, .. } => frame,
        }
    }
    fn node(&self) -> &str {
        match self {
            Waiting::Invoke { node, .. }
            | Waiting::Task { node, .. }
            | Waiting::Wait { node, .. }
            | Waiting::Sleep { node, .. }
            | Waiting::Case { node, .. }
            | Waiting::Suspended { node, .. }
            | Waiting::Parallel { node, .. }
            | Waiting::ForEach { node, .. }
            | Waiting::Repeat { node, .. }
            | Waiting::Subprocess { node, .. }
            | Waiting::Compensate { node, .. } => node,
        }
    }
}

/// One run's state: everything a durable runtime carries across a continuation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub process: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<String>,
    pub trigger: Value,
    pub status: Status,
    pub steps: u32,
    /// Completed activations' outputs.
    pub outputs: BTreeMap<String, Value>,
    /// Effects done, oldest first.
    pub effects: Vec<Effect>,
    /// Decisions effects may rely on, by the task's activation.
    #[serde(default)]
    pub approvals: BTreeMap<String, Approval>,
    frames: BTreeMap<String, Frame>,
    waiting: BTreeMap<String, Waiting>,
}

/// What a move onto another release did: each waiting step's place, and the activations that changed name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Moved {
    pub moved: Vec<String>,
    /// The activations the run waited at before the move.
    pub waiting: Vec<String>,
    /// Waiting activations renamed with their step (old, new).
    pub renamed: Vec<(String, String)>,
}

/// What a step that waits waits as, by its kind.
fn waits_as(n: &Node) -> &'static str {
    match n {
        Node::HumanTask { .. } => "task",
        Node::WaitEvent { .. } => "wait",
        Node::Delay { .. } => "delay",
        Node::Suspend { .. } => "stop",
        Node::Case { .. } => "case",
        Node::Parallel { .. } => "set of branches",
        Node::ForEach { .. } => "loop over items",
        Node::Repeat { .. } => "repeat",
        Node::Subprocess { .. } => "subprocess",
        _ => "step that does not wait",
    }
}

/// The activation of `node` in `frame`.
pub fn activation(frame: &str, node: &str) -> String {
    if frame.is_empty() { node.to_string() } else { format!("{frame}/{node}") }
}

/// A release ready to run its processes.
pub struct Machine<'r> {
    release: &'r Release,
    processes: BTreeMap<&'r str, &'r Process>,
    decisions: BTreeMap<&'r str, &'r decision::Table>,
    lock: BTreeMap<&'r str, &'r Locked>,
    params_ty: Ty,
    params: Val,
}

impl<'r> Machine<'r> {
    pub fn new(release: &'r Release) -> Machine<'r> {
        let mut tys = BTreeMap::new();
        let mut vals = BTreeMap::new();
        for (name, p) in &release.ir.parameters {
            let ty = parameter_type(&p.ty).unwrap_or(Ty::Any);
            vals.insert(name.clone(), Val::from_json(&p.value, &ty));
            tys.insert(name.clone(), ty);
        }
        Machine {
            release,
            processes: release.ir.processes.iter().map(|p| (p.id.as_str(), p)).collect(),
            decisions: release.ir.decisions.iter().map(|d| (d.id.as_str(), d)).collect(),
            lock: release.lock.iter().map(|l| (l.id.as_str(), l)).collect(),
            params_ty: Ty::Record(tys),
            params: Val::Record(vals),
        }
    }

    /// Starts a run of `process` for `trigger`, on behalf of `requester`.
    pub fn start(&self, process: &str, run_id: &str, trigger: Value, requester: Option<String>) -> Result<(Run, Vec<Command>), String> {
        let p = self.processes.get(process).ok_or_else(|| format!("{process} is not a process of this release"))?;
        let mut run = Run {
            id: run_id.to_string(),
            process: process.to_string(),
            requester,
            trigger,
            status: Status::Running,
            steps: 0,
            outputs: BTreeMap::new(),
            effects: vec![],
            approvals: BTreeMap::new(),
            frames: BTreeMap::new(),
            waiting: BTreeMap::new(),
        };
        run.frames.insert(
            String::new(),
            Frame { process: process.to_string(), block: vec![], at: p.entry.clone(), parent: None, trigger: None, item: None, cancelled: false },
        );
        let mut out = vec![];
        self.pump(&mut run, VecDeque::from([String::new()]), &mut out);
        Ok((run, out))
    }

    /// Applies what happened. An event for an activation the run is not waiting for (a repeat, a late timeout)
    /// changes nothing, unless it is an effect that settled late, which is recorded and handed to a person.
    pub fn on(&self, run: &mut Run, event: Event) -> Vec<Command> {
        let mut out = vec![];
        let mut queue = VecDeque::new();
        if matches!(run.status, Status::Finished { .. }) {
            if let Event::Invoked { activation, outcome: Invoked::Done { output } } = event {
                out.push(Command::Attention { activation, why: "an effect settled after its run ended".into(), evidence: output });
            }
            return out;
        }
        match event {
            Event::Migrated { .. } => return out,
            Event::CancelRequested { by, reason } => {
                self.cancel(run, &by, &reason, &mut out);
                return out;
            }
            Event::Invoked { activation, outcome } => match run.waiting.remove(&activation) {
                Some(Waiting::Invoke { frame, node, undoing: Some(idx), .. }) => {
                    self.undone(run, &frame, &node, idx, &activation, outcome, &mut queue, &mut out)
                }
                Some(Waiting::Invoke { frame, node, input, undoing: None }) => {
                    self.invoked(run, &frame, &node, &activation, input, outcome, &mut queue, &mut out)
                }
                Some(other) => {
                    run.waiting.insert(activation, other);
                }
                None => {}
            },
            Event::TaskDecided { activation, approved, by, decision_id, at, values } => {
                if let Some(Waiting::Task { frame, node, material, digest }) = run.waiting.remove(&activation) {
                    let mut output = values.as_object().cloned().unwrap_or_default();
                    output.insert("decision".into(), json!(if approved { "approved" } else { "rejected" }));
                    output.insert("decision_id".into(), json!(decision_id));
                    output.insert("by".into(), json!(by));
                    output.insert("at".into(), json!(at));
                    if approved {
                        let process = run.frames.get(&frame).map(|f| f.process.clone()).unwrap_or_default();
                        let entity = self.types(&process).and_then(|t| t.materials.get(&node)).cloned();
                        run.approvals.insert(activation.clone(), Approval { task: activation.clone(), decision_id, digest, material, entity });
                    }
                    run.outputs.insert(activation, Value::Object(output));
                    self.route(run, &frame, &node, if approved { "next" } else { "on_reject" }, &mut queue);
                } else {
                    // A decision that lost to its timeout, or came twice, decides nothing.
                }
            }
            Event::TaskTimedOut { activation } => {
                if let Some(Waiting::Task { frame, node, .. }) = run.waiting.remove(&activation) {
                    self.route(run, &frame, &node, "on_timeout", &mut queue);
                }
            }
            Event::Signal { activation, payload } => {
                if let Some(Waiting::Wait { frame, node }) = run.waiting.remove(&activation) {
                    run.outputs.insert(activation, payload);
                    self.route(run, &frame, &node, "next", &mut queue);
                }
            }
            Event::WaitTimedOut { activation } => {
                if let Some(Waiting::Wait { frame, node }) = run.waiting.remove(&activation) {
                    self.route(run, &frame, &node, "on_timeout", &mut queue);
                }
            }
            Event::Woke { activation, at } => {
                if let Some(Waiting::Sleep { frame, node }) = run.waiting.remove(&activation) {
                    run.outputs.insert(activation, json!({"at": at}));
                    self.route(run, &frame, &node, "next", &mut queue);
                }
            }
            Event::CaseTaskDone { activation, task, by, decision_id, at, values } => {
                if let Some(Waiting::Case { done, .. }) = run.waiting.get_mut(&activation) {
                    let mut output = values.as_object().cloned().unwrap_or_default();
                    output.insert("decision".into(), json!("done"));
                    output.insert("decision_id".into(), json!(decision_id));
                    output.insert("by".into(), json!(by));
                    output.insert("at".into(), json!(at));
                    done.entry(task).or_insert(Value::Object(output));
                }
            }
            Event::CaseClosed { activation, done: recorded } => {
                if let Some(Waiting::Case { frame, node, mut done }) = run.waiting.get(&activation).cloned() {
                    for (task, output) in recorded.as_object().into_iter().flatten() {
                        done.entry(task.clone()).or_insert_with(|| output.clone());
                    }
                    let required: Vec<&CaseTask> = match self.node(run, &frame, &node) {
                        Some(Node::Case { tasks, .. }) => tasks.iter().filter(|t| t.required).collect(),
                        _ => vec![],
                    };
                    // The runtime offers closing only once the required tasks are done; this says so again.
                    if required.iter().all(|t| done.contains_key(&t.id)) {
                        run.waiting.remove(&activation);
                        run.outputs.insert(activation, Value::Object(done.into_iter().collect()));
                        self.route(run, &frame, &node, "next", &mut queue);
                    }
                }
            }
            Event::CaseTimedOut { activation } => {
                if let Some(Waiting::Case { frame, node, .. }) = run.waiting.remove(&activation) {
                    self.route(run, &frame, &node, "on_timeout", &mut queue);
                }
            }
            Event::Recovered { activation, by, reason, resume } => {
                if let Some(Waiting::Suspended { frame, node }) = run.waiting.remove(&activation) {
                    run.outputs.insert(activation.clone(), json!({"recovered_by": by, "reason": reason, "resumed": resume}));
                    let resume_to = match self.node(run, &frame, &node) {
                        Some(Node::Suspend { resume_to, .. }) => resume_to.clone(),
                        _ => None,
                    };
                    match resume_to.filter(|_| resume) {
                        Some(target) => {
                            if let Some(f) = run.frames.get_mut(&frame) {
                                f.at = target;
                            }
                            queue.push_back(frame);
                        }
                        None => self.ended(run, &frame, Finished::Refused, json!({}), &mut queue, &mut out),
                    }
                }
            }
        }
        self.pump(run, queue, &mut out);
        out
    }

    /// Runs frames until each waits or ends.
    fn pump(&self, run: &mut Run, mut queue: VecDeque<String>, out: &mut Vec<Command>) {
        while let Some(frame) = queue.pop_front() {
            self.advance(run, &frame, &mut queue, out);
        }
        self.settle_status(run);
    }

    fn settle_status(&self, run: &mut Run) {
        if matches!(run.status, Status::Finished { .. }) {
            return;
        }
        if matches!(run.status, Status::Cancelling) {
            return;
        }
        run.status = if run.waiting.values().any(|w| matches!(w, Waiting::Suspended { .. })) { Status::NeedsAttention } else { Status::Running };
    }

    /// Moves `run` onto this release (FLC-MIG-004): only as `map` says, only what can move faithfully, and all
    /// of it or nothing. Each frame of the run, at the top or inside a block (a loop's item, a branch, a round,
    /// a subprocess), must find its block here by the same steps and block ids: a step that holds others keeps
    /// its id. Each step a frame waits at moves, within its block, to the step `map` names for it or to the one
    /// with its id, which must wait the same way: a task stays a task (its decisions so far and its deadline
    /// with it), a wait a wait, a delay a delay, a stop a stop; a set of branches keeps each branch it has, and a
    /// subprocess its process. An approval given stands only for a task of this release that `map` names for it
    /// (`approval:<task>`) or that has its id. A run with an effect or an undoing in hand, or finished or
    /// stopping, does not move. Nothing is dropped: what cannot move is said, and the run stays as it was.
    pub fn migrate(&self, run: &mut Run, map: &BTreeMap<String, String>) -> Result<Moved, Vec<String>> {
        let to = |id: &str| map.get(id).cloned().unwrap_or_else(|| id.to_string());
        let old_process = run.process.clone();
        let process = map.get("process").cloned().unwrap_or_else(|| old_process.clone());
        if !self.processes.contains_key(process.as_str()) {
            return Err(vec![format!("{process} is not a process of {} {}", self.release.solution, self.release.version)]);
        }
        // A frame's process here: the run's own moves to `process`; a subprocess's keeps its id.
        let proc_of = |p: &str| if p == old_process { process.clone() } else { p.to_string() };
        let version = &self.release.version;
        let mut refused = vec![];
        if matches!(run.status, Status::Finished { .. } | Status::Cancelling) {
            refused.push("a run that has finished or is stopping does not move".to_string());
        }
        let waits_in: BTreeSet<&str> = run.waiting.values().map(|w| w.frame()).collect();
        for (name, f) in &run.frames {
            let shown = if name.is_empty() { "The run".to_string() } else { name.clone() };
            match self.nodes_of(&proc_of(&f.process), &f.block) {
                None => refused.push(format!("{shown} is inside {}, which {version} does not have as it was", f.block.join("/"))),
                // Where a frame rests without waiting must be here too; a waiting one is looked at below.
                Some(nodes) if !waits_in.contains(name.as_str()) && !f.at.is_empty() && !nodes.iter().any(|n| n.id() == to(&f.at)) => {
                    refused.push(format!("{shown} stands at {}, which has no counterpart in {version}", f.at))
                }
                Some(_) => {}
            }
        }
        let mut moved = Moved { moved: vec![], waiting: run.waiting.keys().cloned().collect(), renamed: vec![] };
        for (act, w) in &run.waiting {
            let frame = w.frame().to_string();
            let Some(nodes) = run.frames.get(&frame).and_then(|f| self.nodes_of(&proc_of(&f.process), &f.block)) else {
                // A frame the run no longer has, or whose block is not here: said above or here.
                if !run.frames.contains_key(&frame) {
                    refused.push(format!("{act} waits in a part of the run that has ended"));
                }
                continue;
            };
            let (kind, node, leaf) = match w {
                Waiting::Task { node, .. } => ("task", node, true),
                Waiting::Wait { node, .. } => ("wait", node, true),
                Waiting::Sleep { node, .. } => ("delay", node, true),
                Waiting::Suspended { node, .. } => ("stop", node, true),
                Waiting::Case { node, .. } => ("case", node, true),
                Waiting::Parallel { node, .. } => ("set of branches", node, false),
                Waiting::ForEach { node, .. } => ("loop over items", node, false),
                Waiting::Repeat { node, .. } => ("repeat", node, false),
                Waiting::Subprocess { node, .. } => ("subprocess", node, false),
                Waiting::Invoke { .. } => {
                    refused.push(format!("{act} has an effect in hand: it moves once that has settled"));
                    continue;
                }
                Waiting::Compensate { .. } => {
                    refused.push(format!("{act} is undoing what was done: it moves once that has finished"));
                    continue;
                }
            };
            let target = to(node);
            if !leaf && target != *node {
                refused.push(format!("{node} holds other steps, so it keeps its id"));
                continue;
            }
            match nodes.iter().find(|n| n.id() == target) {
                None => refused.push(format!("{node} has no counterpart in {version}: map it to a step that waits the same way")),
                Some(n) if waits_as(n) != kind => refused.push(format!("{node} waits as a {kind}, and {target} is a {}", waits_as(n))),
                Some(n) => {
                    match (w, n) {
                        (Waiting::Parallel { branches, .. }, Node::Parallel { branches: here, .. }) => {
                            if let Some(b) = branches.iter().find(|b| !here.iter().any(|h| h.id == **b)) {
                                refused.push(format!("{node} has a branch {b} that {version} does not"));
                                continue;
                            }
                        }
                        (Waiting::Subprocess { .. }, Node::Subprocess { process: p, .. }) if !self.processes.contains_key(p.as_str()) => {
                            refused.push(format!("{node} calls {p}, which {version} does not have"));
                            continue;
                        }
                        _ => {}
                    }
                    if *node == target {
                        moved.moved.push(format!("{act} stays at {node}"));
                    } else {
                        moved.moved.push(format!("{act} moves to {target}"));
                        moved.renamed.push((act.clone(), activation(&frame, &target)));
                    }
                }
            }
        }
        let approval_of = |task: &str| map.get(&format!("approval:{task}")).cloned().unwrap_or_else(|| to(task));
        for key in run.approvals.keys() {
            let (_, task) = key.rsplit_once('/').unwrap_or(("", key.as_str()));
            let target = approval_of(task);
            if !matches!(self.anywhere(&process, &target), Some(Node::HumanTask { .. })) {
                refused.push(format!("the approval given at {key} has nothing to stand for in {version}: map it to a task (approval:{task})"));
            }
        }
        if !refused.is_empty() {
            return Err(refused);
        }
        // All of it, at once.
        for f in run.frames.values_mut() {
            f.process = proc_of(&f.process);
            f.at = to(&f.at);
        }
        let waiting = std::mem::take(&mut run.waiting);
        for (act, mut w) in waiting {
            if let Waiting::Task { node, .. }
            | Waiting::Wait { node, .. }
            | Waiting::Sleep { node, .. }
            | Waiting::Suspended { node, .. }
            | Waiting::Case { node, .. } = &mut w
            {
                *node = to(node);
            }
            let act = moved.renamed.iter().find(|(old, _)| *old == act).map(|(_, new)| new.clone()).unwrap_or(act);
            run.waiting.insert(act, w);
        }
        let renamed_step = |key: String, name: &dyn Fn(&str) -> String| match key.rsplit_once('/') {
            Some((frame, step)) => activation(frame, &name(step)),
            None => name(&key),
        };
        run.outputs = std::mem::take(&mut run.outputs).into_iter().map(|(act, v)| (renamed_step(act, &to), v)).collect();
        run.approvals = std::mem::take(&mut run.approvals).into_iter().map(|(key, a)| (renamed_step(key, &approval_of), a)).collect();
        run.process = process;
        Ok(moved)
    }

    /// A step of `process` with this id, wherever it is in its blocks.
    fn anywhere(&self, process: &str, id: &str) -> Option<&'r Node> {
        fn find<'n>(nodes: &'n [Node], id: &str) -> Option<&'n Node> {
            nodes.iter().find_map(|n| if n.id() == id { Some(n) } else { n.blocks().into_iter().find_map(|b| find(&b.nodes, id)) })
        }
        find(&self.processes.get(process)?.nodes, id)
    }

    fn nodes_of(&self, process: &str, block: &[String]) -> Option<&'r [Node]> {
        let mut nodes: &'r [Node] = &self.processes.get(process)?.nodes;
        for pair in block.chunks(2) {
            let [step, id] = pair else { return None };
            let holder = nodes.iter().find(|n| n.id() == step)?;
            nodes = &holder.blocks().into_iter().find(|b| b.id == *id)?.nodes;
        }
        Some(nodes)
    }

    fn node(&self, run: &Run, frame: &str, id: &str) -> Option<&'r Node> {
        let f = run.frames.get(frame)?;
        self.nodes_of(&f.process, &f.block)?.iter().find(|n| n.id() == id)
    }

    fn types(&self, process: &str) -> Option<&'r ProcessTypes> {
        self.release.ir.types.get(process)
    }

    /// What expressions in `frame` may name, typed and valued: the trigger, the parameters, the item, and each
    /// completed step of this frame and the frames around it, up to the scope's root.
    fn scope(&self, run: &Run, frame: &str) -> (Env, Facts) {
        let process = run.frames.get(frame).map(|f| f.process.clone()).unwrap_or_default();
        let types = self.types(&process).cloned().unwrap_or_default();
        let mut env = Env::default().with("parameters", self.params_ty.clone());
        let mut facts = Facts::default().with("parameters", self.params.clone());
        let mut trigger = None;
        let mut item: Option<Item> = None;
        let mut here = Some(frame.to_string());
        while let Some(name) = here {
            let Some(f) = run.frames.get(&name) else { break };
            if item.is_none() {
                item = f.item.clone();
            }
            if let Some(nodes) = self.nodes_of(&f.process, &f.block) {
                for n in nodes {
                    if let Some(v) = run.outputs.get(&activation(&name, n.id())) {
                        let ty = types.steps.get(n.id()).cloned().unwrap_or(Ty::Any);
                        env = env.with(n.id(), Ty::Record(BTreeMap::from([("output".into(), ty.clone())])));
                        facts = facts.with(n.id(), Val::Record(BTreeMap::from([("output".into(), Val::from_json(v, &ty))])));
                    }
                }
            }
            if f.trigger.is_some() {
                trigger = f.trigger.clone();
                break;
            }
            here = f.parent.clone();
        }
        let trigger = trigger.unwrap_or_else(|| run.trigger.clone());
        env = env.with("trigger", types.trigger.clone());
        facts = facts.with("trigger", Val::from_json(&trigger, &types.trigger));
        if let Some(it) = item {
            let ty = types.items.get(&it.node).cloned().unwrap_or(Ty::Any);
            env = env.with("item", ty.clone());
            facts = facts.with("item", Val::from_json(&it.value, &ty));
        }
        (env, facts)
    }

    /// An expression over `frame`'s scope and one item of a for-each in it.
    fn eval_item(&self, run: &Run, frame: &str, node: &str, value: &Value, src: &str) -> Result<Val, String> {
        let (env, facts) = self.scope(run, frame);
        let process = run.frames.get(frame).map(|f| f.process.clone()).unwrap_or_default();
        let ty = self.types(&process).and_then(|t| t.items.get(node)).cloned().unwrap_or(Ty::Any);
        let (env, facts) = (env.with("item", ty.clone()), facts.with("item", Val::from_json(value, &ty)));
        match expr::compile(src, &env).map_err(|e| e.message)?.eval(&facts) {
            Val::Unknown(why) => Err(why),
            v => Ok(v),
        }
    }

    fn eval(&self, run: &Run, frame: &str, src: &str) -> Result<Val, String> {
        let (env, facts) = self.scope(run, frame);
        let v = expr::compile(src, &env).map_err(|e| e.message)?.eval(&facts);
        match v {
            Val::Unknown(why) => Err(why),
            v => Ok(v),
        }
    }

    /// An input mapping, dotted keys filling objects, as JSON.
    fn input(&self, run: &Run, frame: &str, mapping: &BTreeMap<String, String>) -> Result<Value, String> {
        let mut out = Map::new();
        for (key, src) in mapping {
            let v = self.eval(run, frame, src).map_err(|why| format!("{key}: {why}"))?.to_json();
            let mut parts: Vec<&str> = key.split('.').collect();
            let last = parts.pop().unwrap_or_default();
            let mut at = &mut out;
            for part in parts {
                at = at.entry(part.to_string()).or_insert_with(|| json!({})).as_object_mut().ok_or("an input field is given twice")?;
            }
            at.insert(last.to_string(), v);
        }
        Ok(Value::Object(out))
    }

    /// Moves a frame along a step's named route, and queues it to run on.
    fn route(&self, run: &mut Run, frame: &str, node: &str, route: &str, queue: &mut VecDeque<String>) {
        let Some(target) = self.node(run, frame, node).and_then(|n| n.routes().into_iter().find(|(r, _)| *r == route).map(|(_, t)| t.to_string())) else {
            return;
        };
        if let Some(f) = run.frames.get_mut(frame) {
            f.at = target;
        }
        queue.push_back(frame.to_string());
    }

    fn fail(&self, run: &mut Run, frame: &str, node: &str, route: &str, why: String, queue: &mut VecDeque<String>) {
        run.outputs.insert(activation(frame, node), json!({"error": why}));
        self.route(run, frame, node, route, queue);
    }

    fn spawn(&self, run: &mut Run, path: String, frame: Frame, queue: &mut VecDeque<String>) {
        run.frames.insert(path.clone(), frame);
        queue.push_back(path);
    }

    /// Runs one frame from where it is until it waits or ends.
    fn advance(&self, run: &mut Run, frame: &str, queue: &mut VecDeque<String>, out: &mut Vec<Command>) {
        loop {
            if matches!(run.status, Status::Finished { .. } | Status::Cancelling) {
                return;
            }
            let Some(f) = run.frames.get(frame).cloned() else { return };
            if f.cancelled || run.waiting.values().any(|w| w.frame() == frame) {
                return;
            }
            let Some(node) = self.node(run, frame, &f.at) else { return };
            let max = self.processes.get(run.process.as_str()).map(|p| p.max_steps).unwrap_or(0);
            run.steps += 1;
            let act = activation(frame, node.id());
            if run.steps > max {
                run.waiting.insert(act.clone(), Waiting::Suspended { frame: frame.into(), node: node.id().into() });
                out.push(Command::Suspend { activation: act, reason: format!("the run reached its {max} steps"), resumable: false });
                return;
            }
            let nid = node.id().to_string();
            match node {
                Node::Query { capability, input, .. } | Node::Action { capability, input, .. } => {
                    let approval = match node {
                        Node::Action { approval: Some(task), .. } => {
                            match self.visible_activation(run, frame, task).and_then(|a| run.approvals.get(&a).cloned()) {
                                Some(a) => Some(a),
                                None => {
                                    self.fail(run, frame, &nid, "on_error", format!("{task}'s approval is not on record"), queue);
                                    return;
                                }
                            }
                        }
                        _ => None,
                    };
                    match self.input(run, frame, input) {
                        Ok(value) => {
                            run.waiting.insert(act.clone(), Waiting::Invoke { frame: frame.into(), node: nid, input: value.clone(), undoing: None });
                            out.push(Command::Invoke {
                                activation: act,
                                capability: capability.id.clone(),
                                version: capability.version.clone(),
                                input: value,
                                changes: matches!(node, Node::Action { .. }),
                                approval,
                            });
                        }
                        Err(why) => self.fail(run, frame, &nid, "on_error", why, queue),
                    }
                    return;
                }
                Node::Decision { decision: name, routes, .. } => {
                    let (env, facts) = self.scope(run, frame);
                    let decided = self
                        .decisions
                        .get(name.as_str())
                        .ok_or_else(|| format!("{name} is not a decision"))
                        .and_then(|t| decision::compile(t, &env).map_err(|e| e.message));
                    match decided.map(|c| c.decide(&facts)) {
                        Ok(Outcome::Decided { outcome, .. }) => match routes.iter().find(|r| r.outcome == outcome) {
                            Some(r) => {
                                run.outputs.insert(act, json!({"outcome": outcome}));
                                if let Some(f) = run.frames.get_mut(frame) {
                                    f.at = r.target.clone();
                                }
                                continue;
                            }
                            None => self.fail(run, frame, &nid, "on_error", format!("{name} decided {outcome}, which has no route"), queue),
                        },
                        Ok(Outcome::Unknown(why)) | Err(why) => self.fail(run, frame, &nid, "on_error", why, queue),
                    }
                    return;
                }
                Node::HumanTask { label, role, material, form, approvals, delegation, timeout, .. } => {
                    let material = match self.eval(run, frame, material) {
                        Ok(v) => v.to_json(),
                        Err(why) => {
                            self.fail(run, frame, &nid, "on_timeout", format!("what to decide on could not be worked out: {why}"), queue);
                            return;
                        }
                    };
                    let digest = canonical::digest(MATERIAL_SCHEMA, &digestible(&material)).unwrap_or_default();
                    run.waiting.insert(act.clone(), Waiting::Task { frame: frame.into(), node: nid, material: material.clone(), digest: digest.clone() });
                    out.push(Command::OpenTask {
                        activation: act,
                        label: label.clone(),
                        role: role.clone(),
                        material,
                        digest,
                        form: form.clone(),
                        approvals: *approvals,
                        delegation: *delegation,
                        exclude: run.requester.iter().cloned().collect(),
                        timeout: timeout.clone(),
                    });
                    return;
                }
                Node::WaitEvent { event, correlation, timeout, .. } => match self.eval(run, frame, correlation) {
                    Ok(v) => {
                        run.waiting.insert(act.clone(), Waiting::Wait { frame: frame.into(), node: nid });
                        out.push(Command::Wait { activation: act, event: event.clone(), correlation: v.to_json(), timeout: timeout.clone() });
                        return;
                    }
                    Err(why) => {
                        self.fail(run, frame, &nid, "on_timeout", format!("which event to wait for could not be worked out: {why}"), queue);
                        return;
                    }
                },
                Node::Delay { wait, .. } => {
                    run.waiting.insert(act.clone(), Waiting::Sleep { frame: frame.into(), node: nid });
                    out.push(Command::Sleep { activation: act, wait: wait.clone() });
                    return;
                }
                Node::Case { label, owner, tasks, timeout, .. } => {
                    run.waiting.insert(act.clone(), Waiting::Case { frame: frame.into(), node: nid, done: BTreeMap::new() });
                    out.push(Command::OpenCase {
                        activation: act,
                        label: label.clone(),
                        owner: owner.clone(),
                        tasks: tasks.clone(),
                        exclude: run.requester.iter().cloned().collect(),
                        timeout: timeout.clone(),
                    });
                    return;
                }
                Node::Parallel { join, branches, .. } => {
                    run.waiting.insert(
                        act.clone(),
                        Waiting::Parallel {
                            frame: frame.into(),
                            node: nid.clone(),
                            join: *join,
                            ended: BTreeMap::new(),
                            branches: branches.iter().map(|b| b.id.clone()).collect(),
                            winner: None,
                        },
                    );
                    for b in branches {
                        let mut block = f.block.clone();
                        block.extend([nid.clone(), b.id.clone()]);
                        let child = Frame {
                            process: f.process.clone(),
                            block,
                            at: b.entry.clone(),
                            parent: Some(frame.into()),
                            trigger: None,
                            item: None,
                            cancelled: false,
                        };
                        self.spawn(run, format!("{act}.{}", b.id), child, queue);
                    }
                    return;
                }
                Node::ForEach { items, key, max_items, concurrency, body, .. } => {
                    let list = match self.eval(run, frame, items) {
                        Ok(Val::List(list)) => list,
                        Ok(other) => {
                            self.fail(run, frame, &nid, "on_error", format!("the items are {}, not a list", other.to_json()), queue);
                            return;
                        }
                        Err(why) => {
                            self.fail(run, frame, &nid, "on_error", why, queue);
                            return;
                        }
                    };
                    if list.len() > *max_items as usize {
                        self.fail(run, frame, &nid, "on_error", format!("{} items, more than its {max_items}", list.len()), queue);
                        return;
                    }
                    let mut keys = vec![];
                    let mut values = vec![];
                    for v in &list {
                        let item = v.to_json();
                        let k = match self.eval_item(run, frame, &nid, &item, key) {
                            Ok(Val::Text(s)) => s,
                            Ok(Val::Int(i)) => i.to_string(),
                            Ok(other) => {
                                self.fail(run, frame, &nid, "on_error", format!("an item's key came out as {}", other.to_json()), queue);
                                return;
                            }
                            Err(why) => {
                                self.fail(run, frame, &nid, "on_error", format!("an item's key could not be worked out: {why}"), queue);
                                return;
                            }
                        };
                        if keys.contains(&k) {
                            self.fail(run, frame, &nid, "on_error", format!("two items share the key {k}"), queue);
                            return;
                        }
                        keys.push(k);
                        values.push(item);
                    }
                    if keys.is_empty() {
                        run.outputs.insert(act, json!([]));
                        self.route(run, frame, &nid, "next", queue);
                        return;
                    }
                    let mut w = Waiting::ForEach {
                        frame: frame.into(),
                        node: nid.clone(),
                        keys,
                        items: values,
                        next: 0,
                        concurrency: *concurrency as usize,
                        running: BTreeSet::new(),
                        results: BTreeMap::new(),
                        failed: false,
                    };
                    self.more_items(run, &act, frame, &nid, &body.id, &body.entry, &mut w, queue);
                    run.waiting.insert(act, w);
                    return;
                }
                Node::Repeat { max_rounds, body, .. } => {
                    run.waiting.insert(act.clone(), Waiting::Repeat { frame: frame.into(), node: nid.clone(), round: 1, max: *max_rounds });
                    let mut block = f.block.clone();
                    block.extend([nid.clone(), body.id.clone()]);
                    let child = Frame {
                        process: f.process.clone(),
                        block,
                        at: body.entry.clone(),
                        parent: Some(frame.into()),
                        trigger: None,
                        item: None,
                        cancelled: false,
                    };
                    self.spawn(run, format!("{act}#1"), child, queue);
                    return;
                }
                Node::Subprocess { process, input, .. } => {
                    let Some(called) = self.processes.get(process.as_str()) else {
                        self.fail(run, frame, &nid, "on_error", format!("{process} is not in this release"), queue);
                        return;
                    };
                    match self.input(run, frame, input) {
                        Ok(trigger) => {
                            run.waiting.insert(act.clone(), Waiting::Subprocess { frame: frame.into(), node: nid });
                            let child = Frame {
                                process: process.clone(),
                                block: vec![],
                                at: called.entry.clone(),
                                parent: Some(frame.into()),
                                trigger: Some(trigger),
                                item: None,
                                cancelled: false,
                            };
                            self.spawn(run, format!("{act}>"), child, queue);
                        }
                        Err(why) => self.fail(run, frame, &nid, "on_error", why, queue),
                    }
                    return;
                }
                Node::Compensate { .. } => {
                    let queue_of: Vec<usize> =
                        (0..run.effects.len()).rev().filter(|&i| run.effects[i].compensation.is_some() && !run.effects[i].compensated).collect();
                    let mut w = Waiting::Compensate { frame: frame.into(), node: nid.clone(), queue: queue_of, done: 0, failed: 0 };
                    if self.next_undo(run, &act, &mut w, out) {
                        run.waiting.insert(act, w);
                    } else {
                        run.outputs.insert(act, json!({"compensated": 0, "failed": 0}));
                        self.route(run, frame, &nid, "next", queue);
                    }
                    return;
                }
                Node::End { outcome, output, .. } => {
                    let mut handed = Map::new();
                    for (name, src) in output {
                        handed.insert(name.clone(), self.eval(run, frame, src).map(|v| v.to_json()).unwrap_or(Value::Null));
                    }
                    let finished = match outcome {
                        EndOutcome::Completed => Finished::Completed,
                        EndOutcome::Refused => Finished::Refused,
                        // A repeat's body asking for another round; anywhere else the compiler refused it.
                        EndOutcome::Again => Finished::Cancelled,
                    };
                    run.outputs.insert(act, Value::Object(handed.clone()));
                    let again = *outcome == EndOutcome::Again;
                    self.end_frame(run, frame, finished, Value::Object(handed), again, queue, out);
                    return;
                }
                Node::Suspend { reason, resume_to, .. } => {
                    run.waiting.insert(act.clone(), Waiting::Suspended { frame: frame.into(), node: nid });
                    out.push(Command::Suspend { activation: act, reason: reason.clone(), resumable: resume_to.is_some() });
                    return;
                }
            }
        }
    }

    /// The activation of a step visible from `frame`, by its id.
    fn visible_activation(&self, run: &Run, frame: &str, step: &str) -> Option<String> {
        let mut here = Some(frame.to_string());
        while let Some(name) = here {
            let f = run.frames.get(&name)?;
            let a = activation(&name, step);
            if run.outputs.contains_key(&a) {
                return Some(a);
            }
            if f.trigger.is_some() {
                return None;
            }
            here = f.parent.clone();
        }
        None
    }

    /// Starts items while fewer than the concurrency run and none has failed.
    #[allow(clippy::too_many_arguments)]
    fn more_items(&self, run: &mut Run, act: &str, holder: &str, node: &str, body_id: &str, entry: &str, w: &mut Waiting, queue: &mut VecDeque<String>) {
        let Waiting::ForEach { keys, items, next, concurrency, running, failed, .. } = w else { return };
        let Some(h) = run.frames.get(holder).cloned() else { return };
        while !*failed && running.len() < *concurrency && *next < keys.len() {
            let key = keys[*next].clone();
            let mut block = h.block.clone();
            block.extend([node.to_string(), body_id.to_string()]);
            let child = Frame {
                process: h.process.clone(),
                block,
                at: entry.to_string(),
                parent: Some(holder.to_string()),
                trigger: None,
                item: Some(Item { node: node.to_string(), key: key.clone(), value: items[*next].clone() }),
                cancelled: false,
            };
            self.spawn(run, format!("{act}[{}]", path_key(&key)), child, queue);
            running.insert(key);
            *next += 1;
        }
    }

    /// Sends the next compensation; false when none is left.
    fn next_undo(&self, run: &mut Run, act: &str, w: &mut Waiting, out: &mut Vec<Command>) -> bool {
        let Waiting::Compensate { frame, node, queue, .. } = w else { return false };
        let Some(idx) = queue.first().copied() else { return false };
        queue.remove(0);
        let e = &run.effects[idx];
        let Some(undo) = e.compensation.clone() else { return false };
        let version = self.lock.get(undo.as_str()).map(|l| l.version.clone()).unwrap_or_default();
        // The same record the effect changed, when it named one; otherwise what the effect was asked.
        let input = match e.input.get("id") {
            Some(id) => json!({"id": id}),
            None => e.input.clone(),
        };
        let a = format!("{act}/{}", e.activation);
        run.waiting.insert(a.clone(), Waiting::Invoke { frame: frame.clone(), node: node.clone(), input: input.clone(), undoing: Some(idx) });
        out.push(Command::Invoke { activation: a, capability: undo, version, input, changes: true, approval: None });
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn invoked(&self, run: &mut Run, frame: &str, node: &str, act: &str, input: Value, outcome: Invoked, queue: &mut VecDeque<String>, out: &mut Vec<Command>) {
        let unwanted = run.frames.get(frame).is_none_or(|f| f.cancelled) || matches!(run.status, Status::Cancelling);
        let step = self.node(run, frame, node);
        let action = matches!(step, Some(Node::Action { .. }));
        if let (Invoked::Done { output }, Some(Node::Action { capability, .. })) = (&outcome, step) {
            run.effects.push(Effect {
                activation: act.to_string(),
                capability: capability.id.clone(),
                input,
                output: output.clone(),
                compensation: self.lock.get(capability.id.as_str()).and_then(|l| l.compensation.clone()),
                compensated: false,
                unwanted,
            });
        }
        if unwanted && !action {
            self.maybe_cancelled(run, out);
            return;
        }
        if unwanted {
            match &outcome {
                Invoked::Done { output } => out.push(Command::Attention {
                    activation: act.to_string(),
                    why: "an effect settled after it was no longer wanted: decide what to do with it".into(),
                    evidence: output.clone(),
                }),
                Invoked::Unknown { why } => out.push(Command::Attention {
                    activation: act.to_string(),
                    why: format!("an effect no longer wanted may have happened: {why}"),
                    evidence: json!({}),
                }),
                Invoked::Refused { .. } => {}
            }
            self.maybe_cancelled(run, out);
            return;
        }
        match outcome {
            Invoked::Done { output } => {
                run.outputs.insert(act.to_string(), output);
                self.route(run, frame, node, "next", queue);
            }
            Invoked::Refused { code, message } => {
                run.outputs.insert(act.to_string(), json!({"error": {"code": code, "message": message}}));
                self.route(run, frame, node, "on_error", queue);
            }
            Invoked::Unknown { why } => {
                run.outputs.insert(act.to_string(), json!({"unknown": why}));
                let route = if matches!(step, Some(Node::Action { on_unknown: Some(_), .. })) { "on_unknown" } else { "on_error" };
                self.route(run, frame, node, route, queue);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn undone(&self, run: &mut Run, frame: &str, node: &str, idx: usize, act: &str, outcome: Invoked, queue: &mut VecDeque<String>, out: &mut Vec<Command>) {
        let holder = activation(frame, node);
        let Some(mut w) = run.waiting.remove(&holder) else { return };
        if let Waiting::Compensate { done, failed, .. } = &mut w {
            match outcome {
                Invoked::Done { .. } => {
                    run.effects[idx].compensated = true;
                    *done += 1;
                }
                Invoked::Refused { message, .. } => {
                    *failed += 1;
                    out.push(Command::Attention {
                        activation: act.to_string(),
                        why: format!("undoing {} was refused, and stays to do: {message}", run.effects[idx].activation),
                        evidence: run.effects[idx].output.clone(),
                    });
                }
                Invoked::Unknown { why } => {
                    *failed += 1;
                    out.push(Command::Attention {
                        activation: act.to_string(),
                        why: format!("undoing {} may or may not have happened: {why}", run.effects[idx].activation),
                        evidence: run.effects[idx].output.clone(),
                    });
                }
            }
        }
        if matches!(run.status, Status::Cancelling) {
            self.maybe_cancelled(run, out);
            return;
        }
        if self.next_undo(run, &holder, &mut w, out) {
            run.waiting.insert(holder, w);
            return;
        }
        if let Waiting::Compensate { done, failed, .. } = w {
            run.outputs.insert(holder, json!({"compensated": done, "failed": failed}));
            self.route(run, frame, node, if failed > 0 { "on_error" } else { "next" }, queue);
        }
    }

    /// A frame reached an end: the run finishes, or the structured step that started it hears.
    #[allow(clippy::too_many_arguments)]
    fn end_frame(&self, run: &mut Run, frame: &str, outcome: Finished, output: Value, again: bool, queue: &mut VecDeque<String>, out: &mut Vec<Command>) {
        if frame.is_empty() {
            run.status = Status::Finished { outcome, output: output.clone() };
            out.push(Command::Finish { outcome, output });
            return;
        }
        let Some(f) = run.frames.get(frame).cloned() else { return };
        let Some(parent) = f.parent.clone() else { return };
        // The child's path is the holder's activation and one more piece: `.branch`, `[key]`, `#round` or `>`.
        let holder = holder_of(frame);
        let Some(mut w) = run.waiting.remove(&holder) else { return };
        let node = w.node().to_string();
        match &mut w {
            Waiting::Parallel { join, ended, branches, winner, .. } => {
                let branch = frame.rsplit_once('.').map(|(_, b)| b.to_string()).unwrap_or_default();
                ended.insert(branch.clone(), (outcome, output));
                let settled = branches.iter().all(|b| ended.contains_key(b));
                match join {
                    Join::Any if outcome == Finished::Completed && winner.is_none() => {
                        *winner = Some(branch.clone());
                        for other in branches.iter().filter(|b| **b != branch && !ended.contains_key(*b)) {
                            self.cancel_frame(run, &format!("{holder}.{other}"), out);
                        }
                        let result = json!({"winner": branch, "branches": {branch.clone(): ended[&branch].1.clone()}});
                        run.outputs.insert(holder.clone(), result);
                        self.route(run, &parent, &node, "next", queue);
                        return;
                    }
                    Join::Any if settled && winner.is_none() => {
                        run.outputs.insert(holder.clone(), json!({"winner": null, "branches": outputs_of(ended)}));
                        self.route(run, &parent, &node, "on_error", queue);
                        return;
                    }
                    Join::All if settled => {
                        let refused = ended.values().any(|(o, _)| *o != Finished::Completed);
                        run.outputs.insert(holder.clone(), json!({"winner": null, "branches": outputs_of(ended)}));
                        self.route(run, &parent, &node, if refused { "on_error" } else { "next" }, queue);
                        return;
                    }
                    _ => {}
                }
            }
            Waiting::ForEach { keys, running, results, failed, next, .. } => {
                let key = f.item.as_ref().map(|i| i.key.clone()).unwrap_or_default();
                running.remove(&key);
                if outcome != Finished::Completed {
                    *failed = true;
                }
                results.insert(key, (outcome, output));
                if running.is_empty() && (*failed || *next >= keys.len()) {
                    let list: Vec<Value> =
                        keys.iter().filter_map(|k| results.get(k).map(|(o, v)| json!({"key": k, "outcome": finished_name(*o), "output": v}))).collect();
                    let failed = *failed;
                    run.outputs.insert(holder.clone(), Value::Array(list));
                    self.route(run, &parent, &node, if failed { "on_error" } else { "next" }, queue);
                    return;
                }
                if let Some(Node::ForEach { body, .. }) = self.node(run, &parent, &node) {
                    self.more_items(run, &holder, &parent, &node, &body.id, &body.entry, &mut w, queue);
                }
            }
            Waiting::Repeat { round, max, .. } => {
                if again && *round < *max {
                    *round += 1;
                    let mut child = f.clone();
                    child.at = match self.node(run, &parent, &node) {
                        Some(Node::Repeat { body, .. }) => body.entry.clone(),
                        _ => return,
                    };
                    self.spawn(run, format!("{holder}#{round}"), child, queue);
                } else {
                    let rounds = *round;
                    run.outputs.insert(holder.clone(), json!({"rounds": rounds, "output": output}));
                    let route = if outcome == Finished::Completed && !again { "next" } else { "on_exhausted" };
                    self.route(run, &parent, &node, route, queue);
                    return;
                }
            }
            Waiting::Subprocess { .. } => {
                run.outputs.insert(holder.clone(), output);
                self.route(run, &parent, &node, if outcome == Finished::Completed { "next" } else { "on_error" }, queue);
                return;
            }
            _ => {}
        }
        run.waiting.insert(holder, w);
    }

    /// A frame ended through a person's recovery: as if it reached a refused end.
    fn ended(&self, run: &mut Run, frame: &str, outcome: Finished, output: Value, queue: &mut VecDeque<String>, out: &mut Vec<Command>) {
        self.end_frame(run, frame, outcome, output, false, queue, out);
    }

    /// Asks a frame and everything in it to stop. Work in hand is kept to settle; waits, tasks and timers end.
    fn cancel_frame(&self, run: &mut Run, frame: &str, out: &mut Vec<Command>) {
        let inside = |name: &str| frame.is_empty() || name == frame || name.starts_with(&format!("{frame}/"));
        for (name, f) in run.frames.iter_mut() {
            if inside(name) {
                f.cancelled = true;
            }
        }
        let held: Vec<String> = run.waiting.iter().filter(|(_, w)| inside(w.frame())).map(|(a, _)| a.clone()).collect();
        for a in held {
            match run.waiting.get(&a) {
                // Work in hand stays awaited: what it did when it settles is recorded, never dropped.
                Some(Waiting::Invoke { .. }) => out.push(Command::Cancel { activation: a }),
                Some(Waiting::Task { .. } | Waiting::Wait { .. } | Waiting::Sleep { .. } | Waiting::Case { .. } | Waiting::Suspended { .. }) => {
                    run.waiting.remove(&a);
                    out.push(Command::Cancel { activation: a });
                }
                Some(_) => {
                    run.waiting.remove(&a);
                }
                None => {}
            }
        }
    }

    fn cancel(&self, run: &mut Run, by: &str, reason: &str, out: &mut Vec<Command>) {
        if matches!(run.status, Status::Cancelling) {
            return;
        }
        run.outputs.insert("cancel".into(), json!({"by": by, "reason": reason}));
        self.cancel_frame(run, "", out);
        run.status = Status::Cancelling;
        self.maybe_cancelled(run, out);
    }

    /// A cancelled run finishes once nothing it sent is still in hand.
    fn maybe_cancelled(&self, run: &mut Run, out: &mut Vec<Command>) {
        if matches!(run.status, Status::Cancelling) && !run.waiting.values().any(|w| matches!(w, Waiting::Invoke { .. })) {
            let output = json!({"cancelled": run.outputs.get("cancel").cloned().unwrap_or(Value::Null)});
            run.status = Status::Finished { outcome: Finished::Cancelled, output: output.clone() };
            out.push(Command::Finish { outcome: Finished::Cancelled, output });
        }
    }
}

impl Run {
    /// The activations the run waits on, with what for.
    pub fn waiting_on(&self) -> Vec<(String, String)> {
        self.waiting
            .iter()
            .map(|(a, w)| {
                let what = match w {
                    Waiting::Invoke { .. } => "invoke",
                    Waiting::Task { .. } => "task",
                    Waiting::Wait { .. } => "event",
                    Waiting::Sleep { .. } => "delay",
                    Waiting::Case { .. } => "case",
                    Waiting::Suspended { .. } => "recovery",
                    Waiting::Parallel { .. } => "branches",
                    Waiting::ForEach { .. } => "items",
                    Waiting::Repeat { .. } => "round",
                    Waiting::Subprocess { .. } => "subprocess",
                    Waiting::Compensate { .. } => "compensation",
                };
                (a.clone(), what.to_string())
            })
            .collect()
    }

    /// The material and its digest a task in hand was opened on.
    pub fn task_material(&self, activation: &str) -> Option<(&Value, &str)> {
        match self.waiting.get(activation) {
            Some(Waiting::Task { material, digest, .. }) => Some((material, digest)),
            _ => None,
        }
    }
}

/// The holder's activation of a child frame: its path without the last piece.
fn holder_of(frame: &str) -> String {
    // Keys are escaped (`path_key`), so the last separator in the path is the holder's.
    frame[..frame.rfind(['.', '[', '#', '>']).unwrap_or(frame.len())].to_string()
}

fn finished_name(f: Finished) -> &'static str {
    match f {
        Finished::Completed => "completed",
        Finished::Refused => "refused",
        Finished::Cancelled => "cancelled",
    }
}

fn outputs_of(ended: &BTreeMap<String, (Finished, Value)>) -> Value {
    Value::Object(ended.iter().map(|(b, (_, v))| (b.clone(), v.clone())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::RuntimeProfile;
    use crate::testkit::{compile_value, purchase, structured};

    fn release(v: Value) -> Release {
        compile_value(v, RuntimeProfile::FullProcess).unwrap_or_else(|d| panic!("{d:#?}"))
    }

    fn done(m: &Machine, run: &mut Run, activation: &str, output: Value) -> Vec<Command> {
        m.on(run, Event::Invoked { activation: activation.into(), outcome: Invoked::Done { output } })
    }

    fn invoked(cmds: &[Command]) -> Vec<(String, String, Value)> {
        cmds.iter()
            .filter_map(|c| match c {
                Command::Invoke { activation, capability, input, .. } => Some((activation.clone(), capability.clone(), input.clone())),
                _ => None,
            })
            .collect()
    }

    fn approve(m: &Machine, run: &mut Run, activation: &str, approved: bool) -> Vec<Command> {
        m.on(
            run,
            Event::TaskDecided {
                activation: activation.into(),
                approved,
                by: "u-rev".into(),
                decision_id: format!("d-{activation}"),
                at: "2026-11-08T10:00:00Z".into(),
                values: json!({}),
            },
        )
    }

    fn snapshot(total: &str) -> Value {
        json!({"total": {"amount": total, "currency": "INR"}, "supplier_email": "orders@kumar.example", "version": 3, "category": "spares"})
    }

    #[test]
    fn the_purchase_example_runs_from_trigger_to_completion() {
        let r = release(purchase());
        let m = Machine::new(&r);
        let (mut run, cmds) = m.start("purchase_exception", "run-1", json!({"id": "MR-1"}), Some("u-req".into())).unwrap();
        assert_eq!(invoked(&cmds), vec![("snapshot".to_string(), "procurement.material_request.read".to_string(), json!({"id": "MR-1"}))]);
        let cmds = done(&m, &mut run, "snapshot", snapshot("150000.00"));
        let Some(Command::OpenTask { activation, role, exclude, digest, material, .. }) = cmds.first() else { panic!("{cmds:?}") };
        assert_eq!((activation.as_str(), role.as_str(), exclude.clone()), ("review", "Purchasing Reviewer", vec!["u-req".to_string()]));
        assert_eq!(digest, &canonical::digest(MATERIAL_SCHEMA, &digestible(material)).unwrap(), "bound to exactly what the reviewer sees");
        let cmds = approve(&m, &mut run, "review", true);
        assert_eq!(invoked(&cmds)[0].0, "create_order");
        let cmds = done(&m, &mut run, "create_order", json!({"order_id": "PO-7"}));
        assert_eq!(
            invoked(&cmds),
            vec![(
                "submit_supplier".to_string(),
                "ferrum.mail".to_string(),
                json!({"to": "orders@kumar.example", "subject": "Purchase order", "body": "PO-7"})
            )]
        );
        let cmds = done(&m, &mut run, "submit_supplier", json!({}));
        assert_eq!(
            cmds,
            vec![Command::Wait {
                activation: "supplier_ack".into(),
                event: "buying.purchase_order.updated".into(),
                correlation: json!("PO-7"),
                timeout: json!(86400)
            }]
        );
        let cmds = m.on(&mut run, Event::Signal { activation: "supplier_ack".into(), payload: json!({"id": "PO-7", "status": "Acknowledged"}) });
        assert!(matches!(cmds.as_slice(), [Command::Finish { outcome: Finished::Completed, .. }]), "{cmds:?}");
        assert_eq!(run.effects.iter().map(|e| e.activation.as_str()).collect::<Vec<_>>(), vec!["create_order", "submit_supplier"]);
        // Under the limit no one is asked.
        let (mut run, _) = m.start("purchase_exception", "run-2", json!({"id": "MR-2"}), None).unwrap();
        assert_eq!(invoked(&done(&m, &mut run, "snapshot", snapshot("5000.00")))[0].0, "create_order");
    }

    #[test]
    fn an_unknown_outcome_waits_for_a_person_and_never_becomes_success() {
        let r = release(purchase());
        let m = Machine::new(&r);
        let (mut run, _) = m.start("purchase_exception", "run-1", json!({"id": "MR-1"}), None).unwrap();
        done(&m, &mut run, "snapshot", snapshot("10.00"));
        let cmds = m.on(&mut run, Event::Invoked { activation: "create_order".into(), outcome: Invoked::Unknown { why: "the connection dropped".into() } });
        assert!(matches!(cmds.as_slice(), [Command::Suspend { activation, resumable: false, .. }] if activation == "needs_attention"), "{cmds:?}");
        assert_eq!(run.status, Status::NeedsAttention);
        // Asking to resume a step its source gives nowhere to resume ends it refused.
        let cmds =
            m.on(&mut run, Event::Recovered { activation: "needs_attention".into(), by: "u-ops".into(), reason: "the order exists".into(), resume: true });
        assert!(matches!(cmds.as_slice(), [Command::Finish { outcome: Finished::Refused, .. }]), "{cmds:?}");
    }

    #[test]
    fn late_and_repeated_events_change_nothing() {
        let r = release(purchase());
        let m = Machine::new(&r);
        let (mut run, _) = m.start("purchase_exception", "run-1", json!({"id": "MR-1"}), None).unwrap();
        done(&m, &mut run, "snapshot", snapshot("150000.00"));
        approve(&m, &mut run, "review", true);
        let before = run.clone();
        assert!(done(&m, &mut run, "snapshot", snapshot("1.00")).is_empty(), "the read came twice");
        assert!(m.on(&mut run, Event::TaskTimedOut { activation: "review".into() }).is_empty(), "the decision won");
        assert!(approve(&m, &mut run, "review", false).is_empty(), "a second decision decides nothing");
        assert_eq!(run, before);
    }

    #[test]
    fn the_same_events_ask_for_the_same_things_and_a_run_survives_its_serialization() {
        let r = release(purchase());
        let m = Machine::new(&r);
        let script = |m: &Machine, cut: bool| {
            let (mut run, mut all) = m.start("purchase_exception", "run-1", json!({"id": "MR-1"}), Some("u".into())).unwrap();
            all.extend(done(m, &mut run, "snapshot", snapshot("150000.00")));
            if cut {
                // A continuation carries the run as data.
                run = serde_json::from_value(serde_json::to_value(&run).unwrap()).unwrap();
            }
            all.extend(approve(m, &mut run, "review", true));
            all.extend(done(m, &mut run, "create_order", json!({"order_id": "PO-1"})));
            (run, all)
        };
        let (a, ca) = script(&m, false);
        let (b, cb) = script(&m, true);
        assert_eq!((a, ca), (b, cb));
    }

    fn lines() -> Value {
        json!({"total": {"amount": "10.00", "currency": "INR"}, "supplier_email": "s@kumar.example", "version": 1, "category": "spares",
            "lines": [{"item_code": "A", "qty": "1"}, {"item_code": "B.2", "qty": "2"}, {"item_code": "C", "qty": "3"}]})
    }

    /// Runs the structured example to its review: holds placed, the portal answered first.
    fn to_review(m: &Machine) -> (Run, Vec<Command>) {
        let (mut run, _) = m.start("purchase_exception", "run-s", json!({"id": "MR-9"}), Some("u-req".into())).unwrap();
        let cmds = done(m, &mut run, "snapshot", lines());
        let holds = invoked(&cmds);
        assert_eq!(
            holds.iter().map(|h| h.0.as_str()).collect::<Vec<_>>(),
            vec!["lines_each[A]/hold_line", "lines_each[B%2E2]/hold_line", "lines_each[C]/hold_line"],
            "one activation per item, named by its key, the key's separators escaped"
        );
        assert_eq!(holds[1].2, json!({"scope_ref": "B.2", "case_ref": "MR-9"}));
        let mut cmds = vec![];
        for (i, (a, _, _)) in holds.iter().enumerate() {
            cmds = done(m, &mut run, a, json!({"hold_id": format!("H-{i}")}));
        }
        let names: Vec<String> = cmds.iter().map(|c| serde_json::to_value(c).unwrap()["activation"].as_str().unwrap_or_default().to_string()).collect();
        assert_eq!(names, vec!["quotes.email/mail", "quotes.portal/wait_portal"], "both branches start: {cmds:?}");
        let cmds = m.on(&mut run, Event::Signal { activation: "quotes.portal/wait_portal".into(), payload: json!({"id": "PO-1", "status": "Acknowledged"}) });
        (run, cmds)
    }

    #[test]
    fn a_run_waiting_inside_a_round_moves_with_its_step_and_a_step_holding_others_keeps_its_id() {
        let r = release(structured());
        let m = Machine::new(&r);
        let (mut run, _) = to_review(&m);
        // The losing branch's mail is still in hand: nothing moves until it settles.
        let mut renamed = structured();
        renamed["version"] = json!("1.1.0");
        let round = &mut renamed["processes"][0]["nodes"][3]["body"];
        round["entry"] = json!("approve");
        round["nodes"][0]["id"] = json!("approve");
        let to = release(renamed);
        let map = BTreeMap::from([("review".to_string(), "approve".to_string())]);
        let held = Machine::new(&to).migrate(&mut run.clone(), &map).unwrap_err();
        assert!(held.iter().any(|w| w.contains("quotes.email/mail has an effect in hand")), "{held:?}");
        done(&m, &mut run, "quotes.email/mail", json!({"queued": true}));
        // Inside the round, the task moves to its new name, with its activation; the round keeps its own.
        let before = run.clone();
        let moved = Machine::new(&to).migrate(&mut run, &map).unwrap();
        assert_eq!(moved.renamed, vec![("review_loop#1/review".to_string(), "review_loop#1/approve".to_string())], "{moved:?}");
        assert!(run.waiting.contains_key("review_loop#1/approve") && !run.waiting.contains_key("review_loop#1/review"));
        let cmds = Machine::new(&to).on(
            &mut run,
            Event::TaskDecided {
                activation: "review_loop#1/approve".into(),
                approved: true,
                by: "u-rev".into(),
                decision_id: "d-1".into(),
                at: "2026-11-08T10:00:00Z".into(),
                values: json!({}),
            },
        );
        assert!(matches!(cmds.as_slice(), [Command::OpenCase { activation, .. }] if activation == "investigate"), "the round ends approved: {cmds:?}");
        // Renaming the step that holds the round, or a release without the round, keeps the run as it was.
        let mut other = before.clone();
        let refused = Machine::new(&to).migrate(&mut other, &BTreeMap::from([("review_loop".to_string(), "rounds".to_string())])).unwrap_err();
        assert!(refused.iter().any(|w| w.contains("review_loop holds other steps, so it keeps its id")), "{refused:?}");
        assert_eq!(other.waiting.keys().collect::<Vec<_>>(), before.waiting.keys().collect::<Vec<_>>(), "nothing changed");
        let mut flat = structured();
        flat["version"] = json!("1.2.0");
        flat["processes"][0]["nodes"][3] = json!({"type": "human_task", "id": "review_loop", "label": "One review", "role": "Purchasing Reviewer", "material": "snapshot.output",
            "timeout": {"business_days": 2, "calendar": "india"}, "next": "investigate", "on_reject": "undo", "on_timeout": "undo"});
        flat["processes"][0]["nodes"][7]["output"] = json!({"lines": "lines_each.output"});
        let flat = release(flat);
        let refused = Machine::new(&flat).migrate(&mut before.clone(), &BTreeMap::new()).unwrap_err();
        assert!(refused.iter().any(|w| w.contains("review_loop#1 is inside review_loop/round, which 1.2.0 does not have as it was")), "{refused:?}");
    }

    #[test]
    fn the_first_branch_wins_and_a_loser_that_settles_late_is_kept_for_a_person() {
        let r = release(structured());
        let m = Machine::new(&r);
        let (mut run, cmds) = to_review(&m);
        assert!(cmds.contains(&Command::Cancel { activation: "quotes.email/mail".into() }), "the losing branch is asked to stop: {cmds:?}");
        assert!(cmds.iter().any(|c| matches!(c, Command::OpenTask { activation, .. } if activation == "review_loop#1/review")), "{cmds:?}");
        assert_eq!(run.outputs["quotes"]["winner"], json!("portal"));
        let late = done(&m, &mut run, "quotes.email/mail", json!({"queued": true}));
        assert!(matches!(late.as_slice(), [Command::Attention { activation, .. }] if activation == "quotes.email/mail"), "{late:?}");
        assert!(run.effects.iter().any(|e| e.activation == "quotes.email/mail" && e.unwanted), "recorded, not erased");
    }

    #[test]
    fn rework_runs_rounds_and_a_case_closes_only_when_its_required_tasks_are_done() {
        let r = release(structured());
        let m = Machine::new(&r);
        let (mut run, _) = to_review(&m);
        let cmds = approve(&m, &mut run, "review_loop#1/review", false);
        assert!(cmds.iter().any(|c| matches!(c, Command::OpenTask { activation, .. } if activation == "review_loop#2/review")), "{cmds:?}");
        let cmds = approve(&m, &mut run, "review_loop#2/review", true);
        assert!(matches!(cmds.as_slice(), [Command::OpenCase { activation, .. }] if activation == "investigate"), "{cmds:?}");
        assert!(m.on(&mut run, Event::CaseClosed { activation: "investigate".into(), done: json!({}) }).is_empty(), "inspect is required and not done");
        // The close carries what the record says was done, so it closes even ahead of the task's own delivery.
        let cmds = m.on(&mut run, Event::CaseClosed { activation: "investigate".into(), done: json!({"inspect": {"passed": true, "decision": "done"}}) });
        assert_eq!(cmds, vec![Command::Sleep { activation: "settle".into(), wait: json!({"business_days": 1, "calendar": "india"}) }]);
        let cmds = m.on(&mut run, Event::Woke { activation: "settle".into(), at: "2026-11-10T10:00:00Z".into() });
        let Some(Command::Finish { outcome: Finished::Completed, output }) = cmds.first() else { panic!("{cmds:?}") };
        assert_eq!(run.outputs["investigate"]["inspect"]["passed"], json!(true));
        assert_eq!(output["rounds"], json!(2));
        assert_eq!(output["lines"].as_array().unwrap().iter().map(|l| l["key"].as_str().unwrap()).collect::<Vec<_>>(), vec!["A", "B.2", "C"]);
    }

    #[test]
    fn running_out_of_rounds_compensates_newest_first_and_a_failed_undo_stays_open() {
        let r = release(structured());
        let m = Machine::new(&r);
        let (mut run, _) = to_review(&m);
        approve(&m, &mut run, "review_loop#1/review", false);
        approve(&m, &mut run, "review_loop#2/review", false);
        let cmds = approve(&m, &mut run, "review_loop#3/review", false);
        let undo = invoked(&cmds);
        assert_eq!(
            undo,
            vec![("undo/lines_each[C]/hold_line".to_string(), "example.inventory.release_hold".to_string(), json!({"scope_ref": "C", "case_ref": "MR-9"}))],
            "newest first, one at a time"
        );
        let cmds = m.on(
            &mut run,
            Event::Invoked {
                activation: "undo/lines_each[C]/hold_line".into(),
                outcome: Invoked::Refused { code: "conflict".into(), message: "already shipped".into() },
            },
        );
        assert!(matches!(cmds.first(), Some(Command::Attention { .. })), "{cmds:?}");
        assert_eq!(invoked(&cmds)[0].0, "undo/lines_each[B%2E2]/hold_line");
        done(&m, &mut run, "undo/lines_each[B%2E2]/hold_line", json!({}));
        let cmds = done(&m, &mut run, "undo/lines_each[A]/hold_line", json!({}));
        assert!(matches!(cmds.as_slice(), [Command::Suspend { activation, resumable: true, .. }] if activation == "attention"), "one failed: {cmds:?}");
        // A person settles it and resumes: only what is still open is tried again.
        let cmds = m.on(&mut run, Event::Recovered { activation: "attention".into(), by: "u-ops".into(), reason: "returned to stock".into(), resume: true });
        assert_eq!(invoked(&cmds).iter().map(|i| i.0.as_str()).collect::<Vec<_>>(), vec!["undo/lines_each[C]/hold_line"]);
        let cmds = done(&m, &mut run, "undo/lines_each[C]/hold_line", json!({}));
        assert!(matches!(cmds.as_slice(), [Command::Finish { outcome: Finished::Refused, .. }]), "{cmds:?}");
        assert!(run.effects.iter().filter(|e| e.activation.starts_with("lines_each")).all(|e| e.compensated));
    }

    #[test]
    fn a_cancelled_run_keeps_what_was_in_hand_and_finishes_when_it_settles() {
        let r = release(structured());
        let m = Machine::new(&r);
        let (mut run, _) = m.start("purchase_exception", "run-c", json!({"id": "MR-3"}), None).unwrap();
        done(&m, &mut run, "snapshot", lines());
        let cmds = m.on(&mut run, Event::CancelRequested { by: "u-boss".into(), reason: "the request was withdrawn".into() });
        assert_eq!(cmds.iter().filter(|c| matches!(c, Command::Cancel { .. })).count(), 3, "{cmds:?}");
        assert_eq!(run.status, Status::Cancelling);
        let cmds = done(&m, &mut run, "lines_each[A]/hold_line", json!({"hold_id": "H-1"}));
        assert!(matches!(cmds.as_slice(), [Command::Attention { .. }]), "it happened anyway, and a person hears of it: {cmds:?}");
        m.on(
            &mut run,
            Event::Invoked {
                activation: "lines_each[B%2E2]/hold_line".into(),
                outcome: Invoked::Refused { code: "cancelled".into(), message: "stopped".into() },
            },
        );
        let cmds = m.on(
            &mut run,
            Event::Invoked { activation: "lines_each[C]/hold_line".into(), outcome: Invoked::Refused { code: "cancelled".into(), message: "stopped".into() } },
        );
        assert!(matches!(cmds.as_slice(), [Command::Finish { outcome: Finished::Cancelled, .. }]), "{cmds:?}");
        assert_eq!(run.effects.len(), 1);
    }

    #[test]
    fn items_need_distinct_keys_and_a_run_stops_at_its_step_budget() {
        let r = release(structured());
        let m = Machine::new(&r);
        let (mut run, _) = m.start("purchase_exception", "run-d", json!({"id": "MR-4"}), None).unwrap();
        let mut twice = lines();
        twice["lines"][2]["item_code"] = json!("A");
        let cmds = done(&m, &mut run, "snapshot", twice);
        assert!(run.outputs["lines_each"]["error"].as_str().unwrap().contains("two items share the key A"), "{:?}", run.outputs);
        assert!(matches!(cmds.as_slice(), [Command::Finish { outcome: Finished::Refused, .. }]), "{cmds:?}");
        // Reaching the review takes 14 steps (one per step run, each item's and branch's included); the first
        // round of rework is the 15th.
        let mut tight = structured();
        tight["processes"][0]["max_steps"] = json!(14);
        let r = release(tight);
        let m = Machine::new(&r);
        let (mut run, cmds) = to_review(&m);
        assert!(cmds.iter().any(|c| matches!(c, Command::OpenTask { .. })), "{cmds:?}");
        let cmds = approve(&m, &mut run, "review_loop#1/review", false);
        assert!(cmds.iter().any(|c| matches!(c, Command::Suspend { reason, resumable: false, .. } if reason.contains("14 steps"))), "{cmds:?}");
        assert_eq!(run.status, Status::NeedsAttention);
    }

    #[test]
    fn an_effect_relying_on_an_approval_carries_it() {
        let mut v = purchase();
        v["processes"][0]["nodes"][1]["routes"] = json!([{"outcome": "review", "target": "review"}, {"outcome": "proceed", "target": "review"}]);
        v["processes"][0]["nodes"][3]["approval"] = json!("review");
        let r = release(v);
        let m = Machine::new(&r);
        let (mut run, _) = m.start("purchase_exception", "run-a", json!({"id": "MR-5"}), Some("u-req".into())).unwrap();
        done(&m, &mut run, "snapshot", snapshot("150000.00"));
        let cmds = approve(&m, &mut run, "review", true);
        let Some(Command::Invoke { approval: Some(a), .. }) = cmds.first() else { panic!("{cmds:?}") };
        assert_eq!((a.task.as_str(), a.decision_id.as_str()), ("review", "d-review"));
        // The material is the read as its type has it: every field the reviewer could see, absent ones null.
        assert_eq!((a.material["total"].clone(), a.material["lines"].clone()), (snapshot("150000.00")["total"].clone(), Value::Null));
        assert_eq!(a.digest, canonical::digest(MATERIAL_SCHEMA, &digestible(&a.material)).unwrap());
        // An approval that does not surely come first is refused when compiled.
        let mut v = purchase();
        v["processes"][0]["nodes"][3]["approval"] = json!("review");
        assert!(compile_value(v, RuntimeProfile::FullProcess).unwrap_err().iter().any(|d| d.message.contains("relies on review")));
    }
}
