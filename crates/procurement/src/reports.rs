//! Procurement reports, read straight from the documents in one read-only transaction:
//! open request quantities by item and warehouse, the request → RFQ → quotation → order →
//! receipt trail per request line, quotations side by side, scorecard ranking and purchase
//! analytics by supplier, item or item group per month. Users who may only read their own
//! material requests see only those in the request reports.

use async_trait::async_trait;
use chrono::{Datelike, NaiveDate};
use ferrum_domain::reports::{Params, ReportCompany, ReportSource, col, param_def, period, read_tx, resolve_company, today};
use ferrum_domain::util::money;
use ferrum_kernel::{Access, Action, Ctx, Error, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

const MAX_ROWS: i64 = 10_000;
const MAX_MONTHS: usize = 36;
const MAX_SUPPLIER_COLUMNS: usize = 50;

pub struct ProcurementReports;

struct Def {
    name: &'static str,
    label: &'static str,
    entity: &'static str,
    params: &'static [&'static str],
}

const DEFS: &[Def] = &[
    Def {
        name: "material_request_status",
        label: "Material Request Status",
        entity: "MaterialRequest",
        params: &["company", "from_date", "to_date", "warehouse", "item", "material_request_type"],
    },
    Def {
        name: "procurement_tracker",
        label: "Procurement Tracker",
        entity: "MaterialRequest",
        params: &["company", "from_date", "to_date", "material_request", "item"],
    },
    Def {
        name: "supplier_quotation_comparison",
        label: "Supplier Quotation Comparison",
        entity: "SupplierQuotation",
        params: &["company", "from_date", "to_date", "request_for_quotation", "item", "include_expired"],
    },
    Def { name: "supplier_scorecard_ranking", label: "Supplier Scorecard Ranking", entity: "SupplierScorecard", params: &["standing"] },
    Def {
        name: "purchase_analytics",
        label: "Purchase Analytics",
        entity: "PurchaseOrder",
        params: &["company", "from_date", "to_date", "based_on", "group_by", "value_or_qty", "supplier", "item"],
    },
];

fn param(name: &str) -> Value {
    let select = |label: &str, options: &[&str]| json!({"name": name, "label": label, "type": "select", "options": options, "default": options[0]});
    match name {
        "material_request_type" => {
            json!({"name": name, "label": "Purpose", "type": "select", "options": ["Purchase", "Material Transfer", "Material Issue", "Manufacture", "Customer Provided"]})
        }
        "material_request" => json!({"name": name, "label": "Material Request", "type": "link", "to": "MaterialRequest"}),
        "request_for_quotation" => json!({"name": name, "label": "Request for Quotation", "type": "link", "to": "RequestForQuotation"}),
        "include_expired" => json!({"name": name, "label": "Include Expired Quotations", "type": "check"}),
        "standing" => json!({"name": name, "label": "Standing", "type": "data"}),
        "based_on" => select("Based On", &["Purchase Order", "Purchase Receipt", "Purchase Invoice"]),
        "group_by" => select("Group By", &["Supplier", "Item", "Item Group"]),
        "value_or_qty" => select("Show", &["Value", "Quantity"]),
        other => param_def(other),
    }
}

#[async_trait]
impl ReportSource for ProcurementReports {
    fn catalog(&self) -> Vec<Value> {
        DEFS.iter()
            .map(|d| {
                // Requests are narrowed to the caller's own when that is all they may read.
                let scoped = if d.entity == "MaterialRequest" { json!("owner") } else { json!(false) };
                json!({"name": d.name, "label": d.label, "module": "procurement", "entity": d.entity, "scoped": scoped, "params": d.params.iter().map(|p| param(p)).collect::<Vec<_>>()})
            })
            .collect()
    }

    async fn run(&self, k: &Kernel, ctx: &Ctx, name: &str, raw: &HashMap<String, String>) -> Result<Value> {
        let p = Params { raw };
        let mut tx = read_tx(k, ctx).await?;
        let out = if name == "supplier_scorecard_ranking" {
            ranking(&mut tx, ctx, &p).await?
        } else {
            let co = resolve_company(&mut tx, ctx, &p).await?;
            let mut out = match name {
                "material_request_status" => request_status(&mut tx, ctx, &co, &p).await?,
                "procurement_tracker" => tracker(&mut tx, ctx, &co, &p).await?,
                "supplier_quotation_comparison" => comparison(&mut tx, ctx, &co, &p).await?,
                "purchase_analytics" => analytics(&mut tx, ctx, &co, &p).await?,
                _ => return Err(Error::NotFound(format!("report {name}"))),
            };
            if let Some(meta) = out.get_mut("meta").and_then(Value::as_object_mut) {
                meta.insert("company".into(), json!(co.name));
                meta.insert("currency".into(), json!(co.currency));
            }
            out
        };
        tx.rollback().await?;
        Ok(out)
    }
}

/// `(user, email)` when the caller may read only their own material requests.
fn own_requests(ctx: &Ctx) -> (Option<Uuid>, Option<String>) {
    if ctx.perms.access("MaterialRequest", Action::Read) == Access::Own { (Some(ctx.user), Some(ctx.email.to_lowercase())) } else { (None, None) }
}

const OWN: &str = "($OWNER::uuid is null or m.created_by = $OWNER or lower(coalesce(m.requested_by, '')) = $EMAIL)";

fn q(d: Decimal) -> Value {
    money(d)
}

// ───────────────────────────── request status ─────────────────────────────

async fn request_status(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let (owner, email) = own_requests(ctx);
    let sql = format!(
        "select i.item_code, i.item_name, coalesce(w.warehouse_name, ''), count(distinct m.id), \
           sum(coalesce(l.stock_qty, 0)), sum(least(coalesce(l.ordered_qty, 0), coalesce(l.stock_qty, 0))), \
           sum(least(coalesce(l.received_qty, 0), coalesce(l.stock_qty, 0))), \
           sum(greatest(coalesce(l.stock_qty, 0) - coalesce(l.ordered_qty, 0), 0)), \
           sum(greatest(coalesce(l.stock_qty, 0) - coalesce(l.received_qty, 0), 0)), \
           min(l.schedule_date) filter (where coalesce(l.stock_qty, 0) > coalesce(l.ordered_qty, 0)), \
           coalesce(sum(greatest(coalesce(l.stock_qty, 0) - coalesce(l.ordered_qty, 0), 0)) filter (where l.schedule_date < $8), 0) \
         from e_material_request m join e_material_request__items l on l.tenant_id = m.tenant_id and l.parent_id = m.id \
         join e_item i on i.tenant_id = l.tenant_id and i.id = l.item left join e_warehouse w on w.tenant_id = l.tenant_id and w.id = l.warehouse \
         where m.tenant_id = $1 and m.company = $2 and m.docstatus = 1 and m.status <> 'Stopped' and m.transaction_date between $3 and $4 \
           and ($5::uuid is null or l.warehouse = $5) and ($6::uuid is null or l.item = $6) and ($7::text is null or m.material_request_type = $7) \
           and (coalesce(l.stock_qty, 0) > coalesce(l.ordered_qty, 0) or coalesce(l.stock_qty, 0) > coalesce(l.received_qty, 0)) and {} \
         group by i.item_code, i.item_name, w.warehouse_name \
         order by i.item_code, 3 limit $11",
        OWN.replace("$OWNER", "$9").replace("$EMAIL", "$10")
    );
    let data: Vec<(String, String, String, i64, Decimal, Decimal, Decimal, Decimal, Decimal, Option<NaiveDate>, Decimal)> =
        sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(ctx.tenant)
            .bind(co.id)
            .bind(from)
            .bind(to)
            .bind(p.uuid("warehouse")?)
            .bind(p.uuid("item")?)
            .bind(p.str("material_request_type"))
            .bind(today())
            .bind(owner)
            .bind(email)
            .bind(MAX_ROWS + 1)
            .fetch_all(&mut **tx)
            .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let mut totals = [Decimal::ZERO; 6];
    let rows: Vec<Value> = data
        .into_iter()
        .take(MAX_ROWS as usize)
        .map(|(code, name, wh, requests, requested, ordered, received, to_order, to_receive, due, overdue)| {
            for (t, v) in totals.iter_mut().zip([requested, ordered, received, to_order, to_receive, overdue]) {
                *t += v;
            }
            json!({"item_code": code, "item_name": name, "warehouse": wh, "requests": requests, "requested_qty": q(requested), "ordered_qty": q(ordered),
                   "received_qty": q(received), "pending_order_qty": q(to_order), "pending_receipt_qty": q(to_receive), "earliest_required_by": due,
                   "overdue_qty": q(overdue)})
        })
        .collect();
    Ok(json!({
        "columns": [col("item_code", "Item", "data"), col("item_name", "Item Name", "data"), col("warehouse", "Warehouse", "data"),
            col("requests", "Requests", "int"), col("requested_qty", "Requested", "decimal"), col("ordered_qty", "Ordered", "decimal"),
            col("received_qty", "Received", "decimal"), col("pending_order_qty", "To Order", "decimal"), col("pending_receipt_qty", "To Receive", "decimal"),
            col("earliest_required_by", "Earliest Required By", "date"), col("overdue_qty", "Overdue To Order", "decimal")],
        "rows": rows,
        "totals": {"requested_qty": q(totals[0]), "ordered_qty": q(totals[1]), "received_qty": q(totals[2]), "pending_order_qty": q(totals[3]),
                   "pending_receipt_qty": q(totals[4]), "overdue_qty": q(totals[5])},
        "meta": {"from_date": from, "to_date": to, "truncated": truncated},
    }))
}

// ───────────────────────────── tracker ─────────────────────────────

/// Where a request line stands on its way from request to receipt.
fn stage(kind: &str, requested: Decimal, ordered: Decimal, received: Decimal, rfqs: bool, quotes: bool) -> &'static str {
    let purchase = kind == "Purchase";
    if requested > Decimal::ZERO && received >= requested {
        if purchase || kind == "Customer Provided" { "Received" } else { "Done" }
    } else if received > Decimal::ZERO {
        "Partially Received"
    } else if requested > Decimal::ZERO && ordered >= requested {
        "Ordered"
    } else if ordered > Decimal::ZERO {
        "Partially Ordered"
    } else if quotes {
        "Quoted"
    } else if rfqs {
        "RFQ Sent"
    } else {
        "Requested"
    }
}

#[derive(sqlx::FromRow)]
struct TrackRow {
    number: Option<String>,
    transaction_date: NaiveDate,
    kind: String,
    status: String,
    item_code: String,
    item_name: String,
    warehouse: String,
    schedule_date: Option<NaiveDate>,
    requested: Decimal,
    ordered: Decimal,
    received: Decimal,
    rfqs: Option<String>,
    quotes: Option<String>,
    best: Option<Decimal>,
    orders: Option<String>,
    receipts: Option<String>,
    invoices: Option<String>,
    entries: Option<String>,
}

async fn tracker(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let (owner, email) = own_requests(ctx);
    let linked = |table: &str, doc: &str| {
        format!(
            "(select string_agg(distinct x.number, ', ') from {table} xl join {doc} x on x.tenant_id = xl.tenant_id and x.id = xl.parent_id \
              where xl.tenant_id = l.tenant_id and xl.ext ? 'proc_mr_line' and xl.ext->>'proc_mr_line' = l.id::text and x.docstatus = 1)"
        )
    };
    let sql = format!(
        "select m.number, m.transaction_date, coalesce(m.material_request_type, 'Purchase') as kind, coalesce(m.status, '') as status, \
           i.item_code, i.item_name, coalesce(w.warehouse_name, '') as warehouse, l.schedule_date, coalesce(l.stock_qty, 0) as requested, \
           coalesce(l.ordered_qty, 0) as ordered, coalesce(l.received_qty, 0) as received, \
           (select string_agg(distinct r.number, ', ') from e_request_for_quotation__items rl join e_request_for_quotation r on r.tenant_id = rl.tenant_id and r.id = rl.parent_id \
             where rl.tenant_id = l.tenant_id and rl.mr_line = l.id::text and r.docstatus = 1) as rfqs, \
           (select string_agg(distinct x.number, ', ') from e_supplier_quotation__items xl join e_supplier_quotation x on x.tenant_id = xl.tenant_id and x.id = xl.parent_id \
             where xl.tenant_id = l.tenant_id and xl.mr_line = l.id::text and x.docstatus = 1) as quotes, \
           (select min(xl.rate * coalesce(nullif(x.conversion_rate, 0), 1)) from e_supplier_quotation__items xl join e_supplier_quotation x on x.tenant_id = xl.tenant_id and x.id = xl.parent_id \
             where xl.tenant_id = l.tenant_id and xl.mr_line = l.id::text and x.docstatus = 1) as best, \
           {} as orders, {} as receipts, {} as invoices, {} as entries \
         from e_material_request m join e_material_request__items l on l.tenant_id = m.tenant_id and l.parent_id = m.id \
         join e_item i on i.tenant_id = l.tenant_id and i.id = l.item left join e_warehouse w on w.tenant_id = l.tenant_id and w.id = l.warehouse \
         where m.tenant_id = $1 and m.company = $2 and m.docstatus = 1 and m.transaction_date between $3 and $4 \
           and ($5::uuid is null or m.id = $5) and ($6::uuid is null or l.item = $6) and {} \
         order by m.transaction_date desc, m.number, l.idx limit $9",
        linked("e_purchase_order__items", "e_purchase_order"),
        linked("e_purchase_receipt__items", "e_purchase_receipt"),
        linked("e_purchase_invoice__items", "e_purchase_invoice"),
        linked("e_stock_entry__items", "e_stock_entry"),
        OWN.replace("$OWNER", "$7").replace("$EMAIL", "$8"),
    );
    let data: Vec<TrackRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(ctx.tenant)
        .bind(co.id)
        .bind(from)
        .bind(to)
        .bind(p.uuid("material_request")?)
        .bind(p.uuid("item")?)
        .bind(owner)
        .bind(email)
        .bind(MAX_ROWS + 1)
        .fetch_all(&mut **tx)
        .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let rows: Vec<Value> = data
        .into_iter()
        .take(MAX_ROWS as usize)
        .map(|r| {
            let receipts: Vec<String> = [r.receipts, r.invoices, r.entries].into_iter().flatten().collect();
            json!({
                "material_request": r.number, "date": r.transaction_date, "purpose": r.kind, "status": r.status, "item_code": r.item_code,
                "item_name": r.item_name, "warehouse": r.warehouse, "required_by": r.schedule_date, "requested_qty": q(r.requested), "rfqs": r.rfqs,
                "supplier_quotations": r.quotes, "best_quoted_rate": r.best.map(|b| money(b.round_dp(co.precision + 2))), "purchase_orders": r.orders,
                "ordered_qty": q(r.ordered), "receipts": (!receipts.is_empty()).then(|| receipts.join(", ")), "received_qty": q(r.received),
                "pending_qty": q((r.requested - r.received).max(Decimal::ZERO)),
                "stage": stage(&r.kind, r.requested, r.ordered, r.received, r.rfqs.is_some(), r.quotes.is_some()),
            })
        })
        .collect();
    Ok(json!({
        "columns": [col("material_request", "Material Request", "data"), col("date", "Date", "date"), col("purpose", "Purpose", "data"),
            col("item_code", "Item", "data"), col("warehouse", "Warehouse", "data"), col("required_by", "Required By", "date"),
            col("requested_qty", "Requested", "decimal"), col("rfqs", "RFQs", "data"), col("supplier_quotations", "Quotations", "data"),
            col("best_quoted_rate", "Best Rate", "currency"), col("purchase_orders", "Purchase Orders", "data"), col("ordered_qty", "Ordered", "decimal"),
            col("receipts", "Receipts / Entries", "data"), col("received_qty", "Received", "decimal"), col("pending_qty", "Pending", "decimal"),
            col("stage", "Stage", "data"), col("status", "Request Status", "data")],
        "rows": rows,
        "meta": {"from_date": from, "to_date": to, "truncated": truncated},
    }))
}

// ───────────────────────────── quotation comparison ─────────────────────────────

async fn comparison(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let include_expired = matches!(p.str("include_expired"), Some("1" | "true" | "yes"));
    let data: Vec<(String, String, Uuid, String, Option<String>, Decimal, Option<i64>)> = sqlx::query_as(
        "select i.item_code, i.item_name, q.supplier, coalesce(s.supplier_name, ''), q.number, \
           ql.rate * coalesce(nullif(q.conversion_rate, 0), 1), ql.lead_time_days \
         from e_supplier_quotation q join e_supplier_quotation__items ql on ql.tenant_id = q.tenant_id and ql.parent_id = q.id \
         join e_item i on i.tenant_id = ql.tenant_id and i.id = ql.item left join e_supplier s on s.tenant_id = q.tenant_id and s.id = q.supplier \
         where q.tenant_id = $1 and q.company = $2 and q.docstatus < 2 and q.posting_date between $3 and $4 \
           and ($5::uuid is null or q.request_for_quotation = $5) and ($6::uuid is null or ql.item = $6) \
           and ($7 or q.valid_till is null or q.valid_till >= $8) and ql.rate > 0 \
         order by i.item_code, 6, s.supplier_name limit $9",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(to)
    .bind(p.uuid("request_for_quotation")?)
    .bind(p.uuid("item")?)
    .bind(include_expired)
    .bind(today())
    .bind(MAX_ROWS + 1)
    .fetch_all(&mut **tx)
    .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    // Suppliers become columns (alphabetically); each cell is the supplier's lowest quoted rate.
    let mut suppliers: BTreeMap<(String, Uuid), String> = BTreeMap::new();
    for (_, _, sup, name, ..) in &data {
        let next = format!("s{}", suppliers.len());
        suppliers.entry((name.clone(), *sup)).or_insert(next);
    }
    if suppliers.len() > MAX_SUPPLIER_COLUMNS {
        return Err(Error::invalid(format!("more than {MAX_SUPPLIER_COLUMNS} suppliers quoted: narrow the report to a request for quotation or an item")));
    }
    let column: HashMap<Uuid, String> = suppliers.iter().map(|((_, sup), c)| (*sup, c.clone())).collect();
    let mut items: Vec<(String, String, BTreeMap<String, (Decimal, Option<String>, Option<i64>)>)> = Vec::new();
    for (code, name, sup, _, number, rate, lead) in data.into_iter().take(MAX_ROWS as usize) {
        if items.last().is_none_or(|(c, ..)| *c != code) {
            items.push((code.clone(), name, BTreeMap::new()));
        }
        let cells = &mut items.last_mut().expect("pushed").2;
        let c = column[&sup].clone();
        if cells.get(&c).is_none_or(|(r, ..)| rate < *r) {
            cells.insert(c, (rate, number, lead));
        }
    }
    let names: HashMap<&String, &String> = suppliers.iter().map(|((n, _), c)| (c, n)).collect();
    let rows: Vec<Value> = items
        .into_iter()
        .map(|(code, name, cells)| {
            let mut row = Map::new();
            row.insert("item_code".into(), json!(code));
            row.insert("item_name".into(), json!(name));
            let lowest = cells.values().map(|(r, ..)| *r).min();
            let mut winners = Vec::new();
            for (c, (rate, number, lead)) in &cells {
                row.insert(c.clone(), money(rate.round_dp(co.precision + 2)));
                row.insert(format!("{c}_quotation"), json!(number));
                row.insert(format!("{c}_lead_time_days"), json!(lead));
                if Some(*rate) == lowest {
                    winners.push(c.clone());
                }
            }
            row.insert("quotes".into(), json!(cells.len()));
            row.insert("lowest_rate".into(), lowest.map(|r| money(r.round_dp(co.precision + 2))).unwrap_or(Value::Null));
            row.insert("lowest_supplier".into(), json!(winners.iter().filter_map(|c| names.get(c).map(|n| n.as_str())).collect::<Vec<_>>().join(", ")));
            row.insert("_highlight".into(), json!(winners));
            Value::Object(row)
        })
        .collect();
    let mut columns = vec![col("item_code", "Item", "data"), col("item_name", "Item Name", "data")];
    columns.extend(suppliers.iter().map(|((name, _), c)| col(c, name, "currency")));
    columns.extend([col("lowest_rate", "Lowest Rate", "currency"), col("lowest_supplier", "Lowest From", "data"), col("quotes", "Quotes", "int")]);
    Ok(json!({
        "columns": columns,
        "rows": rows,
        "meta": {"from_date": from, "to_date": to, "truncated": truncated, "suppliers": suppliers.iter().map(|((n, id), c)| json!({"column": c, "supplier": id, "supplier_name": n})).collect::<Vec<_>>()},
    }))
}

// ───────────────────────────── scorecard ranking ─────────────────────────────

async fn ranking(tx: &mut Tx, ctx: &Ctx, p: &Params<'_>) -> Result<Value> {
    #[allow(clippy::type_complexity)]
    let data: Vec<(String, String, Option<Decimal>, Option<String>, Option<NaiveDate>, bool, bool, bool, bool, bool, Option<Decimal>, Option<Decimal>, Option<Decimal>, Option<Decimal>)> =
        sqlx::query_as(
            "select coalesce(sc.supplier_name, ''), coalesce(sc.period, 'Monthly'), sc.current_score, sc.current_standing, sc.scored_until, \
               coalesce(sc.prevent_pos, false), coalesce(sc.warn_pos, false), coalesce(sc.prevent_rfqs, false), coalesce(sc.warn_rfqs, false), coalesce(sc.disabled, false), \
               p.on_time_pct, p.quality_pct, p.price_variance_pct, p.response_rate_pct \
             from e_supplier_scorecard sc left join lateral ( \
               select x.on_time_pct, x.quality_pct, x.price_variance_pct, x.response_rate_pct from e_supplier_scorecard_period x \
               where x.tenant_id = sc.tenant_id and x.scorecard = sc.id and not coalesce(x.no_activity, false) order by x.period_start desc limit 1) p on true \
             where sc.tenant_id = $1 and ($2::text is null or sc.current_standing = $2) \
             order by sc.current_score desc nulls last, sc.supplier_name limit $3",
        )
        .bind(ctx.tenant)
        .bind(p.str("standing"))
        .bind(MAX_ROWS)
        .fetch_all(&mut **tx)
        .await?;
    let mut rank = 0;
    let rows: Vec<Value> = data
        .into_iter()
        .map(|(name, per, score, standing, until, prevent_pos, warn_pos, prevent_rfqs, warn_rfqs, disabled, on_time, quality, variance, response)| {
            let ranked = score.is_some() && !disabled;
            if ranked {
                rank += 1;
            }
            let effect = if disabled {
                "Disabled"
            } else if prevent_pos && prevent_rfqs {
                "Blocks RFQs and orders"
            } else if prevent_pos {
                "Blocks orders"
            } else if prevent_rfqs {
                "Blocks RFQs"
            } else if warn_pos || warn_rfqs {
                "Warns"
            } else {
                ""
            };
            let pct = |v: Option<Decimal>| v.map(|x| money(x.round_dp(2)));
            json!({"rank": ranked.then_some(rank), "supplier": name, "period": per, "score": score.map(|s| money(s.round_dp(2))), "standing": standing,
                   "on_time_pct": pct(on_time), "quality_pct": pct(quality), "price_variance_pct": pct(variance), "response_rate_pct": pct(response),
                   "scored_until": until, "effect": effect})
        })
        .collect();
    Ok(json!({
        "columns": [col("rank", "Rank", "int"), col("supplier", "Supplier", "data"), col("score", "Score", "decimal"), col("standing", "Standing", "data"),
            col("on_time_pct", "On Time %", "percent"), col("quality_pct", "Quality %", "percent"), col("price_variance_pct", "Price vs Quotes %", "percent"),
            col("response_rate_pct", "RFQ Responses %", "percent"), col("scored_until", "Scored Until", "date"), col("period", "Period", "data"),
            col("effect", "Effect", "data")],
        "rows": rows,
        "meta": {},
    }))
}

// ───────────────────────────── purchase analytics ─────────────────────────────

fn months(from: NaiveDate, to: NaiveDate) -> Vec<String> {
    let mut out = Vec::new();
    let mut m = NaiveDate::from_ymd_opt(from.year(), from.month(), 1).unwrap_or(from);
    while m <= to && out.len() <= MAX_MONTHS {
        out.push(m.format("%Y-%m").to_string());
        m = m.checked_add_months(chrono::Months::new(1)).unwrap_or(to + chrono::Duration::days(1));
    }
    out
}

async fn analytics(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let columns_months = months(from, to);
    if columns_months.len() > MAX_MONTHS {
        return Err(Error::field("from_date", format!("choose a range of at most {MAX_MONTHS} months")));
    }
    // Table names and expressions come from these fixed choices only.
    let based_on = p.str("based_on").unwrap_or("Purchase Order");
    let (doc, lines, base_net, sign) = match based_on {
        "Purchase Order" => ("e_purchase_order", "e_purchase_order__items", "l.net_amount * coalesce(nullif(d.conversion_rate, 0), 1)", "1"),
        "Purchase Receipt" => (
            "e_purchase_receipt",
            "e_purchase_receipt__items",
            "l.net_amount * coalesce(nullif(d.conversion_rate, 0), 1)",
            "(case when coalesce(d.is_return, false) then -1 else 1 end)",
        ),
        "Purchase Invoice" => {
            ("e_purchase_invoice", "e_purchase_invoice__items", "coalesce(l.base_net_amount, l.net_amount * coalesce(nullif(d.conversion_rate, 0), 1))", "1")
        }
        _ => return Err(Error::field("based_on", "choose Purchase Order, Purchase Receipt or Purchase Invoice")),
    };
    // The report is listed under purchase orders; receipts and invoices need their own access.
    ctx.require(&based_on.replace(' ', ""), Action::Read)?;
    let (key, label) = match p.str("group_by").unwrap_or("Supplier") {
        "Supplier" => ("d.supplier::text", "coalesce(s.supplier_name, '')"),
        "Item" => ("l.item::text", "coalesce(i.item_code, '') || ' - ' || coalesce(i.item_name, '')"),
        "Item Group" => ("coalesce(i.item_group::text, '')", "coalesce(g.group_name, 'No group')"),
        _ => return Err(Error::field("group_by", "choose Supplier, Item or Item Group")),
    };
    let quantity = p.str("value_or_qty") == Some("Quantity");
    let measure = if quantity { format!("l.qty * {sign}") } else { format!("{base_net} * {sign}") };
    let sql = format!(
        "select {key}, {label}, to_char(d.posting_date, 'YYYY-MM'), coalesce(sum({measure}), 0) \
         from {doc} d join {lines} l on l.tenant_id = d.tenant_id and l.parent_id = d.id \
         left join e_item i on i.tenant_id = l.tenant_id and i.id = l.item left join e_item_group g on g.tenant_id = i.tenant_id and g.id = i.item_group \
         left join e_supplier s on s.tenant_id = d.tenant_id and s.id = d.supplier \
         where d.tenant_id = $1 and d.company = $2 and d.docstatus = 1 and d.posting_date between $3 and $4 \
           and ($5::uuid is null or d.supplier = $5) and ($6::uuid is null or l.item = $6) \
         group by 1, 2, 3"
    );
    let data: Vec<(String, String, String, Decimal)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(ctx.tenant)
        .bind(co.id)
        .bind(from)
        .bind(to)
        .bind(p.uuid("supplier")?)
        .bind(p.uuid("item")?)
        .fetch_all(&mut **tx)
        .await?;
    let precision = if quantity { 9 } else { co.precision };
    let mut groups: BTreeMap<String, (String, BTreeMap<String, Decimal>)> = BTreeMap::new();
    for (k, name, month, v) in data {
        let g = groups.entry(k).or_insert_with(|| (name, BTreeMap::new()));
        *g.1.entry(month).or_default() += v;
    }
    let mut month_totals: BTreeMap<String, Decimal> = BTreeMap::new();
    let mut rows: Vec<(Decimal, Value)> = groups
        .into_values()
        .map(|(name, per_month)| {
            let mut row = Map::new();
            row.insert("name".into(), json!(name));
            let mut total = Decimal::ZERO;
            for m in &columns_months {
                let v = per_month.get(m).copied().unwrap_or_default();
                *month_totals.entry(m.clone()).or_default() += v;
                total += v;
                row.insert(m.clone(), money(v.round_dp(precision)));
            }
            row.insert("total".into(), money(total.round_dp(precision)));
            (total, Value::Object(row))
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    let grand: Decimal = month_totals.values().copied().sum();
    let mut totals = Map::new();
    for m in &columns_months {
        totals.insert(m.clone(), money(month_totals.get(m).copied().unwrap_or_default().round_dp(precision)));
    }
    totals.insert("total".into(), money(grand.round_dp(precision)));
    let kind = if quantity { "decimal" } else { "currency" };
    let mut columns = vec![col("name", p.str("group_by").unwrap_or("Supplier"), "data")];
    columns.extend(columns_months.iter().map(|m| col(m, m, kind)));
    columns.push(col("total", "Total", kind));
    Ok(json!({
        "columns": columns,
        "rows": rows.into_iter().map(|(_, r)| r).collect::<Vec<_>>(),
        "totals": totals,
        "meta": {"from_date": from, "to_date": to, "based_on": based_on, "show": if quantity { "Quantity" } else { "Value" }},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months_cover_the_range() {
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        assert_eq!(months(d("2026-01-15"), d("2026-03-02")), vec!["2026-01", "2026-02", "2026-03"]);
        assert_eq!(months(d("2025-12-31"), d("2026-01-01")), vec!["2025-12", "2026-01"]);
        assert!(months(d("2020-01-01"), d("2026-01-01")).len() > MAX_MONTHS);
    }

    #[test]
    fn stages_read_naturally() {
        let n = |s: &str| s.parse::<Decimal>().unwrap();
        assert_eq!(stage("Purchase", n("10"), n("0"), n("0"), false, false), "Requested");
        assert_eq!(stage("Purchase", n("10"), n("0"), n("0"), true, false), "RFQ Sent");
        assert_eq!(stage("Purchase", n("10"), n("0"), n("0"), true, true), "Quoted");
        assert_eq!(stage("Purchase", n("10"), n("4"), n("0"), true, true), "Partially Ordered");
        assert_eq!(stage("Purchase", n("10"), n("10"), n("0"), false, false), "Ordered");
        assert_eq!(stage("Purchase", n("10"), n("10"), n("3"), false, false), "Partially Received");
        assert_eq!(stage("Purchase", n("10"), n("10"), n("10"), false, false), "Received");
        assert_eq!(stage("Material Transfer", n("10"), n("10"), n("10"), false, false), "Done");
    }

    #[test]
    fn catalog_names_entities_and_params() {
        let c = ProcurementReports.catalog();
        assert_eq!(c.len(), 5);
        assert!(c.iter().all(|r| r["entity"].is_string() && r["params"].is_array()));
    }
}
