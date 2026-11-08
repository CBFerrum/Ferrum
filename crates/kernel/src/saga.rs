//! Workflows that cross systems (docs/events.md, "What may happen where"): steps that each do one
//! thing outside, in order, and know how to undo it. No transaction is held across them. A workflow is
//! a durable job (jobs.rs): it survives restarts, runs on any replica, and after each step records how
//! far it got, so a retry resumes there rather than doing a step twice where it can help it.
//!
//! Each step runs at least once: [`Saga::key`] is the same every time a step runs for a workflow, for
//! the other system to recognise a repeat (an `Idempotency-Key`). A step that fails for good, one
//! that keeps failing past its tries, or a person asking it to stop ([`crate::Kernel::cancel_job`])
//! turns the workflow around: the steps done are undone, newest first, each at least once too, and it
//! ends `failed` (or `cancelled`) saying what was undone. An undo that fails for good, or keeps
//! failing, stops it there, saying what a person must still undo.

use crate::error::{Error, Result};
use crate::jobs::{Job, JobHandler, NewJob};
use crate::{Kernel, db::Tx};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

/// How many tries a workflow has: forward ones first, then what is left for undoing.
const ATTEMPTS: i32 = 25;
const FORWARD_ATTEMPTS: i32 = 10;

/// What a step is given: the workflow's input, and what the steps before it gave back.
pub struct StepCx<'a> {
    pub tenant: Uuid,
    pub workflow: Uuid,
    pub input: &'a Value,
    pub done: &'a [Done],
    pub key: String,
}

impl StepCx<'_> {
    /// What an earlier step gave back.
    pub fn output(&self, step: &str) -> Option<&Value> {
        self.done.iter().find(|d| d.step == step).map(|d| &d.output)
    }
}

/// One step: something done in another system, and how to undo it.
#[async_trait]
pub trait Step: Send + Sync {
    fn name(&self) -> &'static str;

    /// Does it, at least once; `cx.key` is the same each time. What it returns is kept for later
    /// steps and for undoing it. An error that says it cannot succeed (invalid, forbidden, not
    /// found) turns the workflow around at once; any other is tried again.
    async fn run(&self, k: &Kernel, cx: &StepCx<'_>) -> Result<Value>;

    /// Undoes what `run` did, given what it returned, at least once. Nothing to undo by default (a
    /// read, a message that cannot be unsent).
    async fn undo(&self, _k: &Kernel, _cx: &StepCx<'_>, _done: &Value) -> Result<()> {
        Ok(())
    }
}

/// A step done, and what it gave back.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Done {
    pub step: String,
    pub output: Value,
}

/// How far a workflow got, as its job's progress says.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Progress {
    #[serde(default)]
    done: Vec<Done>,
    /// Why it turned around: `{step, error}`, or `{cancelled: true}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    undoing: Option<Value>,
    #[serde(default)]
    undone: Vec<String>,
}

/// A workflow of steps, registered as the job handler of its kind
/// ([`crate::HookRegistry::add_job_handler`]) and started with [`Saga::start`].
pub struct Saga {
    kind: &'static str,
    title: &'static str,
    steps: Vec<Arc<dyn Step>>,
}

fn final_error(e: &Error) -> bool {
    matches!(e, Error::Invalid(_) | Error::Validation(_) | Error::Forbidden(_) | Error::NotFound(_) | Error::Unauthorized(_) | Error::Refused { .. })
}

impl Saga {
    pub fn new(kind: &'static str, title: &'static str, steps: Vec<Arc<dyn Step>>) -> Saga {
        Saga { kind, title, steps }
    }

    /// Queues a workflow of `kind` in the caller's transaction, as `created_by`: it starts only if
    /// that commits. `dedupe` returns the one already under way with that key instead of a second.
    pub async fn start(tx: &mut Tx, tenant: Uuid, created_by: Option<Uuid>, kind: &str, input: Value, dedupe: Option<String>) -> Result<Uuid> {
        let job = NewJob { max_attempts: ATTEMPTS, dedupe_key: dedupe, undo_on_cancel: true, ..NewJob::new(kind, input) };
        crate::jobs::enqueue(tx, tenant, created_by, job).await
    }

    /// The key a step of a workflow runs under, the same every time.
    pub fn key(workflow: Uuid, step: &str) -> String {
        format!("ferrum-saga-{workflow}-{step}")
    }

    async fn save(k: &Kernel, job: &Job, p: &Progress) -> Result<()> {
        let value = serde_json::to_value(p).map_err(|e| Error::Internal(e.to_string()))?;
        // Another worker holds it now (this one's lease ran out): it goes on from what was saved.
        if job.progress(k, value).await? { Ok(()) } else { Err(Error::Retryable("another worker took this workflow over".into())) }
    }

    fn cx<'a>(&self, job: &'a Job, done: &'a [Done], step: &str) -> StepCx<'a> {
        StepCx { tenant: job.tenant_id, workflow: job.id, input: &job.payload, done, key: Saga::key(job.id, step) }
    }
}

#[async_trait]
impl JobHandler for Saga {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn title(&self) -> &'static str {
        self.title
    }

    async fn run(&self, k: &Kernel, job: &Job) -> Result<Value> {
        let mut p: Progress = serde_json::from_value(job.resumed.clone()).unwrap_or_default();
        if p.undoing.is_none() {
            while p.done.len() < self.steps.len() {
                if job.cancel_requested(k).await? {
                    p.undoing = Some(json!({"cancelled": true}));
                    Saga::save(k, job, &p).await?;
                    break;
                }
                let step = &self.steps[p.done.len()];
                let ran = step.run(k, &self.cx(job, &p.done, step.name())).await;
                match ran {
                    Ok(output) => {
                        p.done.push(Done { step: step.name().to_string(), output });
                        Saga::save(k, job, &p).await?;
                    }
                    Err(e) if final_error(&e) || job.attempts >= FORWARD_ATTEMPTS => {
                        tracing::warn!(workflow = %job.id, kind = self.kind, step = step.name(), "a workflow turns around: {e}");
                        p.undoing = Some(json!({"step": step.name(), "error": e.to_string()}));
                        Saga::save(k, job, &p).await?;
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
            if p.undoing.is_none() {
                return Ok(json!({"done": p.done}));
            }
        }
        let why = p.undoing.clone().unwrap_or_default();
        let why = match why.get("step").and_then(Value::as_str) {
            Some(step) => format!("{step} failed ({})", why["error"].as_str().unwrap_or_default()),
            None => "it was asked to stop".to_string(),
        };
        // Newest first; each step at most once, however often this is resumed.
        for i in (0..p.done.len()).rev() {
            let step = &self.steps[i];
            if p.undone.iter().any(|u| u == step.name()) {
                continue;
            }
            let done = p.done[i].output.clone();
            match step.undo(k, &self.cx(job, &p.done, step.name()), &done).await {
                Ok(()) => {
                    p.undone.push(step.name().to_string());
                    Saga::save(k, job, &p).await?;
                }
                Err(e) if final_error(&e) || job.attempts >= job.max_attempts => {
                    let left: Vec<&str> = p.done[..=i].iter().map(|d| d.step.as_str()).filter(|s| !p.undone.iter().any(|u| u == s)).collect();
                    return Err(Error::Invalid(format!(
                        "{} stopped because {why}, and undoing {} failed ({e}): a person must still undo {}",
                        self.title,
                        step.name(),
                        left.join(", ")
                    )));
                }
                Err(e) => return Err(e),
            }
        }
        let undone = if p.undone.is_empty() { "nothing had been done".to_string() } else { format!("undone: {}", p.undone.join(", ")) };
        Err(Error::Invalid(format!("{} stopped because {why}; {undone}", self.title)))
    }
}
