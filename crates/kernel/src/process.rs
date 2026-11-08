//! Durable processes' business records, and the activities a runtime calls (docs/logic/processes.md).
//!
//! The runtime (`crates/process`, on Temporal) keeps each run's schedule and drives
//! [`ferrum_logic::process::Machine`]. What is the business's lives here: which runs were accepted, on which
//! release, what people were asked and what they decided, what runs wait for, the signals that came for them,
//! and the workspace's calendars. A person's decision is settled here first and then delivered to the run, so a
//! decision and a timeout racing have one outcome (FLC-HUMAN-003), and a delivery lost on the way is found again
//! when the runtime next asks.
//!
//! * **Starts**: an event an active release's process listens for is accepted once per business key, on the
//!   release active as it is accepted, with the generation of that activation, and its start is queued with it
//!   (FLC-PROC-001, FLC-MIG-003). Once a solution imported from an automation runs, each of the automation's
//!   events is claimed once, by the solution or by the automation's runner, never run by both.
//! * **Effects**: each invocation goes through the capability gate under the effect key `{run}/{activation}`,
//!   with the requester's rights as they are when it runs (FLC-PROC-006); an effect that relies on an approval
//!   is refused if what was approved has changed since (FLC-HUMAN-002).
//! * **Signals**: an event a run waits for is matched by the record it is about; one that comes first is kept
//!   until the run waits for it (FLC-PROC-004), and nothing about another record or workspace can match
//!   (FLC-PROC-005).

use crate::Kernel;
use crate::ctx::Ctx;
use crate::db::Tx;
use crate::error::{Error, Result};
use crate::hooks::EventSink;
use crate::outbox::{self, Event as OutboxEvent};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use ferrum_logic::calendar::{self, Calendar, Deadline, Wait};
use ferrum_logic::capability::snake;
use ferrum_logic::compile::{Release, event_type, parameter_type};
use ferrum_logic::expr::{self, Env, Facts, Ty, Val};
use ferrum_logic::process::{Event, Invoked, Run};
use ferrum_logic::refusal::{Code, Refusal};
use ferrum_logic::solution::Node;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, LazyLock};
use uuid::Uuid;

/// A run accepted and waiting for its runtime to start it.
pub const START_TOPIC: &str = "ferrum.logic.start";
/// Something that happened for a run (a decision, a signal, a recovery), for its runtime to hand it.
pub const DELIVER_TOPIC: &str = "ferrum.logic.deliver";
/// How long a signal that came before anything waited for it is kept.
const EARLY_SIGNAL_DAYS: i64 = 7;

fn require_admin(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("only an administrator may do this to a run".into())) }
}

fn refusal(code: Code, message: impl Into<String>) -> Error {
    Refusal::new(code, message).into()
}

/// The runtime's identity for a run: deterministic, so a lost start is asked again under the same name.
pub fn workflow_id(tenant: Uuid, run: Uuid) -> String {
    format!("ferrum-{tenant}-{run}")
}

/// What an active release's process starts on.
struct Listener {
    release: Uuid,
    solution: String,
    process: String,
    business_key: String,
    trigger: Ty,
    params: (Ty, Val),
}

#[derive(Default)]
struct Listening {
    triggers: BTreeMap<String, Vec<Listener>>,
    /// Events some process waits for: one that comes before its wait is kept.
    waits: BTreeSet<String>,
    /// Each active solution's release and the generation of its activation, as this was read.
    generations: BTreeMap<String, (Uuid, i64)>,
}

static LISTENING: LazyLock<DashMap<Uuid, (i64, Arc<Listening>)>> = LazyLock::new(DashMap::new);

fn waits_in(nodes: &[Node], out: &mut BTreeSet<String>) {
    for n in nodes {
        if let Node::WaitEvent { event, .. } = n {
            out.insert(event.clone());
        }
        for b in n.blocks() {
            waits_in(&b.nodes, out);
        }
    }
}

fn parameters(release: &Release) -> (Ty, Val) {
    let mut tys = BTreeMap::new();
    let mut vals = BTreeMap::new();
    for (name, p) in &release.ir.parameters {
        let ty = parameter_type(&p.ty).unwrap_or(Ty::Any);
        vals.insert(name.clone(), Val::from_json(&p.value, &ty));
        tys.insert(name.clone(), ty);
    }
    (Ty::Record(tys), Val::Record(vals))
}

async fn listening(k: &Kernel, tenant: Uuid) -> Result<Arc<Listening>> {
    // The workspace's version as the database has it, not as this replica last heard: an activation committed
    // on another replica is taken up by the very next event here, never passed over in the moment before its
    // notification arrives (FLC-MIG-003).
    let mut tx = k.db.tenant_tx(tenant).await?;
    let version: i64 = sqlx::query_scalar("select meta_version from tenant where id = $1").bind(tenant).fetch_one(&mut *tx).await?;
    if let Some(found) = LISTENING.get(&tenant)
        && found.0 == version
    {
        tx.commit().await?;
        return Ok(found.1.clone());
    }
    let l = Arc::new(build_listening(&mut tx, tenant).await?);
    tx.commit().await?;
    if LISTENING.get(&tenant).is_none_or(|e| e.0 <= version) {
        LISTENING.insert(tenant, (version, l.clone()));
    }
    Ok(l)
}

/// Each active solution's release and the generation of its activation.
async fn generations(tx: &mut Tx, tenant: Uuid) -> Result<BTreeMap<String, (Uuid, i64)>> {
    let rows: Vec<(String, Uuid, i64)> =
        sqlx::query_as("select solution, release_id, generation from logic_activation where tenant_id = $1").bind(tenant).fetch_all(&mut **tx).await?;
    Ok(rows.into_iter().map(|(solution, release, generation)| (solution, (release, generation))).collect())
}

/// What the workspace's active releases start on and wait for, as `tx` sees them.
async fn build_listening(tx: &mut Tx, tenant: Uuid) -> Result<Listening> {
    let mut l = Listening { generations: generations(tx, tenant).await?, ..Default::default() };
    for (origin, release) in crate::logic::active_releases(tx, tenant, None).await? {
        let id: Uuid = origin.release.parse().map_err(Error::internal)?;
        for p in &release.ir.processes {
            waits_in(&p.nodes, &mut l.waits);
            let trigger = release.ir.types.get(&p.id).map(|t| t.trigger.clone()).unwrap_or(Ty::Any);
            l.triggers.entry(p.trigger.event.clone()).or_default().push(Listener {
                release: id,
                solution: origin.solution.clone(),
                process: p.id.clone(),
                business_key: p.trigger.business_key.clone(),
                trigger,
                params: parameters(&release),
            });
        }
    }
    Ok(l)
}

/// Accepts the run `s` starts for an event, once per business key, under the release and generation of its
/// solution's activation as this transaction read it, and queues its start.
async fn start_run(tx: &mut Tx, tenant: Uuid, s: &Listener, generation: i64, ev: &OutboxEvent, event: &str, payload: &Value) -> Result<Option<Uuid>> {
    let env = Env::default().with("trigger", s.trigger.clone()).with("parameters", s.params.0.clone()).with("event", event_type());
    let facts = Facts::default()
        .with("trigger", Val::from_json(payload, &s.trigger))
        .with("parameters", s.params.1.clone())
        .with("event", Val::from_json(&json!({"id": ev.id, "topic": event}), &event_type()));
    let key = match expr::compile(&s.business_key, &env).map(|c| c.eval(&facts)) {
        Ok(Val::Text(t)) if !t.is_empty() => t,
        Ok(Val::Int(i)) => i.to_string(),
        other => {
            tracing::warn!(%tenant, process = %s.process, event = ev.id, "no run started: the business key came out as {other:?}");
            return Ok(None);
        }
    };
    let id = Uuid::now_v7();
    let accepted: Option<Uuid> = sqlx::query_scalar(
        "insert into logic_run (tenant_id, id, release_id, solution, process, business_key, trigger, trigger_event, requester, workflow_id, \
           subject_entity, subject_id, generation) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) on conflict (tenant_id, solution, process, business_key) do nothing returning id",
    )
    .bind(tenant)
    .bind(id)
    .bind(s.release)
    .bind(&s.solution)
    .bind(&s.process)
    .bind(&key)
    .bind(payload)
    .bind(ev.id)
    .bind(ev.actor)
    .bind(workflow_id(tenant, id))
    .bind(ev.entity.as_deref())
    .bind(ev.record_id)
    .bind(generation)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(run) = accepted {
        outbox::enqueue(tx, tenant, START_TOPIC, None, None, json!({"run": run})).await?;
    }
    Ok(accepted)
}

/// Takes, until `tx` ends, the lock an event and the waits for it share for one correlation in a workspace.
async fn signal_lock(tx: &mut Tx, tenant: Uuid, event: &str, correlation: &str) -> Result<()> {
    sqlx::query("select pg_advisory_xact_lock(hashtext('ferrum.logic.signal'), hashtext($1 || '/' || $2 || '/' || $3))")
        .bind(tenant.to_string())
        .bind(event)
        .bind(correlation)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Whether an event of `automation`'s is for the solution imported from it to run, claiming it if so. Once the
/// solution's release is active the automation is off (turned off in the same step), and each of its events is
/// claimed once in the automation's own record of runs, under its lock: by the solution's subscriber or by the
/// automation's runner, whichever comes to it first. An event the automation ran before then stays its own.
/// An automation deleted since leaves the solution as the only writer (FLC-MIG-003).
pub(crate) async fn claim_handover(tx: &mut Tx, tenant: Uuid, automation: Uuid, event: i64) -> Result<bool> {
    let active: Option<bool> = sqlx::query_scalar("select is_active from automation where tenant_id = $1 and id = $2 for share")
        .bind(tenant)
        .bind(automation)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(match active {
        None => true,
        Some(true) => false,
        Some(false) => sqlx::query_scalar::<_, i32>(
            "insert into automation_run (tenant_id, automation_id, outbox_id, status) values ($1, $2, $3, 'handed_over') on conflict do nothing returning 1",
        )
        .bind(tenant)
        .bind(automation)
        .bind(event)
        .fetch_optional(&mut **tx)
        .await?
        .is_some(),
    })
}

/// The catalog's id of an outbox event (`procurement.material_request.submitted` for `MaterialRequest.submitted`),
/// what it is about, and its payload as a run reads it.
pub(crate) fn identify(schema: &ferrum_meta::Schema, ev: &OutboxEvent) -> (String, String, Value) {
    if let Some(entity) = &ev.entity
        && let Some((name, kind)) = ev.topic.split_once('.')
        && name == entity
        && let Some(ent) = schema.entity(entity)
    {
        let id = format!("{}.{}.{kind}", ent.package, snake(entity));
        let subject = ev.record_id.map(|r| r.to_string()).unwrap_or_default();
        // A record's event carries the record as rules may read it: never a restricted field.
        let mut doc = serde_json::Map::new();
        if let (Some(d), Some(shape)) = (ev.payload.get("doc").and_then(Value::as_object), crate::logic::shape(schema, entity)) {
            for (f, s) in &shape.fields {
                if !s.restricted
                    && let Some(v) = d.get(f)
                {
                    doc.insert(f.clone(), v.clone());
                }
            }
        }
        doc.insert("id".into(), json!(subject));
        return (id, subject, Value::Object(doc));
    }
    let subject =
        ev.payload.get("subject").or_else(|| ev.payload.get("id")).map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).unwrap_or_default();
    (ev.topic.clone(), subject, ev.payload.clone())
}

/// A wait's correlation value as stored and matched: text as it is, anything else as JSON.
fn correlation(v: &Value) -> String {
    v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
}

/// Accepts the events active processes start on or wait for (registered by [`Kernel::boot`]).
pub struct Processes;

#[async_trait]
impl EventSink for Processes {
    async fn on_event(&self, k: &Kernel, ev: &OutboxEvent) -> Result<()> {
        if ev.topic.starts_with("ferrum.") {
            return Ok(());
        }
        k.accept_event(ev).await
    }
}

/// How a wait was registered.
#[derive(Debug, Clone, PartialEq)]
pub enum Waited {
    /// Its event had come already: here it is.
    Matched(Value),
    Until(Option<DateTime<Utc>>),
}

/// What came of moving a run onto another release (FLC-MIG-004), for its runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Migration {
    /// Moved: the run's state on its new release, and the waiting activations renamed with their steps.
    Moved { state: Run, renamed: Vec<(String, String)>, moved: Vec<String> },
    /// It stays as it was, and why.
    Refused { why: Vec<String> },
}

/// A run as its runtime loads it.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub release: Release,
    pub process: String,
    pub trigger: Value,
    pub requester: Option<Uuid>,
    pub state: Option<Run>,
    pub status: String,
}

impl Kernel {
    /// Starts the runs an event starts, and hands it to the runs waiting for it.
    pub async fn accept_event(&self, ev: &OutboxEvent) -> Result<()> {
        let tenant = ev.tenant_id;
        let schema = self.registry.schema(tenant).await?;
        let mut l = listening(self, tenant).await?;
        let (event, subject, payload) = identify(&schema, ev);
        if l.triggers.get(&event).is_none_or(Vec::is_empty) && !l.waits.contains(&event) {
            return Ok(());
        }
        let mut tx = self.db.tenant_tx(tenant).await?;
        // An event delivered again (the outbox delivers at least once) starts and hands over nothing more.
        if !outbox::first_time(&mut tx, tenant, "ferrum.logic", ev.id).await? {
            tx.commit().await?;
            return Ok(());
        }
        // Each solution starts under the release active as the event is accepted: a replica that has not yet
        // taken up an activation reads what runs now, here, before starting anything (FLC-MIG-003).
        if generations(&mut tx, tenant).await? != l.generations {
            l = Arc::new(build_listening(&mut tx, tenant).await?);
        }
        for s in l.triggers.get(&event).into_iter().flatten() {
            let generation = l.generations.get(&s.solution).map(|g| g.1).unwrap_or_default();
            if let Some(automation) = crate::authoring::automation_of(&s.solution)
                && !claim_handover(&mut tx, tenant, automation, ev.id).await?
            {
                continue;
            }
            start_run(&mut tx, tenant, s, generation, ev, &event, &payload).await?;
        }
        if l.waits.contains(&event) && !subject.is_empty() {
            // One at a time per event and correlation, with a run registering its wait for it: whichever comes
            // second sees what the first wrote, so an event and its wait arriving together cannot both miss.
            signal_lock(&mut tx, tenant, &event, &subject).await?;
            let digest = ferrum_logic::canonical::digest("ferrum.signal/1", &ferrum_logic::capability::digestible(&payload)).unwrap_or_default();
            let waiting: Vec<(Uuid, String)> =
                sqlx::query_as("select run_id, activation from logic_wait where tenant_id = $1 and event = $2 and correlation = $3 and status = 'waiting' order by run_id, activation for update")
                    .bind(tenant)
                    .bind(&event)
                    .bind(&subject)
                    .fetch_all(&mut *tx)
                    .await?;
            if waiting.is_empty() {
                // It came first: kept, for a while, for the run that will wait for it.
                sqlx::query(
                    "insert into logic_signal (tenant_id, event_id, event, correlation, payload, payload_digest, emitter, expires_at) \
                     values ($1, $2, $3, $4, $5, $6, $7, now() + make_interval(days => $8)) on conflict do nothing",
                )
                .bind(tenant)
                .bind(ev.id)
                .bind(&event)
                .bind(&subject)
                .bind(&payload)
                .bind(&digest)
                .bind(ev.actor)
                .bind(EARLY_SIGNAL_DAYS as i32)
                .execute(&mut *tx)
                .await?;
            }
            for (run, activation) in waiting {
                // Kept beside the wait, so a deadline passing after it still finds what matched it.
                let signal: i64 = sqlx::query_scalar(
                    "insert into logic_signal (tenant_id, event_id, event, correlation, payload, payload_digest, emitter, status, run_id, activation, expires_at) \
                     values ($1, $2, $3, $4, $5, $6, $7, 'delivered', $8, $9, now() + make_interval(days => $10)) returning id",
                )
                .bind(tenant)
                .bind(ev.id)
                .bind(&event)
                .bind(&subject)
                .bind(&payload)
                .bind(&digest)
                .bind(ev.actor)
                .bind(run)
                .bind(&activation)
                .bind(EARLY_SIGNAL_DAYS as i32)
                .fetch_one(&mut *tx)
                .await?;
                sqlx::query("update logic_wait set status = 'matched', signal_id = $4 where tenant_id = $1 and run_id = $2 and activation = $3")
                    .bind(tenant)
                    .bind(run)
                    .bind(&activation)
                    .bind(signal)
                    .execute(&mut *tx)
                    .await?;
                deliver(&mut tx, tenant, run, &Event::Signal { activation, payload: payload.clone() }).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// An event of an automation that is off. When the solution imported from it runs in its place and nobody
    /// has claimed the event yet, it is claimed and the solution's run started here, in the automation runner's
    /// transaction, so an event the solution's own subscriber passed before the cutover is not lost
    /// (FLC-MIG-003). `false` when the automation was only turned off, or the event was claimed already.
    pub(crate) async fn hand_over_automation_event(&self, tx: &mut Tx, ev: &OutboxEvent, automation: Uuid) -> Result<bool> {
        let tenant = ev.tenant_id;
        let solution = crate::authoring::solution_of(automation);
        let now = generations(tx, tenant).await?;
        let Some(&(_, generation)) = now.get(&solution) else { return Ok(false) };
        if !claim_handover(tx, tenant, automation, ev.id).await? {
            return Ok(false);
        }
        let mut l = listening(self, tenant).await?;
        if l.generations != now {
            l = Arc::new(build_listening(tx, tenant).await?);
        }
        let schema = self.registry.schema(tenant).await?;
        let (event, _, payload) = identify(&schema, ev);
        for s in l.triggers.get(&event).into_iter().flatten().filter(|s| s.solution == solution) {
            start_run(tx, tenant, s, generation, ev, &event, &payload).await?;
        }
        Ok(true)
    }

    /// Asks every open run, in every workspace, to say where it stands (a recheck its runtime answers with a
    /// report). Run after a restore is released: a run whose history is ahead of the restored database then
    /// names each effect it did that this database has no receipt for (FLC-OPS-005). How many were asked.
    pub async fn recheck_logic_runs(&self) -> Result<u64> {
        if !self.settings.process_runtime {
            return Ok(0);
        }
        let mut tx = self.db.system_tx().await?;
        let open: Vec<(Uuid, Uuid)> =
            sqlx::query_as("select tenant_id, id from logic_run where status in ('starting', 'running', 'needs_attention', 'cancelling')")
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        for (tenant, run) in &open {
            let mut tx = self.db.tenant_tx(*tenant).await?;
            deliver(&mut tx, *tenant, *run, &Event::Recheck {}).await?;
            tx.commit().await?;
        }
        Ok(open.len() as u64)
    }

    /// Moves a run onto `release` as its runtime holds it (`state`), when an approved move reaches it
    /// (FLC-MIG-004). Mapped again on the run as it is now, and checked where the business records are: what it
    /// waits on must still be open, for a decision or an event that came meanwhile wins and the run stays where it
    /// was. Then, at once, its waiting tasks and waits take their steps' new names, the run takes the release, and
    /// the move is recorded on the run.
    pub async fn process_migrate(&self, tenant: Uuid, run: Uuid, release: Uuid, map: &BTreeMap<String, String>, by: &str, state: &Run) -> Result<Migration> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let row: Option<(Uuid, Option<Value>)> = sqlx::query_as(
            "select l.release_id, (select r.release from logic_release r where r.tenant_id = l.tenant_id and r.id = $3 and r.solution = l.solution) \
             from logic_run l where l.tenant_id = $1 and l.id = $2 for update",
        )
        .bind(tenant)
        .bind(run)
        .bind(release)
        .fetch_optional(&mut *tx)
        .await?;
        let schema = self.registry.schema(tenant).await?;
        let sys = Ctx::system(tenant, schema);
        let refuse = |why: Vec<String>| Migration::Refused { why };
        let outcome = match row {
            None => return Err(Error::NotFound("run".into())),
            Some((from, _)) if from == release => refuse(vec!["it is on that release already".into()]),
            Some((_, None)) => refuse(vec!["that release is not one of its solution's".into()]),
            Some((from, Some(stored))) => {
                let target: Release = serde_json::from_value(stored).map_err(Error::internal)?;
                let mut moved_state = state.clone();
                match ferrum_logic::process::Machine::new(&target).migrate(&mut moved_state, map) {
                    Err(why) => refuse(why),
                    Ok(moved) => {
                        let tasks: Vec<(String, String)> = sqlx::query_as(
                            "select activation, status from logic_task where tenant_id = $1 and run_id = $2 and activation = any($3) for update",
                        )
                        .bind(tenant)
                        .bind(run)
                        .bind(&moved.waiting)
                        .fetch_all(&mut *tx)
                        .await?;
                        let waits: Vec<(String, String)> = sqlx::query_as(
                            "select activation, status from logic_wait where tenant_id = $1 and run_id = $2 and activation = any($3) for update",
                        )
                        .bind(tenant)
                        .bind(run)
                        .bind(&moved.waiting)
                        .fetch_all(&mut *tx)
                        .await?;
                        let settled: Vec<String> = tasks
                            .iter()
                            .filter(|(_, st)| st != "open")
                            .chain(waits.iter().filter(|(_, st)| st != "waiting"))
                            .map(|(act, st)| format!("{act} was {st} meanwhile"))
                            .collect();
                        if !settled.is_empty() {
                            refuse(settled)
                        } else {
                            for (old, new) in &moved.renamed {
                                for table in ["logic_task", "logic_wait", "logic_signal"] {
                                    sqlx::query(sqlx::AssertSqlSafe(format!(
                                        "update {table} set activation = $4 where tenant_id = $1 and run_id = $2 and activation = $3"
                                    )))
                                    .bind(tenant)
                                    .bind(run)
                                    .bind(old)
                                    .bind(new)
                                    .execute(&mut *tx)
                                    .await?;
                                }
                            }
                            sqlx::query("update logic_run set release_id = $3, process = $4, state = $5, updated_at = now() where tenant_id = $1 and id = $2")
                                .bind(tenant)
                                .bind(run)
                                .bind(release)
                                .bind(&moved_state.process)
                                .bind(serde_json::to_value(&moved_state).map_err(Error::internal)?)
                                .execute(&mut *tx)
                                .await?;
                            crate::audit::record(
                                &mut tx,
                                &sys,
                                "LogicRun",
                                run,
                                "migrated",
                                None,
                                json!({"from": from, "to": release, "map": map, "moved": moved.moved, "approved_by": by}),
                            )
                            .await?;
                            tx.commit().await?;
                            return Ok(Migration::Moved { state: moved_state, renamed: moved.renamed, moved: moved.moved });
                        }
                    }
                }
            }
        };
        if let Migration::Refused { why } = &outcome {
            crate::audit::record(&mut tx, &sys, "LogicRun", run, "migration_refused", None, json!({"to": release, "map": map, "why": why, "approved_by": by}))
                .await?;
        }
        tx.commit().await?;
        Ok(outcome)
    }

    /// Whether `run` can move onto `release` as `map` says, as it last reported where it is: the release must be
    /// the one that runs now for its solution (reviewed and activated), and the mapping must hold.
    async fn migration_check(&self, tx: &mut Tx, tenant: Uuid, run: Uuid, release: Uuid, map: &BTreeMap<String, String>) -> Result<Vec<String>> {
        type Row = (String, Uuid, Option<Value>, Option<Value>, Option<Uuid>);
        let row: Option<Row> = sqlx::query_as(
            "select l.solution, l.release_id, l.state, \
               (select r.release from logic_release r where r.tenant_id = l.tenant_id and r.id = $3 and r.solution = l.solution), \
               (select a.release_id from logic_activation a where a.tenant_id = l.tenant_id and a.solution = l.solution) \
             from logic_run l where l.tenant_id = $1 and l.id = $2",
        )
        .bind(tenant)
        .bind(run)
        .bind(release)
        .fetch_optional(&mut **tx)
        .await?;
        let Some((solution, from, state, target, active)) = row else { return Err(Error::NotFound("run".into())) };
        let Some(target) = target else { return Err(Error::field("release", format!("is a release of {solution}"))) };
        if active != Some(release) {
            return Err(Error::Conflict(format!("a run moves only onto the release that runs now for {solution}: activate that one first")));
        }
        if from == release {
            return Err(Error::Conflict("it is on that release already".into()));
        }
        let mut state: Run = state
            .and_then(|s| serde_json::from_value(s).ok())
            .ok_or_else(|| Error::Conflict("the run has not said where it is yet: try again in a moment".into()))?;
        let target: Release = serde_json::from_value(target).map_err(Error::internal)?;
        ferrum_logic::process::Machine::new(&target)
            .migrate(&mut state, map)
            .map(|m| m.moved)
            .map_err(|why| Error::Conflict(format!("it cannot move as mapped: {}", why.join("; "))))
    }

    /// Plans moving a run onto the release that runs now for its solution (administrators; FLC-MIG-004): checked
    /// as the run stands, and recorded on the run under a digest of exactly this run, release and mapping, for
    /// another administrator to approve.
    pub async fn plan_logic_migration(&self, ctx: &Ctx, run: Uuid, release: Uuid, map: &BTreeMap<String, String>) -> Result<Value> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let moved = self.migration_check(&mut tx, ctx.tenant, run, release, map).await?;
        let digest = ferrum_logic::canonical::digest("ferrum.logic.migration/1", &json!({"run": run, "release": release, "map": map}))
            .map_err(|e| Error::field("map", e.to_string()))?;
        crate::audit::record(&mut tx, ctx, "LogicRun", run, "plan_migration", None, json!({"release": release, "map": map, "digest": digest, "moved": moved}))
            .await?;
        tx.commit().await?;
        Ok(json!({"digest": digest, "moved": moved, "next": "Another administrator approves exactly this; the run moves when its runtime takes it up."}))
    }

    /// Approves a planned move (another administrator than who planned it), checked again as the run stands, and
    /// hands it to the run's runtime.
    pub async fn approve_logic_migration(&self, ctx: &Ctx, run: Uuid, digest: &str, reason: &str) -> Result<Value> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let plan: Option<(Option<Uuid>, Value)> = sqlx::query_as(
            "select actor, diff from audit_event where tenant_id = $1 and entity = 'LogicRun' and record_id = $2 and action = 'plan_migration' \
             and diff->>'digest' = $3 order by id desc limit 1",
        )
        .bind(ctx.tenant)
        .bind(run)
        .bind(digest)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((planner, plan)) = plan else { return Err(Error::NotFound("planned move".into())) };
        if planner == Some(ctx.user) {
            return Err(Error::Forbidden("someone other than who planned a move approves it".into()));
        }
        let release: Uuid = plan["release"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| Error::internal("a plan without its release"))?;
        let map: BTreeMap<String, String> = serde_json::from_value(plan["map"].clone()).map_err(Error::internal)?;
        self.migration_check(&mut tx, ctx.tenant, run, release, &map).await?;
        deliver(&mut tx, ctx.tenant, run, &Event::Migrated { release: release.to_string(), map, by: ctx.user.to_string() }).await?;
        crate::audit::record(&mut tx, ctx, "LogicRun", run, "approve_migration", None, json!({"digest": digest, "reason": reason})).await?;
        tx.commit().await?;
        Ok(json!({
            "delivered": true,
            "note": "The run moves when its runtime takes this up; if it moved on before then, the move is refused and that is recorded on the run.",
        }))
    }

    /// A run, its release and its last reported state, for its runtime.
    pub async fn process_load(&self, tenant: Uuid, run: Uuid) -> Result<Loaded> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let row: Option<(Value, String, Value, Option<Uuid>, Option<Value>, String)> = sqlx::query_as(
            "select r.release, l.process, l.trigger, l.requester, l.state, l.status from logic_run l \
             join logic_release r on r.tenant_id = l.tenant_id and r.id = l.release_id where l.tenant_id = $1 and l.id = $2",
        )
        .bind(tenant)
        .bind(run)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let Some((release, process, trigger, requester, state, status)) = row else { return Err(Error::NotFound("run".into())) };
        Ok(Loaded {
            release: serde_json::from_value(release).map_err(|e| Error::Internal(format!("the run's release cannot be read: {e}")))?,
            process,
            trigger,
            requester,
            state: state.and_then(|s| serde_json::from_value(s).ok()),
            status,
        })
    }

    /// The context a run's effects run under: its requester's rights as they are now, or for a run no person
    /// started, those of whoever activated its release. Revoked rights refuse the effect; nothing stronger is
    /// borrowed (FLC-PROC-006).
    async fn run_ctx(&self, tenant: Uuid, run: Uuid) -> Result<Ctx> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let who: Option<(Option<Uuid>, Uuid)> = sqlx::query_as(
            "select l.requester, a.activated_by from logic_run l left join logic_activation a on a.tenant_id = l.tenant_id and a.solution = l.solution \
             where l.tenant_id = $1 and l.id = $2",
        )
        .bind(tenant)
        .bind(run)
        .fetch_optional(&mut *tx)
        .await?
        .map(|(r, a): (Option<Uuid>, Option<Uuid>)| (r, a.unwrap_or_default()));
        tx.commit().await?;
        let Some((requester, activator)) = who else { return Err(Error::NotFound("run".into())) };
        let user = requester.unwrap_or(activator);
        if user.is_nil() {
            return Err(Error::Forbidden("nobody's rights cover this run any more".into()));
        }
        self.load_ctx(tenant, user, None, format!("run-{run}")).await.map_err(|_| Error::Forbidden("whoever the run acts for can no longer act here".into()))
    }

    /// Runs one capability for a run. `Err` only for what may be tried again (the database or a dependency
    /// away); a refusal or an unknown outcome is the run's to route.
    #[allow(clippy::too_many_arguments)]
    pub async fn process_invoke(
        &self,
        tenant: Uuid,
        run: Uuid,
        activation: &str,
        capability: &str,
        version: &str,
        input: Value,
        changes: bool,
        approval: Option<&ferrum_logic::process::Approval>,
    ) -> Result<Invoked> {
        let out = self.invoke_step(tenant, run, activation, capability, version, input, changes, approval).await;
        // Each try counted, with how it went, for whoever looks at the run (FLC-OPS-001). The database away is the
        // one try that cannot be counted; counting never fails the step itself.
        let outcome = match &out {
            Ok(Invoked::Done { .. }) => "done".to_string(),
            Ok(Invoked::Refused { code, message }) => format!("refused ({code}): {message}"),
            Ok(Invoked::Unknown { why }) => format!("outcome unknown: {why}"),
            Err(Error::Db(_)) => return out,
            Err(e) => format!("waits: {e}"),
        };
        if let Err(e) = self.count_attempt(tenant, run, activation, &outcome).await {
            tracing::warn!(%tenant, %run, %activation, "a step's try was not counted: {e}");
        }
        out
    }

    async fn count_attempt(&self, tenant: Uuid, run: Uuid, activation: &str, outcome: &str) -> Result<()> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        sqlx::query(
            "insert into logic_attempt (tenant_id, run_id, activation, attempts, last_outcome) values ($1, $2, $3, 1, $4) \
             on conflict (tenant_id, run_id, activation) do update set attempts = logic_attempt.attempts + 1, last_outcome = excluded.last_outcome, last_at = now()",
        )
        .bind(tenant)
        .bind(run)
        .bind(activation)
        .bind(outcome.chars().take(500).collect::<String>())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn invoke_step(
        &self,
        tenant: Uuid,
        run: Uuid,
        activation: &str,
        capability: &str,
        version: &str,
        input: Value,
        changes: bool,
        approval: Option<&ferrum_logic::process::Approval>,
    ) -> Result<Invoked> {
        // What may be tried again is tried again; anything else is a refusal the run routes on.
        let settle = |e: Error| match e {
            Error::Unavailable(_) | Error::Retryable(_) | Error::Db(_) => Err(e),
            e => Ok(Invoked::Refused { code: e.code().into(), message: e.to_string() }),
        };
        // After a restore, or with another region holding the platform, nothing a run does may change the world
        // until a person has reconciled what came back and released the hold: the effect waits and is tried
        // again, as jobs and the outbox wait (FLC-OPS-004). A read changes nothing and goes ahead.
        if changes && (self.db.fenced() || self.holding().await?) {
            return Err(Error::Unavailable("held after a restore, or fenced to another region: effects wait until the hold is released".into()));
        }
        let ctx = match self.run_ctx(tenant, run).await {
            Ok(c) => c,
            Err(e) => return settle(e),
        };
        // A capability taken out of service since the release was activated (its app's release revoked or taken
        // out of the workspace, or the version the release is bound to replaced) is neither used nor failed for:
        // the step waits, as it waits for a hold, administrators see why and may end the run, and it goes on by
        // itself once the capability is back. Nothing similarly named stands in for it (FLC-OPS-007).
        match self.capability(&ctx, capability) {
            Ok(c) if c.version == version => self.unpaused(tenant, run, activation).await?,
            Ok(c) => {
                let why = format!("the release is bound to version {version} of {capability}, and this workspace has {}", c.version);
                return self.paused(tenant, run, activation, capability, &why).await;
            }
            Err(e) if e.code() == Code::CapabilityUnavailable.as_str() => return self.paused(tenant, run, activation, capability, &e.to_string()).await,
            // Rights: the invocation below refuses it, for the run to route.
            Err(_) => {}
        }
        if let Some(a) = approval
            && let Err(e) = self.approval_holds(tenant, run, a).await
        {
            return settle(e);
        }
        let mut inv = crate::capability::Invocation::new(capability, input);
        inv.version = Some(version.to_string());
        if changes {
            inv.effect_key = Some(format!("{run}/{activation}"));
        }
        match self.invoke(&ctx, inv).await {
            Ok(receipt) => Ok(Invoked::Done { output: receipt.result }),
            Err(e @ (Error::Unavailable(_) | Error::Retryable(_) | Error::Db(_))) => Err(e),
            Err(e) if e.code() == Code::OutcomeUnknown.as_str() => Ok(Invoked::Unknown { why: e.to_string() }),
            Err(e) => Ok(Invoked::Refused { code: e.code().into(), message: e.to_string() }),
        }
    }

    /// A step waiting for a capability out of service: an item telling administrators, opened once, and the step
    /// tried again later.
    async fn paused(&self, tenant: Uuid, run: Uuid, activation: &str, capability: &str, why: &str) -> Result<Invoked> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        sqlx::query(
            "insert into logic_task (tenant_id, run_id, activation, kind, label, detail) values ($1, $2, $3, 'attention', $4, $5) on conflict do nothing",
        )
        .bind(tenant)
        .bind(run)
        .bind(format!("{activation}#unavailable"))
        .bind(format!("Waiting for {capability}, which is out of service"))
        .bind(json!({
            "why": format!("{why}. The step waits and goes on by itself once {capability} is back; to stop instead, cancel the run."),
            "capability": capability, "step": activation,
        }))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        tracing::warn!(%tenant, %run, %activation, %capability, "a step waits for a capability out of service: {why}");
        Err(Error::Unavailable(format!("{capability} is out of service here ({why}): the step waits until it is back")))
    }

    /// A capability in service again: the item about waiting for it, if one is open, is resolved.
    async fn unpaused(&self, tenant: Uuid, run: Uuid, activation: &str) -> Result<()> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        sqlx::query(
            "update logic_task set status = 'resolved', closed_at = now(), version = version + 1 \
             where tenant_id = $1 and run_id = $2 and activation = $3 and status = 'open'",
        )
        .bind(tenant)
        .bind(run)
        .bind(format!("{activation}#unavailable"))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Whether an approval an effect relies on still holds: decided as recorded, on the material it names, and
    /// the approved record unchanged since (its version, when the material carries one).
    async fn approval_holds(&self, tenant: Uuid, run: Uuid, a: &ferrum_logic::process::Approval) -> Result<()> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let approved: Option<(String, String)> =
            sqlx::query_as("select status, digest from logic_task where tenant_id = $1 and run_id = $2 and activation = $3")
                .bind(tenant)
                .bind(run)
                .bind(&a.task)
                .fetch_optional(&mut *tx)
                .await?;
        if approved.as_ref().is_none_or(|(status, digest)| status != "approved" || *digest != a.digest) {
            return Err(refusal(Code::StaleApproval, format!("the approval {} relies on is not on record as given", a.task)));
        }
        let (Some(id), Some(version)) =
            (a.material.get("id").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok()), a.material.get("version").and_then(Value::as_i64))
        else {
            tx.commit().await?;
            return Ok(());
        };
        let schema = self.registry.schema(tenant).await?;
        let sys = Ctx::system(tenant, schema.clone());
        if let Some(entity) = a.entity.clone() {
            let now = match self.get_tx(&sys, &mut tx, &entity, id, false).await {
                Ok(d) => d.get("version").and_then(Value::as_i64),
                Err(Error::NotFound(_)) => {
                    tx.commit().await?;
                    return Err(refusal(Code::StaleApproval, format!("the {entity} that was approved no longer exists")));
                }
                Err(e) => return Err(e),
            };
            if now.is_some_and(|v| v != version) {
                tx.commit().await?;
                return Err(refusal(
                    Code::StaleApproval,
                    format!("the {entity} approved at version {version} is at version {} now: it needs approving again", now.unwrap_or_default()),
                ));
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// When a wait started now ends, from the workspace's calendars.
    pub async fn process_deadline(&self, tenant: Uuid, wait: &Value, from: DateTime<Utc>) -> Result<Deadline> {
        let w = Wait::parse(wait).ok_or_else(|| Error::invalid(format!("{wait} is not a wait")))?;
        let cal = match w.calendar() {
            None => None,
            Some(name) => {
                let mut tx = self.db.tenant_tx(tenant).await?;
                let row: Option<(String, Vec<String>, Vec<chrono::NaiveDate>, i64)> =
                    sqlx::query_as("select zone, weekends, holidays, revision from logic_calendar where tenant_id = $1 and name = $2")
                        .bind(tenant)
                        .bind(name)
                        .fetch_optional(&mut *tx)
                        .await?;
                tx.commit().await?;
                row.map(|(zone, weekends, holidays, revision)| Calendar {
                    name: name.into(),
                    zone,
                    weekends,
                    holidays: holidays.into_iter().collect(),
                    revision,
                })
            }
        };
        calendar::deadline(&w, from, cal.as_ref()).map_err(Error::Invalid)
    }

    /// Opens a person's task, a case or a recovery item for a run, once per activation.
    #[allow(clippy::too_many_arguments)]
    pub async fn process_open_task(
        &self,
        tenant: Uuid,
        run: Uuid,
        activation: &str,
        kind: &str,
        label: &str,
        role: &str,
        material: &Value,
        digest: &str,
        form: &Value,
        approvals: u32,
        delegation: u32,
        exclude: &[String],
        detail: &Value,
        timeout: Option<&Value>,
    ) -> Result<Option<DateTime<Utc>>> {
        let deadline = match timeout {
            Some(t) => Some(self.process_deadline(tenant, t, Utc::now()).await?),
            None => None,
        };
        let exclude: Vec<Uuid> = exclude.iter().filter_map(|s| s.parse().ok()).collect();
        let mut tx = self.db.tenant_tx(tenant).await?;
        let at: Option<Option<DateTime<Utc>>> = sqlx::query_scalar(
            "insert into logic_task (tenant_id, run_id, activation, kind, label, role, material, digest, form, approvals, delegation, exclude, detail, deadline, deadline_basis) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) on conflict (tenant_id, run_id, activation) do nothing returning deadline",
        )
        .bind(tenant)
        .bind(run)
        .bind(activation)
        .bind(kind)
        .bind(label)
        .bind(role)
        .bind(material)
        .bind(digest)
        .bind(form)
        .bind(approvals.clamp(1, 9) as i32)
        .bind(delegation.min(3) as i32)
        .bind(&exclude)
        .bind(detail)
        .bind(deadline.as_ref().map(|d| d.at))
        .bind(deadline.as_ref().map(|d| serde_json::to_value(d).unwrap_or(Value::Null)))
        .fetch_optional(&mut *tx)
        .await?;
        let at = match at {
            Some(at) => at,
            // Opened by an earlier attempt: the same task, the same deadline.
            None => {
                sqlx::query_scalar("select deadline from logic_task where tenant_id = $1 and run_id = $2 and activation = $3")
                    .bind(tenant)
                    .bind(run)
                    .bind(activation)
                    .fetch_one(&mut *tx)
                    .await?
            }
        };
        tx.commit().await?;
        Ok(at)
    }

    /// The deadline of a task passed: it times out, unless it was decided first, in which case that decision is
    /// what the run hears (one outcome, decided under the task's row lock).
    pub async fn process_expire_task(&self, tenant: Uuid, run: Uuid, activation: &str) -> Result<Option<Event>> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let row: Option<(Uuid, String, String)> =
            sqlx::query_as("select id, status, kind from logic_task where tenant_id = $1 and run_id = $2 and activation = $3 for update")
                .bind(tenant)
                .bind(run)
                .bind(activation)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((task, status, kind)) = row else { return Err(Error::NotFound("task".into())) };
        let out = match status.as_str() {
            // The deadline came first, now or when this was asked before (an attempt whose answer was lost).
            "open" | "timed_out" => {
                if status == "open" {
                    sqlx::query("update logic_task set status = 'timed_out', closed_at = now(), version = version + 1 where tenant_id = $1 and id = $2")
                        .bind(tenant)
                        .bind(task)
                        .execute(&mut *tx)
                        .await?;
                }
                Some(if kind == "case" { Event::CaseTimedOut { activation: activation.into() } } else { Event::TaskTimedOut { activation: activation.into() } })
            }
            // Decided first: that decision is the outcome. Cancelled: nothing to tell.
            "approved" | "rejected" | "done" => self.decided_event(&mut tx, tenant, task, activation, &kind, &status).await?,
            _ => None,
        };
        tx.commit().await?;
        Ok(out)
    }

    /// What a closed task told its run, rebuilt from its decisions.
    async fn decided_event(&self, tx: &mut Tx, tenant: Uuid, task: Uuid, activation: &str, kind: &str, status: &str) -> Result<Option<Event>> {
        if kind == "case" {
            return Ok(match status {
                "done" => Some(Event::CaseClosed { activation: activation.into(), done: case_done(tx, tenant, task).await? }),
                _ => None,
            });
        }
        // The decision that closed it: the last approval of an approved task (the one that made the count), or
        // the rejection.
        let decision = if status == "approved" { "approve" } else { "reject" };
        let last: Option<(Uuid, Uuid, Value, DateTime<Utc>)> = sqlx::query_as(
            "select id, by_user, values, decided_at from logic_task_decision where tenant_id = $1 and task_id = $2 and decision = $3 order by decided_at desc, id desc limit 1",
        )
        .bind(tenant)
        .bind(task)
        .bind(decision)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(last.map(|(id, by, values, at)| Event::TaskDecided {
            activation: activation.into(),
            approved: status == "approved",
            by: by.to_string(),
            decision_id: id.to_string(),
            at: at.to_rfc3339(),
            values,
        }))
    }

    /// Registers what a run waits for; an event that came already is handed over at once, once.
    pub async fn process_register_wait(&self, tenant: Uuid, run: Uuid, activation: &str, event: &str, value: &Value, timeout: &Value) -> Result<Waited> {
        let deadline = self.process_deadline(tenant, timeout, Utc::now()).await?.at;
        let key = correlation(value);
        let mut tx = self.db.tenant_tx(tenant).await?;
        // Serialised with an event being accepted for the same correlation (see `accept_event`).
        signal_lock(&mut tx, tenant, event, &key).await?;
        let inserted = sqlx::query(
            "insert into logic_wait (tenant_id, run_id, activation, event, correlation, deadline) values ($1, $2, $3, $4, $5, $6) on conflict (tenant_id, run_id, activation) do nothing",
        )
        .bind(tenant)
        .bind(run)
        .bind(activation)
        .bind(event)
        .bind(&key)
        .bind(deadline)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if inserted == 0 {
            let row: (String, Option<i64>, Option<DateTime<Utc>>) =
                sqlx::query_as("select status, signal_id, deadline from logic_wait where tenant_id = $1 and run_id = $2 and activation = $3")
                    .bind(tenant)
                    .bind(run)
                    .bind(activation)
                    .fetch_one(&mut *tx)
                    .await?;
            let out = match row {
                (s, Some(signal), _) if s == "matched" => Waited::Matched(
                    sqlx::query_scalar("select payload from logic_signal where tenant_id = $1 and id = $2")
                        .bind(tenant)
                        .bind(signal)
                        .fetch_one(&mut *tx)
                        .await?,
                ),
                (_, _, at) => Waited::Until(at),
            };
            tx.commit().await?;
            return Ok(out);
        }
        let early: Option<(i64, Value)> = sqlx::query_as(
            "update logic_signal set status = 'delivered', run_id = $4, activation = $5 where tenant_id = $1 and id = \
               (select id from logic_signal where tenant_id = $1 and event = $2 and correlation = $3 and status = 'pending' and expires_at > now() order by id limit 1 for update skip locked) \
             returning id, payload",
        )
        .bind(tenant)
        .bind(event)
        .bind(&key)
        .bind(run)
        .bind(activation)
        .fetch_optional(&mut *tx)
        .await?;
        let out = match early {
            Some((signal, payload)) => {
                sqlx::query("update logic_wait set status = 'matched', signal_id = $4 where tenant_id = $1 and run_id = $2 and activation = $3")
                    .bind(tenant)
                    .bind(run)
                    .bind(activation)
                    .bind(signal)
                    .execute(&mut *tx)
                    .await?;
                Waited::Matched(payload)
            }
            None => Waited::Until(Some(deadline)),
        };
        tx.commit().await?;
        Ok(out)
    }

    /// A wait's deadline passed: it times out, unless its event came first.
    pub async fn process_expire_wait(&self, tenant: Uuid, run: Uuid, activation: &str) -> Result<Option<Value>> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        let row: Option<(String, Option<i64>)> =
            sqlx::query_as("select status, signal_id from logic_wait where tenant_id = $1 and run_id = $2 and activation = $3 for update")
                .bind(tenant)
                .bind(run)
                .bind(activation)
                .fetch_optional(&mut *tx)
                .await?;
        let out = match row {
            Some((s, _)) if s == "waiting" => {
                sqlx::query("update logic_wait set status = 'timed_out' where tenant_id = $1 and run_id = $2 and activation = $3")
                    .bind(tenant)
                    .bind(run)
                    .bind(activation)
                    .execute(&mut *tx)
                    .await?;
                None
            }
            Some((_, Some(signal))) => Some(
                sqlx::query_scalar("select payload from logic_signal where tenant_id = $1 and id = $2").bind(tenant).bind(signal).fetch_one(&mut *tx).await?,
            ),
            _ => None,
        };
        tx.commit().await?;
        Ok(out)
    }

    /// A run asked work in hand to stop: its open tasks, case and waits close.
    pub async fn process_cancel(&self, tenant: Uuid, run: Uuid, activation: &str) -> Result<()> {
        let mut tx = self.db.tenant_tx(tenant).await?;
        sqlx::query("update logic_task set status = 'cancelled', closed_at = now(), version = version + 1 where tenant_id = $1 and run_id = $2 and activation = $3 and status = 'open'")
            .bind(tenant)
            .bind(run)
            .bind(activation)
            .execute(&mut *tx)
            .await?;
        sqlx::query("update logic_wait set status = 'cancelled' where tenant_id = $1 and run_id = $2 and activation = $3 and status = 'waiting'")
            .bind(tenant)
            .bind(run)
            .bind(activation)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Reports a run's state for people to see: a read model, rebuilt from the runtime.
    pub async fn process_report(&self, tenant: Uuid, run: Uuid, state: &Run) -> Result<()> {
        use ferrum_logic::process::{Finished, Status};
        let (status, output, finished) = match &state.status {
            Status::Running => ("running", None, false),
            Status::NeedsAttention => ("needs_attention", None, false),
            Status::Cancelling => ("cancelling", None, false),
            Status::Finished { outcome, output } => (
                match outcome {
                    Finished::Completed => "completed",
                    Finished::Refused => "refused",
                    Finished::Cancelled => "cancelled",
                },
                Some(output.clone()),
                true,
            ),
        };
        let mut tx = self.db.tenant_tx(tenant).await?;
        sqlx::query(
            "update logic_run set state = $3, status = $4, output = coalesce($5, output), reported = reported + 1, updated_at = now(), \
             finished_at = case when $6 then coalesce(finished_at, now()) else finished_at end where tenant_id = $1 and id = $2",
        )
        .bind(tenant)
        .bind(run)
        .bind(serde_json::to_value(state).map_err(Error::internal)?)
        .bind(status)
        .bind(output)
        .bind(finished)
        .execute(&mut *tx)
        .await?;
        // A history ahead of this database: on a database restored to a point before Temporal's histories, an
        // effect the run says it did can have no receipt here. It may have happened, and nothing here can tell,
        // so a person looks; nothing is done again by itself (FLC-OPS-005). Only a database that has been held
        // after a restore is checked, for runs it accepted before then.
        if !state.effects.is_empty() {
            let restored: Option<DateTime<Utc>> = sqlx::query_scalar(
                "select coalesce(p.held_since, p.released_at) from platform_incarnation p, logic_run l \
                 where l.tenant_id = $1 and l.id = $2 and l.accepted_at < coalesce(p.held_since, p.released_at)",
            )
            .bind(tenant)
            .bind(run)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
            if restored.is_some() {
                let keys: Vec<String> = state.effects.iter().map(|e| format!("{run}/{}", e.activation)).collect();
                let have: Vec<String> = sqlx::query_scalar("select effect_key from logic_effect where tenant_id = $1 and effect_key = any($2)")
                    .bind(tenant)
                    .bind(&keys)
                    .fetch_all(&mut *tx)
                    .await?;
                for e in state.effects.iter().filter(|e| !have.contains(&format!("{run}/{}", e.activation))) {
                    sqlx::query(
                        "insert into logic_task (tenant_id, run_id, activation, kind, label, detail) values ($1, $2, $3, 'attention', $4, $5) on conflict do nothing",
                    )
                    .bind(tenant)
                    .bind(run)
                    .bind(format!("{}#restored", e.activation))
                    .bind(format!("No receipt here for {}: check whether it happened", e.capability))
                    .bind(json!({
                        "why": format!(
                            "The run's history says {} was done at {}, and this database, restored to a point before it, has no receipt of it. It may have happened: check, put right what the restore lost, then acknowledge. Nothing is done again by itself.",
                            e.capability, e.activation
                        ),
                        "capability": e.capability, "step": e.activation, "output": e.output,
                    }))
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// The runs of the workspace a person may see: an administrator all, anyone else those they started.
    pub async fn logic_runs(&self, ctx: &Ctx, limit: i64) -> Result<Vec<Value>> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', id, 'solution', solution, 'process', process, 'business_key', business_key, 'status', status, \
               'requester', requester, 'accepted_at', accepted_at, 'updated_at', updated_at, 'finished_at', finished_at) \
             from logic_run where tenant_id = $1 and ($2 or requester = $3) order by accepted_at desc limit $4",
        )
        .bind(ctx.tenant)
        .bind(ctx.is_admin)
        .bind(ctx.user)
        .bind(limit.clamp(1, 500))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// One run, with its tasks, for an administrator or whoever started it.
    pub async fn logic_run(&self, ctx: &Ctx, run: Uuid) -> Result<Value> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', l.id, 'solution', l.solution, 'process', l.process, 'business_key', l.business_key, 'status', l.status, \
               'requester', l.requester, 'output', l.output, 'state', l.state, 'accepted_at', l.accepted_at, 'finished_at', l.finished_at, \
               'release', jsonb_build_object('id', l.release_id, 'version', (select r.version from logic_release r where r.tenant_id = l.tenant_id and r.id = l.release_id)), \
               'generation', l.generation, 'trigger_event', l.trigger_event, \
               'attempts', coalesce((select jsonb_object_agg(a.activation, jsonb_build_object('attempts', a.attempts, 'last', a.last_outcome, \
                  'first_at', a.first_at, 'last_at', a.last_at)) from logic_attempt a where a.tenant_id = l.tenant_id and a.run_id = l.id), '{}'::jsonb), \
               'moves', coalesce((select jsonb_agg(jsonb_build_object('action', e.action, 'at', e.at, 'by', e.actor, 'diff', e.diff) order by e.id) \
                  from audit_event e where e.tenant_id = l.tenant_id and e.entity = 'LogicRun' and e.record_id = l.id \
                  and e.action in ('plan_migration', 'approve_migration', 'migrated', 'migration_refused')), '[]'::jsonb), \
               'tasks', coalesce((select jsonb_agg(jsonb_build_object('id', t.id, 'activation', t.activation, 'kind', t.kind, 'label', t.label, \
                  'status', t.status, 'deadline', t.deadline) order by t.opened_at) from logic_task t where t.tenant_id = l.tenant_id and t.run_id = l.id), '[]'::jsonb)) \
             from logic_run l where l.tenant_id = $1 and l.id = $2 and ($3 or l.requester = $4)",
        )
        .bind(ctx.tenant)
        .bind(run)
        .bind(ctx.is_admin)
        .bind(ctx.user)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        row.ok_or_else(|| Error::NotFound("run".into()))
    }

    /// Runs that changed since `cursor`, for a page that keeps up with them (FLC-PROC-014). Without a cursor, the
    /// runs as they stand; either way a new cursor. The cursor is a transaction boundary: every transaction below
    /// it had finished when this read, so a change a slower transaction commits later is in a later answer, never
    /// skipped. A run may come twice; the page keeps the latest. A cursor from another database (a restore) gets
    /// `resync`: the runs as they stand, and a fresh cursor.
    pub async fn logic_run_changes(&self, ctx: &Ctx, cursor: Option<&str>) -> Result<Value> {
        const PAGE: i64 = 500;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let (boundary, next_xid): (String, String) =
            sqlx::query_as("select pg_snapshot_xmin(s)::text, pg_snapshot_xmax(s)::text from (select pg_current_snapshot() as s) x")
                .fetch_one(&mut *tx)
                .await?;
        let parsed = cursor.and_then(|c| c.parse::<u64>().ok());
        let resync = cursor.is_some() && parsed.is_none_or(|c| next_xid.parse::<u64>().is_ok_and(|n| c > n));
        let fields = "jsonb_build_object('id', id, 'solution', solution, 'process', process, 'business_key', business_key, 'status', status, \
             'requester', requester, 'subject_entity', subject_entity, 'subject_id', subject_id, 'accepted_at', accepted_at, \
             'updated_at', updated_at, 'finished_at', finished_at)";
        let rows: Vec<(Value, String)> = match parsed.filter(|_| !resync) {
            None => {
                sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "select {fields}, changed::text from logic_run where tenant_id = $1 and ($2 or requester = $3) order by accepted_at desc limit {PAGE}"
                )))
                .bind(ctx.tenant)
                .bind(ctx.is_admin)
                .bind(ctx.user)
                .fetch_all(&mut *tx)
                .await?
            }
            Some(c) => {
                sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "select {fields}, changed::text from logic_run where tenant_id = $1 and ($2 or requester = $3) and changed >= $4::text::xid8 \
                 order by changed limit {PAGE}"
                )))
                .bind(ctx.tenant)
                .bind(ctx.is_admin)
                .bind(ctx.user)
                .bind(c.to_string())
                .fetch_all(&mut *tx)
                .await?
            }
        };
        tx.commit().await?;
        // A full page of changes ends at its last change, which the next answer starts from again.
        let next = match (parsed.filter(|_| !resync), rows.len() as i64 == PAGE) {
            (Some(_), true) => rows.last().map(|(_, c)| c.clone()).unwrap_or(boundary),
            _ => boundary,
        };
        Ok(json!({"runs": rows.into_iter().map(|(r, _)| r).collect::<Vec<_>>(), "cursor": next, "resync": resync}))
    }

    /// The processes about a record, for its page, to whoever may read the record: their state and the tasks
    /// they wait on, not what those tasks are about.
    pub async fn logic_runs_about(&self, ctx: &Ctx, entity: &str, record: Uuid) -> Result<Vec<Value>> {
        self.get(ctx, entity, record, false).await?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', l.id, 'solution', l.solution, 'process', l.process, 'status', l.status, 'accepted_at', l.accepted_at, \
               'finished_at', l.finished_at, 'waiting', coalesce((select jsonb_agg(jsonb_build_object('kind', t.kind, 'label', t.label, 'role', t.role, \
                  'deadline', t.deadline) order by t.opened_at) from logic_task t where t.tenant_id = l.tenant_id and t.run_id = l.id and t.status = 'open'), '[]'::jsonb)) \
             from logic_run l where l.tenant_id = $1 and l.subject_entity = $2 and l.subject_id = $3 order by l.accepted_at desc limit 50",
        )
        .bind(ctx.tenant)
        .bind(entity)
        .bind(record)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Whether `ctx` may decide a task now: its role, not excluded, not a decision already given, and when it was
    /// handed on, the person it was handed to.
    fn may_decide(ctx: &Ctx, role: &str, exclude: &[Uuid], delegated: &[Uuid]) -> Result<()> {
        if exclude.contains(&ctx.user) {
            return Err(Error::Forbidden("someone else decides this: it is your own request".into()));
        }
        if let Some(to) = delegated.last() {
            return if *to == ctx.user { Ok(()) } else { Err(Error::Forbidden("this task was handed to someone else".into())) };
        }
        // The solution's role decides, administrators included: being one is not a second pair of eyes.
        if !role.is_empty() && !ctx.roles.iter().any(|r| r == role) {
            return Err(Error::Forbidden(format!("this task is for the {role} role")));
        }
        Ok(())
    }

    /// The open tasks `ctx` may decide.
    pub async fn my_logic_tasks(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<(Uuid, String, String, String, Vec<Uuid>, Vec<Uuid>, Option<DateTime<Utc>>)> = sqlx::query_as(
            "select id, kind, label, role, exclude, delegated, deadline from logic_task where tenant_id = $1 and status = 'open' order by opened_at limit 500",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .filter(|(_, kind, _, role, exclude, delegated, _)| match kind.as_str() {
                // What needs recovering or looking at is an administrator's.
                "recovery" | "attention" => ctx.is_admin,
                _ => Self::may_decide(ctx, role, exclude, delegated).is_ok(),
            })
            .map(|(id, kind, label, role, _, _, deadline)| json!({"id": id, "kind": kind, "label": label, "role": role, "deadline": deadline}))
            .collect())
    }

    /// One task with what it is about, for someone who may decide it or an administrator: another task's material
    /// is not theirs to read (FLC-HUMAN-006).
    pub async fn logic_task(&self, ctx: &Ctx, id: Uuid) -> Result<Value> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(Value, String, Vec<Uuid>, Vec<Uuid>)> = sqlx::query_as(
            "select jsonb_build_object('id', id, 'run', run_id, 'activation', activation, 'kind', kind, 'label', label, 'role', role, 'material', material, \
               'digest', digest, 'form', form, 'approvals', approvals, 'status', status, 'deadline', deadline, 'deadline_basis', deadline_basis, 'detail', detail, \
               'delegation', delegation, 'handed_on', greatest(cardinality(delegated) - 1, 0)), \
               role, exclude, delegated from logic_task where tenant_id = $1 and id = $2",
        )
        .bind(ctx.tenant)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((mut task, role, exclude, delegated)) = row else { return Err(Error::NotFound("task".into())) };
        if !ctx.is_admin && Self::may_decide(ctx, &role, &exclude, &delegated).is_err() {
            return Err(Error::NotFound("task".into()));
        }
        // Who did what on it, and why: the record a closed task is read by.
        let history: Value = sqlx::query_scalar(
            "select coalesce(jsonb_agg(jsonb_build_object('decision', d.decision, 'part', d.part, 'reason', d.reason, 'at', d.decided_at, \
               'by', coalesce(nullif(u.full_name, ''), u.email::text, d.by_user::text), \
               'to', case when d.to_user is null then null else coalesce(nullif(t.full_name, ''), t.email::text, d.to_user::text) end) \
               order by d.decided_at, d.id), '[]') \
             from logic_task_decision d \
             left join app_user u on u.tenant_id = d.tenant_id and u.id = d.by_user \
             left join app_user t on t.tenant_id = d.tenant_id and t.id = d.to_user \
             where d.tenant_id = $1 and d.task_id = $2",
        )
        .bind(ctx.tenant)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        task["history"] = history;
        Ok(task)
    }

    /// Decides a person's task, on the material shown to them: approved once enough different people approve,
    /// rejected by one. A task that has timed out, or been decided, is not decided again.
    pub async fn decide_logic_task(&self, ctx: &Ctx, id: Uuid, approve: bool, shown: &str, values: &Value, reason: &str) -> Result<Value> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(Uuid, String, String, String, i32, Vec<Uuid>, Vec<Uuid>, String)> =
            sqlx::query_as("select run_id, activation, role, digest, approvals, exclude, delegated, status from logic_task where tenant_id = $1 and id = $2 and kind = 'task' for update")
                .bind(ctx.tenant)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((run, activation, role, digest, approvals, exclude, delegated, status)) = row else { return Err(Error::NotFound("task".into())) };
        Self::may_decide(ctx, &role, &exclude, &delegated)?;
        if status != "open" {
            return Err(Error::Conflict(format!("this task is {} already", status.replace('_', " "))));
        }
        if shown != digest {
            return Err(refusal(Code::StaleApproval, "what you decided on is not what the task is about now: open it again"));
        }
        let form: Value =
            sqlx::query_scalar("select form from logic_task where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).fetch_one(&mut *tx).await?;
        if approve {
            filled(&form, values)?;
        }
        let decision_id: Option<Uuid> = sqlx::query_scalar(
            "insert into logic_task_decision (tenant_id, task_id, by_user, decision, digest, values, reason) values ($1, $2, $3, $4, $5, $6, $7) \
             on conflict do nothing returning id",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(ctx.user)
        .bind(if approve { "approve" } else { "reject" })
        .bind(shown)
        .bind(values)
        .bind(reason)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(decision_id) = decision_id else { return Err(Error::Conflict("you have decided this task already".into())) };
        let approved_by: i64 =
            sqlx::query_scalar("select count(distinct by_user) from logic_task_decision where tenant_id = $1 and task_id = $2 and decision = 'approve'")
                .bind(ctx.tenant)
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        let outcome = if !approve {
            Some(false)
        } else if approved_by >= i64::from(approvals) {
            Some(true)
        } else {
            None
        };
        if let Some(approved) = outcome {
            sqlx::query("update logic_task set status = $3, closed_at = now(), version = version + 1 where tenant_id = $1 and id = $2")
                .bind(ctx.tenant)
                .bind(id)
                .bind(if approved { "approved" } else { "rejected" })
                .execute(&mut *tx)
                .await?;
            let event = Event::TaskDecided {
                activation,
                approved,
                by: ctx.user.to_string(),
                decision_id: decision_id.to_string(),
                at: Utc::now().to_rfc3339(),
                values: values.clone(),
            };
            deliver(&mut tx, ctx.tenant, run, &event).await?;
        }
        tx.commit().await?;
        Ok(json!({"decision": decision_id, "closed": outcome.is_some(), "approvals": approved_by, "needed": approvals}))
    }

    /// Hands a task on to another person who could decide it, within the task's bound, never back along its chain.
    /// A member of the workspace by email, to hand a task on by the address people know each other by. Members
    /// see each other already (mentions, sharing), so this tells nobody anything new.
    pub async fn member_by_email(&self, ctx: &Ctx, email: &str) -> Result<Uuid> {
        let email = email.trim();
        if email.is_empty() || email.len() > 254 {
            return Err(Error::field("to", "is an email of someone in this workspace"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let id: Option<Uuid> =
            sqlx::query_scalar("select id from app_user where tenant_id = $1 and lower(email::text) = lower($2) and is_active and kind = 'member'")
                .bind(ctx.tenant)
                .bind(email)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        id.ok_or_else(|| Error::field("to", "nobody in this workspace has that email"))
    }

    pub async fn delegate_logic_task(&self, ctx: &Ctx, id: Uuid, to: Uuid, reason: &str) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(String, i32, Vec<Uuid>, Vec<Uuid>, String)> =
            sqlx::query_as("select role, delegation, exclude, delegated, status from logic_task where tenant_id = $1 and id = $2 and kind = 'task' for update")
                .bind(ctx.tenant)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((role, bound, exclude, delegated, status)) = row else { return Err(Error::NotFound("task".into())) };
        Self::may_decide(ctx, &role, &exclude, &delegated)?;
        if status != "open" {
            return Err(Error::Conflict("this task is closed".into()));
        }
        // The chain holds whoever handed it on first and each person it went to: one fewer hand-ons than people.
        if delegated.len().saturating_sub(1) as i32 >= bound {
            return Err(Error::Forbidden(format!("this task may be handed on at most {bound} times")));
        }
        if to == ctx.user || delegated.contains(&to) || exclude.contains(&to) {
            return Err(Error::Forbidden("it cannot go to that person: they handed it on, asked for it, or hold it already".into()));
        }
        let target = self.load_ctx(ctx.tenant, to, None, "delegate".into()).await.map_err(|_| Error::Forbidden("that person cannot act here".into()))?;
        if !role.is_empty() && !target.roles.iter().any(|r| r == &role) {
            return Err(Error::Forbidden(format!("that person does not hold the {role} role")));
        }
        let mut chain = delegated.clone();
        if chain.is_empty() {
            chain.push(ctx.user);
        }
        chain.push(to);
        sqlx::query("update logic_task set delegated = $3, version = version + 1 where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(id)
            .bind(&chain)
            .execute(&mut *tx)
            .await?;
        sqlx::query("insert into logic_task_decision (tenant_id, task_id, by_user, decision, to_user, reason) values ($1, $2, $3, 'delegate', $4, $5)")
            .bind(ctx.tenant)
            .bind(id)
            .bind(ctx.user)
            .bind(to)
            .bind(reason)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Records one of a case's listed tasks as done; a task the case does not list, or whose dependencies are not
    /// done, is refused: a case owner directs listed work, never new work (FLC-HUMAN-007).
    pub async fn case_task_done(&self, ctx: &Ctx, id: Uuid, part: &str, values: &Value) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(Uuid, String, Value, Vec<Uuid>, String)> =
            sqlx::query_as("select run_id, activation, detail, exclude, status from logic_task where tenant_id = $1 and id = $2 and kind = 'case' for update")
                .bind(ctx.tenant)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((run, activation, detail, exclude, status)) = row else { return Err(Error::NotFound("case".into())) };
        if status != "open" {
            return Err(Error::Conflict("this case is closed".into()));
        }
        let tasks = detail.get("tasks").and_then(Value::as_array).cloned().unwrap_or_default();
        let Some(task) = tasks.iter().find(|t| t["id"] == part) else {
            return Err(Error::Forbidden(format!("{part} is not one of this case's tasks: other work needs a new reviewed definition")));
        };
        let role = task["role"].as_str().unwrap_or_default();
        Self::may_decide(ctx, role, &exclude, &[])?;
        let done: Vec<String> = sqlx::query_scalar("select part from logic_task_decision where tenant_id = $1 and task_id = $2 and decision = 'done'")
            .bind(ctx.tenant)
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
        if let Some(missing) = task["requires"].as_array().into_iter().flatten().filter_map(Value::as_str).find(|r| !done.iter().any(|d| d == r)) {
            return Err(Error::Conflict(format!("{part} waits for {missing}")));
        }
        let decision: Option<Uuid> = sqlx::query_scalar(
            "insert into logic_task_decision (tenant_id, task_id, by_user, decision, part, values) values ($1, $2, $3, 'done', $4, $5) on conflict do nothing returning id",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(ctx.user)
        .bind(part)
        .bind(values)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(decision) = decision else { return Err(Error::Conflict(format!("{part} is done already"))) };
        let event = Event::CaseTaskDone {
            activation,
            task: part.into(),
            by: ctx.user.to_string(),
            decision_id: decision.to_string(),
            at: Utc::now().to_rfc3339(),
            values: values.clone(),
        };
        deliver(&mut tx, ctx.tenant, run, &event).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Closes a case, by its owner, once its required tasks are done.
    pub async fn close_case(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(Uuid, String, String, Value, Vec<Uuid>, String)> = sqlx::query_as(
            "select run_id, activation, role, detail, exclude, status from logic_task where tenant_id = $1 and id = $2 and kind = 'case' for update",
        )
        .bind(ctx.tenant)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((run, activation, owner, detail, exclude, status)) = row else { return Err(Error::NotFound("case".into())) };
        if !ctx.roles.contains(&owner) || exclude.contains(&ctx.user) {
            return Err(Error::Forbidden(format!("the case is the {owner} role's to close, and not whoever asked for it")));
        }
        if status != "open" {
            return Err(Error::Conflict("this case is closed".into()));
        }
        let done: Vec<String> = sqlx::query_scalar("select part from logic_task_decision where tenant_id = $1 and task_id = $2 and decision = 'done'")
            .bind(ctx.tenant)
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
        let tasks = detail.get("tasks").and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some(missing) = tasks.iter().filter(|t| t["required"] == true).filter_map(|t| t["id"].as_str()).find(|t| !done.iter().any(|d| d == t)) {
            return Err(Error::Conflict(format!("{missing} is required and not done")));
        }
        sqlx::query("update logic_task set status = 'done', closed_at = now(), version = version + 1 where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("insert into logic_task_decision (tenant_id, task_id, by_user, decision) values ($1, $2, $3, 'close')")
            .bind(ctx.tenant)
            .bind(id)
            .bind(ctx.user)
            .execute(&mut *tx)
            .await?;
        let recorded = case_done(&mut tx, ctx.tenant, id).await?;
        deliver(&mut tx, ctx.tenant, run, &Event::CaseClosed { activation, done: recorded }).await?;
        tx.commit().await?;
        Ok(())
    }

    /// An administrator's recovery of a suspended step, with a reason: resume where its source allows, or end it
    /// refused. There is no way to mark it a success (FLC-HUMAN-008).
    pub async fn recover_logic_task(&self, ctx: &Ctx, id: Uuid, resume: bool, reason: &str) -> Result<()> {
        require_admin(ctx)?;
        if reason.trim().is_empty() {
            return Err(Error::field("reason", "say what was done about it"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(Uuid, String, Value, String)> =
            sqlx::query_as("select run_id, activation, detail, status from logic_task where tenant_id = $1 and id = $2 and kind = 'recovery' for update")
                .bind(ctx.tenant)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((run, activation, detail, status)) = row else { return Err(Error::NotFound("recovery".into())) };
        if status != "open" {
            return Err(Error::Conflict("this has been recovered already".into()));
        }
        if resume && detail.get("resumable") != Some(&Value::Bool(true)) {
            return Err(Error::Forbidden("this step has nowhere to resume: it can only end".into()));
        }
        sqlx::query("update logic_task set status = 'resolved', closed_at = now(), version = version + 1 where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("insert into logic_task_decision (tenant_id, task_id, by_user, decision, reason) values ($1, $2, $3, $4, $5)")
            .bind(ctx.tenant)
            .bind(id)
            .bind(ctx.user)
            .bind(if resume { "resume" } else { "end" })
            .bind(reason.trim())
            .execute(&mut *tx)
            .await?;
        deliver(&mut tx, ctx.tenant, run, &Event::Recovered { activation, by: ctx.user.to_string(), reason: reason.trim().into(), resume }).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Acknowledges something a run handed to a person (an effect that settled when no longer wanted, a failed
    /// compensation), with what was done about it.
    pub async fn acknowledge_logic_attention(&self, ctx: &Ctx, id: Uuid, reason: &str) -> Result<()> {
        require_admin(ctx)?;
        if reason.trim().is_empty() {
            return Err(Error::field("reason", "say what was done about it"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("update logic_task set status = 'resolved', closed_at = now(), version = version + 1 where tenant_id = $1 and id = $2 and kind = 'attention' and status = 'open'")
            .bind(ctx.tenant)
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("open attention item".into()));
        }
        sqlx::query("insert into logic_task_decision (tenant_id, task_id, by_user, decision, reason) values ($1, $2, $3, 'acknowledge', $4)")
            .bind(ctx.tenant)
            .bind(id)
            .bind(ctx.user)
            .bind(reason.trim())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Asks a run to stop (an administrator, or whoever started it).
    pub async fn cancel_logic_run(&self, ctx: &Ctx, run: Uuid, reason: &str) -> Result<()> {
        if reason.trim().is_empty() {
            return Err(Error::field("reason", "say why it stops"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let requester: Option<Option<Uuid>> = sqlx::query_scalar("select requester from logic_run where tenant_id = $1 and id = $2 and finished_at is null")
            .bind(ctx.tenant)
            .bind(run)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(requester) = requester else { return Err(Error::NotFound("open run".into())) };
        if !ctx.is_admin && requester != Some(ctx.user) {
            return Err(Error::Forbidden("an administrator or whoever started the run stops it".into()));
        }
        deliver(&mut tx, ctx.tenant, run, &Event::CancelRequested { by: ctx.user.to_string(), reason: reason.trim().into() }).await?;
        crate::audit::record(&mut tx, ctx, "LogicRun", run, "cancel", None, json!({"reason": reason.trim()})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Sets a business calendar (administrators); each change is a new revision.
    pub async fn set_logic_calendar(&self, ctx: &Ctx, cal: &Calendar) -> Result<i64> {
        require_admin(ctx)?;
        let mut chars = cal.name.chars();
        if cal.name.len() > 64
            || !chars.next().is_some_and(|c| c.is_ascii_lowercase())
            || !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(Error::field("name", "lower_snake_case, at most 64 characters"));
        }
        if let Some(p) = cal.problem() {
            return Err(Error::field("calendar", p));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let holidays: Vec<chrono::NaiveDate> = cal.holidays.iter().copied().collect();
        let revision: i64 = sqlx::query_scalar(
            "insert into logic_calendar (tenant_id, name, zone, weekends, holidays, updated_by) values ($1, $2, $3, $4, $5, $6) \
             on conflict (tenant_id, name) do update set zone = $3, weekends = $4, holidays = $5, updated_by = $6, revision = logic_calendar.revision + 1, updated_at = now() \
             returning revision",
        )
        .bind(ctx.tenant)
        .bind(&cal.name)
        .bind(&cal.zone)
        .bind(&cal.weekends)
        .bind(&holidays)
        .bind(ctx.user)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(revision)
    }
}

/// Whether a decision fills the task's form: its required fields given, each of its type, choices among the
/// options, and nothing the form does not ask.
fn filled(form: &Value, values: &Value) -> Result<()> {
    let fields: Vec<ferrum_logic::solution::FormField> = serde_json::from_value(form.clone()).unwrap_or_default();
    let given = values.as_object().cloned().unwrap_or_default();
    let mut errors = vec![];
    for f in &fields {
        let v = given.get(&f.name).filter(|v| !v.is_null());
        let ty = parameter_type(&f.ty).unwrap_or(Ty::Any);
        match v {
            None if f.required => errors.push(crate::error::FieldError::new(&f.name, "is required")),
            None => {}
            Some(v) => {
                if Val::from_json(v, &ty).is_unknown() {
                    errors.push(crate::error::FieldError::new(&f.name, format!("must be {ty}")));
                } else if !f.options.is_empty() && !v.as_str().is_some_and(|s| f.options.iter().any(|o| o == s)) {
                    errors.push(crate::error::FieldError::new(&f.name, format!("must be one of {}", f.options.join(", "))));
                }
            }
        }
    }
    for k in given.keys().filter(|k| !fields.iter().any(|f| &f.name == *k)) {
        errors.push(crate::error::FieldError::new(k, "is not asked by this task"));
    }
    if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
}

/// A case's finished tasks as their outputs: what each was filled in with, and who did it when.
async fn case_done(tx: &mut Tx, tenant: Uuid, task: Uuid) -> Result<Value> {
    let rows: Vec<(String, Value, Uuid, Uuid, DateTime<Utc>)> =
        sqlx::query_as("select part, values, by_user, id, decided_at from logic_task_decision where tenant_id = $1 and task_id = $2 and decision = 'done'")
            .bind(tenant)
            .bind(task)
            .fetch_all(&mut **tx)
            .await?;
    Ok(Value::Object(
        rows.into_iter()
            .map(|(part, values, by, id, at)| {
                let mut out = values.as_object().cloned().unwrap_or_default();
                out.insert("decision".into(), json!("done"));
                out.insert("decision_id".into(), json!(id));
                out.insert("by".into(), json!(by));
                out.insert("at".into(), json!(at.to_rfc3339()));
                (part, Value::Object(out))
            })
            .collect(),
    ))
}

/// Queues something that happened for a run, in the transaction that made it so: its runtime hands it over.
pub async fn deliver(tx: &mut Tx, tenant: Uuid, run: Uuid, event: &Event) -> Result<()> {
    outbox::enqueue(tx, tenant, DELIVER_TOPIC, None, None, json!({"run": run, "event": event})).await
}
