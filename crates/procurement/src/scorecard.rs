//! Supplier scorecards. Each ended month or quarter gets a `SupplierScorecardPeriod` with the
//! supplier's metrics, a 0-100 score from the weighted criteria (CEL formulas over the metrics)
//! and the standing that score falls in. The latest scored standing decides whether new RFQs and
//! purchase orders to the supplier are warned about or refused. A period without any activity
//! carries the previous score forward instead of rewarding or punishing silence.

use async_trait::async_trait;
use chrono::{Datelike, Months, NaiveDate};
use ferrum_domain::util::{b, d, find, id, money, rows, s, set, sys};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, Filter, HookCtx, Kernel, Result, Tx};
use ferrum_meta::{Expr, Scope};
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use serde_json::{Value, json};
use uuid::Uuid;

/// Periods scored per scorecard per run, so a scorecard started years back catches up in steps.
const PERIODS_PER_RUN: usize = 24;

/// A supplier's current standing on an enabled scorecard that has scored them.
#[derive(Debug, Clone)]
pub struct Standing {
    pub name: String,
    pub score: Decimal,
    pub warn_rfqs: bool,
    pub prevent_rfqs: bool,
    pub warn_pos: bool,
    pub prevent_pos: bool,
}

impl Standing {
    pub fn label(&self) -> String {
        format!("{} ({})", self.name, self.score.round_dp(2).normalize())
    }
}

pub async fn standing(k: &Kernel, ctx: &Ctx, tx: &mut Tx, supplier: Uuid) -> Result<Option<Standing>> {
    let found = find(k, ctx, tx, "SupplierScorecard", vec![Filter::eq("supplier", supplier.to_string()), Filter::eq("disabled", false)], None, 1).await?;
    Ok(found.first().and_then(|sc| {
        Some(Standing {
            name: s(sc, "current_standing")?.to_string(),
            score: d(sc, "current_score"),
            warn_rfqs: b(sc, "warn_rfqs"),
            prevent_rfqs: b(sc, "prevent_rfqs"),
            warn_pos: b(sc, "warn_pos"),
            prevent_pos: b(sc, "prevent_pos"),
        })
    }))
}

// ───────────────────────────── periods ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Period {
    Monthly,
    Quarterly,
}

impl Period {
    fn of(doc: &Doc) -> Period {
        if s(doc, "period") == Some("Quarterly") { Period::Quarterly } else { Period::Monthly }
    }

    /// First day of the calendar month or quarter containing `day`.
    fn start(self, day: NaiveDate) -> NaiveDate {
        let month = match self {
            Period::Monthly => day.month(),
            Period::Quarterly => (day.month() - 1) / 3 * 3 + 1,
        };
        NaiveDate::from_ymd_opt(day.year(), month, 1).unwrap_or(day)
    }

    /// Last day of the period starting on `start`.
    fn end(self, start: NaiveDate) -> NaiveDate {
        let months = if self == Period::Monthly { 1 } else { 3 };
        start.checked_add_months(Months::new(months)).and_then(|n| n.pred_opt()).unwrap_or(start)
    }
}

// ───────────────────────────── metrics ─────────────────────────────

/// What a supplier did in one period. Percentages without any underlying event are neutral
/// (100, or 0 for the price variance), so a criterion only moves the score when there is evidence.
#[derive(Debug, Clone, Default, PartialEq)]
struct Metrics {
    accepted_qty: Decimal,
    rejected_qty: Decimal,
    on_time_qty: Decimal,
    po_count: i64,
    po_amount: Decimal,
    /// Σ qty × last quoted rate, and Σ qty × (ordered rate − quoted rate), over order lines with a prior quote.
    quoted_value: Decimal,
    variance_value: Decimal,
    rfq_count: i64,
    rfq_responded: i64,
}

fn pct(part: Decimal, whole: Decimal, neutral: Decimal) -> Decimal {
    if whole.is_zero() { neutral } else { (part / whole * Decimal::ONE_HUNDRED).round_dp(4) }
}

impl Metrics {
    fn received_qty(&self) -> Decimal {
        self.accepted_qty + self.rejected_qty
    }
    fn no_activity(&self) -> bool {
        self.received_qty().is_zero() && self.po_count == 0 && self.rfq_count == 0
    }
    fn on_time_pct(&self) -> Decimal {
        pct(self.on_time_qty, self.received_qty(), Decimal::ONE_HUNDRED)
    }
    fn quality_pct(&self) -> Decimal {
        pct(self.accepted_qty, self.received_qty(), Decimal::ONE_HUNDRED)
    }
    fn price_variance_pct(&self) -> Decimal {
        pct(self.variance_value, self.quoted_value, Decimal::ZERO)
    }
    fn response_rate_pct(&self) -> Decimal {
        pct(Decimal::from(self.rfq_responded), Decimal::from(self.rfq_count), Decimal::ONE_HUNDRED)
    }

    /// The metrics as formulas see them (`doc.<name>`); all doubles, as CEL does not mix int and double.
    fn cel(&self) -> Value {
        let f = |v: Decimal| v.to_f64().unwrap_or(0.0);
        json!({
            "on_time_pct": f(self.on_time_pct()),
            "quality_pct": f(self.quality_pct()),
            "price_variance_pct": f(self.price_variance_pct()),
            "response_rate_pct": f(self.response_rate_pct()),
            "received_qty": f(self.received_qty()),
            "accepted_qty": f(self.accepted_qty),
            "rejected_qty": f(self.rejected_qty),
            "on_time_qty": f(self.on_time_qty),
            "po_count": self.po_count as f64,
            "po_amount": f(self.po_amount),
            "rfq_count": self.rfq_count as f64,
            "rfq_responded": self.rfq_responded as f64,
        })
    }
}

async fn metrics(tx: &mut Tx, tenant: Uuid, supplier: Uuid, from: NaiveDate, to: NaiveDate) -> Result<Metrics> {
    // Receipts (returns excluded): on time when received by the order line's (or order's) required date.
    let (accepted_qty, rejected_qty, on_time_qty): (Decimal, Decimal, Decimal) = sqlx::query_as(
        "select coalesce(sum(i.qty), 0), coalesce(sum(coalesce(i.rejected_qty, 0)), 0), \
         coalesce(sum(case when coalesce(ol.schedule_date, o.schedule_date) is null or r.posting_date <= coalesce(ol.schedule_date, o.schedule_date) \
                      then i.qty + coalesce(i.rejected_qty, 0) else 0 end), 0) \
         from e_purchase_receipt r join e_purchase_receipt__items i on i.tenant_id = r.tenant_id and i.parent_id = r.id \
         left join e_purchase_order o on o.tenant_id = i.tenant_id and o.id = i.purchase_order \
         left join e_purchase_order__items ol on ol.tenant_id = i.tenant_id and ol.parent_id = i.purchase_order and ol.id::text = i.po_line \
         where r.tenant_id = $1 and r.supplier = $2 and r.docstatus = 1 and not coalesce(r.is_return, false) and r.posting_date between $3 and $4",
    )
    .bind(tenant)
    .bind(supplier)
    .bind(from)
    .bind(to)
    .fetch_one(&mut **tx)
    .await?;
    let (po_count, po_amount): (i64, Decimal) = sqlx::query_as(
        "select count(*), coalesce(sum(base_grand_total), 0) from e_purchase_order \
         where tenant_id = $1 and supplier = $2 and docstatus = 1 and posting_date between $3 and $4",
    )
    .bind(tenant)
    .bind(supplier)
    .bind(from)
    .bind(to)
    .fetch_one(&mut **tx)
    .await?;
    // Ordered rate against the supplier's own latest quotation for the item before the order
    // (company currency on both sides).
    let (quoted_value, variance_value): (Decimal, Decimal) = sqlx::query_as(
        "select coalesce(sum(x.qty * x.quoted), 0), coalesce(sum(x.qty * (x.paid - x.quoted)), 0) from ( \
           select l.qty, l.rate * coalesce(nullif(o.conversion_rate, 0), 1) as paid, \
             (select ql.rate * coalesce(nullif(q.conversion_rate, 0), 1) from e_supplier_quotation q \
              join e_supplier_quotation__items ql on ql.tenant_id = q.tenant_id and ql.parent_id = q.id \
              where q.tenant_id = o.tenant_id and q.supplier = o.supplier and q.docstatus = 1 and ql.item = l.item and q.posting_date <= o.posting_date \
              order by q.posting_date desc, q.created_at desc, ql.idx limit 1) as quoted \
           from e_purchase_order o join e_purchase_order__items l on l.tenant_id = o.tenant_id and l.parent_id = o.id \
           where o.tenant_id = $1 and o.supplier = $2 and o.docstatus = 1 and o.posting_date between $3 and $4) x \
         where x.quoted > 0",
    )
    .bind(tenant)
    .bind(supplier)
    .bind(from)
    .bind(to)
    .fetch_one(&mut **tx)
    .await?;
    // Invitations whose deadline fell in the period; declining counts as an answer.
    let (rfq_count, rfq_responded): (i64, i64) = sqlx::query_as(
        "select count(*), count(*) filter (where s.quote_status in ('Received', 'No Quote')) \
         from e_request_for_quotation r join e_request_for_quotation__suppliers s on s.tenant_id = r.tenant_id and s.parent_id = r.id \
         where r.tenant_id = $1 and s.supplier = $2 and r.docstatus = 1 and r.response_deadline between $3 and $4",
    )
    .bind(tenant)
    .bind(supplier)
    .bind(from)
    .bind(to)
    .fetch_one(&mut **tx)
    .await?;
    Ok(Metrics { accepted_qty, rejected_qty, on_time_qty, po_count, po_amount, quoted_value, variance_value, rfq_count, rfq_responded })
}

// ───────────────────────────── scoring ─────────────────────────────

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// The weighted 0-100 score and each criterion's part. A formula that yields no finite number
/// for a period (e.g. dividing by zero) scores 0 for that criterion.
fn evaluate(criteria: &[Doc], metrics: &Value) -> std::result::Result<(Decimal, Vec<Value>), String> {
    let user = json!({});
    let (mut weighted, mut weights) = (Decimal::ZERO, Decimal::ZERO);
    let mut parts = Vec::new();
    for c in criteria {
        let name = s(c, "criterion_name").unwrap_or("criterion");
        let expr = Expr::compile(s(c, "formula").unwrap_or_default()).map_err(|e| format!("{name}: {e}"))?;
        let out = expr.eval(&Scope { doc: metrics, old: None, parent: None, user: &user }).map_err(|e| format!("{name}: {e}"))?;
        // Non-finite doubles reach us as null.
        let value = match &out {
            Value::Null => None,
            v => Some(number(v).ok_or_else(|| format!("{name}: the formula must give a number, not {out}"))?),
        }
        .and_then(Decimal::from_f64);
        let max = d(c, "max_score");
        let max = if max > Decimal::ZERO { max } else { Decimal::ONE_HUNDRED };
        let score = value.map(|v| (v / max * Decimal::ONE_HUNDRED).clamp(Decimal::ZERO, Decimal::ONE_HUNDRED).round_dp(4)).unwrap_or(Decimal::ZERO);
        let weight = d(c, "weight");
        weighted += score * weight;
        weights += weight;
        parts.push(json!({"criterion": name, "value": value.map(|v| money(v.round_dp(4))), "score": money(score), "weight": money(weight)}));
    }
    let total = if weights.is_zero() { Decimal::ZERO } else { (weighted / weights).round_dp(2) };
    Ok((total, parts))
}

/// The standing whose range holds `score` (ranges are contiguous from 0 to 100).
fn standing_for(standings: &[Doc], score: Decimal) -> Option<&Doc> {
    standings.iter().filter(|st| d(st, "min_score") <= score).max_by_key(|st| d(st, "min_score"))
}

/// The scorecard fields that follow from its score and standings.
fn standing_values(standings: &[Doc], score: Option<Decimal>) -> Doc {
    let st = score.and_then(|sc| standing_for(standings, sc));
    let flag = |f: &str| Value::Bool(st.is_some_and(|x| b(x, f)));
    let mut v = Doc::new();
    v.insert("current_score".into(), score.map(money).unwrap_or(Value::Null));
    v.insert("current_standing".into(), st.and_then(|x| s(x, "standing_name")).map(|n| json!(n)).unwrap_or(Value::Null));
    for f in ["warn_rfqs", "prevent_rfqs", "warn_pos", "prevent_pos"] {
        v.insert(f.into(), flag(f));
    }
    v
}

fn default_criteria() -> Value {
    json!([
        {"criterion_name": "On-time delivery", "weight": "40", "formula": "doc.on_time_pct", "max_score": "100"},
        {"criterion_name": "Received quality", "weight": "30", "formula": "doc.quality_pct", "max_score": "100"},
        {"criterion_name": "Price against quotations", "weight": "15", "formula": "100.0 - doc.price_variance_pct", "max_score": "100"},
        {"criterion_name": "RFQ responsiveness", "weight": "15", "formula": "doc.response_rate_pct", "max_score": "100"},
    ])
}

fn default_standings() -> Value {
    json!([
        {"standing_name": "Very Poor", "min_score": "0", "max_score": "30", "warn_rfqs": false, "prevent_rfqs": true, "warn_pos": false, "prevent_pos": true},
        {"standing_name": "Poor", "min_score": "30", "max_score": "50", "warn_rfqs": true, "prevent_rfqs": false, "warn_pos": true, "prevent_pos": false},
        {"standing_name": "Good", "min_score": "50", "max_score": "80", "warn_rfqs": false, "prevent_rfqs": false, "warn_pos": false, "prevent_pos": false},
        {"standing_name": "Excellent", "min_score": "80", "max_score": "100", "warn_rfqs": false, "prevent_rfqs": false, "warn_pos": false, "prevent_pos": false},
    ])
}

/// Metrics that exercise every formula when a scorecard is saved.
fn sample_metrics() -> Value {
    Metrics {
        accepted_qty: Decimal::from(90),
        rejected_qty: Decimal::from(10),
        on_time_qty: Decimal::from(80),
        po_count: 4,
        po_amount: Decimal::from(1000),
        quoted_value: Decimal::from(100),
        variance_value: Decimal::from(5),
        rfq_count: 4,
        rfq_responded: 3,
    }
    .cel()
}

fn check_setup(doc: &Doc) -> Result<()> {
    let mut errors = Vec::new();
    let criteria = rows(doc, "criteria");
    let weights: Decimal = criteria.iter().map(|c| d(c, "weight")).sum();
    if weights != Decimal::ONE_HUNDRED {
        errors.push(FieldError::new("criteria", format!("criteria weights must add up to 100 (they add up to {})", weights.normalize())));
    }
    for (i, c) in criteria.iter().enumerate() {
        if let Err(m) = evaluate(std::slice::from_ref(c), &sample_metrics()) {
            errors.push(FieldError::new("formula", m).in_row("criteria", i));
        }
    }
    let mut standings: Vec<(usize, Doc)> = rows(doc, "standings").into_iter().enumerate().collect();
    standings.sort_by_key(|(_, st)| d(st, "min_score"));
    let mut expected = Decimal::ZERO;
    for (i, st) in &standings {
        let (min, max) = (d(st, "min_score"), d(st, "max_score"));
        if max <= min {
            errors.push(FieldError::new("max_score", "must be above the minimum score").in_row("standings", *i));
        } else if min != expected {
            errors.push(
                FieldError::new(
                    "min_score",
                    format!("standings must cover 0-100 without gaps or overlaps: expected a range starting at {}", expected.normalize()),
                )
                .in_row("standings", *i),
            );
        }
        expected = max;
    }
    if !standings.is_empty() && expected != Decimal::ONE_HUNDRED {
        errors.push(FieldError::new("standings", "the highest standing must end at 100"));
    }
    if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
}

/// Scores every ended, not yet scored period of one scorecard (locking it). Returns how many.
pub(crate) async fn score_scorecard(k: &Kernel, ctx: &Ctx, tx: &mut Tx, scorecard: Uuid) -> Result<usize> {
    let sc = k.get_tx(ctx, tx, "SupplierScorecard", scorecard, true).await?;
    let supplier = id(&sc, "supplier").ok_or_else(|| Error::internal("scorecard without a supplier"))?;
    let period = Period::of(&sc);
    let today = ctx.today();
    let criteria = rows(&sc, "criteria");
    let standings = rows(&sc, "standings");
    let mut start = match sc.get("scored_until").and_then(Value::as_str).and_then(|x| NaiveDate::parse_from_str(x, "%Y-%m-%d").ok()) {
        Some(until) => until.succ_opt().unwrap_or(until),
        None => period.start(ferrum_domain::util::date(&sc, "start_date")?),
    };
    let mut last: Option<Decimal> = sc.get("current_score").filter(|v| !v.is_null()).map(|_| d(&sc, "current_score"));
    let mut scored_until = None;
    let mut n = 0;
    while n < PERIODS_PER_RUN {
        let end = period.end(start);
        if end >= today {
            break;
        }
        let stored: Option<(Option<Decimal>, bool)> = sqlx::query_as(
            "select score, coalesce(no_activity, false) from e_supplier_scorecard_period where tenant_id = $1 and scorecard = $2 and period_start = $3",
        )
        .bind(ctx.tenant)
        .bind(scorecard)
        .bind(start)
        .fetch_optional(&mut **tx)
        .await?;
        match stored {
            // Written by an earlier run that stopped before moving the scorecard on: keep it.
            Some((score, quiet)) => {
                if !quiet && score.is_some() {
                    last = score;
                }
            }
            None => {
                let m = metrics(tx, ctx.tenant, supplier, start, end).await?;
                let quiet = m.no_activity();
                let (score, parts) = if quiet { (last, vec![]) } else { evaluate(&criteria, &m.cel()).map(|(s, p)| (Some(s), p)).map_err(Error::invalid)? };
                let mut rec = Doc::new();
                rec.insert("scorecard".into(), json!(scorecard.to_string()));
                rec.insert("supplier".into(), json!(supplier.to_string()));
                rec.insert("period_start".into(), json!(start.to_string()));
                rec.insert("period_end".into(), json!(end.to_string()));
                rec.insert("score".into(), score.map(money).unwrap_or(Value::Null));
                rec.insert(
                    "standing".into(),
                    score.and_then(|x| standing_for(&standings, x)).and_then(|st| s(st, "standing_name")).map(|x| json!(x)).unwrap_or(Value::Null),
                );
                rec.insert("no_activity".into(), json!(quiet));
                for (f, v) in [
                    ("on_time_pct", m.on_time_pct()),
                    ("quality_pct", m.quality_pct()),
                    ("price_variance_pct", m.price_variance_pct()),
                    ("response_rate_pct", m.response_rate_pct()),
                    ("received_qty", m.received_qty()),
                    ("rejected_qty", m.rejected_qty),
                    ("on_time_qty", m.on_time_qty),
                    ("po_amount", m.po_amount),
                ] {
                    rec.insert(f.into(), money(v));
                }
                rec.insert("po_count".into(), json!(m.po_count));
                rec.insert("rfq_count".into(), json!(m.rfq_count));
                rec.insert("rfq_responded".into(), json!(m.rfq_responded));
                rec.insert("criteria_scores".into(), Value::Array(parts));
                k.insert_tx(ctx, tx, "SupplierScorecardPeriod", rec).await?;
                last = score;
            }
        }
        scored_until = Some(end);
        start = end.succ_opt().unwrap_or(end);
        n += 1;
    }
    if let Some(until) = scored_until {
        let mut v = standing_values(&standings, last);
        v.insert("scored_until".into(), json!(until.to_string()));
        k.set_values_tx(ctx, tx, "SupplierScorecard", scorecard, v).await?;
    }
    Ok(n)
}

/// Upkeep: scores the ended periods of every enabled scorecard, each in its own transaction.
pub(crate) async fn score_due(k: &Kernel, ctx: &Ctx) -> Result<()> {
    let today = ctx.today();
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let cards: Vec<(Uuid, Option<String>, NaiveDate, Option<NaiveDate>)> = sqlx::query_as(
        "select id, period, start_date, scored_until from e_supplier_scorecard where tenant_id = $1 and not coalesce(disabled, false) order by id",
    )
    .bind(ctx.tenant)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut first_error = None;
    for (card, period, start_date, scored_until) in cards {
        let period = if period.as_deref() == Some("Quarterly") { Period::Quarterly } else { Period::Monthly };
        let next = scored_until.and_then(|u| u.succ_opt()).unwrap_or_else(|| period.start(start_date));
        if period.end(next) >= today {
            continue;
        }
        let outcome = async {
            let mut tx = k.db.tenant_tx(ctx.tenant).await?;
            score_scorecard(k, ctx, &mut tx, card).await?;
            tx.commit().await?;
            Ok::<(), Error>(())
        }
        .await;
        if let Err(e) = outcome {
            tracing::warn!(tenant = %ctx.tenant, scorecard = %card, "scoring failed: {e}");
            first_error.get_or_insert(e);
        }
    }
    first_error.map_or(Ok(()), Err)
}

// ───────────────────────────── hook ─────────────────────────────

pub struct ScorecardHook;

#[async_trait]
impl DocHook for ScorecardHook {
    async fn before_validate(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        for (table, defaults) in [("criteria", default_criteria()), ("standings", default_standings())] {
            if rows(doc, table).is_empty() {
                // Rows a hook adds need the ids and positions input rows get from the kernel.
                let mut list = defaults;
                for (i, r) in list.as_array_mut().into_iter().flatten().enumerate() {
                    r["id"] = json!(Uuid::now_v7().to_string());
                    r["idx"] = json!(i);
                }
                set(doc, table, list);
            }
        }
        // Edited standings apply to the score already earned.
        if doc.get("current_score").is_some_and(|v| !v.is_null()) {
            let score = d(doc, "current_score");
            for (k, v) in standing_values(&rows(doc, "standings"), Some(score)) {
                doc.insert(k, v);
            }
        }
        Ok(())
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        check_setup(doc)
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "refresh" {
            return Ok(None);
        }
        let card = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let sctx = sys(hc.ctx);
        let scored = score_scorecard(hc.kernel, &sctx, hc.tx, card).await?;
        let now = hc.kernel.get_tx(&sctx, hc.tx, "SupplierScorecard", card, false).await?;
        Ok(Some(json!({
            "periods_scored": scored,
            "current_score": now.get("current_score"),
            "current_standing": now.get("current_standing"),
            "scored_until": now.get("scored_until"),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn docs(v: Value) -> Vec<Doc> {
        v.as_array().unwrap().iter().map(|r| r.as_object().unwrap().clone()).collect()
    }

    #[test]
    fn periods_follow_the_calendar() {
        let day = NaiveDate::from_ymd_opt(2026, 8, 17).unwrap();
        assert_eq!(Period::Monthly.start(day), NaiveDate::from_ymd_opt(2026, 8, 1).unwrap());
        assert_eq!(Period::Monthly.end(NaiveDate::from_ymd_opt(2026, 2, 1).unwrap()), NaiveDate::from_ymd_opt(2026, 2, 28).unwrap());
        assert_eq!(Period::Quarterly.start(day), NaiveDate::from_ymd_opt(2026, 7, 1).unwrap());
        assert_eq!(Period::Quarterly.end(NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()), NaiveDate::from_ymd_opt(2026, 12, 31).unwrap());
    }

    #[test]
    fn neutral_metrics_without_evidence() {
        let quiet = Metrics::default();
        assert!(quiet.no_activity());
        assert_eq!((quiet.on_time_pct(), quiet.quality_pct(), quiet.price_variance_pct(), quiet.response_rate_pct()), (n("100"), n("100"), n("0"), n("100")));
        let m = Metrics { accepted_qty: n("80"), rejected_qty: n("20"), quoted_value: n("1000"), variance_value: n("200"), rfq_count: 2, ..Default::default() };
        assert_eq!((m.on_time_pct(), m.quality_pct(), m.price_variance_pct(), m.response_rate_pct()), (n("0"), n("80"), n("20"), n("0")));
    }

    #[test]
    fn default_scorecard_scores_and_ranks() {
        let m = Metrics {
            accepted_qty: n("80"),
            rejected_qty: n("20"),
            quoted_value: n("1000"),
            variance_value: n("200"),
            rfq_count: 2,
            po_count: 1,
            ..Default::default()
        };
        let (score, parts) = evaluate(&docs(default_criteria()), &m.cel()).unwrap();
        // 40% × 0 on time + 30% × 80 quality + 15% × (100 − 20) price + 15% × 0 responses.
        assert_eq!(score, n("36"));
        assert_eq!(parts.len(), 4);
        let standings = docs(default_standings());
        assert_eq!(s(standing_for(&standings, score).unwrap(), "standing_name"), Some("Poor"));
        assert_eq!(s(standing_for(&standings, n("100")).unwrap(), "standing_name"), Some("Excellent"));
        assert_eq!(s(standing_for(&standings, n("0")).unwrap(), "standing_name"), Some("Very Poor"));
        let v = standing_values(&standings, Some(n("12")));
        assert_eq!((v["prevent_pos"].clone(), v["warn_pos"].clone()), (json!(true), json!(false)));
        assert_eq!(standing_values(&standings, None)["current_standing"], Value::Null);
    }

    #[test]
    fn formulas_are_clamped_and_checked() {
        let crit = |f: &str| docs(json!([{"criterion_name": "c", "weight": "100", "formula": f, "max_score": "50"}]));
        let m = Metrics::default().cel();
        assert_eq!(evaluate(&crit("doc.on_time_pct"), &m).unwrap().0, n("100"), "100 against a max of 50 is capped");
        assert_eq!(evaluate(&crit("25.0"), &m).unwrap().0, n("50"));
        assert_eq!(evaluate(&crit("-5.0"), &m).unwrap().0, n("0"));
        assert_eq!(evaluate(&crit("doc.po_count / doc.rfq_count"), &m).unwrap().0, n("0"), "no finite value scores 0");
        assert!(evaluate(&crit("doc.po_count > 1.0"), &m).is_err(), "a condition is not a score");
        assert!(evaluate(&crit("doc.nope + 1.0"), &m).is_err());
        let mut sc = Doc::new();
        sc.insert("criteria".into(), json!([{"criterion_name": "a", "weight": "60", "formula": "doc.quality_pct"}]));
        sc.insert(
            "standings".into(),
            json!([{"standing_name": "x", "min_score": "0", "max_score": "40"}, {"standing_name": "y", "min_score": "50", "max_score": "100"}]),
        );
        let Err(Error::Validation(errs)) = check_setup(&sc) else { panic!("weights and gaps must be refused") };
        assert!(errs.iter().any(|e| e.field == "criteria") && errs.iter().any(|e| e.field == "min_score"), "{errs:?}");
        sc.insert("criteria".into(), default_criteria());
        sc.insert("standings".into(), default_standings());
        check_setup(&sc).unwrap();
    }
}
