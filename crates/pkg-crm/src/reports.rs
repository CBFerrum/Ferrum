//! CRM reports: the open pipeline by stage, the lead funnel with conversion rates, campaign
//! performance and the quotation register (quoted vs ordered vs lost). Each report sees only the
//! records the caller may read (own records for a sales user), like lists do.

use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_domain::reports::{Params, ReportCompany, ReportSource, col, param_def, period, read_tx, resolve_company};
use ferrum_domain::util::money;
use ferrum_kernel::{Action, Ctx, Error, Kernel, Result, Tx, row_scope};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};
use std::collections::HashMap;
use uuid::Uuid;

const MAX_ROWS: i64 = 10_000;

pub struct CrmReports;

#[async_trait]
impl ReportSource for CrmReports {
    fn catalog(&self) -> Vec<Value> {
        let date = |name: &str, label: &str| json!({"name": name, "label": label, "type": "date"});
        vec![
            json!({"name": "sales_pipeline", "label": "Sales Pipeline", "module": "crm", "entity": "Opportunity", "params": [
                param_def("company"), {"name": "assigned_to", "label": "Opportunity Owner (email)", "type": "data"},
                date("from_date", "Expected Close From"), date("to_date", "Expected Close To")]}),
            json!({"name": "lead_funnel", "label": "Lead Funnel and Conversion", "module": "crm", "entity": "Lead", "params": [
                param_def("company"), param_def("from_date"), param_def("to_date"),
                {"name": "group_by", "label": "Group By", "type": "select", "options": ["Source", "Campaign", "Territory", "Lead Owner"], "default": "Source"}]}),
            json!({"name": "campaign_performance", "label": "Campaign Performance", "module": "crm", "entity": "Campaign", "params": [
                param_def("company"), date("from_date", "Running From"), date("to_date", "Running To")]}),
            json!({"name": "quotation_register", "label": "Quotation Register", "module": "crm", "entity": "Quotation", "params": [
                param_def("company"), param_def("from_date"), param_def("to_date"), param_def("customer"),
                {"name": "status", "label": "Status", "type": "select", "options": ["Open", "Partially Ordered", "Ordered", "Lost", "Expired"]}]}),
        ]
    }

    async fn run(&self, k: &Kernel, ctx: &Ctx, name: &str, params: &HashMap<String, String>) -> Result<Value> {
        let p = Params { raw: params };
        let mut tx = read_tx(k, ctx).await?;
        let co = resolve_company(&mut tx, ctx, &p).await?;
        let mut out = match name {
            "sales_pipeline" => pipeline(k, ctx, &mut tx, &co, &p).await?,
            "lead_funnel" => funnel(k, ctx, &mut tx, &co, &p).await?,
            "campaign_performance" => campaigns(k, ctx, &mut tx, &co, &p).await?,
            "quotation_register" => quotations(k, ctx, &mut tx, &co, &p).await?,
            _ => return Err(Error::NotFound(format!("report {name}"))),
        };
        tx.rollback().await?;
        if let Some(meta) = out.get_mut("meta").and_then(Value::as_object_mut) {
            meta.insert("company".into(), json!(co.name));
            meta.insert("currency".into(), json!(co.currency));
        }
        Ok(out)
    }
}

/// Restricts `alias` to the rows of `entity` the caller may read (owner and company scopes).
fn scope(k: &Kernel, ctx: &Ctx, entity: &str, qb: &mut QueryBuilder<Postgres>, alias: &str) -> Result<()> {
    let ent = k.entity(ctx, entity)?;
    row_scope(ctx, &ent, Action::Read)?.push_sql(qb, alias);
    Ok(())
}

/// `part` as a percentage of `whole`, to two decimals.
pub(crate) fn pct(part: Decimal, whole: Decimal) -> Decimal {
    if whole.is_zero() { Decimal::ZERO } else { (part / whole * Decimal::ONE_HUNDRED).round_dp(2) }
}

fn next_day(d: NaiveDate) -> NaiveDate {
    d.succ_opt().unwrap_or(d)
}

async fn pipeline(k: &Kernel, ctx: &Ctx, tx: &mut Tx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = (p.date("from_date")?, p.date("to_date")?);
    let mut qb = QueryBuilder::new(
        "select o.sales_stage, count(*), coalesce(sum(o.base_amount), 0), coalesce(sum(o.base_weighted_amount), 0) \
         from e_opportunity o where o.status = 'Open' and o.tenant_id = ",
    );
    qb.push_bind(ctx.tenant).push(" and o.company = ").push_bind(co.id);
    if let Some(owner) = p.str("assigned_to") {
        qb.push(" and o.assigned_to = ").push_bind(owner.to_lowercase());
    }
    if let Some(f) = from {
        qb.push(" and o.expected_close_date >= ").push_bind(f);
    }
    if let Some(t) = to {
        qb.push(" and o.expected_close_date <= ").push_bind(t);
    }
    scope(k, ctx, "Opportunity", &mut qb, "o")?;
    qb.push(" group by o.sales_stage");
    let data: Vec<(Option<Uuid>, i64, Decimal, Decimal)> = qb.build_query_as().fetch_all(&mut **tx).await?;
    let mut by_stage: HashMap<Option<Uuid>, (i64, Decimal, Decimal)> = data.into_iter().map(|(s, c, a, w)| (s, (c, a, w))).collect();
    let stages: Vec<(Uuid, Option<String>, Option<Decimal>, Option<String>)> =
        sqlx::query_as("select id, stage_name, probability, outcome from e_sales_stage where tenant_id = $1 order by stage_order, stage_name")
            .bind(ctx.tenant)
            .fetch_all(&mut **tx)
            .await?;
    let (mut count, mut amount, mut weighted) = (0i64, Decimal::ZERO, Decimal::ZERO);
    let mut rows = Vec::new();
    let mut push = |stage: String, probability: Option<Decimal>, (c, a, w): (i64, Decimal, Decimal)| {
        count += c;
        amount += a;
        weighted += w;
        rows.push(json!({"sales_stage": stage, "probability": probability.map(money), "count": c, "amount": money(a), "weighted_amount": money(w)}));
    };
    for (sid, name, probability, outcome) in stages {
        let v = by_stage.remove(&Some(sid));
        // Every open stage shows (an empty step is information too); closed ones never hold open deals.
        if v.is_some() || outcome.as_deref() == Some("Open") {
            push(name.unwrap_or_default(), probability, v.unwrap_or_default());
        }
    }
    for (_, v) in by_stage {
        push("(no stage)".into(), None, v);
    }
    let average = if count == 0 { Decimal::ZERO } else { (amount / Decimal::from(count)).round_dp(co.precision) };
    Ok(json!({
        "columns": [col("sales_stage", "Sales Stage", "data"), col("probability", "Probability %", "decimal"), col("count", "Opportunities", "int"),
            col("amount", "Amount", "currency"), col("weighted_amount", "Weighted Amount", "currency")],
        "rows": rows,
        "totals": {"count": count, "amount": money(amount), "weighted_amount": money(weighted)},
        "meta": {"from_date": from, "to_date": to, "average_deal": money(average)},
    }))
}

async fn funnel(k: &Kernel, ctx: &Ctx, tx: &mut Tx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let group_by = p.str("group_by").unwrap_or("Source");
    let (label, join) = match group_by {
        "Source" => ("coalesce(l.source, '(not set)')", ""),
        "Campaign" => ("coalesce(c.campaign_name, '(no campaign)')", " left join e_campaign c on c.tenant_id = l.tenant_id and c.id = l.campaign"),
        "Territory" => ("coalesce(t.territory_name, '(no territory)')", " left join e_territory t on t.tenant_id = l.tenant_id and t.id = l.territory"),
        "Lead Owner" => ("coalesce(l.assigned_to, '(unassigned)')", ""),
        other => return Err(Error::field("group_by", format!("cannot group by '{other}'"))),
    };
    // Reaching a later step implies the earlier ones: a qualified lead was contacted.
    let mut qb = QueryBuilder::new(format!(
        "select {label}, count(*), \
         count(*) filter (where l.last_contacted_on is not null or l.qualified_on is not null or l.status in ('Contacted', 'Qualified', 'Converted')), \
         count(*) filter (where l.qualified_on is not null or l.status in ('Qualified', 'Converted')), \
         count(*) filter (where l.status = 'Converted'), count(*) filter (where l.status in ('Lost', 'Junk')) \
         from e_lead l{join} where l.tenant_id = "
    ));
    qb.push_bind(ctx.tenant).push(" and l.company = ").push_bind(co.id);
    qb.push(" and l.created_at >= ").push_bind(from).push(" and l.created_at < ").push_bind(next_day(to));
    scope(k, ctx, "Lead", &mut qb, "l")?;
    qb.push(" group by 1 order by 2 desc, 1");
    let data: Vec<(String, i64, i64, i64, i64, i64)> = qb.build_query_as().fetch_all(&mut **tx).await?;
    let mut totals = [0i64; 5];
    let row = |group: &str, n: [i64; 5]| {
        let d = |i: usize| Decimal::from(n[i]);
        json!({"group": group, "leads": n[0], "contacted": n[1], "qualified": n[2], "converted": n[3], "lost": n[4],
            "qualification_pct": money(pct(d(2), d(0))), "conversion_pct": money(pct(d(3), d(0)))})
    };
    let mut rows = Vec::new();
    for (group, a, b, c, d, e) in data {
        let n = [a, b, c, d, e];
        for (t, v) in totals.iter_mut().zip(n) {
            *t += v;
        }
        rows.push(row(&group, n));
    }
    let total = row("Total", totals);
    Ok(json!({
        "columns": [col("group", group_by, "data"), col("leads", "Leads", "int"), col("contacted", "Contacted", "int"), col("qualified", "Qualified", "int"),
            col("converted", "Converted", "int"), col("lost", "Lost / Junk", "int"), col("qualification_pct", "Qualified %", "decimal"),
            col("conversion_pct", "Conversion %", "decimal")],
        "rows": rows,
        "totals": total,
        "meta": {"from_date": from, "to_date": to, "group_by": group_by},
    }))
}

async fn campaigns(k: &Kernel, ctx: &Ctx, tx: &mut Tx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = (p.date("from_date")?, p.date("to_date")?);
    let mut qb = QueryBuilder::new(
        "select c.id, c.campaign_name, c.campaign_type, c.status, c.start_date, coalesce(c.budget, 0), coalesce(c.actual_cost, 0), \
         coalesce(c.expected_revenue, 0) from e_campaign c where c.tenant_id = ",
    );
    qb.push_bind(ctx.tenant).push(" and c.company = ").push_bind(co.id);
    // Campaigns running at some point of the period.
    if let Some(f) = from {
        qb.push(" and (c.end_date is null or c.end_date >= ").push_bind(f).push(")");
    }
    if let Some(t) = to {
        qb.push(" and (c.start_date is null or c.start_date <= ").push_bind(t).push(")");
    }
    scope(k, ctx, "Campaign", &mut qb, "c")?;
    qb.push(" order by c.start_date desc nulls last, c.campaign_name limit ").push_bind(MAX_ROWS + 1);
    let list: Vec<(Uuid, String, Option<String>, Option<String>, Option<NaiveDate>, Decimal, Decimal, Decimal)> =
        qb.build_query_as().fetch_all(&mut **tx).await?;
    let truncated = list.len() as i64 > MAX_ROWS;

    let mut qb = QueryBuilder::new(
        "select l.campaign, count(*), count(*) filter (where l.status = 'Converted') from e_lead l where l.campaign is not null and l.tenant_id = ",
    );
    qb.push_bind(ctx.tenant).push(" and l.company = ").push_bind(co.id);
    scope(k, ctx, "Lead", &mut qb, "l")?;
    qb.push(" group by l.campaign");
    let leads: HashMap<Uuid, (i64, i64)> =
        qb.build_query_as::<(Uuid, i64, i64)>().fetch_all(&mut **tx).await?.into_iter().map(|(c, n, w)| (c, (n, w))).collect();

    let mut qb = QueryBuilder::new(
        "select o.campaign, count(*), count(*) filter (where o.status = 'Won'), coalesce(sum(o.base_amount) filter (where o.status = 'Won'), 0), \
         coalesce(sum(o.base_amount) filter (where o.status = 'Open'), 0) from e_opportunity o where o.campaign is not null and o.tenant_id = ",
    );
    qb.push_bind(ctx.tenant).push(" and o.company = ").push_bind(co.id);
    scope(k, ctx, "Opportunity", &mut qb, "o")?;
    qb.push(" group by o.campaign");
    let opps: HashMap<Uuid, (i64, i64, Decimal, Decimal)> =
        qb.build_query_as::<(Uuid, i64, i64, Decimal, Decimal)>().fetch_all(&mut **tx).await?.into_iter().map(|(c, n, w, a, o)| (c, (n, w, a, o))).collect();

    let mut t = (Decimal::ZERO, Decimal::ZERO, 0i64, 0i64, 0i64, 0i64, Decimal::ZERO, Decimal::ZERO);
    let mut rows = Vec::new();
    for (id, name, kind, status, start, budget, cost, expected) in list.into_iter().take(MAX_ROWS as usize) {
        let (n_leads, converted) = leads.get(&id).copied().unwrap_or_default();
        let (n_opps, won, won_amount, open_amount) = opps.get(&id).copied().unwrap_or_default();
        t.0 += budget;
        t.1 += cost;
        t.2 += n_leads;
        t.3 += converted;
        t.4 += n_opps;
        t.5 += won;
        t.6 += won_amount;
        t.7 += open_amount;
        rows.push(campaign_row(
            json!({"campaign": name, "campaign_id": id, "campaign_type": kind, "status": status, "start_date": start}),
            budget,
            cost,
            expected,
            n_leads,
            converted,
            n_opps,
            won,
            won_amount,
            open_amount,
            co.precision,
        ));
    }
    let totals = campaign_row(json!({"campaign": "Total"}), t.0, t.1, Decimal::ZERO, t.2, t.3, t.4, t.5, t.6, t.7, co.precision);
    Ok(json!({
        "columns": [col("campaign", "Campaign", "data"), col("campaign_type", "Type", "data"), col("status", "Status", "data"),
            col("start_date", "Start", "date"), col("budget", "Budget", "currency"), col("actual_cost", "Cost", "currency"),
            col("leads", "Leads", "int"), col("converted_leads", "Converted", "int"), col("opportunities", "Opportunities", "int"),
            col("won", "Won", "int"), col("won_amount", "Won Amount", "currency"), col("open_pipeline", "Open Pipeline", "currency"),
            col("cost_per_lead", "Cost per Lead", "currency"), col("roi_pct", "ROI %", "decimal")],
        "rows": rows,
        "totals": totals,
        "meta": {"from_date": from, "to_date": to, "truncated": truncated},
    }))
}

/// One campaign line: counts, money, cost per lead and return on the money spent.
fn campaign_row(
    mut head: Value,
    budget: Decimal,
    cost: Decimal,
    expected: Decimal,
    leads: i64,
    converted: i64,
    opportunities: i64,
    won: i64,
    won_amount: Decimal,
    open_amount: Decimal,
    precision: u32,
) -> Value {
    let per_lead = if leads == 0 { Decimal::ZERO } else { (cost / Decimal::from(leads)).round_dp(precision) };
    let roi = if cost.is_zero() { Decimal::ZERO } else { pct(won_amount - cost, cost) };
    if let Value::Object(m) = &mut head {
        for (k, v) in [
            ("budget", money(budget)),
            ("actual_cost", money(cost)),
            ("expected_revenue", money(expected)),
            ("leads", json!(leads)),
            ("converted_leads", json!(converted)),
            ("opportunities", json!(opportunities)),
            ("won", json!(won)),
            ("won_amount", money(won_amount)),
            ("open_pipeline", money(open_amount)),
            ("cost_per_lead", money(per_lead)),
            ("roi_pct", money(roi)),
        ] {
            m.insert(k.into(), v);
        }
    }
    head
}

async fn quotations(k: &Kernel, ctx: &Ctx, tx: &mut Tx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let customer = p.uuid("customer")?;
    // What was ordered is what the submitted sales orders made from each quotation add up to.
    let mut qb = QueryBuilder::new(
        "select q.id, q.number, q.posting_date, q.party_name, q.party_type, q.valid_till, q.status, cur.code, coalesce(q.grand_total, 0), \
         coalesce(q.base_grand_total, 0), coalesce(q.per_ordered, 0), coalesce(so.ordered, 0) \
         from e_quotation q left join e_currency cur on cur.tenant_id = q.tenant_id and cur.id = q.currency \
         left join (select nullif(s.ext->>'crm_quotation', '')::uuid as quotation, sum(s.base_grand_total) as ordered from e_sales_order s \
                    where s.docstatus = 1 and s.ext ? 'crm_quotation' and s.tenant_id = ",
    );
    qb.push_bind(ctx.tenant).push(" group by 1) so on so.quotation = q.id where q.docstatus = 1 and q.tenant_id = ").push_bind(ctx.tenant);
    qb.push(" and q.company = ").push_bind(co.id).push(" and q.posting_date between ").push_bind(from).push(" and ").push_bind(to);
    if let Some(c) = customer {
        qb.push(" and q.customer = ").push_bind(c);
    }
    if let Some(st) = p.str("status") {
        qb.push(" and q.status = ").push_bind(st.to_string());
    }
    scope(k, ctx, "Quotation", &mut qb, "q")?;
    qb.push(" order by q.posting_date, q.number limit ").push_bind(MAX_ROWS + 1);
    let data: Vec<(
        Uuid,
        Option<String>,
        NaiveDate,
        Option<String>,
        Option<String>,
        Option<NaiveDate>,
        Option<String>,
        Option<String>,
        Decimal,
        Decimal,
        Decimal,
        Decimal,
    )> = qb.build_query_as().fetch_all(&mut **tx).await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let mut by_status: HashMap<String, Decimal> = HashMap::new();
    let (mut quoted, mut ordered, mut converted) = (Decimal::ZERO, Decimal::ZERO, 0i64);
    let mut rows = Vec::new();
    for (qid, number, day, party, party_type, valid_till, status, currency, grand, base, per_ordered, ordered_base) in data.into_iter().take(MAX_ROWS as usize)
    {
        let status = status.unwrap_or_default();
        quoted += base;
        ordered += ordered_base;
        if ordered_base > Decimal::ZERO {
            converted += 1;
        }
        *by_status.entry(status.clone()).or_default() += base;
        rows.push(json!({"quotation": number, "quotation_id": qid, "posting_date": day, "party": party, "party_type": party_type, "valid_till": valid_till,
            "status": status, "currency": currency, "grand_total": money(grand), "base_grand_total": money(base), "per_ordered": money(per_ordered),
            "ordered_amount": money(ordered_base)}));
    }
    let count = rows.len() as i64;
    let sum = |s: &str| money(by_status.get(s).copied().unwrap_or_default());
    Ok(json!({
        "columns": [col("quotation", "Quotation", "data"), col("posting_date", "Date", "date"), col("party", "Customer / Lead", "data"),
            col("valid_till", "Valid Till", "date"), col("status", "Status", "data"), col("currency", "Currency", "data"),
            col("grand_total", "Grand Total", "currency"), col("base_grand_total", "Grand Total (Company Currency)", "currency"),
            col("per_ordered", "% Ordered", "decimal"), col("ordered_amount", "Ordered (Company Currency)", "currency")],
        "rows": rows,
        "totals": {"count": count, "quoted": money(quoted), "ordered": money(ordered), "open": sum("Open"), "partially_ordered": sum("Partially Ordered"),
            "lost": sum("Lost"), "expired": sum("Expired"), "conversion_pct": money(pct(ordered, quoted)),
            "win_rate_pct": money(pct(Decimal::from(converted), Decimal::from(count)))},
        "meta": {"from_date": from, "to_date": to, "truncated": truncated},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentages() {
        assert_eq!(pct(Decimal::from(1), Decimal::from(3)), Decimal::new(3333, 2));
        assert_eq!(pct(Decimal::from(5), Decimal::ZERO), Decimal::ZERO);
        assert_eq!(pct(Decimal::from(-50), Decimal::from(100)), Decimal::from(-50));
    }

    #[test]
    fn campaign_maths() {
        let r = campaign_row(json!({}), Decimal::from(1000), Decimal::from(800), Decimal::ZERO, 16, 4, 5, 2, Decimal::from(2000), Decimal::from(300), 2);
        assert_eq!(r["cost_per_lead"], json!("50"));
        assert_eq!(r["roi_pct"], json!("150"));
        let none = campaign_row(json!({}), Decimal::ZERO, Decimal::ZERO, Decimal::ZERO, 0, 0, 0, 0, Decimal::ZERO, Decimal::ZERO, 2);
        assert_eq!(none["cost_per_lead"], json!("0"));
        assert_eq!(none["roi_pct"], json!("0"));
    }

    #[test]
    fn catalog_names_entities_and_params() {
        let c = CrmReports.catalog();
        assert_eq!(c.len(), 4);
        assert!(c.iter().all(|r| r["entity"].is_string() && r["params"].as_array().is_some_and(|p| p.iter().any(|x| x["name"] == "company"))));
    }
}
