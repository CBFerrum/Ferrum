//! Automations that fire on time, with no workflow engine (docs/api.md, Automations). An automation with a
//! `schedule` in place of a record event has the event `scheduled`, and a schedule is one of two forms:
//!
//! - `{"relative": {"field", "offset"?, "at"?, "repeat"?, "until"?}}`: a moment of each record, its date or
//!   datetime field plus a signed offset (a date field's moment is at `at` in the workspace's zone), then
//!   every `repeat` after that up to the record's `until`;
//! - `{"every": {"period", "at", "on"?, "filter"?, "absent"?}}`: each day, week or month at `at`, every record that
//!   passes the filter; with `absent` (BD-27), only those that no record of another entity linking to them matches in
//!   a period relative to the run (this week, this month, the last N days), once a period for each record.
//!
//! A loop on every replica looks once a minute at each scheduled automation's window from `scanned_until` to
//! now. One SQL statement per page of the entity's records finds the (record, due moment) pairs in the
//! window, inserts each into `automation_occurrence` unless it is there, and queues a `<Entity>.scheduled`
//! event for each it inserted, in the transaction that moves the automation's cursor on: the occurrence's
//! primary key makes a moment fire once however many replicas look. The run (automation.rs) reads the record
//! afresh and runs only if the moment is still one of the record's.

use crate::ctx::RowScope;
use crate::error::{Error, Result};
use crate::query::{Filter, parse_filters, push_filter, push_from_where};
use crate::store::{SYSTEM_KEYS, field_expr};
use crate::{Kernel, outbox};
use chrono::{DateTime, NaiveTime, Timelike, Utc};
use ferrum_meta::{EffEntity, FieldType, Schema};
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

/// Records looked at in one statement: a large entity is worked through a page at a time, each its own
/// transaction, so no scan holds locks or a transaction open for long.
pub const PAGE: i64 = 500;
/// Scheduled automations looked at in one round, and at most this many of one workspace's.
const BATCH: i64 = 100;
const TENANT_SHARE: i64 = 10;
/// Moments older than this when a scan reaches them (after downtime) are skipped, not fired late.
pub const CATCH_UP_DAYS: i64 = 7;
/// Occurrences older than this are pruned: no scan looks further back than [`CATCH_UP_DAYS`].
pub const KEEP_DAYS: i64 = 30;
const LOOK_EVERY: std::time::Duration = std::time::Duration::from_secs(60);
/// An automation looked at less than this long ago waits for the next round.
const SETTLE_SECS: f64 = 30.0;
const MAX_FILTERS: usize = 8;
/// An offset is at most about five years either way.
const MAX_OFFSET_DAYS: i64 = 1826;

fn bad(msg: impl Into<String>) -> Error {
    Error::field("schedule", msg)
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Repeat {
    Day,
    Week,
    Month,
    Year,
}

impl Repeat {
    fn parse(s: &str) -> Option<Repeat> {
        Some(match s {
            "P1D" => Repeat::Day,
            "P1W" => Repeat::Week,
            "P1M" => Repeat::Month,
            "P1Y" => Repeat::Year,
            _ => return None,
        })
    }
    fn interval(self) -> &'static str {
        match self {
            Repeat::Day => "1 day",
            Repeat::Week => "1 week",
            Repeat::Month => "1 month",
            Repeat::Year => "1 year",
        }
    }
    /// The shortest and longest one step is in wall-clock seconds, to bound which steps reach a window.
    fn steps(self) -> (i64, i64) {
        const DAY: i64 = 86_400;
        match self {
            Repeat::Day => (DAY, DAY),
            Repeat::Week => (7 * DAY, 7 * DAY),
            Repeat::Month => (28 * DAY, 31 * DAY),
            Repeat::Year => (365 * DAY, 366 * DAY),
        }
    }
}

/// A schedule as it stands against its entity: which field, which offset, which days.
#[derive(Debug, Clone)]
pub(crate) enum Plan {
    Relative { field: String, datetime: bool, days: i32, mins: i32, at: NaiveTime, repeat: Option<Repeat>, until: Option<(String, bool)> },
    Every { at: NaiveTime, rule: String, filters: Vec<Filter>, absent: Option<Absent> },
}

/// What must be missing for a record to be due (BD-27): no record of `entity` whose link field `link` names it, passing
/// `filters`, with its date field `date` in the period `within` of the run.
#[derive(Debug, Clone)]
pub(crate) struct Absent {
    entity: String,
    link: String,
    date: String,
    datetime: bool,
    filters: Vec<Filter>,
    within: Within,
}

/// The period a run looks back over; its start keys the occurrence, so a record is due once a period.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Within {
    Week,
    Month,
    /// The last N days, today included; once each N days counted from 1 January 1970.
    Days(i32),
}

/// The longest period an absence looks back over.
const MAX_WITHIN_DAYS: i32 = 366;

impl Within {
    fn parse(s: &str) -> Option<Within> {
        match s {
            "week" => Some(Within::Week),
            "month" => Some(Within::Month),
            _ => {
                let n = s.strip_prefix('P')?.strip_suffix('D')?;
                let n: i32 = n.parse().ok().filter(|_| n.bytes().all(|b| b.is_ascii_digit()))?;
                (1..=MAX_WITHIN_DAYS).contains(&n).then_some(Within::Days(n))
            }
        }
    }

    /// SQL of the period's start, end (not included) and key, as local timestamps, around the local moment `l`.
    fn sql(self, l: &str) -> (String, String, String) {
        match self {
            Within::Week => (format!("date_trunc('week', {l})"), format!("(date_trunc('week', {l}) + interval '1 week')"), format!("date_trunc('week', {l})")),
            Within::Month => {
                (format!("date_trunc('month', {l})"), format!("(date_trunc('month', {l}) + interval '1 month')"), format!("date_trunc('month', {l})"))
            }
            Within::Days(n) => (
                format!("(({l})::date - {})::timestamp", n - 1),
                format!("(({l})::date + 1)::timestamp"),
                format!("(date '1970-01-01' + ((({l})::date - date '1970-01-01') / {n}) * {n})::timestamp"),
            ),
        }
    }
}

/// A signed ISO-8601 duration of days, hours and minutes (`-P3D`, `PT2H`, `P1DT30M`): (days, minutes).
fn duration(s: &str) -> Option<(i64, i64)> {
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let rest = rest.strip_prefix('P')?;
    let (date, time) = match rest.split_once('T') {
        Some((d, t)) if !t.is_empty() => (d, Some(t)),
        Some(_) => return None,
        None => (rest, None),
    };
    let num = |s: &str| -> Option<i64> { if !s.is_empty() && s.len() <= 6 && s.bytes().all(|b| b.is_ascii_digit()) { s.parse().ok() } else { None } };
    let days = if date.is_empty() { 0 } else { num(date.strip_suffix('D')?)? };
    let mut mins = 0;
    if let Some(mut t) = time {
        if let Some((h, r)) = t.split_once('H') {
            mins += num(h)? * 60;
            t = r;
        }
        if !t.is_empty() {
            mins += num(t.strip_suffix('M')?)?;
        }
    }
    if date.is_empty() && time.is_none() {
        return None;
    }
    Some(if neg { (-days, -mins) } else { (days, mins) })
}

fn time(s: &str, at: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M").ok().filter(|_| s.len() == 5).ok_or_else(|| bad(format!("{at} '{s}' is not a time of day such as 09:00")))
}

/// Whether `name` is a date (false) or datetime (true) field of the entity.
fn date_field(ent: &EffEntity, name: &str, at: &str) -> Result<bool> {
    match ent.field(name).map(|f| f.ftype) {
        Some(FieldType::Date) => Ok(false),
        Some(FieldType::Datetime) => Ok(true),
        Some(_) => Err(bad(format!("{at}: {name} is not a date or datetime field"))),
        None => Err(bad(format!("{at}: {} has no field {name}", ent.def.name))),
    }
}

fn filters(schema: &Schema, ent: &EffEntity, v: &Value) -> Result<Vec<Filter>> {
    filters_at(schema, ent, v, "every.filter")
}

fn filters_at(schema: &Schema, ent: &EffEntity, v: &Value, at: &str) -> Result<Vec<Filter>> {
    let list = parse_filters(v).map_err(|e| bad(format!("{at}: {e}")))?;
    if list.len() > MAX_FILTERS {
        return Err(bad(format!("{at} has at most {MAX_FILTERS} conditions")));
    }
    for f in &list {
        if !(SYSTEM_KEYS.contains(&f.field.as_str()) || ent.field(&f.field).is_some_and(|d| d.ftype != FieldType::Table)) {
            return Err(bad(format!("{at}: {} has no field {}", ent.def.name, f.field)));
        }
        // Built once to see that the operator and the value fit the field.
        push_filter(&mut QueryBuilder::<Postgres>::new(""), schema, ent, "a", f).map_err(|e| bad(format!("{at} on {}: {e}", f.field)))?;
    }
    Ok(list)
}

/// Checks `every.absent` against the entity the schedule is on (`ent`) and the one that must be missing.
fn absent(schema: &Schema, ent: &EffEntity, v: &Value) -> Result<Absent> {
    let body = v.as_object().ok_or_else(|| bad("every.absent is {\"entity\", \"link\", \"date\", \"within\", \"filter\"?}"))?;
    if let Some(k) = body.keys().find(|k| !["entity", "link", "date", "within", "filter"].contains(&k.as_str())) {
        return Err(bad(format!("every.absent.{k} is not a setting of this schedule")));
    }
    let text = |k: &str| body.get(k).and_then(Value::as_str).ok_or_else(|| bad(format!("every.absent.{k} is required")));
    let entity = text("entity")?;
    let other = schema.entity(entity).ok_or_else(|| bad(format!("every.absent.entity: there is no entity {entity}")))?;
    if other.table.is_empty() {
        return Err(bad(format!("every.absent.entity: {entity} keeps no records of its own")));
    }
    let link = text("link")?;
    if !other.field(link).is_some_and(|f| f.ftype == FieldType::Link && f.to.as_deref() == Some(ent.def.name.as_str())) {
        return Err(bad(format!("every.absent.link: {link} is not a field of {entity} linking to {}", ent.def.name)));
    }
    let date = text("date")?;
    let datetime = date_field(other, date, "every.absent.date")?;
    let within = Within::parse(text("within")?).ok_or_else(|| bad(format!("every.absent.within is week, month or P1D to P{MAX_WITHIN_DAYS}D")))?;
    let filters = match body.get("filter") {
        None | Some(Value::Null) => Vec::new(),
        Some(f) => filters_at(schema, other, f, "every.absent.filter")?,
    };
    Ok(Absent { entity: entity.into(), link: link.into(), date: date.into(), datetime, filters, within })
}

/// Pushes ` and not exists (…)`: no record of `a.entity` linking to the record `id` (SQL) passes its filter with its
/// date in the period around the local moment `l` (SQL), in zone `tz` (SQL).
fn push_absent(qb: &mut QueryBuilder<Postgres>, schema: &Schema, tenant: Uuid, a: &Absent, id: &str, l: &str, tz: &str) -> Result<()> {
    let other = schema.entity(&a.entity).ok_or_else(|| bad(format!("{} is not installed", a.entity)))?;
    let link = field_expr(other, &a.link, "b").ok_or_else(|| bad(format!("{} has no field {}", a.entity, a.link)))?;
    let date = field_expr(other, &a.date, "b").ok_or_else(|| bad(format!("{} has no field {}", a.entity, a.date)))?;
    let (start, end, _) = a.within.sql(l);
    let range = if a.datetime {
        format!("({date}) >= ({start}) at time zone {tz} and ({date}) < ({end}) at time zone {tz}")
    } else {
        format!("({date}) >= ({start})::date and ({date}) < ({end})::date")
    };
    qb.push(format!(" and not exists (select 1 from {} b where b.tenant_id = ", crate::ddl::q(&other.table)));
    qb.push_bind(tenant);
    qb.push(format!("{} and ({link})::text = ({id})::text and {range}", crate::store::entity_predicate(other, "b")));
    for f in &a.filters {
        push_filter(qb, schema, other, "b", f)?;
    }
    qb.push(")");
    Ok(())
}

/// Checks a schedule against the entity it is on.
pub(crate) fn plan(schema: &Schema, ent: &EffEntity, v: &Value) -> Result<Plan> {
    let shape = || bad("is {\"relative\": {...}} or {\"every\": {...}}");
    let (form, body) = v.as_object().filter(|o| o.len() == 1).and_then(|o| o.iter().next()).ok_or_else(shape)?;
    let body = body.as_object().ok_or_else(shape)?;
    let text = |k: &str| -> Result<Option<&str>> {
        match body.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.as_str())),
            Some(_) => Err(bad(format!("{form}.{k} is text"))),
        }
    };
    let known = |keys: &[&str]| match body.keys().find(|k| !keys.contains(&k.as_str())) {
        Some(k) => Err(bad(format!("{form}.{k} is not a setting of this schedule"))),
        None => Ok(()),
    };
    match form.as_str() {
        "relative" => {
            known(&["field", "offset", "at", "repeat", "until"])?;
            let field = text("field")?.ok_or_else(|| bad("relative.field is required: a date or datetime field"))?;
            let datetime = date_field(ent, field, "relative.field")?;
            let (days, mins) = match text("offset")? {
                None => (0, 0),
                Some(s) => duration(s).ok_or_else(|| bad(format!("relative.offset '{s}' is not a duration such as -P3D, PT2H or -P1DT30M")))?,
            };
            if days.abs() * 1440 + mins.abs() > MAX_OFFSET_DAYS * 1440 {
                return Err(bad("relative.offset is at most five years either way"));
            }
            let at = time(text("at")?.unwrap_or("09:00"), "relative.at")?;
            let repeat = text("repeat")?.map(|s| Repeat::parse(s).ok_or_else(|| bad("relative.repeat is P1D, P1W, P1M or P1Y"))).transpose()?;
            let until = match text("until")? {
                None => None,
                Some(_) if repeat.is_none() => return Err(bad("relative.until goes with repeat")),
                Some(u) => Some((u.to_string(), date_field(ent, u, "relative.until")?)),
            };
            Ok(Plan::Relative { field: field.to_string(), datetime, days: days as i32, mins: mins as i32, at, repeat, until })
        }
        "every" => {
            known(&["period", "at", "on", "filter", "absent"])?;
            let period = text("period")?.ok_or_else(|| bad("every.period is required: day, week or month"))?;
            let at = time(text("at")?.ok_or_else(|| bad("every.at is required: a time of day such as 09:00"))?, "every.at")?;
            let on = body.get("on").filter(|v| !v.is_null());
            let number = |range: std::ops::RangeInclusive<u64>, default: u64| on.map_or(Some(default), |v| v.as_u64().filter(|n| range.contains(n)));
            let rule = match period {
                "day" if on.is_none() => "true".to_string(),
                "day" => return Err(bad("every.on is not for a daily schedule")),
                "week" => format!("extract(isodow from d) = {}", number(1..=7, 1).ok_or_else(|| bad("every.on is 1 (Monday) to 7 (Sunday) for a week"))?),
                "month" if on.and_then(Value::as_str) == Some("last") => "extract(day from d + interval '1 day') = 1".to_string(),
                "month" => format!("extract(day from d) = {}", number(1..=28, 1).ok_or_else(|| bad("every.on is 1 to 28, or \"last\", for a month"))?),
                _ => return Err(bad("every.period is day, week or month")),
            };
            let filters = match body.get("filter") {
                None | Some(Value::Null) => Vec::new(),
                Some(f) => filters(schema, ent, f)?,
            };
            let absent = match body.get("absent") {
                None | Some(Value::Null) => None,
                Some(v) => Some(absent(schema, ent, v)?),
            };
            Ok(Plan::Every { at, rule, filters, absent })
        }
        _ => Err(shape()),
    }
}

/// Pushes `with prm, page, dues`: `dues (id, due)` is each (record, due moment) with the moment in `(lo, hi]`,
/// over the records after `after` (or the one record `only`), at most `limit` of them in `page`.
#[allow(clippy::too_many_arguments)]
fn push_dues(
    qb: &mut QueryBuilder<Postgres>,
    schema: &Schema,
    ent: &EffEntity,
    tenant: Uuid,
    plan: &Plan,
    tz: &str,
    (lo, hi): (DateTime<Utc>, DateTime<Utc>),
    after: Option<Uuid>,
    only: Option<Uuid>,
    limit: Option<i64>,
) -> Result<()> {
    let (at, days, mins) = match plan {
        Plan::Relative { at, days, mins, .. } => (*at, *days, *mins),
        Plan::Every { at, .. } => (*at, 0, 0),
    };
    qb.push("with prm0 as (select ").push_bind(lo).push("::timestamptz as lo, ").push_bind(hi).push("::timestamptz as hi, ");
    qb.push_bind(tz.to_string()).push("::text as tz, ").push_bind(at).push("::time as at_, make_interval(days => ").push_bind(days);
    qb.push(", mins => ").push_bind(mins).push(") as off), prm as (select *, lo at time zone tz as lo_l, hi at time zone tz as hi_l from prm0), ");
    qb.push("page as (select a.id");
    let p = |c: &str| format!("(select {c} from prm)");
    let filters: &[Filter] = match plan {
        Plan::Every { filters, .. } => filters,
        Plan::Relative { .. } => &[],
    };
    let mut relative = None;
    if let Plan::Relative { field, datetime, repeat, until, .. } = plan {
        let e = field_expr(ent, field, "a").ok_or_else(|| bad(format!("{} has no field {field}", ent.def.name)))?;
        let (tz, off) = (p("tz"), p("off"));
        // The first moment, in wall-clock time where the workspace is.
        if *datetime {
            qb.push(format!(", (({e}) at time zone {tz}) + {off} as base"));
        } else {
            qb.push(format!(", (({e}) + {}) + {off} as base", p("at_")));
        }
        // The moment a repeating schedule stops at (not included).
        let until = until
            .as_ref()
            .map(|(u, dt)| field_expr(ent, u, "a").map(|x| (x, *dt)).ok_or_else(|| bad(format!("{} has no field {u}", ent.def.name))))
            .transpose()?;
        match &until {
            Some((u, true)) => qb.push(format!(", ({u}) + interval '1 microsecond' as until")),
            Some((u, false)) => qb.push(format!(", ((({u}) + 1)::timestamp at time zone {tz}) as until")),
            None => qb.push(", null::timestamptz as until"),
        };
        push_from_where(qb, schema, ent, tenant, &RowScope::default(), &[], None)?;
        // Only records whose moments can reach the window, as a range an index on the field serves.
        qb.push(format!(" and ({e}) is not null"));
        let (from, to) = if *datetime {
            (format!("{} - {off} - interval '2 days'", p("lo")), format!("{} - {off} + interval '2 days'", p("hi")))
        } else {
            (format!("({} - {off})::date - 2", p("lo_l")), format!("({} - {off})::date + 2", p("hi_l")))
        };
        match repeat {
            None => qb.push(format!(" and ({e}) between {from} and {to}")),
            Some(_) => qb.push(format!(" and ({e}) <= {to}")),
        };
        if let Some((u, dt)) = &until {
            let from = if *dt { format!("{} - interval '2 days'", p("lo")) } else { format!("{}::date - 2", p("lo_l")) };
            qb.push(format!(" and (({u}) is null or ({u}) >= {from})"));
        }
        relative = Some(*repeat);
    } else {
        qb.push(", null::timestamptz as until");
        push_from_where(qb, schema, ent, tenant, &RowScope::default(), filters, None)?;
    }
    if let Some(c) = after {
        qb.push(" and a.id > ").push_bind(c);
    }
    if let Some(r) = only {
        qb.push(" and a.id = ").push_bind(r);
    }
    qb.push(" order by a.id");
    if let Some(n) = limit {
        qb.push(" limit ").push_bind(n);
    }
    let absent = match plan {
        Plan::Every { absent: Some(a), .. } => Some(a),
        _ => None,
    };
    // Absence keys a moment by its period's start: one occurrence a period for each record.
    match absent {
        Some(a) => qb.push(format!("), dues as (select p.id, ({}) at time zone prm.tz as due", a.within.sql("(m.due at time zone prm.tz)").2)),
        None => qb.push("), dues as (select p.id, m.due"),
    };
    qb.push(" from page p cross join prm cross join lateral (");
    match (relative, plan) {
        (Some(None), _) => {
            qb.push("select p.base at time zone prm.tz as due");
        }
        (Some(Some(r)), _) => {
            // Only the steps that can land in the window: a handful, however long ago the first moment was.
            let (short, long) = r.steps();
            qb.push(format!(
                "select (p.base + k * interval '{}') at time zone prm.tz as due from generate_series(\
                 greatest(0, floor(extract(epoch from prm.lo_l - p.base) / {long}) - 1)::int, \
                 (ceil(extract(epoch from prm.hi_l - p.base) / {short}) + 1)::int) k",
                r.interval()
            ));
        }
        (None, Plan::Every { rule, .. }) => {
            qb.push(format!(
                "select (d::date + prm.at_) at time zone prm.tz as due from generate_series(prm.lo_l::date - 1, prm.hi_l::date + 1, interval '1 day') d where {rule}"
            ));
        }
        (None, Plan::Relative { .. }) => unreachable!("a relative plan sets `relative`"),
    }
    qb.push(") m where m.due > prm.lo and m.due <= prm.hi and (p.until is null or m.due < p.until)");
    if let Some(a) = absent {
        push_absent(qb, schema, tenant, a, "p.id", "(m.due at time zone prm.tz)", "prm.tz")?;
    }
    qb.push(")");
    Ok(())
}

/// Whether a daily moment at `at` (wall-clock, where the workspace is) falls in `(lo, hi]` on a day `rule`
/// admits. Most rounds' windows are a minute long: a schedule whose moments all fall at one time of day
/// looks at no record in the others.
async fn any_moment(tx: &mut crate::db::Tx, tz: &str, at: NaiveTime, rule: &str, lo: DateTime<Utc>, hi: DateTime<Utc>) -> Result<bool> {
    let mut qb = QueryBuilder::<Postgres>::new("with prm as (select ");
    qb.push_bind(lo).push("::timestamptz as lo, ").push_bind(hi).push("::timestamptz as hi, ").push_bind(tz.to_string());
    qb.push("::text as tz, ").push_bind(at).push(format!(
        "::time as at_) select exists (select 1 from prm cross join generate_series((prm.lo at time zone prm.tz)::date - 1, \
         (prm.hi at time zone prm.tz)::date + 1, interval '1 day') d where {rule} \
         and (d::date + prm.at_) at time zone prm.tz > prm.lo and (d::date + prm.at_) at time zone prm.tz <= prm.hi)"
    ));
    Ok(qb.build_query_scalar().fetch_one(&mut **tx).await?)
}

/// Whether any moment of the plan can fall in the window, before looking at records.
async fn worth_a_look(tx: &mut crate::db::Tx, plan: &Plan, tz: &str, lo: DateTime<Utc>, hi: DateTime<Utc>) -> Result<bool> {
    match plan {
        Plan::Every { at, rule, .. } => any_moment(tx, tz, *at, rule, lo, hi).await,
        // A date field's moments fall at one time of day: `at` moved by the offset's hours and minutes.
        Plan::Relative { datetime: false, at, mins, .. } => {
            let secs = (at.num_seconds_from_midnight() as i64 + *mins as i64 * 60).rem_euclid(86_400);
            let tod = NaiveTime::from_num_seconds_from_midnight_opt(secs as u32, 0).unwrap_or(*at);
            any_moment(tx, tz, tod, "true", lo, hi).await
        }
        Plan::Relative { datetime: true, .. } => Ok(true),
    }
}

/// Whether `due` is still one of the record's due moments, as the record and the schedule are now: a date
/// moved since the scan, or a record that no longer passes the filter, is not due.
pub(crate) async fn still_due(k: &Kernel, tenant: Uuid, schema: &Schema, ent: &EffEntity, plan: &Plan, record: Uuid, due: DateTime<Utc>) -> Result<bool> {
    let mut tx = k.db.tenant_tx(tenant).await?;
    if let Plan::Every { filters, absent: Some(a), .. } = plan {
        // `due` keys a period: the record still passes the filter, and still misses what it missed, over the period
        // that key starts (the last N days: those before the run).
        let mut qb = QueryBuilder::<Postgres>::new("with prm as (select ");
        qb.push_bind(due).push("::timestamptz as k, ").push_bind(schema.timezone.clone()).push("::text as tz) select exists (select 1");
        push_from_where(&mut qb, schema, ent, tenant, &RowScope::default(), filters, None)?;
        qb.push(" and a.id = ").push_bind(record);
        let l = match a.within {
            Within::Days(_) => "(now() at time zone (select tz from prm))",
            _ => "((select k from prm) at time zone (select tz from prm))",
        };
        push_absent(&mut qb, schema, tenant, a, "a.id", l, "(select tz from prm)")?;
        qb.push(")");
        let yes: bool = qb.build_query_scalar().fetch_one(&mut *tx).await?;
        tx.commit().await?;
        return Ok(yes);
    }
    let mut qb = QueryBuilder::<Postgres>::new("");
    push_dues(&mut qb, schema, ent, tenant, plan, &schema.timezone, (due - chrono::Duration::seconds(1), due), None, Some(record), None)?;
    qb.push(" select exists (select 1 from dues where due = ").push_bind(due).push(")");
    let yes: bool = qb.build_query_scalar().fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(yes)
}

impl Kernel {
    /// Looks at scheduled automations once a minute, on every replica; at once again while a large entity
    /// is being worked through.
    pub fn spawn_schedule_scanner(self: &Arc<Self>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        let k = self.clone();
        tokio::spawn(async move {
            loop {
                let more = match k.scan_schedules_at(Utc::now(), PAGE).await {
                    Ok(more) => more,
                    Err(e) => {
                        tracing::warn!("scheduled automations: {e}");
                        false
                    }
                };
                // Service targets past their due moment (targets.rs); a full pass looks again at once.
                let more = more
                    | match k.sweep_targets_at(Utc::now()).await {
                        Ok(n) => n as i64 >= crate::targets::SWEEP_BATCH,
                        Err(e) => {
                            tracing::warn!("service targets: {e}");
                            false
                        }
                    };
                // Approvals owed a reminder or an escalation (approvals.rs); a full pass looks again at once.
                let more = more
                    | match k.sweep_approvals_at(Utc::now()).await {
                        Ok(n) => n as i64 >= crate::approvals::SWEEP_BATCH,
                        Err(e) => {
                            tracing::warn!("approvals: {e}");
                            false
                        }
                    };
                if more {
                    tokio::task::yield_now().await;
                    if *shutdown.borrow() {
                        return;
                    }
                    continue;
                }
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(LOOK_EVERY) => {}
                }
            }
        })
    }

    /// One round at the clock `now`: the scheduled automations due a look, fairly across workspaces, each a
    /// page of `page` records further. Whether some have more to work through.
    pub async fn scan_schedules_at(&self, now: DateTime<Utc>, page: i64) -> Result<bool> {
        if self.db.fenced() || self.holding().await? {
            return Ok(false);
        }
        let mut tx = self.db.system_tx().await?;
        let due: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "select a.tenant_id, a.id from (select distinct tenant_id from automation where event = 'scheduled' and is_active) t \
             cross join lateral (select tenant_id, id from automation a where a.tenant_id = t.tenant_id and a.event = 'scheduled' and a.is_active \
               and (a.scan_to is not null or a.scanned_until is null or a.scanned_until <= $1 - make_interval(secs => $2)) \
               order by a.scanned_until nulls first, a.id limit $3) a \
             order by random() limit $4",
        )
        .bind(now)
        .bind(SETTLE_SECS)
        .bind(TENANT_SHARE)
        .bind(BATCH)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut more = false;
        for (tenant, id) in due {
            match self.scan_automation(tenant, id, now, page).await {
                Ok(m) => more |= m,
                Err(e) => tracing::warn!(%tenant, automation = %id, "scheduled automation not looked at: {e}"),
            }
        }
        Ok(more)
    }

    /// One page of one automation's window, in one transaction that holds its row: the occurrences found, their
    /// events and the cursor's move commit together. Whether the window has more pages.
    async fn scan_automation(&self, tenant: Uuid, id: Uuid, now: DateTime<Utc>, page: i64) -> Result<bool> {
        let schema = self.registry.schema(tenant).await?;
        let mut tx = self.db.tenant_tx(tenant).await?;
        type Row = (String, Value, Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<Uuid>);
        let row: Option<Row> = sqlx::query_as(
            "select entity, schedule, scanned_until, scan_to, scan_cursor from automation where tenant_id = $1 and id = $2 \
             and event = 'scheduled' and is_active and (scan_to is not null or scanned_until is null or scanned_until <= $3 - make_interval(secs => $4)) \
             for update skip locked",
        )
        .bind(tenant)
        .bind(id)
        .bind(now)
        .bind(SETTLE_SECS)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((entity, schedule, scanned_until, scan_to, cursor)) = row else { return Ok(false) };
        let finish = |until: DateTime<Utc>| {
            sqlx::query("update automation set scanned_until = $3, scan_to = null, scan_cursor = null where tenant_id = $1 and id = $2")
                .bind(tenant)
                .bind(id)
                .bind(until)
        };
        let plan = schema.entity(&entity).ok_or_else(|| bad(format!("{entity} is not installed"))).and_then(|ent| Ok((ent, plan(&schema, ent, &schedule)?)));
        let (ent, plan) = match plan {
            Ok(p) => p,
            Err(e) => {
                // The entity or the field has gone since it was saved: nothing is due until it is saved again.
                tracing::warn!(%tenant, automation = %id, "scheduled automation cannot be looked at: {e}");
                finish(now).execute(&mut *tx).await?;
                tx.commit().await?;
                return Ok(false);
            }
        };
        let tz = schema.timezone.as_str();
        let (lo, hi) = match scan_to {
            Some(to) => (scanned_until.unwrap_or(to), to),
            None => {
                // A new automation starts from now; after downtime, moments older than a week are skipped.
                let from = scanned_until.unwrap_or(now);
                let floor = now - chrono::Duration::days(CATCH_UP_DAYS);
                if from < floor {
                    let mut sp = sqlx::Connection::begin(&mut *tx).await?;
                    let mut qb = QueryBuilder::<Postgres>::new("");
                    push_dues(&mut qb, &schema, ent, tenant, &plan, tz, (from, floor), None, None, None)?;
                    qb.push(" select count(*) from dues");
                    match qb.build_query_scalar::<i64>().fetch_one(&mut *sp).await {
                        Ok(0) => sp.commit().await?,
                        Ok(n) => {
                            sp.commit().await?;
                            tracing::warn!(%tenant, automation = %id, skipped = n, since = %from, "scheduled automation: moments more than {CATCH_UP_DAYS} days old skipped");
                        }
                        Err(e) => {
                            sp.rollback().await?;
                            tracing::warn!(%tenant, automation = %id, since = %from, "scheduled automation: moments more than {CATCH_UP_DAYS} days old skipped, not counted: {e}");
                        }
                    }
                    (floor, now)
                } else {
                    (from, now)
                }
            }
        };
        if hi <= lo || (scan_to.is_none() && !worth_a_look(&mut tx, &plan, tz, lo, hi).await?) {
            finish(hi.max(lo)).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(false);
        }
        let mut qb = QueryBuilder::<Postgres>::new("");
        push_dues(&mut qb, &schema, ent, tenant, &plan, tz, (lo, hi), cursor, None, Some(page))?;
        qb.push(", ins as (insert into automation_occurrence (tenant_id, automation_id, record_id, due_at) select ").push_bind(tenant).push(", ");
        qb.push_bind(id).push(
            ", id, due from dues on conflict do nothing returning record_id, due_at) \
             select (select id from page order by id desc limit 1), (select count(*) from page), \
             (select array_agg(record_id order by record_id, due_at) from ins), (select array_agg(due_at order by record_id, due_at) from ins)",
        );
        type Found = (Option<Uuid>, i64, Option<Vec<Uuid>>, Option<Vec<DateTime<Utc>>>);
        let (last, n, records, dues): Found = qb.build_query_as().fetch_one(&mut *tx).await?;
        let topic = format!("{entity}.scheduled");
        for (record, due) in records.unwrap_or_default().into_iter().zip(dues.unwrap_or_default()) {
            let payload = json!({"automation": id, "record": record, "due_at": due.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)});
            outbox::enqueue(&mut tx, tenant, &topic, None, Some(record), payload).await?;
        }
        let more = n >= page;
        if more {
            sqlx::query("update automation set scanned_until = $3, scan_to = $4, scan_cursor = $5 where tenant_id = $1 and id = $2")
                .bind(tenant)
                .bind(id)
                .bind(lo)
                .bind(hi)
                .bind(last)
                .execute(&mut *tx)
                .await?;
        } else {
            finish(hi).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(more)
    }

    /// Hourly: occurrences older than [`KEEP_DAYS`]; no scan looks back further than [`CATCH_UP_DAYS`].
    pub async fn prune_occurrences(&self) -> Result<u64> {
        let mut tx = self.db.system_tx().await?;
        // An absence's occurrence keys its period (BD-27): kept while the period may still be running.
        let n = sqlx::query(
            "delete from automation_occurrence o where o.due_at < now() - make_interval(days => $1) \
             and (o.due_at < now() - make_interval(days => $2) or not exists (select 1 from automation a \
               where a.tenant_id = o.tenant_id and a.id = o.automation_id and a.schedule->'every' ? 'absent'))",
        )
        .bind(KEEP_DAYS as i32)
        .bind(MAX_WITHIN_DAYS + 1)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration("-P3D"), Some((-3, 0)));
        assert_eq!(duration("PT2H"), Some((0, 120)));
        assert_eq!(duration("P1DT2H30M"), Some((1, 150)));
        assert_eq!(duration("+PT45M"), Some((0, 45)));
        assert_eq!(duration("P0D"), Some((0, 0)));
        for bad in ["", "P", "PT", "3D", "P3", "P1W", "P1M", "PT1S", "P-3D", "P+3D", "-P3DT", "P1.5D", "PT2M3H"] {
            assert_eq!(duration(bad), None, "{bad}");
        }
    }
}
