//! Authoring solutions (docs/logic/studio.md): drafts people edit, checks and simulations that keep nothing,
//! and what a reviewer sees. Releases, reviews and activation are `logic.rs`'s; runs and tasks `process.rs`'s.
//!
//! * A draft is saved against the version its editor read: a stale save is refused with what is there now, so
//!   two editors never overwrite each other (FLC-UI-006). A draft is not a release, and nothing runs from one.
//! * A simulation runs in the state machine the runtime drives, on a virtual clock, against the workspace's
//!   calendars, and reaches nothing (FLC-UI-007, FLC-UI-010).
//! * A shadow comparison replays what happened against a candidate and what runs now, both simulated with the
//!   answers, decisions and events met then, and says case by case what differs; nothing runs (FLC-MIG-002).
//! * A release leaves as a pack (its source, digests, what it needs and the evidence of its review, never a
//!   secret) and arrives in another workspace as a draft, with what must be mapped there (FLC-OPS-008).
//! * A reviewer sees the release, what runs now, and what changed between them by meaning; the changing
//!   capabilities the release may use that what runs now cannot are consented to by name (FLC-UI-005).
//!
//! Administrators and holders of [`AUTHOR_ROLE`] author; reviewing and activating stay administrators', and
//! nobody reviews their own release.

use crate::Kernel;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use ferrum_logic::calendar::Calendar;
use ferrum_logic::compile::Release;
use ferrum_logic::diff;
use ferrum_logic::process::Invoked;
use ferrum_logic::simulate::{self, Decision, Scenario, Signal, Simulation};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// Studio's role for people who build solutions without administering the workspace: they edit drafts,
/// check, simulate and keep releases; they neither review nor activate. Granted until a date, it ends then.
pub const AUTHOR_ROLE: &str = "Solution Author";
/// The largest source a draft holds.
pub const MAX_SOURCE: usize = 1_048_576;
/// What a solution pack says it is.
pub const PACK_FORMAT: &str = "ferrum.solution-pack/1";

/// The events and calendars a source names.
fn named(v: &Value, events: &mut BTreeSet<String>, calendars: &mut BTreeSet<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                match (k.as_str(), x.as_str()) {
                    ("event", Some(e)) if !e.is_empty() => {
                        events.insert(e.to_string());
                    }
                    ("calendar", Some(c)) if !c.is_empty() => {
                        calendars.insert(c.to_string());
                    }
                    _ => named(x, events, calendars),
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| named(x, events, calendars)),
        _ => {}
    }
}

pub(crate) fn require_author(ctx: &Ctx) -> Result<()> {
    if ctx.has_role(AUTHOR_ROLE) { Ok(()) } else { Err(Error::Forbidden(format!("solutions are edited by administrators and the {AUTHOR_ROLE} role"))) }
}

/// A draft's solution id and label, read loosely: a draft may not parse while it is being written.
fn identity(source: &str) -> (String, String) {
    let v: Value = serde_json::from_str(source).unwrap_or(Value::Null);
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(|s| s.chars().take(200).collect()).unwrap_or_default();
    (text("id"), text("label"))
}

fn sized(source: &str) -> Result<()> {
    if source.len() > MAX_SOURCE {
        return Err(Error::field("source", format!("a solution's source is at most {MAX_SOURCE} bytes")));
    }
    Ok(())
}

impl Kernel {
    /// The workspace's drafts, the latest first.
    pub async fn logic_drafts(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        require_author(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', id, 'solution', solution, 'label', label, 'version', version, 'created_by', created_by, \
               'updated_by', updated_by, 'updated_at', updated_at) from logic_draft where tenant_id = $1 order by updated_at desc limit 500",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    pub async fn create_logic_draft(&self, ctx: &Ctx, source: &str) -> Result<Value> {
        require_author(ctx)?;
        sized(source)?;
        let (solution, label) = identity(source);
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let id: Uuid = sqlx::query_scalar(
            "insert into logic_draft (tenant_id, solution, label, source, created_by, updated_by) values ($1, $2, $3, $4, $5, $5) returning id",
        )
        .bind(ctx.tenant)
        .bind(&solution)
        .bind(&label)
        .bind(source)
        .bind(ctx.user)
        .fetch_one(&mut *tx)
        .await?;
        crate::audit::record(&mut tx, ctx, "LogicDraft", id, "create", Some(1), json!({"solution": solution})).await?;
        tx.commit().await?;
        Ok(json!({"id": id, "version": 1, "solution": solution, "label": label}))
    }

    pub async fn logic_draft(&self, ctx: &Ctx, id: Uuid) -> Result<Value> {
        require_author(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', id, 'solution', solution, 'label', label, 'source', source, 'version', version, \
               'created_by', created_by, 'updated_by', updated_by, 'updated_at', updated_at) from logic_draft where tenant_id = $1 and id = $2",
        )
        .bind(ctx.tenant)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        row.ok_or_else(|| Error::NotFound("draft".into()))
    }

    /// Saves a draft against the version its editor read. Another save in between refuses this one, with the
    /// version and who saved it, so the editor compares and decides instead of losing either.
    pub async fn save_logic_draft(&self, ctx: &Ctx, id: Uuid, expected: i32, source: &str) -> Result<Value> {
        require_author(ctx)?;
        sized(source)?;
        let (solution, label) = identity(source);
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let saved: Option<i32> = sqlx::query_scalar(
            "update logic_draft set source = $4, solution = $5, label = $6, version = version + 1, updated_by = $7, updated_at = now() \
             where tenant_id = $1 and id = $2 and version = $3 returning version",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(expected)
        .bind(source)
        .bind(&solution)
        .bind(&label)
        .bind(ctx.user)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(version) = saved else {
            // Who saved it, by name: the person reading this decides whether to talk to them first.
            let now: Option<(i32, Option<String>, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
                "select d.version, coalesce(nullif(u.full_name, ''), u.email::text), d.updated_at from logic_draft d \
                 left join app_user u on u.tenant_id = d.tenant_id and u.id = d.updated_by where d.tenant_id = $1 and d.id = $2",
            )
            .bind(ctx.tenant)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            return Err(match now {
                None => Error::NotFound("draft".into()),
                Some((v, by, at)) => Error::Conflict(format!(
                    "this draft changed since you opened it: it is at version {v}, saved by {} at {}; open it again and compare before saving",
                    by.unwrap_or_else(|| "someone".into()),
                    at.format("%-d %b %Y %H:%M UTC")
                )),
            });
        };
        crate::audit::record(&mut tx, ctx, "LogicDraft", id, "update", Some(version), json!({"solution": solution})).await?;
        tx.commit().await?;
        Ok(json!({"id": id, "version": version}))
    }

    pub async fn delete_logic_draft(&self, ctx: &Ctx, id: Uuid, expected: i32) -> Result<()> {
        require_author(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("delete from logic_draft where tenant_id = $1 and id = $2 and version = $3")
            .bind(ctx.tenant)
            .bind(id)
            .bind(expected)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::Conflict("this draft changed since you opened it, or is gone: open it again".into()));
        }
        crate::audit::record(&mut tx, ctx, "LogicDraft", id, "delete", Some(expected), json!({})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Keeps the draft as it was at `expected` as an immutable release.
    pub async fn release_logic_draft(&self, ctx: &Ctx, id: Uuid, expected: i32) -> Result<Value> {
        let draft = self.logic_draft(ctx, id).await?;
        if draft["version"].as_i64() != Some(i64::from(expected)) {
            return Err(Error::Conflict(format!("this draft is at version {} now, not {expected}: open it again", draft["version"])));
        }
        self.save_release(ctx, draft["source"].as_str().unwrap_or_default()).await
    }

    /// A release to try: a source compiled here and kept nowhere, or a kept release.
    async fn candidate_release(&self, ctx: &Ctx, source: Option<&str>, release: Option<Uuid>) -> Result<Release> {
        match (source, release) {
            (Some(src), _) => {
                sized(src)?;
                self.compile_solution(ctx, src).await?.map_err(crate::logic::diagnostics)
            }
            (None, Some(id)) => {
                let mut tx = self.db.tenant_tx(ctx.tenant).await?;
                let stored: Option<Value> = sqlx::query_scalar("select release from logic_release where tenant_id = $1 and id = $2")
                    .bind(ctx.tenant)
                    .bind(id)
                    .fetch_optional(&mut *tx)
                    .await?;
                tx.commit().await?;
                serde_json::from_value(stored.ok_or_else(|| Error::NotFound("release".into()))?).map_err(Error::internal)
            }
            (None, None) => Err(Error::field("source", "give a source or a release")),
        }
    }

    /// The workspace's calendars, as a scenario runs on them.
    async fn calendars(&self, ctx: &Ctx) -> Result<Vec<Calendar>> {
        Ok(self.logic_calendars(ctx).await?.into_iter().filter_map(|c| serde_json::from_value(c).ok()).collect())
    }

    /// Runs a scenario against a source (compiled here, kept nowhere) or a kept release, on a virtual clock.
    pub async fn simulate_solution(&self, ctx: &Ctx, source: Option<&str>, release: Option<Uuid>, scenario: Value) -> Result<Value> {
        require_author(ctx)?;
        let release = self.candidate_release(ctx, source, release).await?;
        let mut scenario: Scenario = serde_json::from_value(scenario).map_err(|e| Error::field("scenario", e.to_string()))?;
        // The workspace's calendars, unless the scenario brings its own of the same name.
        for c in self.calendars(ctx).await? {
            if !scenario.calendars.iter().any(|x| x.name == c.name) {
                scenario.calendars.push(c);
            }
        }
        let sim = simulate::simulate(&release, &scenario).map_err(|e| Error::field("scenario", e))?;
        Ok(json!({"release": {"solution": release.solution, "version": release.version, "artifact_digest": release.artifact_digest}, "simulation": sim}))
    }

    /// A candidate compared with what runs now, on what happened, running nothing (FLC-MIG-002). Each of the
    /// last runs of its solution (`last`, 1 to 200) is replayed against both releases with the answers,
    /// decisions and events that run met; a solution imported from an automation that has not taken over yet
    /// is compared, event by event, with what the automation did. Both sides are simulations: nothing is
    /// invoked, changed or kept, and what differs (the outcome, the steps taken, the effects asked for) is
    /// said case by case. A step the candidate takes that never ran before has no answer on record: `assumed`
    /// answers it, by activation or by capability id, only where history has none, and each case lists the
    /// assumptions it used; without one the step says it was not modelled.
    pub async fn shadow_solution(
        &self,
        ctx: &Ctx,
        source: Option<&str>,
        release: Option<Uuid>,
        last: i64,
        assumed: &BTreeMap<String, Invoked>,
    ) -> Result<Value> {
        require_author(ctx)?;
        let candidate = self.candidate_release(ctx, source, release).await?;
        let calendars = self.calendars(ctx).await?;
        let about = json!({"solution": candidate.solution, "version": candidate.version, "artifact_digest": candidate.artifact_digest});
        let scenario = |process: String, trigger: Value, start: chrono::DateTime<chrono::Utc>| Scenario {
            process,
            trigger,
            requester: None,
            start,
            results: Default::default(),
            decisions: Default::default(),
            signals: Default::default(),
            cases: Default::default(),
            recoveries: Default::default(),
            calendars: calendars.clone(),
        };
        let last = last.clamp(1, 200);
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let running: Option<(Uuid, String, Value)> = sqlx::query_as(
            "select r.id, r.version, r.release from logic_activation a join logic_release r on r.tenant_id = a.tenant_id and r.id = a.release_id \
             where a.tenant_id = $1 and a.solution = $2",
        )
        .bind(ctx.tenant)
        .bind(&candidate.solution)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((id, version, stored)) = running {
            let now: Release = serde_json::from_value(stored).map_err(Error::internal)?;
            type Past = (Uuid, String, Value, chrono::DateTime<chrono::Utc>, Option<Value>);
            let runs: Vec<Past> = sqlx::query_as(
                "select id, process, trigger, accepted_at, state from logic_run where tenant_id = $1 and solution = $2 order by accepted_at desc limit $3",
            )
            .bind(ctx.tenant)
            .bind(&candidate.solution)
            .bind(last)
            .fetch_all(&mut *tx)
            .await?;
            let ids: Vec<Uuid> = runs.iter().map(|r| r.0).collect();
            // What people decided, and the events that came for what waited.
            let decided: Vec<(Uuid, String, String, Value)> = sqlx::query_as(
                "select t.run_id, t.activation, d.decision, d.values from logic_task t \
                 join logic_task_decision d on d.tenant_id = t.tenant_id and d.task_id = t.id \
                 where t.tenant_id = $1 and t.run_id = any($2) and d.decision in ('approve', 'reject') order by d.decided_at",
            )
            .bind(ctx.tenant)
            .bind(&ids)
            .fetch_all(&mut *tx)
            .await?;
            let heard: Vec<(Uuid, String, Value)> =
                sqlx::query_as("select run_id, activation, payload from logic_signal where tenant_id = $1 and run_id = any($2) and status = 'delivered'")
                    .bind(ctx.tenant)
                    .bind(&ids)
                    .fetch_all(&mut *tx)
                    .await?;
            // How people recovered stopped steps: resumed, or ended.
            let recovered: Vec<(Uuid, String, String)> = sqlx::query_as(
                "select t.run_id, t.activation, d.decision from logic_task t \
                 join logic_task_decision d on d.tenant_id = t.tenant_id and d.task_id = t.id \
                 where t.tenant_id = $1 and t.run_id = any($2) and t.kind in ('recovery', 'attention') and d.decision in ('resume', 'end')",
            )
            .bind(ctx.tenant)
            .bind(&ids)
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            let mut cases = vec![];
            for (run, process, trigger, at, state) in runs {
                let mut s = scenario(process, trigger, at);
                // The answers it met, as they came: an output, a refusal, or an outcome nobody knew.
                for (activation, output) in state.as_ref().and_then(|s| s.get("outputs")).and_then(Value::as_object).into_iter().flatten() {
                    s.results.insert(activation.clone(), answer_met(output));
                }
                for (_, activation, decision) in recovered.iter().filter(|r| r.0 == run) {
                    s.recoveries.insert(activation.clone(), decision == "resume");
                }
                for (_, activation, decision, values) in decided.iter().filter(|d| d.0 == run) {
                    s.decisions.insert(activation.clone(), Decision { approve: decision == "approve", by: "someone".into(), values: values.clone(), after: 0 });
                }
                for (_, activation, payload) in heard.iter().filter(|h| h.0 == run) {
                    s.signals.insert(activation.clone(), Signal { payload: payload.clone(), after: 0 });
                }
                // What the person assumes, only where history has no answer of its own.
                let mut used = vec![];
                for (key, answer) in assumed {
                    if !s.results.contains_key(key) {
                        s.results.insert(key.clone(), answer.clone());
                        used.push(key.clone());
                    }
                }
                let (was, would) = (Side::of(&now, simulate::simulate(&now, &s)), Side::of(&candidate, simulate::simulate(&candidate, &s)));
                let mut case = was.against(would, json!({"run": run, "at": at}));
                case["assumed"] = json!(used);
                cases.push(case);
            }
            return Ok(shadowed(json!({"kind": "release", "release": id, "version": version}), about, cases));
        }
        // A solution imported from an automation, before it takes over: what the automation did, event by event.
        let Some(automation) = automation_of(&candidate.solution) else {
            tx.commit().await?;
            return Err(Error::field("source", format!("nothing runs for {} to compare it with: simulate it instead", candidate.solution)));
        };
        let found: Option<(String, Value)> = sqlx::query_as("select name, actions from automation where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(automation)
            .fetch_optional(&mut *tx)
            .await?;
        let Some((name, actions)) = found else {
            tx.commit().await?;
            return Err(Error::field("source", "the automation it was imported from is gone, and nothing else runs for it to compare with"));
        };
        type Event = (i64, String, Option<String>, Option<Uuid>, Value, String, chrono::DateTime<chrono::Utc>);
        let events: Vec<Event> = sqlx::query_as(
            "select o.id, o.topic, o.entity, o.record_id, o.payload, r.status, r.created_at from automation_run r \
             join outbox o on o.tenant_id = r.tenant_id and o.id = r.outbox_id \
             where r.tenant_id = $1 and r.automation_id = $2 and r.status in ('done', 'skipped') order by r.created_at desc limit $3",
        )
        .bind(ctx.tenant)
        .bind(automation)
        .bind(last)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let schema = self.registry.schema(ctx.tenant).await?;
        let process = candidate.ir.processes.first().map(|p| p.id.clone()).unwrap_or_default();
        let did: Vec<Value> = crate::automation::validate_actions(&actions)
            .unwrap_or_default()
            .iter()
            .map(|a| serde_json::to_value(a).map(|v| json!({"action": v["type"], "to": v.get("to").or(v.get("entity"))})).unwrap_or_default())
            .collect();
        let mut cases = vec![];
        for (id, topic, entity, record_id, payload, status, at) in events {
            let ev = crate::outbox::Event { id, tenant_id: ctx.tenant, topic, entity, record_id, payload, ..Default::default() };
            let (_, _, trigger) = crate::process::identify(&schema, &ev);
            let acted = status == "done";
            let was = Side {
                outcome: if acted { "acted".into() } else { "passed it by".into() },
                decisions: BTreeMap::new(),
                steps: vec![],
                effects: if acted { did.clone() } else { vec![] },
                not_modelled: vec![],
            };
            let mut s = scenario(process.clone(), trigger, at);
            s.results = assumed.clone().into_iter().collect();
            let mut would = Side::of(&candidate, simulate::simulate(&candidate, &s));
            would.outcome = if would.effects.is_empty() { "passed it by".into() } else { "acted".into() };
            let mut case = json!({"event": id, "at": at, "now": was.json(), "candidate": would.json()});
            let differs = (acted != (would.outcome == "acted")).then_some("whether it acts");
            case["differences"] = json!(differs.into_iter().collect::<Vec<_>>());
            case["same"] = json!(differs.is_none());
            cases.push(case);
        }
        Ok(shadowed(json!({"kind": "automation", "automation": automation, "name": name}), about, cases))
    }

    /// A kept release as a pack another workspace can take in (FLC-OPS-008): its source as written, the source
    /// and artifact digests, what it needs of a workspace (capabilities at their versions, events, calendars),
    /// and the evidence of its review (decisions on which digest, and when; not who, which is this workspace's
    /// own). A pack carries no secret: a source names capabilities, never their connections or credentials.
    pub async fn solution_pack(&self, ctx: &Ctx, release: Uuid) -> Result<Value> {
        require_author(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(String, String, String, String, Value, Value)> =
            sqlx::query_as("select solution, version, source_digest, artifact_digest, source, release from logic_release where tenant_id = $1 and id = $2")
                .bind(ctx.tenant)
                .bind(release)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((solution, version, source_digest, artifact_digest, source, stored)) = row else { return Err(Error::NotFound("release".into())) };
        let reviews: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('decision', decision, 'digest', artifact_digest, 'at', decided_at) from logic_review \
             where tenant_id = $1 and release_id = $2 order by decided_at",
        )
        .bind(ctx.tenant)
        .bind(release)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let compiled: Release = serde_json::from_value(stored).map_err(Error::internal)?;
        let (mut events, mut calendars) = (BTreeSet::new(), BTreeSet::new());
        named(&source, &mut events, &mut calendars);
        Ok(json!({
            "format": PACK_FORMAT, "solution": solution, "version": version,
            "label": source.get("label").cloned().unwrap_or(Value::Null),
            "source": source.to_string(), "source_digest": source_digest, "artifact_digest": artifact_digest,
            "needs": {
                "capabilities": compiled.lock.iter().map(|l| json!({"id": l.id, "version": l.version})).collect::<Vec<_>>(),
                "events": events, "calendars": calendars,
            },
            "evidence": {"compiler": compiled.compiler, "profile": compiled.profile, "reviews": reviews},
            "exported_at": chrono::Utc::now(),
        }))
    }

    /// Takes in a pack from another workspace (FLC-OPS-008): checked against its own source digest, kept as a
    /// draft and nothing more, and checked here, with what must be mapped (a capability, event or calendar this
    /// workspace has differently or not at all). It runs only after a person finishes it, keeps it as a release,
    /// someone else reviews it and an administrator activates it, consenting to what it may change here.
    pub async fn import_solution_pack(&self, ctx: &Ctx, pack: &Value) -> Result<Value> {
        require_author(ctx)?;
        if pack.get("format").and_then(Value::as_str) != Some(PACK_FORMAT) {
            return Err(Error::field("pack", format!("is a {PACK_FORMAT} pack")));
        }
        let text = pack.get("source").and_then(Value::as_str).ok_or_else(|| Error::field("pack", "carries its source"))?;
        sized(text)?;
        let parsed = ferrum_logic::solution::Solution::parse(text).map_err(|r| Error::field("pack", format!("its source cannot be read: {}", r.message)))?;
        let digest = ferrum_logic::canonical::digest(ferrum_logic::compile::SOURCE_SCHEMA, &ferrum_logic::capability::digestible(&parsed.semantic()))
            .map_err(|e| Error::field("pack", e.to_string()))?;
        if pack.get("source_digest").and_then(Value::as_str) != Some(digest.as_str()) {
            return Err(Error::field("pack", "its source is not the one it was exported with: it changed on the way"));
        }
        let draft = self.create_logic_draft(ctx, text).await?;
        let (check, diagnostics) = match self.compile_solution(ctx, text).await? {
            Ok(r) => (json!({"ok": true, "diagnostics": r.notes}), vec![]),
            Err(d) => (json!({"ok": false, "diagnostics": d}), d),
        };
        let mut to_map: Vec<String> = diagnostics.iter().filter(|d| d.fault).map(|d| d.message.clone()).collect();
        let have: BTreeSet<String> = self.logic_calendars(ctx).await?.iter().filter_map(|c| c["name"].as_str().map(str::to_string)).collect();
        for cal in pack["needs"]["calendars"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if !have.contains(cal) && !to_map.iter().any(|m| m.contains(cal)) {
                to_map.push(format!("the calendar {cal}, which this workspace does not have"));
            }
        }
        Ok(json!({
            "draft": draft, "check": check, "to_map": to_map,
            "note": "A pack arrives as a draft: nothing runs until it is checked here, kept as a release, reviewed by someone other than its author and activated. Connections and credentials are this workspace's own; none came with it.",
        }))
    }

    /// The workspace's business calendars.
    pub async fn logic_calendars(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        require_author(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('name', name, 'zone', zone, 'weekends', weekends, 'holidays', holidays, 'revision', revision, \
               'updated_by', updated_by, 'updated_at', updated_at) from logic_calendar where tenant_id = $1 order by name",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// What a reviewer looks at: the release, what runs now for its solution, the decisions so far, and what
    /// changed between the two by meaning, with the new power to consent to.
    pub async fn logic_release_review(&self, ctx: &Ctx, release: Uuid) -> Result<Value> {
        require_author(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(String, String, String, String, Option<Uuid>, chrono::DateTime<chrono::Utc>, Value)> = sqlx::query_as(
            "select solution, version, source_digest, artifact_digest, created_by, created_at, release from logic_release where tenant_id = $1 and id = $2",
        )
        .bind(ctx.tenant)
        .bind(release)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((solution, version, source_digest, artifact_digest, created_by, created_at, stored)) = row else {
            return Err(Error::NotFound("release".into()));
        };
        let active: Option<(Uuid, String, i64, Value)> = sqlx::query_as(
            "select r.id, r.version, a.generation, r.release from logic_activation a join logic_release r on r.tenant_id = a.tenant_id and r.id = a.release_id \
             where a.tenant_id = $1 and a.solution = $2",
        )
        .bind(ctx.tenant)
        .bind(&solution)
        .fetch_optional(&mut *tx)
        .await?;
        let reviews: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('by', decided_by, 'decision', decision, 'reason', reason, 'digest', artifact_digest, 'at', decided_at) \
             from logic_review where tenant_id = $1 and release_id = $2 order by decided_at",
        )
        .bind(ctx.tenant)
        .bind(release)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let this: Release = serde_json::from_value(stored).map_err(Error::internal)?;
        let running: Option<Release> = active.as_ref().and_then(|(_, _, _, r)| serde_json::from_value(r.clone()).ok());
        let changes = diff::diff(running.as_ref().filter(|_| active.as_ref().is_some_and(|(id, ..)| *id != release)), &this);
        let processes: Vec<Value> = this.ir.processes.iter().map(|p| json!({"id": p.id, "label": p.label, "trigger": p.trigger.event})).collect();
        Ok(json!({
            "release": {
                "id": release, "solution": solution, "version": version, "source_digest": source_digest, "artifact_digest": artifact_digest,
                "created_by": created_by, "created_at": created_at, "profile": this.profile, "compiler": this.compiler, "lock": this.lock,
                "notes": this.notes, "rules": this.ir.rules.len(), "decisions": this.ir.decisions.len(), "processes": processes,
            },
            "active": active.map(|(id, version, generation, _)| json!({"release": id, "version": version, "generation": generation})),
            "reviews": reviews,
            "diff": changes,
        }))
    }
}

/// The changing capabilities `release` may use that what runs now for its solution cannot.
pub(crate) async fn privilege_growth(tx: &mut crate::db::Tx, tenant: Uuid, release: Uuid) -> Result<Vec<String>> {
    let rows: Vec<(Uuid, Value, bool)> = sqlx::query_as(
        "select r.id, r.release, r.id = $2 from logic_release r where r.tenant_id = $1 and (r.id = $2 or r.id = \
           (select a.release_id from logic_activation a join logic_release x on x.tenant_id = a.tenant_id and x.id = $2 \
            where a.tenant_id = $1 and a.solution = x.solution))",
    )
    .bind(tenant)
    .bind(release)
    .fetch_all(&mut **tx)
    .await?;
    let this = rows.iter().find(|(_, _, mine)| *mine).ok_or_else(|| Error::NotFound("release".into()))?;
    let this: Release = serde_json::from_value(this.1.clone()).map_err(Error::internal)?;
    let running: Option<Release> = rows.iter().find(|(id, _, _)| *id != release).and_then(|(_, r, _)| serde_json::from_value(r.clone()).ok());
    Ok(diff::diff(running.as_ref(), &this).privilege_growth)
}

// ───────────────────────────── importing automations ─────────────────────────────

/// The solution an automation is imported as.
pub fn solution_of(automation: Uuid) -> String {
    format!("workspace.automation_{}", automation.simple())
}

/// The automation a solution was imported from, when it was.
pub fn automation_of(solution: &str) -> Option<Uuid> {
    solution.strip_prefix("workspace.automation_").filter(|hex| hex.len() == 32).and_then(|hex| Uuid::parse_str(hex).ok())
}

/// `doc.` in an automation's expressions is the triggering record, which a process calls `trigger`.
fn trigger_of(cel: &str) -> String {
    let mut out = String::with_capacity(cel.len());
    let bytes = cel.as_bytes();
    let mut i = 0;
    while i < cel.len() {
        let boundary = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_' || bytes[i - 1] == b'.');
        if boundary && cel[i..].starts_with("doc.") {
            out.push_str("trigger.");
            i += 4;
        } else {
            let c = cel[i..].chars().next().unwrap_or_default();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

impl Kernel {
    /// Each automation of the workspace as a draft Solution (FLC-MIG-001): the same event as its trigger, its
    /// condition carried as written into a decision, its actions as steps through capabilities, and where it
    /// came from in its description. An automation ran with its saver's administrator rights; a process runs
    /// with the requester's, and what it may change is consented to when its release is reviewed, so nothing
    /// carries the old rights over. What does not map (a notification to a role or a field) is left as a step
    /// with nothing to say who, which the check refuses until a person decides. An automation imported before
    /// is not imported again. Each event starts its own run, as the automation ran once per event. Nothing is
    /// released or turned off here: the automation runs until its solution's release is activated, which turns
    /// it off in the same step, so each of its events is run by one of the two (FLC-MIG-003).
    pub async fn import_automations(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("an administrator imports automations".into()));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let autos: Vec<(Uuid, String, String, String, Option<String>, Value, bool)> = sqlx::query_as(
            "select id, name, entity, event, condition, actions, is_active from automation where tenant_id = $1 order by created_at, id limit 500",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        let existing: Vec<String> = sqlx::query_scalar("select solution from logic_draft where tenant_id = $1").bind(ctx.tenant).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let mut out = Vec::new();
        for (id, name, entity, event, condition, actions, active) in autos {
            let solution = solution_of(id);
            if existing.contains(&solution) {
                out.push(json!({"automation": id, "name": name, "skipped": "imported already"}));
                continue;
            }
            let Some(ent) = ctx.schema.entity(&entity) else {
                out.push(json!({"automation": id, "name": name, "skipped": format!("{entity} is not installed")}));
                continue;
            };
            let package = ent.package.clone();
            let version = |cap: &str| self.capability(ctx, cap).map(|c| c.version).unwrap_or_else(|_| "1.0.0".into());
            let mut notes: Vec<String> = Vec::new();
            let mut steps: Vec<Value> = Vec::new();
            let list = crate::automation::validate_actions(&actions).unwrap_or_default();
            for (i, a) in list.iter().enumerate() {
                let step = format!("step_{}", i + 1);
                let action = |cap: String, input: Value, label: String| {
                    let v = version(&cap);
                    json!({"type": "action", "id": step, "label": label, "capability": {"id": cap, "version": v}, "input": input,
                           "next": "", "on_error": "attention", "on_unknown": "attention"})
                };
                let values = |m: &std::collections::BTreeMap<String, String>| -> Value {
                    Value::Object(m.iter().map(|(k, v)| (format!("values.{k}"), json!(trigger_of(v)))).collect())
                };
                steps.push(match a {
                    crate::automation::AutomationAction::SetFields { values: v } => {
                        let mut input = values(v);
                        input["id"] = json!("trigger.id");
                        action(format!("{package}.{}.update", ferrum_logic::capability::snake(&entity)), input, format!("Set fields on the {entity}"))
                    }
                    crate::automation::AutomationAction::CreateDocument { entity: other, values: v } => {
                        let pkg = ctx.schema.entity(other).map(|e| e.package.clone()).unwrap_or_else(|| package.clone());
                        action(format!("{pkg}.{}.create", ferrum_logic::capability::snake(other)), values(v), format!("Create a {other}"))
                    }
                    crate::automation::AutomationAction::Email { to, subject, body, template, purpose } => {
                        if template.is_some() {
                            notes.push(format!("{step}: its e-mail template is not carried over; the mail sends the body as text"));
                        }
                        action(
                            match purpose {
                                crate::retention::Purpose::Marketing => "ferrum.mail.marketing".into(),
                                crate::retention::Purpose::Transactional => "ferrum.mail".into(),
                            },
                            json!({"to": trigger_of(to), "subject": trigger_of(subject), "body": trigger_of(body)}),
                            "Send an e-mail".into(),
                        )
                    }
                    crate::automation::AutomationAction::Notify { to, title, body } => {
                        let user = match to.strip_prefix("user:") {
                            Some(u) => json!(format!("\"{u}\"")),
                            None => {
                                notes.push(format!("{step}: it told {to}; say which person a process tells"));
                                json!("")
                            }
                        };
                        // A title and a body are expressions, as they were in the automation.
                        let mut input = json!({"user": user, "title": trigger_of(title)});
                        if let Some(b) = body {
                            input["body"] = json!(trigger_of(b));
                        }
                        action("ferrum.notify".into(), input, "Tell someone".into())
                    }
                });
            }
            if steps.is_empty() {
                notes.push("it had no actions this platform still reads".into());
            }
            // Each step leads to the next, the last to done.
            for i in 0..steps.len() {
                let next = if i + 1 < steps.len() { format!("step_{}", i + 2) } else { "done".into() };
                steps[i]["next"] = json!(next);
            }
            let first = if steps.is_empty() { "done".to_string() } else { "step_1".to_string() };
            let (entry, decisions) = match condition.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
                Some(cel) => {
                    steps.insert(
                        0,
                        json!({"type": "decision", "id": "applies", "label": "Whether it applies", "decision": "applies",
                               "routes": [{"outcome": "run", "target": first}, {"outcome": "skip", "target": "not_for_this"}], "on_error": "attention"}),
                    );
                    let table = json!({"id": "applies", "hit_policy": "first", "inputs": [{"name": "doc", "value": "trigger"}],
                                       "rows": [{"id": "condition", "when": cel, "outcome": "run"}], "default": "skip"});
                    ("applies".to_string(), vec![table])
                }
                None => (first, vec![]),
            };
            steps.push(json!({"type": "end", "id": "done", "label": "Done", "outcome": "completed"}));
            // Only a condition can pass a record by.
            if !decisions.is_empty() {
                steps.push(json!({"type": "end", "id": "not_for_this", "label": "Not for this record", "outcome": "completed"}));
            }
            steps.push(json!({"type": "suspend", "id": "attention", "label": "Needs attention", "reason": "A step did not go as planned: look into it before anything more is done."}));
            let source = json!({
                "schema_version": "ferrum.solution/1", "kind": "Solution", "id": solution, "version": "0.1.0", "label": name,
                "description": format!(
                    "Imported from the automation {name} ({id}){} on {}. It ran with the rights of the administrator who saved it; this runs with the rights of whoever starts it, and what it may change is consented to when its release is reviewed.{}",
                    if active { "" } else { ", which was off" },
                    chrono::Utc::now().date_naive(),
                    if notes.is_empty() { String::new() } else { format!(" To decide: {}.", notes.join("; ")) }
                ),
                "requires": [],
                "decisions": decisions,
                "processes": [{"id": "automation", "label": name, "trigger": {"event": format!("{package}.{}.{event}", ferrum_logic::capability::snake(&entity)), "business_key": "event.id"},
                               "entry": entry, "max_steps": 50, "nodes": steps}]
            });
            // What the capabilities need is declared as the guided editor would: each one's installed version.
            let mut source = source;
            let caps: std::collections::BTreeMap<String, Value> = source["processes"][0]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|n| n.get("capability").cloned())
                .filter_map(|c| Some((c["id"].as_str()?.to_string(), c)))
                .collect();
            source["requires"] = json!(caps.into_values().collect::<Vec<_>>());
            let text = source.to_string();
            let draft = self.create_logic_draft(ctx, &text).await?;
            let check = match self.compile_solution(ctx, &text).await? {
                Ok(_) => json!({"ok": true}),
                Err(d) => json!({"ok": false, "diagnostics": serde_json::to_value(&d).unwrap_or_default()}),
            };
            out.push(json!({"automation": id, "name": name, "draft": draft, "check": check, "notes": notes, "active": active}));
        }
        Ok(out)
    }
}

/// A step's recorded output as the answer it was: the machine records a refusal as `{"error": {code, message}}`
/// and an outcome nobody knew as `{"unknown": why}`; anything else was a result.
fn answer_met(output: &Value) -> Invoked {
    let text = |v: &Value| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
    match (output.get("error"), output.get("unknown")) {
        (Some(e), _) => Invoked::Refused {
            code: e.get("code").and_then(Value::as_str).unwrap_or("refused").to_string(),
            message: e.get("message").map(text).unwrap_or_else(|| text(e)),
        },
        (None, Some(u)) => Invoked::Unknown { why: text(u) },
        _ => Invoked::Done { output: output.clone() },
    }
}

/// One side of a shadow comparison: how a simulation ended, what its decisions came to, the steps it took, and
/// the effects it asked for.
struct Side {
    outcome: String,
    decisions: BTreeMap<String, Value>,
    steps: Vec<Value>,
    effects: Vec<Value>,
    not_modelled: Vec<String>,
}

/// The ids of a release's decision steps, in blocks too.
fn decision_steps(release: &Release) -> BTreeSet<String> {
    fn walk(nodes: &[ferrum_logic::solution::Node], out: &mut BTreeSet<String>) {
        for n in nodes {
            if let ferrum_logic::solution::Node::Decision { id, .. } = n {
                out.insert(id.clone());
            }
            for b in n.blocks() {
                walk(&b.nodes, out);
            }
        }
    }
    let mut out = BTreeSet::new();
    for p in &release.ir.processes {
        walk(&p.nodes, &mut out);
    }
    out
}

impl Side {
    fn of(release: &Release, sim: std::result::Result<Simulation, String>) -> Side {
        let sim = match sim {
            Ok(s) => s,
            Err(e) => {
                return Side { outcome: format!("could not run: {e}"), decisions: BTreeMap::new(), steps: vec![], effects: vec![], not_modelled: vec![] };
            }
        };
        let ids = decision_steps(release);
        let decisions = sim
            .run
            .outputs
            .iter()
            .filter(|(act, _)| ids.contains(act.rsplit('/').next().unwrap_or_default()))
            .filter_map(|(act, out)| Some((act.clone(), out.get("outcome")?.clone())))
            .collect();
        let commands = sim.entries.iter().filter(|e| e.kind == "command");
        let steps = commands.clone().filter_map(|e| e.activation.clone().map(Value::String)).collect();
        let effects = commands
            .filter(|e| e.what["do"] == "invoke" && e.what["changes"] == json!(true))
            .map(|e| json!({"step": e.activation, "capability": e.what["capability"], "input": e.what["input"]}))
            .collect();
        let outcome = match serde_json::to_value(&sim.status).unwrap_or_default() {
            v if v["status"] == "finished" => v["outcome"].as_str().unwrap_or("finished").to_string(),
            v => v["status"].as_str().unwrap_or("running").to_string(),
        };
        Side { outcome, decisions, steps, effects, not_modelled: sim.not_modelled }
    }

    fn json(&self) -> Value {
        json!({"outcome": self.outcome, "decisions": self.decisions, "steps": self.steps, "effects": self.effects, "not_modelled": self.not_modelled})
    }

    /// This, what runs now, beside what the candidate would do.
    fn against(self, would: Side, case: Value) -> Value {
        let mut differences = vec![];
        if self.outcome != would.outcome {
            differences.push("outcome");
        }
        if self.decisions != would.decisions {
            differences.push("decisions");
        }
        if self.steps != would.steps {
            differences.push("steps");
        }
        if self.effects != would.effects {
            differences.push("effects");
        }
        let mut case = case;
        case["now"] = self.json();
        case["candidate"] = would.json();
        case["same"] = json!(differences.is_empty());
        case["differences"] = json!(differences);
        case
    }
}

fn shadowed(against: Value, candidate: Value, cases: Vec<Value>) -> Value {
    let differing = cases.iter().filter(|c| c["same"] == json!(false)).count();
    json!({
        "against": against, "candidate": candidate, "compared": cases.len(), "differing": differing, "cases": cases,
        "note": "Nothing was run, changed or kept: both sides were simulated on what happened, with the answers, decisions and events met then.",
    })
}
