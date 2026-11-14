//! Service targets (docs/api.md, "Service targets"; docs/builder/decisions.md BD-04): by configuration alone an
//! entity says "this must happen within so much working time", and every save of one of its records keeps the
//! round (`record_target`), the due moment and the state word in step, in the save's transaction, after the
//! derivations, rules and hooks have run on the values being stored. What a round does is one pure function,
//! [`ferrum_logic::target::step`]; here it is loaded, called and stored.
//!
//! A breach is the minute loop's: a round past its due moment, not met, paused or breached, is marked by an
//! update that succeeds once however many replicas or passes try, then the record is saved as the system so that
//! its state field reads `breached` through the ordinary pipeline (and an automation on `updated` sees it). A save
//! that is refused leaves the breach recorded; the field catches up at the record's next save.

use crate::ctx::Ctx;
use crate::db::Tx;
use crate::error::{Error, FieldError, Result};
use crate::value::normalize;
use crate::{Doc, Kernel};
use chrono::{DateTime, Utc};
use ferrum_logic::target::{self, Round, Said};
use ferrum_meta::{EffEntity, Schema, Scope};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// Rounds the sweep marks in one pass, and at most this many of one workspace's.
pub const SWEEP_BATCH: i64 = 200;
const SWEEP_SHARE: i64 = 50;

/// What the sweep looks for.
const OVERDUE: &str = "started_at is not null and met_at is null and paused_at is null and breached_at is null and due_at <= $1";

#[derive(sqlx::FromRow)]
struct Row {
    target: String,
    round: i32,
    started_at: Option<DateTime<Utc>>,
    resumed_at: Option<DateTime<Utc>>,
    paused_at: Option<DateTime<Utc>>,
    worked_secs: i64,
    within_secs: i64,
    due_at: Option<DateTime<Utc>>,
    met_at: Option<DateTime<Utc>>,
    breached_at: Option<DateTime<Utc>>,
    calendar: Option<String>,
    start_from: Option<DateTime<Utc>>,
    anchored: bool,
}

/// What a round was worked out from (BD-41): its calendar's name (`""`, plain time) and the record's start moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Anchors {
    calendar: String,
    start_from: Option<DateTime<Utc>>,
}

impl Row {
    fn round(self) -> (Round, Option<Anchors>) {
        let Row { round, started_at, resumed_at, paused_at, worked_secs, within_secs, due_at, met_at, breached_at, calendar, start_from, anchored, .. } = self;
        let anchors = anchored.then(|| Anchors { calendar: calendar.unwrap_or_default(), start_from });
        (Round { round, started_at, resumed_at, paused_at, worked_secs, within_secs, due_at, met_at, breached_at }, anchors)
    }
}

/// The calendar's name target `t` counts on for the record: the one the record names where it says
/// (`calendar_field`), else the target's own; `""` for plain time.
fn calendar_of(t: &ferrum_meta::TargetDef, doc: &Doc, links: &Value) -> String {
    let named = t.calendar_field.as_deref().and_then(|path| match path.split_once('.') {
        None => doc.get(path).and_then(Value::as_str).map(str::to_string),
        Some((link, field)) => links.get(link).and_then(|r| r.get(field)).and_then(Value::as_str).map(str::to_string),
    });
    named.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).or_else(|| t.calendar.clone()).unwrap_or_default()
}

/// The record's start moment for target `t` (`start_field`); none when empty or still to come, so the clock starts
/// at the save, as without one.
fn start_of(t: &ferrum_meta::TargetDef, doc: &Doc, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let v = doc.get(t.start_field.as_deref()?)?.as_str()?;
    crate::value::parse_datetime(v).ok().filter(|at| *at <= now)
}

impl Kernel {
    /// The moment service targets take as now: the clock, or what [`Kernel::pin_clock`] set.
    pub(crate) fn now(&self) -> DateTime<Utc> {
        self.clock.lock().ok().and_then(|c| *c).unwrap_or_else(Utc::now)
    }

    /// For tests: the moment service targets take as now from here on (`None`: the clock again).
    #[doc(hidden)]
    pub fn pin_clock(&self, at: Option<DateTime<Utc>>) {
        if let Ok(mut c) = self.clock.lock() {
            *c = at;
        }
    }
}

/// Keeps record `id`'s targets in step with `doc` as it will be stored, writing each target's due moment and
/// state word into `doc`. An expression that cannot be evaluated refuses the save, as a rule's does.
pub(crate) async fn keep(k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, id: Uuid, doc: &mut Doc) -> Result<()> {
    if ent.def.targets.is_empty() {
        return Ok(());
    }
    let now = k.now();
    let rows: Vec<Row> = sqlx::query_as(
        "select target, round, started_at, resumed_at, paused_at, worked_secs, within_secs, due_at, met_at, breached_at, calendar, start_from, anchored \
         from record_target where tenant_id = $1 and entity = $2 and record_id = $3 for update",
    )
    .bind(ctx.tenant)
    .bind(&ent.def.name)
    .bind(id)
    .fetch_all(&mut **tx)
    .await?;
    let mut rounds: BTreeMap<String, (Round, Option<Anchors>)> = rows.into_iter().map(|r| (r.target.clone(), r.round())).collect();
    let user = ctx.user_value();
    let view = ent.cel_view(&Value::Object(doc.clone()), &ctx.schema);
    let scope = Scope { doc: &view, old: None, parent: None, user: &user };
    let all = ent.exprs.targets.values().flat_map(|x| [x.start_when.as_ref(), Some(&x.within), x.pause_when.as_ref(), Some(&x.met_when)]).flatten();
    let mut names = crate::links::used(all);
    names.extend(ent.def.targets.iter().filter_map(|t| t.calendar_field.as_deref()?.split_once('.').map(|(link, _)| link.to_string())));
    let links = crate::links::load(k, ctx, tx, ent, &view, &names).await?;
    let extra = [("links", &links)];
    for t in &ent.def.targets {
        let x = ent.exprs.targets.get(&t.name).ok_or_else(|| Error::internal(format!("target {} is not compiled", t.name)))?;
        let fail = |m: String| Error::Validation(vec![FieldError::new(&t.state_field, format!("target {} could not be worked out: {m}", t.label))]);
        let holds = |e: Option<&ferrum_meta::Expr>, none: bool| e.map_or(Ok(none), |e| e.eval_bool_with(&scope, &extra)).map_err(fail);
        let within = target::within(&x.within.eval_with(&scope, &extra).map_err(fail)?).map_err(fail)?;
        let said =
            Said { start: holds(x.start_when.as_ref(), true)?, within, pause: holds(x.pause_when.as_ref(), false)?, met: holds(Some(&x.met_when), false)? };
        let anchors = Anchors { calendar: calendar_of(t, doc, &links), start_from: start_of(t, doc, now) };
        let calendar = match within {
            Some(_) => load(tx, ctx.tenant, &anchors.calendar).await?.map_err(fail)?,
            None => None,
        };
        let (prev, was) = rounds.remove(&t.name).unwrap_or_default();
        // A round already running whose calendar or start moved is worked out again, once (BD-41).
        let mut base = prev.clone();
        if let Some(was) = was.as_ref().filter(|w| **w != anchors) {
            let old_cal = if was.calendar == anchors.calendar { calendar.clone() } else { load(tx, ctx.tenant, &was.calendar).await?.unwrap_or(None) };
            let moved = match (was.start_from, anchors.start_from) {
                (Some(a), Some(b)) if a != b => Some((a, b)),
                _ => None,
            };
            if was.calendar != anchors.calendar || moved.is_some() {
                base = target::reanchor(&prev, old_cal.as_ref(), calendar.as_ref(), moved, now).map_err(fail)?;
            }
        }
        let (next, due, word) = target::step_from(&base, said, now, anchors.start_from, calendar.as_ref()).map_err(fail)?;
        if next != prev || was.as_ref() != Some(&anchors) {
            sqlx::query(
                "insert into record_target (tenant_id, entity, record_id, target, round, started_at, resumed_at, paused_at, worked_secs, within_secs, \
                 due_at, met_at, breached_at, calendar, start_from, anchored) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, true) \
                 on conflict (tenant_id, entity, record_id, target) do update set round = $5, started_at = $6, resumed_at = $7, paused_at = $8, \
                 worked_secs = $9, within_secs = $10, due_at = $11, met_at = $12, breached_at = $13, calendar = $14, start_from = $15, anchored = true",
            )
            .bind(ctx.tenant)
            .bind(&ent.def.name)
            .bind(id)
            .bind(&t.name)
            .bind(next.round)
            .bind(next.started_at)
            .bind(next.resumed_at)
            .bind(next.paused_at)
            .bind(next.worked_secs)
            .bind(next.within_secs)
            .bind(next.due_at)
            .bind(next.met_at)
            .bind(next.breached_at)
            .bind(&anchors.calendar)
            .bind(anchors.start_from)
            .execute(&mut **tx)
            .await?;
        }
        for (field, v) in [(&t.due_field, due.map(|d| json!(d.to_rfc3339()))), (&t.state_field, word.map(|w| json!(w)))] {
            let f = ent.field(field).ok_or_else(|| Error::internal(format!("target {}: no field {field}", t.name)))?;
            let v = match v {
                Some(v) => normalize(f, &v).map_err(fail)?,
                None => Value::Null,
            };
            doc.insert(field.clone(), v);
        }
    }
    Ok(())
}

/// Calendar `name` (`""`: none, plain time); an inner error when the workspace has no such calendar.
async fn load(tx: &mut Tx, tenant: Uuid, name: &str) -> Result<std::result::Result<Option<ferrum_logic::calendar::Calendar>, String>> {
    if name.is_empty() {
        return Ok(Ok(None));
    }
    Ok(crate::load_calendar(tx, tenant, name).await?.map(Some).ok_or_else(|| format!("the calendar {name} is not there")))
}

/// The business calendars `schema`'s targets name.
fn named(schema: &Schema) -> BTreeSet<String> {
    schema.entities.values().flat_map(|e| e.def.targets.iter().filter_map(|t| t.calendar.clone())).collect()
}

/// Refuses customizations that leave a target naming a calendar the workspace does not have, that `was` did
/// not name already.
pub(crate) async fn check_calendars(tx: &mut Tx, tenant: Uuid, was: &Schema, now: &Schema) -> Result<()> {
    let new: Vec<String> = named(now).difference(&named(was)).cloned().collect();
    if new.is_empty() {
        return Ok(());
    }
    let have: Vec<String> =
        sqlx::query_scalar("select name from logic_calendar where tenant_id = $1 and name = any($2)").bind(tenant).bind(&new).fetch_all(&mut **tx).await?;
    match new.iter().find(|n| !have.contains(n)) {
        Some(n) => Err(Error::invalid(format!("customization rejected: there is no business calendar {n} (add it under calendars first)"))),
        None => Ok(()),
    }
}

/// The first target of `schema` naming calendar `name`: "Ticket's target respond".
pub(crate) fn naming(schema: &Schema, name: &str) -> Option<String> {
    schema.entities.values().find_map(|e| {
        let t = e.def.targets.iter().find(|t| t.calendar.as_deref() == Some(name))?;
        Some(format!("{}'s target {}", e.def.name, t.name))
    })
}

impl Kernel {
    /// One pass of the minute loop's breach sweep at the clock `now`: rounds past their due moment, not met,
    /// paused or breached, fairly across workspaces, at most [`SWEEP_BATCH`]. Each is marked once (the update
    /// finds nothing on a second replica or pass), then its record is saved as the system. How many it marked.
    pub async fn sweep_targets_at(&self, now: DateTime<Utc>) -> Result<usize> {
        if self.db.fenced() || self.holding().await? {
            return Ok(0);
        }
        let mut tx = self.db.system_tx().await?;
        let due: Vec<(Uuid, String, Uuid, String, i32)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select t.tenant_id, t.entity, t.record_id, t.target, t.round from (select distinct tenant_id from record_target where {OVERDUE} and not ferrum_workspace_held(tenant_id)) w \
             cross join lateral (select tenant_id, entity, record_id, target, round, due_at from record_target r where r.tenant_id = w.tenant_id and {OVERDUE} \
               order by due_at limit $2) t order by t.due_at limit $3"
        )))
        .bind(now)
        .bind(SWEEP_SHARE)
        .bind(SWEEP_BATCH)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut marked = 0;
        for (tenant, entity, record, name, round) in due {
            match self.breach(tenant, &entity, record, &name, round, now).await {
                Ok(true) => marked += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(%tenant, %entity, %record, target = %name, "service target not swept: {e}"),
            }
        }
        Ok(marked)
    }

    /// Marks one round breached, once, then saves its record as the system. Whether this call marked it.
    async fn breach(&self, tenant: Uuid, entity: &str, record: Uuid, name: &str, round: i32, now: DateTime<Utc>) -> Result<bool> {
        let schema = self.registry.schema(tenant).await?;
        let mut tx = self.db.tenant_tx(tenant).await?;
        let key = "tenant_id = $2 and entity = $3 and record_id = $4 and target = $5";
        if !schema.entity(entity).is_some_and(|e| e.def.targets.iter().any(|t| t.name == name)) {
            // Unknown here, but perhaps a newer build's: kept for it (window.rs).
            if self.newer_build_here().await? {
                return Ok(false);
            }
            // The target or its entity is gone: its rounds go with it.
            sqlx::query("delete from record_target where tenant_id = $1 and entity = $2 and record_id = $3 and target = $4")
                .bind(tenant)
                .bind(entity)
                .bind(record)
                .bind(name)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(false);
        }
        let n = sqlx::query(sqlx::AssertSqlSafe(format!("update record_target set breached_at = $1 where {key} and round = $6 and {OVERDUE}")))
            .bind(now)
            .bind(tenant)
            .bind(entity)
            .bind(record)
            .bind(name)
            .bind(round)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        if n == 0 {
            return Ok(false);
        }
        // The breach is the platform's fact, in any document status: written into the record's kept fields as a
        // transition writes them, with the record's `updated` event. Only a real failure leaves it for the next save.
        let system = Ctx::system(tenant, schema);
        let mut tx = self.db.tenant_tx(tenant).await?;
        match self.keep_targets_tx(&system, &mut tx, entity, record).await {
            Ok(_) => tx.commit().await?,
            Err(e) => tracing::warn!(%tenant, %entity, %record, target = %name, "breach recorded; the record's state field catches up at its next save: {e}"),
        }
        Ok(true)
    }
}
