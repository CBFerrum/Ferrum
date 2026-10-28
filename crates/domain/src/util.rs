//! Small helpers shared by the domain modules.

use chrono::{NaiveDate, NaiveTime};
use ferrum_calc::Rounding;
use ferrum_kernel::value::{dec, decimal_value, parse_date};
use ferrum_kernel::{Ctx, Doc, Error, Filter, Kernel, Result, Tx};
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::Value;
use uuid::Uuid;

/// A context with full read access for engine lookups (company defaults, accounts, prices).
/// Writes that represent user intent keep using the caller's context.
pub fn sys(ctx: &Ctx) -> Ctx {
    let mut s = Ctx::system(ctx.tenant, ctx.schema.clone());
    s.request_id = ctx.request_id.clone();
    s.depth = ctx.depth;
    s.fy_start_month = ctx.fy_start_month;
    s
}

/// The caller's identity with engine privileges, for documents the engine creates on the
/// user's behalf (e.g. a work order's manufacture entry): read-only system fields can be set
/// and permission checks are skipped, while `created_by` and the audit trail still name the user.
pub fn elevated(ctx: &Ctx) -> Ctx {
    let mut c = ctx.clone();
    c.is_system = true;
    c.is_admin = true;
    c.perms = std::sync::Arc::new(ferrum_kernel::PermSet { admin: true, entities: Default::default() });
    c
}

pub fn d(doc: &Doc, k: &str) -> Decimal {
    dec(doc, k)
}

pub fn s<'a>(doc: &'a Doc, k: &str) -> Option<&'a str> {
    doc.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

pub fn id(doc: &Doc, k: &str) -> Option<Uuid> {
    s(doc, k).and_then(|v| v.parse().ok())
}

pub fn b(doc: &Doc, k: &str) -> bool {
    doc.get(k).and_then(Value::as_bool).unwrap_or(false)
}

pub fn rows(doc: &Doc, k: &str) -> Vec<Doc> {
    doc.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(|r| r.as_object().cloned()).collect()).unwrap_or_default()
}

pub fn set(doc: &mut Doc, k: &str, v: impl Into<Value>) {
    doc.insert(k.to_string(), v.into());
}

pub fn round(v: Decimal, p: u32) -> Decimal {
    let mut x = v.round_dp_with_strategy(p, RoundingStrategy::MidpointAwayFromZero);
    x.rescale(p);
    x
}

pub fn money(v: Decimal) -> Value {
    decimal_value(v)
}

pub fn date(doc: &Doc, k: &str) -> Result<NaiveDate> {
    let raw = s(doc, k).ok_or_else(|| Error::field(k, "is required"))?;
    parse_date(&raw[..raw.len().min(10)]).map_err(|m| Error::field(k, m))
}

/// `posting_date` + `posting_time` (HH:MM[:SS]) as a UTC timestamp for ordering stock moves.
pub fn posting_ts(doc: &Doc) -> Result<chrono::DateTime<chrono::Utc>> {
    let day = date(doc, "posting_date")?;
    let time = s(doc, "posting_time")
        .and_then(|t| NaiveTime::parse_from_str(t, "%H:%M:%S").or_else(|_| NaiveTime::parse_from_str(t, "%H:%M")).ok())
        .unwrap_or_else(|| NaiveTime::from_hms_opt(23, 59, 59).unwrap_or_default());
    Ok(day.and_time(time).and_utc())
}

pub fn row_id(row: &Doc) -> Result<Uuid> {
    id(row, "id").ok_or_else(|| Error::internal("child row without id"))
}

pub async fn get(k: &Kernel, ctx: &Ctx, tx: &mut Tx, entity: &str, record: Uuid) -> Result<Doc> {
    k.get_tx(&sys(ctx), tx, entity, record, false).await
}

pub async fn find(k: &Kernel, ctx: &Ctx, tx: &mut Tx, entity: &str, filters: Vec<Filter>, sort: Option<&str>, limit: u32) -> Result<Vec<Doc>> {
    k.find_tx(&sys(ctx), tx, entity, &filters, sort, limit).await
}

pub fn has(hc_entity: &ferrum_meta::EffEntity, field: &str) -> bool {
    hc_entity.field(field).is_some()
}

#[derive(Debug, Clone)]
pub struct Company {
    pub id: Uuid,
    pub doc: Doc,
    pub currency_code: String,
    pub currency_id: Uuid,
    pub precision: u32,
    pub rounding: Rounding,
    pub round_total_to: Option<Decimal>,
    pub allow_negative_stock: bool,
}

impl Company {
    pub fn account(&self, key: &str) -> Option<Uuid> {
        id(&self.doc, key)
    }

    pub fn require(&self, key: &str, what: &str) -> Result<Uuid> {
        self.account(key)
            .ok_or_else(|| Error::invalid(format!("set the company's {what} (Company → Default Accounts → {key}) or create the standard chart of accounts")))
    }

    pub fn name(&self) -> &str {
        s(&self.doc, "company_name").unwrap_or_default()
    }
}

pub async fn currency(k: &Kernel, ctx: &Ctx, tx: &mut Tx, currency_id: Uuid) -> Result<(String, u32)> {
    let c = get(k, ctx, tx, "Currency", currency_id).await?;
    let code = s(&c, "code").unwrap_or("XXX").to_string();
    let precision = c.get("precision").and_then(Value::as_i64).unwrap_or(2).clamp(0, 6) as u32;
    Ok((code, precision))
}

pub async fn company(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company_id: Uuid) -> Result<Company> {
    let doc = get(k, ctx, tx, "Company", company_id).await?;
    let currency_id = id(&doc, "default_currency").ok_or_else(|| Error::invalid("company has no default currency"))?;
    let (currency_code, precision) = currency(k, ctx, tx, currency_id).await?;
    let rounding = if s(&doc, "rounding_mode") == Some("Half Even") { Rounding::HalfEven } else { Rounding::HalfUp };
    let rt = d(&doc, "round_total_to");
    Ok(Company {
        id: company_id,
        currency_code,
        currency_id,
        precision,
        rounding,
        round_total_to: (rt > Decimal::ZERO).then_some(rt),
        allow_negative_stock: b(&doc, "allow_negative_stock"),
        doc,
    })
}

/// Latest exchange rate valid on `on` for from → to (or its inverse).
pub async fn exchange_rate(k: &Kernel, ctx: &Ctx, tx: &mut Tx, from: Uuid, to: Uuid, on: NaiveDate) -> Result<Decimal> {
    if from == to {
        return Ok(Decimal::ONE);
    }
    let day = on.format("%Y-%m-%d").to_string();
    let direct = find(
        k,
        ctx,
        tx,
        "ExchangeRate",
        vec![
            Filter::eq("from_currency", from.to_string()),
            Filter::eq("to_currency", to.to_string()),
            ferrum_kernel::Filter { field: "valid_from".into(), op: ferrum_kernel::Op::Lte, value: Value::String(day.clone()) },
        ],
        Some("valid_from desc"),
        1,
    )
    .await?;
    if let Some(r) = direct.first() {
        return Ok(d(r, "rate"));
    }
    let inverse = find(
        k,
        ctx,
        tx,
        "ExchangeRate",
        vec![
            Filter::eq("from_currency", to.to_string()),
            Filter::eq("to_currency", from.to_string()),
            ferrum_kernel::Filter { field: "valid_from".into(), op: ferrum_kernel::Op::Lte, value: Value::String(day) },
        ],
        Some("valid_from desc"),
        1,
    )
    .await?;
    match inverse.first().map(|r| d(r, "rate")).filter(|r| !r.is_zero()) {
        Some(r) => Ok((Decimal::ONE / r).round_dp(9)),
        None => Err(Error::field("conversion_rate", "no exchange rate is defined for this currency and date; add one under Exchange Rate")),
    }
}
