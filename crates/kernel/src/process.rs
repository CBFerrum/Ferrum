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
//!   release active at that moment, and its start is queued with it (FLC-PROC-001).
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
use ferrum_logic::compile::{Release, parameter_type};
use ferrum_logic::expr::{self, Env, Facts, Ty, Val};
use ferrum_logic::process::{Event, Invoked, Run};
use ferrum_logic::refusal::{Code, Refusal};
use ferrum_logic::solution::Node;
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
    let version = k.registry.schema(tenant).await?.version;
    if let Some(found) = LISTENING.get(&tenant)
        && found.0 == version
    {
        return Ok(found.1.clone());
    }
    let mut l = Listening::default();
    let mut tx = k.db.tenant_tx(tenant).await?;
    let releases = crate::logic::active_releases(&mut tx, tenant, None).await?;
    tx.commit().await?;
    for (origin, release) in releases {
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
    let l = Arc::new(l);
    if LISTENING.get(&tenant).is_none_or(|e| e.0 <= version) {
        LISTENING.insert(tenant, (version, l.clone()));
    }
    Ok(l)
}

/// The catalog's id of an outbox event (`procurement.material_request.submitted` for `MaterialRequest.submitted`),
/// what it is about, and its payload as a run reads it.
fn identify(schema: &ferrum_meta::Schema, ev: &OutboxEvent) -> (String, String, Value) {
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
        let l = listening(self, tenant).await?;
        let (event, subject, payload) = identify(&schema, ev);
        let starts = l.triggers.get(&event).map(|v| v.as_slice()).unwrap_or_default();
        if starts.is_empty() && !l.waits.contains(&event) {
            return Ok(());
        }
        let mut tx = self.db.tenant_tx(tenant).await?;
        // An event delivered again (the outbox delivers at least once) starts and hands over nothing more.
        if !outbox::first_time(&mut tx, tenant, "ferrum.logic", ev.id).await? {
            tx.commit().await?;
            return Ok(());
        }
        for s in starts {
            let env = Env::default().with("trigger", s.trigger.clone()).with("parameters", s.params.0.clone());
            let facts = Facts::default().with("trigger", Val::from_json(&payload, &s.trigger)).with("parameters", s.params.1.clone());
            let key = match expr::compile(&s.business_key, &env).map(|c| c.eval(&facts)) {
                Ok(Val::Text(t)) if !t.is_empty() => t,
                Ok(Val::Int(i)) => i.to_string(),
                other => {
                    tracing::warn!(%tenant, process = %s.process, event = ev.id, "no run started: the business key came out as {other:?}");
                    continue;
                }
            };
            let id = Uuid::now_v7();
            let accepted: Option<Uuid> = sqlx::query_scalar(
                "insert into logic_run (tenant_id, id, release_id, solution, process, business_key, trigger, trigger_event, requester, workflow_id) \
                 values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) on conflict (tenant_id, solution, process, business_key) do nothing returning id",
            )
            .bind(tenant)
            .bind(id)
            .bind(s.release)
            .bind(&s.solution)
            .bind(&s.process)
            .bind(&key)
            .bind(&payload)
            .bind(ev.id)
            .bind(ev.actor)
            .bind(workflow_id(tenant, id))
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(run) = accepted {
                outbox::enqueue(&mut tx, tenant, START_TOPIC, None, None, json!({"run": run})).await?;
            }
        }
        if l.waits.contains(&event) && !subject.is_empty() {
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
        let ctx = match self.run_ctx(tenant, run).await {
            Ok(c) => c,
            Err(e) => return Ok(Invoked::Refused { code: e.code().into(), message: e.to_string() }),
        };
        if let Some(a) = approval
            && let Err(e) = self.approval_holds(tenant, run, a).await
        {
            return Ok(Invoked::Refused { code: e.code().into(), message: e.to_string() });
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
            let now = self.get_tx(&sys, &mut tx, &entity, id, false).await.ok().and_then(|d| d.get("version").and_then(Value::as_i64));
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
        let out = if status == "open" {
            sqlx::query("update logic_task set status = 'timed_out', closed_at = now(), version = version + 1 where tenant_id = $1 and id = $2")
                .bind(tenant)
                .bind(task)
                .execute(&mut *tx)
                .await?;
            Some(if kind == "case" { Event::CaseTimedOut { activation: activation.into() } } else { Event::TaskTimedOut { activation: activation.into() } })
        } else {
            // Decided first: that decision is the outcome. Cancelled: nothing to tell.
            self.decided_event(&mut tx, tenant, task, activation, &kind).await?
        };
        tx.commit().await?;
        Ok(out)
    }

    /// What a closed task told its run, rebuilt from its decisions.
    async fn decided_event(&self, tx: &mut Tx, tenant: Uuid, task: Uuid, activation: &str, kind: &str) -> Result<Option<Event>> {
        let last: Option<(Uuid, Uuid, String, Value, DateTime<Utc>)> = sqlx::query_as(
            "select id, by_user, decision, values, decided_at from logic_task_decision where tenant_id = $1 and task_id = $2 and decision in ('approve', 'reject', 'close') \
             order by decided_at desc, id desc limit 1",
        )
        .bind(tenant)
        .bind(task)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(last.map(|(id, by, decision, values, at)| {
            if kind == "case" {
                Event::CaseClosed { activation: activation.into() }
            } else {
                Event::TaskDecided {
                    activation: activation.into(),
                    approved: decision == "approve",
                    by: by.to_string(),
                    decision_id: id.to_string(),
                    at: at.to_rfc3339(),
                    values,
                }
            }
        }))
    }

    /// Registers what a run waits for; an event that came already is handed over at once, once.
    pub async fn process_register_wait(&self, tenant: Uuid, run: Uuid, activation: &str, event: &str, value: &Value, timeout: &Value) -> Result<Waited> {
        let deadline = self.process_deadline(tenant, timeout, Utc::now()).await?.at;
        let key = correlation(value);
        let mut tx = self.db.tenant_tx(tenant).await?;
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
               'digest', digest, 'form', form, 'approvals', approvals, 'status', status, 'deadline', deadline, 'deadline_basis', deadline_basis, 'detail', detail), \
               role, exclude, delegated from logic_task where tenant_id = $1 and id = $2",
        )
        .bind(ctx.tenant)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let Some((task, role, exclude, delegated)) = row else { return Err(Error::NotFound("task".into())) };
        if !ctx.is_admin && Self::may_decide(ctx, &role, &exclude, &delegated).is_err() {
            return Err(Error::NotFound("task".into()));
        }
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
        deliver(&mut tx, ctx.tenant, run, &Event::CaseClosed { activation }).await?;
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

/// Queues something that happened for a run, in the transaction that made it so: its runtime hands it over.
pub async fn deliver(tx: &mut Tx, tenant: Uuid, run: Uuid, event: &Event) -> Result<()> {
    outbox::enqueue(tx, tenant, DELIVER_TOPIC, None, None, json!({"run": run, "event": event})).await
}
