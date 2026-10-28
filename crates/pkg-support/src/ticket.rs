//! Tickets: the conversation, the status flow, automatic assignment and the SLA clock, plus the
//! masters that drive them (agreements, teams, assignment rules) and the periodic SLA refresh and
//! auto-close.
//!
//! The SLA fields are recomputed from scratch on every save (and by maintenance) from the
//! ticket's status timeline, so a priority change, a pause, a reopening or an edited holiday list
//! always yields the same deadlines as if they had been known from the start.

use crate::sla::{self, Calendar, Clock, State, Ts};
use crate::{int, linked};
use async_trait::async_trait;
use chrono::{Duration, SecondsFormat, Utc};
use ferrum_domain::flow::created;
use ferrum_domain::trade::create_for_user;
use ferrum_domain::util::{b, d, get, id, money, rows, s, set, sys};
use ferrum_kernel::value::{parse_date, parse_datetime};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, Filter, HookCtx, Kernel, Result, Tx, notify, outbox, store};
use ferrum_meta::{Expr, Scope};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Statuses of a ticket that is still being worked on.
pub const OPEN: [&str; 3] = ["Open", "Replied", "On Hold"];
const WEEKDAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
/// Fields maintenance may move on its own; a change of the verdict ones is published.
const VERDICT: [&str; 3] = ["sla_status", "response_breached", "resolution_breached"];

fn ts(doc: &Doc, k: &str) -> Option<Ts> {
    s(doc, k).and_then(|v| parse_datetime(v).ok())
}

fn ts_value(t: Ts) -> Value {
    json!(t.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn hours(ms: i64) -> Value {
    money((Decimal::from(ms) / Decimal::from(3_600_000)).round_dp(2))
}

/// "HH:MM" (one-digit hours allowed) → minutes since midnight; "24:00" is the end of the day.
pub fn parse_hm(v: &str) -> Option<u32> {
    let (h, m) = v.trim().split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (m < 60 && (h < 24 || (h == 24 && m == 0))).then_some(h * 60 + m)
}

/// The status timeline `[{status, at}]` kept on the ticket, oldest first.
fn timeline(doc: &Doc) -> Vec<(String, Ts)> {
    let mut out: Vec<(String, Ts)> = doc
        .get("status_history")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|e| Some((e.get("status")?.as_str()?.to_string(), parse_datetime(e.get("at")?.as_str()?).ok()?))).collect())
        .unwrap_or_default();
    out.sort_by_key(|(_, at)| *at);
    out
}

fn timeline_value(tl: &[(String, Ts)]) -> Value {
    Value::Array(tl.iter().map(|(status, at)| json!({"status": status, "at": ts_value(*at)})).collect())
}

// ───────────────────────────── agreements ─────────────────────────────

/// An enabled service level agreement, parsed for evaluation.
pub struct Sla {
    id: Uuid,
    name: String,
    customer: Option<Uuid>,
    customer_group: Option<Uuid>,
    ticket_type: Option<Uuid>,
    pause_on_hold: bool,
    pause_on_replied: bool,
    /// priority → (response, resolution) in milliseconds of business time.
    targets: HashMap<String, (i64, i64)>,
    calendar: Calendar,
}

fn hours_to_ms(h: Decimal) -> i64 {
    (h * Decimal::from(3_600_000)).round().to_i64().unwrap_or(i64::MAX)
}

impl Sla {
    fn from_doc(doc: &Doc) -> Option<Sla> {
        let windows: Vec<(usize, u32, u32)> = rows(doc, "working_hours")
            .iter()
            .filter_map(|r| Some((WEEKDAYS.iter().position(|w| Some(*w) == s(r, "weekday"))?, parse_hm(s(r, "start_time")?)?, parse_hm(s(r, "end_time")?)?)))
            .collect();
        let holidays = rows(doc, "holidays").iter().filter_map(|r| s(r, "holiday_date").and_then(|v| parse_date(v).ok())).collect();
        let targets = rows(doc, "priorities")
            .iter()
            .filter_map(|r| Some((s(r, "priority")?.to_string(), (hours_to_ms(d(r, "response_hours")), hours_to_ms(d(r, "resolution_hours"))))))
            .collect();
        Some(Sla {
            id: id(doc, "id")?,
            name: s(doc, "sla_name").unwrap_or_default().to_string(),
            customer: id(doc, "customer"),
            customer_group: id(doc, "customer_group"),
            ticket_type: id(doc, "ticket_type"),
            pause_on_hold: b(doc, "pause_on_hold"),
            pause_on_replied: b(doc, "pause_on_replied"),
            targets,
            calendar: Calendar::new(int(doc, "time_zone_offset"), &windows, holidays),
        })
    }

    /// How specifically the agreement fits (customer, then nearest customer group, then ticket
    /// type; higher wins), or `None` when one of its criteria does not match.
    fn rank(&self, customer: Option<Uuid>, groups: &[Uuid], ticket_type: Option<Uuid>) -> Option<(bool, usize, bool)> {
        if self.customer.is_some_and(|c| Some(c) != customer) || self.ticket_type.is_some_and(|t| Some(t) != ticket_type) {
            return None;
        }
        let group = match self.customer_group {
            Some(g) => groups.len() - groups.iter().position(|x| *x == g)?,
            None => 0,
        };
        Some((self.customer.is_some(), group, self.ticket_type.is_some()))
    }
}

/// Cheap pre-check on an agreement's header, before its tables are loaded.
fn applies(head: &Doc, customer: Option<Uuid>, groups: &[Uuid], ticket_type: Option<Uuid>) -> bool {
    id(head, "customer").is_none_or(|c| Some(c) == customer)
        && id(head, "customer_group").is_none_or(|g| groups.contains(&g))
        && id(head, "ticket_type").is_none_or(|t| Some(t) == ticket_type)
}

/// The most specific enabled agreement that sets targets for the ticket's priority (ties, which
/// validation prevents, go to the first name).
fn pick<'a>(slas: &'a [Sla], doc: &Doc, groups: &[Uuid]) -> Option<&'a Sla> {
    let priority = s(doc, "priority").unwrap_or("Medium");
    slas.iter()
        .filter(|x| x.targets.contains_key(priority))
        .filter_map(|x| x.rank(id(doc, "customer"), groups, id(doc, "ticket_type")).map(|r| (r, x)))
        .min_by(|(ra, a), (rb, b)| rb.cmp(ra).then_with(|| a.name.cmp(&b.name)))
        .map(|(_, x)| x)
}

/// The SLA fields of a ticket as of `now`: agreement, deadlines, verdicts and business hours
/// taken (on a round-the-clock calendar when no agreement applies). While a pause is running,
/// pending deadlines are blank (they resume with the ticket) and its time is counted when it ends,
/// so a paused ticket's fields do not change with time.
pub fn sla_fields(slas: &[Sla], groups: &[Uuid], doc: &Doc, now: Ts) -> Doc {
    let mut out = Doc::new();
    let Some(opened) = ts(doc, "opened_at") else { return out };
    let sla = pick(slas, doc, groups);
    let always = Calendar::always();
    let (cal, on_hold, replied) = sla.map_or((&always, true, true), |x| (&x.calendar, x.pause_on_hold, x.pause_on_replied));
    let pausing = |st: &str| (on_hold && st == "On Hold") || (replied && st == "Replied");
    let tl = timeline(doc);
    let stopped = sla::stopped_intervals(&tl, |st| pausing(st) || matches!(st, "Resolved" | "Closed"), now);
    let running = tl.last().filter(|(st, _)| pausing(st)).map(|(_, at)| *at);
    let clock = Clock { opened, stopped: &stopped, now };
    let (responded, resolved) = (ts(doc, "first_responded_at"), ts(doc, "resolved_at"));
    out.insert("first_response_hours".into(), responded.map_or(Value::Null, |t| hours(sla::elapsed(cal, &clock, t))));
    out.insert("resolution_hours".into(), resolved.map_or(Value::Null, |t| hours(sla::elapsed(cal, &clock, t))));
    let paused: i64 = sla::clip(&stopped, resolved.or(running).unwrap_or(now)).iter().map(|&(a, z)| cal.between(a, z, &[])).sum();
    out.insert("paused_hours".into(), hours(paused));
    let priority = s(doc, "priority").unwrap_or("Medium");
    match sla.and_then(|x| x.targets.get(priority).map(|t| (x.id, *t))) {
        Some((sla_id, (response_ms, resolution_ms))) => {
            let judge = |ms: i64, done: Option<Ts>| {
                let mut v = sla::verdict(cal, &clock, ms, done);
                if running.is_some() && done.is_none() && v.state != State::Breached {
                    v.deadline = None;
                }
                v
            };
            let (r, z) = (judge(response_ms, responded), judge(resolution_ms, resolved));
            out.insert("service_level_agreement".into(), json!(sla_id.to_string()));
            out.insert("response_by".into(), r.deadline.map_or(Value::Null, ts_value));
            out.insert("resolution_by".into(), z.deadline.map_or(Value::Null, ts_value));
            out.insert("response_breached".into(), json!(r.state == State::Breached));
            out.insert("resolution_breached".into(), json!(z.state == State::Breached));
            out.insert("sla_status".into(), json!(sla::overall(r.state, z.state).label()));
        }
        None => {
            for f in ["service_level_agreement", "response_by", "resolution_by", "sla_status"] {
                out.insert(f.into(), Value::Null);
            }
            out.insert("response_breached".into(), json!(false));
            out.insert("resolution_breached".into(), json!(false));
        }
    }
    out
}

/// Enabled agreements whose header passes `keep`, with their tables.
async fn load_slas(k: &Kernel, ctx: &Ctx, tx: &mut Tx, keep: impl Fn(&Doc) -> bool) -> Result<Vec<Sla>> {
    let sctx = sys(ctx);
    let heads = k.find_tx(&sctx, tx, "ServiceLevelAgreement", &[Filter::eq("enabled", true)], Some("sla_name asc"), 1000).await?;
    let mut out = Vec::new();
    for head in heads.iter().filter(|h| keep(h)) {
        let Some(sla_id) = id(head, "id") else { continue };
        out.extend(Sla::from_doc(&k.get_tx(&sctx, tx, "ServiceLevelAgreement", sla_id, false).await?));
    }
    Ok(out)
}

/// The customer (if it exists) and its customer group with the group's ancestors, nearest first.
async fn customer_and_groups(k: &Kernel, ctx: &Ctx, tx: &mut Tx, customer: Option<Uuid>) -> Result<(Option<Doc>, Vec<Uuid>)> {
    let doc = match customer {
        Some(c) => linked(k, ctx, tx, "Customer", c).await?,
        None => None,
    };
    let mut chain = Vec::new();
    let mut next = doc.as_ref().and_then(|c| id(c, "customer_group"));
    while let Some(g) = next {
        if chain.contains(&g) || chain.len() >= 50 {
            break;
        }
        chain.push(g);
        next = id(&get(k, ctx, tx, "CustomerGroup", g).await?, "parent_group");
    }
    Ok((doc, chain))
}

pub struct SlaHook;

#[async_trait]
impl DocHook for SlaHook {
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        let mut seen = HashSet::new();
        for (i, r) in rows(doc, "priorities").iter().enumerate() {
            if let Some(p) = s(r, "priority")
                && !seen.insert(p.to_string())
            {
                errors.push(FieldError::new("priority", "is listed twice").in_row("priorities", i));
            }
        }
        for (i, r) in rows(doc, "working_hours").iter().enumerate() {
            let (start, end) = (s(r, "start_time").map(parse_hm), s(r, "end_time").map(parse_hm));
            match (start, end) {
                (Some(None), _) => errors.push(FieldError::new("start_time", "use HH:MM, e.g. 09:00").in_row("working_hours", i)),
                (_, Some(None)) => errors.push(FieldError::new("end_time", "use HH:MM, e.g. 18:00 (24:00 for midnight)").in_row("working_hours", i)),
                (Some(Some(a)), Some(Some(z))) if a >= z => {
                    errors.push(FieldError::new("end_time", "must be after the start time; enter a night shift as one row per day").in_row("working_hours", i))
                }
                _ => {}
            }
        }
        if !errors.is_empty() {
            return Err(Error::Validation(errors));
        }
        if b(doc, "enabled") {
            let mut filters = vec![Filter::eq("enabled", true)];
            filters.extend(["customer", "customer_group", "ticket_type"].map(|f| Filter::eq(f, doc.get(f).cloned().unwrap_or(Value::Null))));
            let same = hc.kernel.find_tx(&sys(hc.ctx), hc.tx, "ServiceLevelAgreement", &filters, None, 2).await?;
            if let Some(other) = same.iter().find(|o| id(o, "id") != id(doc, "id")) {
                return Err(Error::invalid(format!(
                    "the enabled agreement '{}' already applies to the same customer, customer group and ticket type; disable it or change the criteria",
                    s(other, "sla_name").unwrap_or_default()
                )));
            }
        }
        Ok(())
    }
}

// ───────────────────────────── teams and assignment ─────────────────────────────

pub struct TeamHook;

#[async_trait]
impl DocHook for TeamHook {
    async fn before_validate(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        // Rows added by an edit do not get the kernel's insert-time defaults: a new member is active.
        let members: Vec<Value> = rows(doc, "members")
            .into_iter()
            .map(|mut m| {
                if m.get("active").is_none_or(Value::is_null) {
                    m.insert("active".into(), json!(true));
                }
                Value::Object(m)
            })
            .collect();
        set(doc, "members", Value::Array(members));
        Ok(())
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut seen = HashSet::new();
        let errors: Vec<FieldError> = rows(doc, "members")
            .iter()
            .enumerate()
            .filter(|(_, m)| s(m, "agent").is_some_and(|a| !seen.insert(a.to_string())))
            .map(|(i, _)| FieldError::new("agent", "is already a member").in_row("members", i))
            .collect();
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }
}

pub struct RuleHook;

#[async_trait]
impl DocHook for RuleHook {
    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if let Some(c) = s(doc, "condition") {
            Expr::compile(c).map_err(|m| Error::field("condition", m))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Member {
    pub agent: String,
    pub active: bool,
    /// 0 = no limit.
    pub max_open: i64,
}

/// The next agent for a rule: active members under their open-ticket limit, taken in turn after
/// `last`; load balancing takes the one with the fewest open tickets (ties in that same turn order).
pub fn pick_member(members: &[Member], last: Option<&str>, open: &HashMap<String, i64>, balance: bool) -> Option<String> {
    let n = members.len();
    let start = last.and_then(|l| members.iter().position(|m| m.agent == l)).map_or(0, |i| i + 1);
    let load = |m: &Member| open.get(&m.agent).copied().unwrap_or(0);
    let mut eligible = (0..n).map(|k| &members[(start + k) % n]).filter(|m| m.active && (m.max_open <= 0 || load(m) < m.max_open));
    let chosen = if balance { eligible.min_by_key(|m| load(m)) } else { eligible.next() };
    chosen.map(|m| m.agent.clone())
}

/// Routes a new ticket: the first enabled rule (highest priority) whose team fits and whose
/// condition matches sets the team and, if a member is available, the agent. The rule row is locked
/// for the rest of the transaction, so concurrent tickets take turns instead of colliding.
async fn assign(hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
    if s(doc, "assigned_to").is_some() {
        return Ok(());
    }
    let sctx = sys(hc.ctx);
    let rules = hc.kernel.find_tx(&sctx, hc.tx, "TicketAssignmentRule", &[Filter::eq("enabled", true)], Some("priority desc"), 1000).await?;
    let view = hc.entity.cel_view(&Value::Object(doc.clone()), &hc.ctx.schema);
    let user = hc.ctx.user_value();
    // Agents limited to some teams route only into those teams (anything else would be refused).
    let allowed = if hc.ctx.is_admin || hc.ctx.is_system { None } else { hc.ctx.scopes.get("SupportTeam").cloned() };
    for rule in rules {
        let (Some(rule_id), Some(team)) = (id(&rule, "id"), id(&rule, "team")) else { continue };
        if id(doc, "team").is_some_and(|t| t != team) || allowed.as_ref().is_some_and(|a| !a.contains(&team)) {
            continue;
        }
        if let Some(cond) = s(&rule, "condition") {
            match Expr::compile(cond).and_then(|e| e.eval_bool(&Scope { doc: &view, old: None, parent: None, user: &user })) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    tracing::warn!(rule = %rule_id, "assignment rule condition failed: {e}");
                    continue;
                }
            }
        }
        let rule = hc.kernel.get_tx(&sctx, hc.tx, "TicketAssignmentRule", rule_id, true).await?;
        let members: Vec<Member> = rows(&get(hc.kernel, hc.ctx, hc.tx, "SupportTeam", team).await?, "members")
            .iter()
            .filter_map(|m| Some(Member { agent: s(m, "agent")?.to_string(), active: b(m, "active"), max_open: int(m, "max_open_tickets") }))
            .collect();
        let agents: Vec<String> = members.iter().map(|m| m.agent.clone()).collect();
        let open: HashMap<String, i64> = sqlx::query_as::<_, (String, i64)>(
            "select assigned_to, count(*) from e_ticket where tenant_id = $1 and assigned_to = any($2) and status = any($3) group by assigned_to",
        )
        .bind(hc.ctx.tenant)
        .bind(&agents)
        .bind(&OPEN[..])
        .fetch_all(&mut **hc.tx)
        .await?
        .into_iter()
        .collect();
        set(doc, "team", team.to_string());
        if let Some(agent) = pick_member(&members, s(&rule, "last_assigned_to"), &open, s(&rule, "rule_type") == Some("Load Balancing")) {
            set(doc, "assigned_to", agent.clone());
            let mut v = Doc::new();
            v.insert("last_assigned_to".into(), json!(agent));
            hc.kernel.set_values_tx(&sctx, hc.tx, "TicketAssignmentRule", rule_id, v).await?;
        }
        return Ok(());
    }
    Ok(())
}

// ───────────────────────────── tickets ─────────────────────────────

pub struct TicketHook;

/// An in-app notification for the user signed in with `email`, if there is one.
async fn notify_agent(tx: &mut Tx, tenant: Uuid, email: &str, title: &str, ticket: Uuid) -> Result<()> {
    let user: Option<Uuid> = sqlx::query_scalar("select id from app_user where tenant_id = $1 and email = $2 and is_active")
        .bind(tenant)
        .bind(email)
        .fetch_optional(&mut **tx)
        .await?;
    if let Some(user) = user {
        notify::send(tx, tenant, user, title, None, Some("Ticket"), Some(ticket)).await?;
    }
    Ok(())
}

/// The stored version of the document being saved (`None` while it is being created).
async fn stored(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Option<Doc>> {
    match id(doc, "id") {
        Some(i) => store::load(hc.tx, &hc.ctx.schema, hc.entity, hc.ctx.tenant, i, false).await,
        None => Ok(None),
    }
}

#[async_trait]
impl DocHook for TicketHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let now = Utc::now();
        let old = stored(hc, doc).await?;
        if let Some(o) = &old
            && s(o, "status") == Some("Closed")
        {
            // Final, checked before anything else so the refusal is the first thing said.
            let changed =
                hc.entity.def.fields.iter().any(|f| !matches!(f.name.as_str(), "satisfaction_rating" | "feedback") && o.get(&f.name) != doc.get(&f.name));
            return if changed {
                Err(Error::invalid("a closed ticket is final: only the satisfaction rating and feedback can still be recorded; open a new ticket instead"))
            } else {
                Ok(())
            };
        }

        // New messages: author and server time (replies to an existing ticket cannot be backdated).
        let known: HashSet<String> =
            old.as_ref().map(|o| rows(o, "conversation").iter().filter_map(|r| s(r, "id").map(str::to_string)).collect()).unwrap_or_default();
        let contact = s(doc, "contact_email").map(str::to_string);
        let mut last_new = None;
        let mut messages = rows(doc, "conversation");
        for m in messages.iter_mut().filter(|m| !s(m, "id").is_some_and(|i| known.contains(i))) {
            let kind = s(m, "author_kind").unwrap_or("Agent").to_string();
            if s(m, "author").is_none() {
                let author = if kind == "Customer" { contact.clone() } else { Some(hc.ctx.email.clone()) };
                set(m, "author", author.map_or(Value::Null, Value::String));
            }
            if old.is_some() || s(m, "sent_at").is_none() {
                set(m, "sent_at", ts_value(now));
            }
            last_new = Some((kind == "Customer", b(m, "is_internal")));
            set(m, "author_kind", kind);
        }
        set(doc, "conversation", Value::Array(messages.into_iter().map(Value::Object).collect()));

        // The status follows the conversation unless this save sets it: a customer reply reopens,
        // the first public agent reply on an open ticket marks it replied.
        let explicit = old.as_ref().is_some_and(|o| s(o, "status") != s(doc, "status"));
        if !explicit && let Some((from_customer, internal)) = last_new {
            let current = s(doc, "status").unwrap_or("Open").to_string();
            match (from_customer, internal, current.as_str()) {
                (true, _, "Replied" | "Resolved" | "On Hold") => set(doc, "status", "Open"),
                (false, false, "Open") => set(doc, "status", "Replied"),
                _ => {}
            }
        }

        let status = s(doc, "status").unwrap_or("Open").to_string();
        let opened = ts(doc, "opened_at").unwrap_or(now);
        let at = now.max(opened);
        let mut tl = timeline(doc);
        if tl.is_empty() {
            tl.push(("Open".into(), opened));
        }
        if tl.last().is_some_and(|(st, _)| *st != status) {
            tl.push((status.clone(), at));
        }
        set(doc, "status_history", timeline_value(&tl));

        if ts(doc, "first_responded_at").is_none() {
            let first = rows(doc, "conversation")
                .iter()
                .filter(|m| s(m, "author_kind") == Some("Agent") && !b(m, "is_internal"))
                .filter_map(|m| ts(m, "sent_at"))
                .min();
            if let Some(t) = first {
                set(doc, "first_responded_at", ts_value(t.max(opened)));
            }
        }
        if matches!(status.as_str(), "Resolved" | "Closed") {
            let resolved = ts(doc, "resolved_at").unwrap_or(at);
            set(doc, "resolved_at", ts_value(resolved));
            // The resolution answers the customer too.
            if ts(doc, "first_responded_at").is_none() {
                set(doc, "first_responded_at", ts_value(resolved));
            }
            if status == "Closed" && ts(doc, "closed_at").is_none() {
                set(doc, "closed_at", ts_value(at));
            }
        } else {
            set(doc, "resolved_at", Value::Null);
        }

        let (customer, groups) = customer_and_groups(hc.kernel, hc.ctx, hc.tx, id(doc, "customer")).await?;
        if old.is_none() {
            if s(doc, "contact_email").is_none()
                && let Some(email) = customer.as_ref().and_then(|c| s(c, "email"))
            {
                set(doc, "contact_email", email.to_string());
            }
            assign(hc, doc).await?;
        }
        let (cust, ticket_type) = (id(doc, "customer"), id(doc, "ticket_type"));
        let slas = load_slas(hc.kernel, hc.ctx, hc.tx, |h| applies(h, cust, &groups, ticket_type)).await?;
        doc.extend(sla_fields(&slas, &groups, doc, now));
        Ok(())
    }

    /// Tells the agent in the app when a ticket becomes theirs or the customer writes back
    /// (automations cannot address the user behind an email field).
    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        let (Some(agent), Some(ticket)) = (s(doc, "assigned_to"), id(doc, "id")) else { return Ok(()) };
        let label = format!("{} {}", s(doc, "number").unwrap_or("Ticket"), s(doc, "subject").unwrap_or_default());
        if old.is_none_or(|o| s(o, "assigned_to") != Some(agent)) && agent != hc.ctx.email {
            notify_agent(hc.tx, hc.ctx.tenant, agent, &format!("{label} was assigned to you"), ticket).await?;
        }
        let known: HashSet<String> = old.map(|o| rows(o, "conversation").iter().filter_map(|r| s(r, "id").map(str::to_string)).collect()).unwrap_or_default();
        let replied = rows(doc, "conversation").iter().any(|m| s(m, "author_kind") == Some("Customer") && !s(m, "id").is_some_and(|i| known.contains(i)));
        if old.is_some() && replied {
            notify_agent(hc.tx, hc.ctx.tenant, agent, &format!("The customer replied on {label}"), ticket).await?;
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let old = stored(hc, doc).await?;
        let mut errors = Vec::new();
        let opened = ts(doc, "opened_at");
        match &old {
            Some(o) if o.get("opened_at") != doc.get("opened_at") => errors.push(FieldError::new("opened_at", "cannot be changed once the ticket exists")),
            None if opened.is_some_and(|t| t > Utc::now() + Duration::minutes(5)) => errors.push(FieldError::new("opened_at", "cannot be in the future")),
            _ => {}
        }
        // The conversation is a record: what was sent stays as it was.
        let messages = rows(doc, "conversation");
        if let Some(o) = &old {
            let fields: Vec<String> = hc.ctx.schema.entity("TicketMessage").map(|e| e.value_fields().map(|f| f.name.clone()).collect()).unwrap_or_default();
            for before in rows(o, "conversation") {
                match messages.iter().position(|m| s(m, "id") == s(&before, "id")) {
                    None => {
                        errors.push(FieldError::new("conversation", "sent messages cannot be removed"));
                        break;
                    }
                    Some(i) if fields.iter().any(|f| messages[i].get(f) != before.get(f)) => {
                        errors.push(FieldError::new("body", "sent messages cannot be edited").in_row("conversation", i))
                    }
                    _ => {}
                }
            }
        }
        for (i, m) in messages.iter().enumerate() {
            if s(m, "author_kind") == Some("Customer") && b(m, "is_internal") {
                errors.push(FieldError::new("is_internal", "customers cannot write internal notes").in_row("conversation", i));
            }
            if let (Some(sent), Some(o)) = (ts(m, "sent_at"), opened)
                && sent < o
            {
                errors.push(FieldError::new("sent_at", "is before the ticket was opened").in_row("conversation", i));
            }
        }
        let rating = doc.get("satisfaction_rating").filter(|v| !v.is_null());
        let rated = rating.is_some() && old.as_ref().is_none_or(|o| o.get("satisfaction_rating") != doc.get("satisfaction_rating"));
        if rated && !matches!(s(doc, "status"), Some("Resolved" | "Closed")) {
            errors.push(FieldError::new("satisfaction_rating", "can be given once the ticket is resolved"));
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "make_warranty_claim" {
            return Ok(None);
        }
        let mut input = Doc::new();
        for f in ["customer", "item", "serial_no"] {
            input.insert(f.into(), doc.get(f).cloned().unwrap_or(Value::Null));
        }
        input.insert("complaint".into(), json!(s(doc, "description").or(s(doc, "subject")).unwrap_or_default()));
        input.insert("ticket".into(), doc.get("id").cloned().unwrap_or(Value::Null));
        // The back link is read-only: only the engine sets it, for a user allowed to create claims.
        let claim = create_for_user(hc, "WarrantyClaim", input).await?;
        Ok(Some(created("WarrantyClaim", &claim)))
    }
}

// ───────────────────────────── maintenance ─────────────────────────────

fn delta(doc: &Doc, fields: Doc) -> Doc {
    fields.into_iter().filter(|(k, v)| doc.get(k).unwrap_or(&Value::Null) != v).collect()
}

async fn groups_cached(k: &Kernel, ctx: &Ctx, tx: &mut Tx, cache: &mut HashMap<Option<Uuid>, Vec<Uuid>>, customer: Option<Uuid>) -> Result<Vec<Uuid>> {
    if let Some(g) = cache.get(&customer) {
        return Ok(g.clone());
    }
    let (_, groups) = customer_and_groups(k, ctx, tx, customer).await?;
    cache.insert(customer, groups.clone());
    Ok(groups)
}

/// Re-evaluates the SLA of every ticket still being worked on (verdicts change with time; a new
/// or edited agreement may now apply). Only changed values are written, without a new version, so
/// an agent's open form stays valid; a changed verdict is published as `Ticket.updated` for
/// automations and webhooks. Safe on several replicas at once: rows are re-read under a lock.
pub async fn refresh_slas(k: &Kernel, ctx: &Ctx) -> Result<usize> {
    // ponytail: one read per open ticket per run; select by a stored next-deadline column if a
    // tenant keeps ~100k tickets open.
    let ent = k.entity(ctx, "Ticket")?;
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let slas = load_slas(k, ctx, &mut tx, |_| true).await?;
    let ids: Vec<Uuid> = sqlx::query_scalar("select id from e_ticket where tenant_id = $1 and status = any($2) order by id")
        .bind(ctx.tenant)
        .bind(&OPEN[..])
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let mut groups = HashMap::new();
    let mut changed = 0;
    for chunk in ids.chunks(200) {
        let now = Utc::now();
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        for &ticket in chunk {
            let Some(doc) = store::load(&mut tx, &ctx.schema, &ent, ctx.tenant, ticket, false).await? else { continue };
            let g = groups_cached(k, ctx, &mut tx, &mut groups, id(&doc, "customer")).await?;
            if delta(&doc, sla_fields(&slas, &g, &doc, now)).is_empty() {
                continue;
            }
            // Something moved: recompute from the locked row (an agent may have just saved it).
            let Some(doc) = store::load(&mut tx, &ctx.schema, &ent, ctx.tenant, ticket, true).await? else { continue };
            if !s(&doc, "status").is_some_and(|st| OPEN.contains(&st)) {
                continue;
            }
            let g = groups_cached(k, ctx, &mut tx, &mut groups, id(&doc, "customer")).await?;
            let values = delta(&doc, sla_fields(&slas, &g, &doc, now));
            if values.is_empty() {
                continue;
            }
            let publish = VERDICT.iter().any(|f| values.contains_key(*f));
            let breached = values.get("sla_status") == Some(&json!("Breached"));
            k.set_values_tx(ctx, &mut tx, "Ticket", ticket, values).await?;
            if breached && let Some(agent) = s(&doc, "assigned_to") {
                let label = format!("{} {}", s(&doc, "number").unwrap_or("Ticket"), s(&doc, "subject").unwrap_or_default());
                notify_agent(&mut tx, ctx.tenant, agent, &format!("SLA breached on {label}"), ticket).await?;
            }
            if publish {
                let fresh = store::load(&mut tx, &ctx.schema, &ent, ctx.tenant, ticket, false).await?;
                let payload = json!({"entity": "Ticket", "id": ticket, "event": "updated", "actor": Value::Null, "depth": ctx.depth, "request_id": ctx.request_id, "doc": fresh});
                outbox::enqueue(&mut tx, ctx.tenant, "Ticket.updated", Some("Ticket"), Some(ticket), payload).await?;
            }
            changed += 1;
        }
        tx.commit().await?;
    }
    Ok(changed)
}

/// Closes tickets resolved longer ago than the grace period (Support Settings, 7 days by default)
/// through the full document pipeline (event, audit). The version read with the candidate is
/// required, so a ticket reopened in the meantime is left alone.
pub async fn auto_close(k: &Kernel, ctx: &Ctx) -> Result<usize> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let setting: Option<Option<i64>> =
        sqlx::query_scalar("select auto_close_days from e_support_settings where tenant_id = $1 limit 1").bind(ctx.tenant).fetch_optional(&mut *tx).await?;
    let days = setting.flatten().unwrap_or(7);
    if days <= 0 {
        tx.commit().await?;
        return Ok(0);
    }
    let due: Vec<(Uuid, i32)> = sqlx::query_as(
        "select id, version from e_ticket where tenant_id = $1 and status = 'Resolved' and resolved_at < now() - make_interval(days => $2) order by resolved_at",
    )
    .bind(ctx.tenant)
    .bind(days.min(36_500) as i32)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut closed = 0;
    for (ticket, version) in due {
        let mut patch = Doc::new();
        patch.insert("status".into(), json!("Closed"));
        match k.update(ctx, "Ticket", ticket, patch, Some(version.into())).await {
            Ok(_) => closed += 1,
            Err(Error::Conflict(_)) => {}
            Err(e) => tracing::warn!(%ticket, "auto-close failed: {e}"),
        }
    }
    Ok(closed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(agent: &str, active: bool, max_open: i64) -> Member {
        Member { agent: agent.into(), active, max_open }
    }

    #[test]
    fn round_robin_takes_turns_and_skips_inactive_and_full_members() {
        let team = [m("a", true, 0), m("b", false, 0), m("c", true, 2), m("d", true, 0)];
        let mut open = HashMap::new();
        let mut last: Option<String> = None;
        let mut got = Vec::new();
        for _ in 0..6 {
            let next = pick_member(&team, last.as_deref(), &open, false).unwrap();
            *open.entry(next.clone()).or_insert(0) += 1;
            got.push(next.clone());
            last = Some(next);
        }
        assert_eq!(got, ["a", "c", "d", "a", "c", "d"]);
        // c now has 2 open tickets (its limit): it is skipped from now on.
        assert_eq!(pick_member(&team, Some("a"), &open, false).as_deref(), Some("d"));
        assert_eq!(pick_member(&team, Some("unknown"), &open, false).as_deref(), Some("a"), "a removed agent restarts the turn");
        let nobody = [m("x", false, 0), m("y", true, 1)];
        assert_eq!(pick_member(&nobody, None, &HashMap::from([("y".to_string(), 1)]), false), None);
        assert_eq!(pick_member(&[], None, &HashMap::new(), false), None);
    }

    #[test]
    fn load_balancing_picks_the_least_loaded_in_turn_order() {
        let team = [m("a", true, 0), m("b", true, 0), m("c", true, 0)];
        let open = HashMap::from([("a".to_string(), 3), ("b".to_string(), 1), ("c".to_string(), 1)]);
        assert_eq!(pick_member(&team, None, &open, true).as_deref(), Some("b"));
        assert_eq!(pick_member(&team, Some("b"), &open, true).as_deref(), Some("c"), "ties go to whoever's turn comes first");
        let capped = [m("a", true, 0), m("b", true, 1)];
        assert_eq!(pick_member(&capped, None, &HashMap::from([("a".to_string(), 5), ("b".to_string(), 1)]), true).as_deref(), Some("a"));
    }

    #[test]
    fn working_hours_parse() {
        assert_eq!(parse_hm("09:00"), Some(540));
        assert_eq!(parse_hm("9:30"), Some(570));
        assert_eq!(parse_hm("24:00"), Some(1440));
        for bad in ["24:01", "25:00", "09:60", "0900", "9", "09:0", ":30", "ab:cd"] {
            assert_eq!(parse_hm(bad), None, "{bad}");
        }
    }

    fn sla(name: &str, customer: Option<Uuid>, group: Option<Uuid>, ticket_type: Option<Uuid>, priorities: &[(&str, &str, &str)]) -> Sla {
        let mut doc = Doc::new();
        doc.insert("id".into(), json!(Uuid::now_v7().to_string()));
        doc.insert("sla_name".into(), json!(name));
        doc.insert("customer".into(), json!(customer.map(|c| c.to_string())));
        doc.insert("customer_group".into(), json!(group.map(|c| c.to_string())));
        doc.insert("ticket_type".into(), json!(ticket_type.map(|c| c.to_string())));
        doc.insert("pause_on_hold".into(), json!(true));
        doc.insert("pause_on_replied".into(), json!(true));
        doc.insert(
            "priorities".into(),
            Value::Array(priorities.iter().map(|(p, r, z)| json!({"priority": p, "response_hours": r, "resolution_hours": z})).collect()),
        );
        doc.insert(
            "working_hours".into(),
            Value::Array(WEEKDAYS[..5].iter().map(|w| json!({"weekday": w, "start_time": "09:00", "end_time": "18:00"})).collect()),
        );
        doc.insert("holidays".into(), json!([{"holiday_date": "2026-11-02"}]));
        Sla::from_doc(&doc).unwrap()
    }

    fn ticket(fields: Value) -> Doc {
        fields.as_object().cloned().unwrap()
    }

    #[test]
    fn the_most_specific_agreement_with_the_priority_applies() {
        let (acme, retail, all, incident) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let slas = [
            sla("Default", None, None, None, &[("Low", "16", "80"), ("Medium", "8", "40"), ("High", "4", "16"), ("Urgent", "1", "4")]),
            sla("Incidents", None, None, Some(incident), &[("High", "2", "8")]),
            sla("All Customers", None, Some(all), None, &[("High", "3", "12")]),
            sla("Retail", None, Some(retail), None, &[("High", "2", "10")]),
            sla("Acme", Some(acme), None, None, &[("High", "1", "4")]),
        ];
        let chain = [retail, all];
        let name = |doc: Value, groups: &[Uuid]| pick(&slas, &ticket(doc), groups).map(|x| x.name.clone());
        assert_eq!(name(json!({"priority": "High", "customer": acme.to_string()}), &chain).as_deref(), Some("Acme"));
        assert_eq!(name(json!({"priority": "High"}), &chain).as_deref(), Some("Retail"), "the nearest group wins");
        assert_eq!(name(json!({"priority": "High"}), &[all]).as_deref(), Some("All Customers"));
        assert_eq!(name(json!({"priority": "High", "ticket_type": incident.to_string()}), &[]).as_deref(), Some("Incidents"));
        assert_eq!(name(json!({"priority": "Low", "customer": acme.to_string()}), &chain).as_deref(), Some("Default"), "Acme sets no Low target");
    }

    #[test]
    fn sla_fields_from_the_timeline() {
        let slas = [sla("Default", None, None, None, &[("Medium", "8", "40"), ("Urgent", "1", "4")])];
        let t = |s: &str| parse_datetime(s).unwrap();
        // Opened Friday 17:00 (Monday 2 November is a holiday): 1h on Friday + 7h on Tuesday.
        let mut doc = ticket(json!({
            "priority": "Medium", "status": "Open", "opened_at": "2026-10-30T17:00:00.000Z",
            "status_history": [{"status": "Open", "at": "2026-10-30T17:00:00.000Z"}],
        }));
        let f = sla_fields(&slas, &[], &doc, t("2026-10-30T17:30:00Z"));
        assert_eq!(f["response_by"], json!("2026-11-03T16:00:00.000Z"));
        assert_eq!(f["sla_status"], json!("On Track"));
        assert_eq!(f["service_level_agreement"], json!(slas[0].id.to_string()));
        // Urgent: due Friday 18:00, so by Tuesday it has breached.
        doc.insert("priority".into(), json!("Urgent"));
        let f = sla_fields(&slas, &[], &doc, t("2026-11-03T09:30:00Z"));
        assert_eq!(f["response_by"], json!("2026-10-30T18:00:00.000Z"));
        assert_eq!((f["sla_status"].clone(), f["response_breached"].clone()), (json!("Breached"), json!(true)));
        // On hold since Friday 17:30: pending deadlines wait for the resume, the verdict is frozen.
        doc.insert("priority".into(), json!("Medium"));
        doc.insert("status".into(), json!("On Hold"));
        doc.insert(
            "status_history".into(),
            json!([{"status": "Open", "at": "2026-10-30T17:00:00.000Z"}, {"status": "On Hold", "at": "2026-10-30T17:30:00.000Z"}]),
        );
        let f = sla_fields(&slas, &[], &doc, t("2026-11-05T12:00:00Z"));
        assert_eq!((f["response_by"].clone(), f["sla_status"].clone(), f["paused_hours"].clone()), (Value::Null, json!("On Track"), json!("0")));
        // Resumed Wednesday 10:00: the 7.5 business hours left run from then; 10.5 were paused.
        doc.insert("status".into(), json!("Open"));
        doc.insert(
            "status_history".into(),
            json!([{"status": "Open", "at": "2026-10-30T17:00:00.000Z"}, {"status": "On Hold", "at": "2026-10-30T17:30:00.000Z"}, {"status": "Open", "at": "2026-11-04T10:00:00.000Z"}]),
        );
        let f = sla_fields(&slas, &[], &doc, t("2026-11-04T10:00:00Z"));
        assert_eq!(f["response_by"], json!("2026-11-04T17:30:00.000Z"));
        assert_eq!(f["paused_hours"], json!("10.5"), "0.5h Friday + 9h Tuesday + 1h Wednesday");
        // No agreement: no deadlines, but hours are still measured (round the clock).
        let none = sla_fields(&[], &[], &doc, t("2026-11-04T10:00:00Z"));
        assert_eq!((none["sla_status"].clone(), none["response_breached"].clone()), (Value::Null, json!(false)));
        assert_eq!(none["paused_hours"], json!("112.5"));
    }
}
