//! Helpdesk reports: ticket volume, SLA compliance, response and resolution times in business
//! hours, agent workload and customer satisfaction. Every query carries the caller's record scopes
//! on tickets, so an agent limited to some teams sees those teams' numbers only.

use crate::child_table;
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_domain::reports::{Params, ReportCompany, ReportSource, col, param_def, period, read_tx};
use ferrum_domain::util::money;
use ferrum_kernel::{Action, Ctx, Error, Kernel, Result, Tx, row_scope};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};
use std::collections::HashMap;
use uuid::Uuid;

pub struct SupportReports;

/// Groupings: label → SQL over the ticket `a`, its team `tm`, type `tt` and agreement `sl`.
const GROUPS: [(&str, &str); 8] = [
    ("Status", "a.status"),
    ("Priority", "a.priority"),
    ("Team", "coalesce(tm.team_name, 'No Team')"),
    ("Agent", "coalesce(a.assigned_to, 'Unassigned')"),
    ("Ticket Type", "coalesce(tt.type_name, 'No Type')"),
    ("Channel", "a.channel"),
    ("Customer", "coalesce(a.customer_name, a.contact_email, 'Unknown')"),
    ("SLA", "coalesce(sl.sla_name, 'No SLA')"),
];

fn group_param(default: &str) -> Value {
    json!({"name": "group_by", "label": "Group By", "type": "select", "options": GROUPS.map(|(l, _)| l), "default": default})
}

fn grouping(p: &Params<'_>, default: &str) -> Result<(&'static str, &'static str)> {
    let want = p.str("group_by").unwrap_or(default);
    GROUPS
        .iter()
        .copied()
        .find(|(label, _)| label.eq_ignore_ascii_case(want))
        .ok_or_else(|| Error::field("group_by", format!("use one of: {}", GROUPS.map(|(l, _)| l).join(", "))))
}

/// Tickets belong to no company: the default period follows the tenant's fiscal year.
fn period_of(ctx: &Ctx, p: &Params<'_>) -> Result<(NaiveDate, NaiveDate)> {
    let co = ReportCompany { id: Uuid::nil(), name: String::new(), currency: String::new(), fy_month: ctx.fy_start_month, precision: 2 };
    period(p, &co)
}

/// `select <columns> from` the caller's visible tickets with their team, type and agreement.
fn tickets(ctx: &Ctx, columns: &str) -> Result<QueryBuilder<Postgres>> {
    let ent = ctx.schema.entity("Ticket").ok_or_else(|| Error::NotFound("Ticket".into()))?;
    let scope = row_scope(ctx, ent, Action::Read)?;
    let mut qb = QueryBuilder::new(format!(
        "select {columns} from e_ticket a \
         left join e_support_team tm on tm.tenant_id = a.tenant_id and tm.id = a.team \
         left join e_ticket_type tt on tt.tenant_id = a.tenant_id and tt.id = a.ticket_type \
         left join e_service_level_agreement sl on sl.tenant_id = a.tenant_id and sl.id = a.service_level_agreement \
         where a.tenant_id = "
    ));
    qb.push_bind(ctx.tenant);
    scope.push_sql(&mut qb, "a");
    Ok(qb)
}

fn within(qb: &mut QueryBuilder<Postgres>, column: &str, (from, to): (NaiveDate, NaiveDate)) {
    qb.push(format!(" and {column} >= ")).push_bind(from).push(format!(" and {column} < ")).push_bind(to).push(" + 1");
}

fn pct(part: i64, whole: i64) -> Value {
    if whole == 0 { Value::Null } else { money((Decimal::from(part) * Decimal::ONE_HUNDRED / Decimal::from(whole)).round_dp(2)) }
}

fn dec(v: Option<Decimal>) -> Value {
    v.map_or(Value::Null, money)
}

fn meta(span: (NaiveDate, NaiveDate), group: &str) -> Value {
    json!({"from_date": span.0, "to_date": span.1, "group_by": group})
}

async fn summary(ctx: &Ctx, p: &Params<'_>, tx: &mut Tx) -> Result<Value> {
    let span = period_of(ctx, p)?;
    let (label, expr) = grouping(p, "Status")?;
    let mut qb = tickets(
        ctx,
        &format!(
            "{expr}, count(*), count(*) filter (where a.status = 'Open'), count(*) filter (where a.status = 'Replied'), \
             count(*) filter (where a.status = 'On Hold'), count(*) filter (where a.status = 'Resolved'), \
             count(*) filter (where a.status = 'Closed'), count(*) filter (where a.sla_status = 'Breached')"
        ),
    )?;
    within(&mut qb, "a.opened_at", span);
    qb.push(" group by 1 order by 2 desc, 1");
    let data: Vec<(String, i64, i64, i64, i64, i64, i64, i64)> = qb.build_query_as().fetch_all(&mut **tx).await?;
    let keys = ["tickets", "open", "replied", "on_hold", "resolved", "closed", "breached"];
    let mut totals = [0i64; 7];
    let rows: Vec<Value> = data
        .into_iter()
        .map(|(group, a, b, c, d, e, f, g)| {
            let mut row = json!({"group": group});
            for ((k, t), x) in keys.iter().zip(totals.iter_mut()).zip([a, b, c, d, e, f, g]) {
                row[*k] = json!(x);
                *t += x;
            }
            row
        })
        .collect();
    Ok(json!({
        "columns": [col("group", label, "data"), col("tickets", "Tickets", "int"), col("open", "Open", "int"), col("replied", "Replied", "int"),
            col("on_hold", "On Hold", "int"), col("resolved", "Resolved", "int"), col("closed", "Closed", "int"), col("breached", "SLA Breached", "int")],
        "rows": rows,
        "totals": keys.iter().zip(totals).map(|(k, t)| (k.to_string(), json!(t))).collect::<serde_json::Map<_, _>>(),
        "meta": meta(span, label),
    }))
}

async fn compliance(ctx: &Ctx, p: &Params<'_>, tx: &mut Tx) -> Result<Value> {
    let span = period_of(ctx, p)?;
    let (label, expr) = grouping(p, "SLA")?;
    let mut qb = tickets(
        ctx,
        &format!(
            "{expr}, count(*), count(*) filter (where a.first_responded_at is not null and not a.response_breached), \
             count(*) filter (where a.response_breached), count(*) filter (where a.resolved_at is not null and not a.resolution_breached), \
             count(*) filter (where a.resolution_breached)"
        ),
    )?;
    within(&mut qb, "a.opened_at", span);
    qb.push(" and a.service_level_agreement is not null group by 1 order by 1");
    let data: Vec<(String, i64, i64, i64, i64, i64)> = qb.build_query_as().fetch_all(&mut **tx).await?;
    let row = |group: String, n: i64, rm: i64, rb: i64, zm: i64, zb: i64| {
        json!({"group": group, "tickets": n,
            "response_met": rm, "response_breached": rb, "response_pending": n - rm - rb, "response_pct": pct(rm, rm + rb),
            "resolution_met": zm, "resolution_breached": zb, "resolution_pending": n - zm - zb, "resolution_pct": pct(zm, zm + zb)})
    };
    let mut t = (0, 0, 0, 0, 0);
    let rows: Vec<Value> = data
        .into_iter()
        .map(|(g, n, rm, rb, zm, zb)| {
            t = (t.0 + n, t.1 + rm, t.2 + rb, t.3 + zm, t.4 + zb);
            row(g, n, rm, rb, zm, zb)
        })
        .collect();
    let mut totals = row(String::new(), t.0, t.1, t.2, t.3, t.4);
    if let Value::Object(m) = &mut totals {
        m.remove("group");
    }
    Ok(json!({
        "columns": [col("group", label, "data"), col("tickets", "Tickets", "int"),
            col("response_met", "Response Met", "int"), col("response_breached", "Response Breached", "int"), col("response_pending", "Response Pending", "int"),
            col("response_pct", "Response Compliance %", "decimal"),
            col("resolution_met", "Resolution Met", "int"), col("resolution_breached", "Resolution Breached", "int"), col("resolution_pending", "Resolution Pending", "int"),
            col("resolution_pct", "Resolution Compliance %", "decimal")],
        "rows": rows,
        "totals": totals,
        "meta": meta(span, label),
    }))
}

type TimesRow = (String, i64, i64, Option<Decimal>, Option<Decimal>, Option<Decimal>, i64, Option<Decimal>, Option<Decimal>, Option<Decimal>);

async fn times(ctx: &Ctx, p: &Params<'_>, tx: &mut Tx) -> Result<Value> {
    let span = period_of(ctx, p)?;
    let (label, expr) = grouping(p, "Priority")?;
    let stats = |f: &str| {
        format!(
            "count(a.{f}), round(avg(a.{f}), 2), round((percentile_cont(0.5) within group (order by a.{f}))::numeric, 2), \
             round((percentile_cont(0.9) within group (order by a.{f}))::numeric, 2)"
        )
    };
    let columns = format!("count(*), {}, {}", stats("first_response_hours"), stats("resolution_hours"));
    let mut grouped = tickets(ctx, &format!("{expr}, {columns}"))?;
    within(&mut grouped, "a.opened_at", span);
    grouped.push(" group by 1 order by 1");
    let data: Vec<TimesRow> = grouped.build_query_as().fetch_all(&mut **tx).await?;
    let mut all = tickets(ctx, &format!("'All'::text, {columns}"))?;
    within(&mut all, "a.opened_at", span);
    let total: TimesRow = all.build_query_as().fetch_one(&mut **tx).await?;
    let row = |(group, n, rn, ravg, rmed, rp90, zn, zavg, zmed, zp90): TimesRow| {
        json!({"group": group, "tickets": n,
            "responded": rn, "response_avg": dec(ravg), "response_median": dec(rmed), "response_p90": dec(rp90),
            "resolved": zn, "resolution_avg": dec(zavg), "resolution_median": dec(zmed), "resolution_p90": dec(zp90)})
    };
    let mut totals = row(total);
    if let Value::Object(m) = &mut totals {
        m.remove("group");
    }
    Ok(json!({
        "columns": [col("group", label, "data"), col("tickets", "Tickets", "int"), col("responded", "Responded", "int"),
            col("response_avg", "First Response Avg (h)", "decimal"), col("response_median", "First Response Median (h)", "decimal"),
            col("response_p90", "First Response 90th Pct (h)", "decimal"), col("resolved", "Resolved", "int"),
            col("resolution_avg", "Resolution Avg (h)", "decimal"), col("resolution_median", "Resolution Median (h)", "decimal"),
            col("resolution_p90", "Resolution 90th Pct (h)", "decimal")],
        "rows": data.into_iter().map(row).collect::<Vec<_>>(),
        "totals": totals,
        "meta": {"from_date": span.0, "to_date": span.1, "group_by": label, "unit": "business hours (round the clock without an SLA), pauses excluded"},
    }))
}

type WorkRow = (String, i64, i64, i64, i64, i64, i64, i64, i64, Option<Decimal>, Option<String>);

async fn workload(ctx: &Ctx, p: &Params<'_>, tx: &mut Tx) -> Result<Value> {
    let team = p.uuid("team")?;
    let mut qb = tickets(
        ctx,
        "coalesce(a.assigned_to, ''), count(*), count(*) filter (where a.status = 'Open'), count(*) filter (where a.status = 'Replied'), \
         count(*) filter (where a.status = 'On Hold'), count(*) filter (where a.priority = 'Urgent'), count(*) filter (where a.priority = 'High'), \
         count(*) filter (where a.sla_status = 'At Risk'), count(*) filter (where a.sla_status = 'Breached'), \
         round((extract(epoch from now() - min(a.opened_at)) / 86400)::numeric, 2), (array_agg(a.number order by a.opened_at))[1]",
    )?;
    qb.push(" and a.status in ('Open', 'Replied', 'On Hold')");
    if let Some(t) = team {
        qb.push(" and a.team = ").push_bind(t);
    }
    qb.push(" group by 1 order by 2 desc, 1");
    let mut data: Vec<WorkRow> = qb.build_query_as().fetch_all(&mut **tx).await?;
    // Active team members without open tickets belong in a workload view too.
    let members = child_table(ctx, "SupportTeam", "members")?;
    let mut q = QueryBuilder::new(format!("select distinct m.agent from {members} m where m.tenant_id = "));
    q.push_bind(ctx.tenant).push(" and m.active and m.agent is not null");
    if let Some(t) = team {
        q.push(" and m.parent_id = ").push_bind(t);
    }
    if let Some(allowed) = ctx.scopes.get("SupportTeam").filter(|_| !ctx.is_admin && !ctx.is_system) {
        q.push(" and m.parent_id = any(").push_bind(allowed.clone()).push(")");
    }
    let mut idle: Vec<String> = q.build_query_scalar().fetch_all(&mut **tx).await?;
    idle.retain(|agent| !data.iter().any(|r| &r.0 == agent));
    idle.sort();
    data.extend(idle.into_iter().map(|agent| (agent, 0, 0, 0, 0, 0, 0, 0, 0, None, None)));
    let keys = ["open_tickets", "open", "replied", "on_hold", "urgent", "high", "at_risk", "breached"];
    let mut totals = [0i64; 8];
    let rows: Vec<Value> = data
        .into_iter()
        .map(|(agent, n, o, r, h, u, hi, ar, br, oldest, number)| {
            let mut row =
                json!({"agent": if agent.is_empty() { "Unassigned".to_string() } else { agent }, "oldest_open_days": dec(oldest), "oldest_ticket": number});
            for ((k, t), x) in keys.iter().zip(totals.iter_mut()).zip([n, o, r, h, u, hi, ar, br]) {
                row[*k] = json!(x);
                *t += x;
            }
            row
        })
        .collect();
    Ok(json!({
        "columns": [col("agent", "Agent", "data"), col("open_tickets", "Open Tickets", "int"), col("open", "Open", "int"), col("replied", "Replied", "int"),
            col("on_hold", "On Hold", "int"), col("urgent", "Urgent", "int"), col("high", "High", "int"), col("at_risk", "At Risk", "int"),
            col("breached", "Breached", "int"), col("oldest_open_days", "Oldest Open (Days)", "decimal"), col("oldest_ticket", "Oldest Ticket", "data")],
        "rows": rows,
        "totals": keys.iter().zip(totals).map(|(k, t)| (k.to_string(), json!(t))).collect::<serde_json::Map<_, _>>(),
    }))
}

type CsatRow = (String, i64, Option<Decimal>, i64, i64, i64, i64, i64);

async fn satisfaction(ctx: &Ctx, p: &Params<'_>, tx: &mut Tx) -> Result<Value> {
    let span = period_of(ctx, p)?;
    let (label, expr) = grouping(p, "Agent")?;
    let columns = "count(*), round(avg(a.satisfaction_rating), 2), count(*) filter (where a.satisfaction_rating = 1), \
                   count(*) filter (where a.satisfaction_rating = 2), count(*) filter (where a.satisfaction_rating = 3), \
                   count(*) filter (where a.satisfaction_rating = 4), count(*) filter (where a.satisfaction_rating = 5)";
    let rated = |qb: &mut QueryBuilder<Postgres>| {
        qb.push(" and a.satisfaction_rating is not null");
        within(qb, "a.resolved_at", span);
    };
    let mut grouped = tickets(ctx, &format!("{expr}, {columns}"))?;
    rated(&mut grouped);
    grouped.push(" group by 1 order by 3 desc, 1");
    let data: Vec<CsatRow> = grouped.build_query_as().fetch_all(&mut **tx).await?;
    let mut all = tickets(ctx, &format!("'All'::text, {columns}"))?;
    rated(&mut all);
    let total: CsatRow = all.build_query_as().fetch_one(&mut **tx).await?;
    let row = |(group, n, avg, s1, s2, s3, s4, s5): CsatRow| {
        json!({"group": group, "rated": n, "average_rating": dec(avg), "star_1": s1, "star_2": s2, "star_3": s3, "star_4": s4, "star_5": s5,
            "satisfied_pct": pct(s4 + s5, n)})
    };
    let mut totals = row(total);
    if let Value::Object(m) = &mut totals {
        m.remove("group");
    }
    Ok(json!({
        "columns": [col("group", label, "data"), col("rated", "Rated Tickets", "int"), col("average_rating", "Average Rating", "decimal"),
            col("star_1", "1 Star", "int"), col("star_2", "2 Stars", "int"), col("star_3", "3 Stars", "int"), col("star_4", "4 Stars", "int"),
            col("star_5", "5 Stars", "int"), col("satisfied_pct", "Satisfied % (4-5)", "decimal")],
        "rows": data.into_iter().map(row).collect::<Vec<_>>(),
        "totals": totals,
        "meta": meta(span, label),
    }))
}

#[async_trait]
impl ReportSource for SupportReports {
    fn catalog(&self) -> Vec<Value> {
        let entry =
            |name: &str, label: &str, params: Vec<Value>| json!({"name": name, "label": label, "module": "support", "entity": "Ticket", "params": params});
        let span = || vec![param_def("from_date"), param_def("to_date")];
        vec![
            entry("support_ticket_summary", "Ticket Summary", [span(), vec![group_param("Status")]].concat()),
            entry("support_sla_compliance", "SLA Compliance", [span(), vec![group_param("SLA")]].concat()),
            entry("support_response_times", "Response & Resolution Times", [span(), vec![group_param("Priority")]].concat()),
            entry("support_agent_workload", "Agent Workload", vec![json!({"name": "team", "label": "Support Team", "type": "link", "to": "SupportTeam"})]),
            entry("support_customer_satisfaction", "Customer Satisfaction", [span(), vec![group_param("Agent")]].concat()),
        ]
    }

    async fn run(&self, k: &Kernel, ctx: &Ctx, name: &str, params: &HashMap<String, String>) -> Result<Value> {
        let p = Params { raw: params };
        let mut tx = read_tx(k, ctx).await?;
        let out = match name {
            "support_ticket_summary" => summary(ctx, &p, &mut tx).await,
            "support_sla_compliance" => compliance(ctx, &p, &mut tx).await,
            "support_response_times" => times(ctx, &p, &mut tx).await,
            "support_agent_workload" => workload(ctx, &p, &mut tx).await,
            "support_customer_satisfaction" => satisfaction(ctx, &p, &mut tx).await,
            _ => Err(Error::NotFound(format!("report {name}"))),
        };
        tx.rollback().await?;
        out
    }
}
