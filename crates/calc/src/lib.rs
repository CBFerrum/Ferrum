//! Exact-decimal document totals (pricing, discounts, taxes, rounding, base currency).
//!
//! Pure and I/O-free so the same code runs on the server and in the browser (feature `wasm`).
//! Invariants guaranteed for every successful result (property-tested below):
//! * every money value has at most `precision` (or `base_precision`) decimals;
//! * `Σ line.net_amount == net_total` and `Σ line.taxes[k] == taxes[k].tax_amount` (same in base currency);
//! * `grand_total == net_total + Σ ±tax_amount`, `rounded_total == grand_total + rounding_adjustment`;
//! * when every tax row is price-inclusive and there is no document discount, `grand_total == Σ line.amount`.

use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

pub const MAX_LINES: usize = 10_000;
pub const MAX_TAX_ROWS: usize = 100;
pub const MAX_PRECISION: u32 = 6;

/// Bounds keep every intermediate product far below `Decimal::MAX` (~7.9e28).
fn max_qty() -> Decimal {
    Decimal::new(1_000_000_000, 0)
}
fn max_money() -> Decimal {
    Decimal::new(1_000_000_000_000, 0)
}
fn hundred() -> Decimal {
    Decimal::ONE_HUNDRED
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CalcError {
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("arithmetic overflow while computing {0}")]
    Overflow(&'static str),
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, CalcError> {
    Err(CalcError::Invalid(msg.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Rounding {
    /// Commercial rounding: midpoint away from zero (2.5 → 3, -2.5 → -3).
    #[default]
    HalfUp,
    /// Banker's rounding: midpoint to even (2.5 → 2).
    HalfEven,
}

impl Rounding {
    fn strategy(self) -> RoundingStrategy {
        match self {
            Rounding::HalfUp => RoundingStrategy::MidpointAwayFromZero,
            Rounding::HalfEven => RoundingStrategy::MidpointNearestEven,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChargeType {
    /// `rate`% of each line's net amount.
    OnNetTotal,
    /// `rate`% of the tax amount of row `row_ref`.
    OnPreviousRowAmount,
    /// `rate`% of the running total after row `row_ref`.
    OnPreviousRowTotal,
    /// Fixed `amount`, distributed over lines by net amount.
    Actual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineIn {
    pub key: String,
    pub qty: Decimal,
    /// Unit price before line discount; tax-inclusive when inclusive tax rows apply.
    pub price: Decimal,
    #[serde(default)]
    pub discount_percent: Decimal,
    /// Per-line rate overrides keyed by tax row key (item tax templates, exemptions).
    #[serde(default)]
    pub tax_rates: BTreeMap<String, Decimal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaxIn {
    pub key: String,
    pub charge_type: ChargeType,
    #[serde(default)]
    pub rate: Decimal,
    /// Only for `Actual`.
    #[serde(default)]
    pub amount: Decimal,
    /// 0-based index of an earlier tax row, for the previous-row charge types.
    #[serde(default)]
    pub row_ref: Option<usize>,
    #[serde(default)]
    pub included_in_price: bool,
    /// Subtract instead of add (withholding taxes).
    #[serde(default)]
    pub deduct: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DocDiscount {
    Percent { value: Decimal },
    Amount { value: Decimal },
}

fn one() -> Decimal {
    Decimal::ONE
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalcInput {
    /// Decimal places of the document currency.
    pub precision: u32,
    /// Decimal places of the company (base) currency; defaults to `precision`.
    #[serde(default)]
    pub base_precision: Option<u32>,
    /// Document currency → base currency.
    #[serde(default = "one")]
    pub conversion_rate: Decimal,
    #[serde(default)]
    pub rounding: Rounding,
    pub lines: Vec<LineIn>,
    #[serde(default)]
    pub taxes: Vec<TaxIn>,
    /// Applied to the net (tax-exclusive) total before taxes.
    #[serde(default)]
    pub discount: Option<DocDiscount>,
    /// Cash rounding step for `rounded_total`, e.g. `1` or `0.05`. `None` disables it.
    #[serde(default)]
    pub round_total_to: Option<Decimal>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineOut {
    pub key: String,
    /// Effective unit price after line discount (as entered: inclusive or exclusive).
    pub rate: Decimal,
    /// `qty × rate`, rounded.
    pub amount: Decimal,
    /// Tax-exclusive amount after the document discount share.
    pub net_amount: Decimal,
    /// Tax per row, aligned with `CalcOutput::taxes`.
    pub taxes: Vec<Decimal>,
    pub base_net_amount: Decimal,
    pub base_taxes: Vec<Decimal>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaxOut {
    pub key: String,
    /// Unsigned row amount; apply `deduct` for the sign.
    pub tax_amount: Decimal,
    pub deduct: bool,
    /// Running total after this row.
    pub total: Decimal,
    pub base_tax_amount: Decimal,
    pub base_total: Decimal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalcOutput {
    pub lines: Vec<LineOut>,
    pub taxes: Vec<TaxOut>,
    /// Σ line amounts as entered.
    pub total: Decimal,
    pub discount_amount: Decimal,
    pub net_total: Decimal,
    /// Σ ±tax amounts.
    pub total_taxes: Decimal,
    pub grand_total: Decimal,
    pub rounding_adjustment: Decimal,
    pub rounded_total: Decimal,
    pub base_net_total: Decimal,
    pub base_total_taxes: Decimal,
    pub base_grand_total: Decimal,
    pub base_rounding_adjustment: Decimal,
    pub base_rounded_total: Decimal,
}

// ---------- checked arithmetic ----------

fn add(a: Decimal, b: Decimal, what: &'static str) -> Result<Decimal, CalcError> {
    a.checked_add(b).ok_or(CalcError::Overflow(what))
}
fn sub(a: Decimal, b: Decimal, what: &'static str) -> Result<Decimal, CalcError> {
    a.checked_sub(b).ok_or(CalcError::Overflow(what))
}
fn mul(a: Decimal, b: Decimal, what: &'static str) -> Result<Decimal, CalcError> {
    a.checked_mul(b).ok_or(CalcError::Overflow(what))
}
fn div(a: Decimal, b: Decimal, what: &'static str) -> Result<Decimal, CalcError> {
    a.checked_div(b).ok_or(CalcError::Overflow(what))
}
fn sum(values: &[Decimal], what: &'static str) -> Result<Decimal, CalcError> {
    values.iter().try_fold(Decimal::ZERO, |acc, v| add(acc, *v, what))
}
fn pct(base: Decimal, rate: Decimal, what: &'static str) -> Result<Decimal, CalcError> {
    div(mul(base, rate, what)?, hundred(), what)
}
fn round(v: Decimal, dp: u32, r: Rounding) -> Decimal {
    let mut x = v.round_dp_with_strategy(dp, r.strategy());
    x.rescale(dp);
    x
}

/// Split `total` (already at `dp` decimals) over `weights` so the parts sum exactly to `total`.
/// Largest-remainder method; works for any signs; ties go to the earlier index.
pub fn allocate(total: Decimal, weights: &[Decimal], dp: u32, r: Rounding) -> Result<Vec<Decimal>, CalcError> {
    let n = weights.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    let total = round(total, dp, r);
    let sum_w = sum(weights, "allocation weights")?;
    let raw: Vec<Decimal> = if sum_w.is_zero() {
        let share = div(total, Decimal::from(n as u64), "allocation")?;
        vec![share; n]
    } else {
        weights.iter().map(|w| mul(total, div(*w, sum_w, "allocation")?, "allocation")).collect::<Result<_, _>>()?
    };
    let mut out: Vec<Decimal> = raw.iter().map(|x| round(*x, dp, r)).collect();
    let unit = Decimal::new(1, dp);
    let diff = sub(total, sum(&out, "allocation")?, "allocation")?;
    if !diff.is_zero() {
        let up = diff.is_sign_positive();
        let mut order: Vec<usize> = (0..n).collect();
        // Most under-allocated first when adding units, most over-allocated first when removing.
        order.sort_by(|&a, &b| {
            let ra = if up { raw[a] - out[a] } else { out[a] - raw[a] };
            let rb = if up { raw[b] - out[b] } else { out[b] - raw[b] };
            rb.cmp(&ra).then(a.cmp(&b))
        });
        let steps = (diff / unit).abs().trunc();
        let step = if up { unit } else { -unit };
        let mut i = 0usize;
        let mut done = Decimal::ZERO;
        while done < steps {
            let idx = order[i % n];
            out[idx] = add(out[idx], step, "allocation")?;
            done += Decimal::ONE;
            i += 1;
        }
    }
    for v in &mut out {
        v.rescale(dp);
    }
    Ok(out)
}

fn validate(input: &CalcInput) -> Result<(), CalcError> {
    if input.precision > MAX_PRECISION {
        return invalid(format!("precision must be ≤ {MAX_PRECISION}"));
    }
    if input.base_precision.is_some_and(|bp| bp > MAX_PRECISION) {
        return invalid(format!("base_precision must be ≤ {MAX_PRECISION}"));
    }
    if input.conversion_rate <= Decimal::ZERO || input.conversion_rate > max_money() {
        return invalid("conversion_rate must be > 0");
    }
    if input.lines.len() > MAX_LINES {
        return invalid(format!("at most {MAX_LINES} lines"));
    }
    if input.taxes.len() > MAX_TAX_ROWS {
        return invalid(format!("at most {MAX_TAX_ROWS} tax rows"));
    }
    for (i, l) in input.lines.iter().enumerate() {
        if l.qty.abs() > max_qty() {
            return invalid(format!("line {}: qty out of range", i + 1));
        }
        if l.price.abs() > max_money() {
            return invalid(format!("line {}: price out of range", i + 1));
        }
        if l.discount_percent < Decimal::ZERO || l.discount_percent > hundred() {
            return invalid(format!("line {}: discount_percent must be within 0..100", i + 1));
        }
        for (k, r) in &l.tax_rates {
            if *r < Decimal::ZERO || *r > Decimal::from(1000) {
                return invalid(format!("line {}: tax rate for {k} must be within 0..1000", i + 1));
            }
        }
    }
    let mut keys = HashSet::new();
    for (k, t) in input.taxes.iter().enumerate() {
        if !keys.insert(t.key.as_str()) {
            return invalid(format!("duplicate tax row key {}", t.key));
        }
        if t.rate < Decimal::ZERO || t.rate > Decimal::from(1000) {
            return invalid(format!("tax row {}: rate must be within 0..1000", k + 1));
        }
        if t.included_in_price && t.deduct {
            return invalid(format!("tax row {}: a deducted tax cannot be price-inclusive", k + 1));
        }
        match t.charge_type {
            ChargeType::Actual => {
                if t.included_in_price {
                    return invalid(format!("tax row {}: an Actual tax cannot be price-inclusive", k + 1));
                }
                if t.amount.abs() > max_money() {
                    return invalid(format!("tax row {}: amount out of range", k + 1));
                }
            }
            ChargeType::OnPreviousRowAmount | ChargeType::OnPreviousRowTotal => {
                let Some(j) = t.row_ref else {
                    return invalid(format!("tax row {}: row_ref is required", k + 1));
                };
                if j >= k {
                    return invalid(format!("tax row {}: row_ref must point to an earlier row", k + 1));
                }
                if t.included_in_price && !input.taxes[j].included_in_price {
                    return invalid(format!("tax row {}: an inclusive tax can only reference inclusive rows", k + 1));
                }
            }
            ChargeType::OnNetTotal => {}
        }
        if t.included_in_price && t.charge_type == ChargeType::OnPreviousRowTotal {
            // Referenced rows before `row_ref` must also be inclusive, otherwise the factor is undefined.
            let j = t.row_ref.unwrap_or(0);
            if input.taxes[..=j].iter().any(|x| !x.included_in_price) {
                return invalid(format!("tax row {}: inclusive 'previous row total' needs all earlier rows inclusive", k + 1));
            }
        }
    }
    match &input.discount {
        Some(DocDiscount::Percent { value }) if *value < Decimal::ZERO || *value > hundred() => invalid("discount percent must be within 0..100")?,
        Some(DocDiscount::Amount { value }) if *value < Decimal::ZERO || *value > max_money() => invalid("discount amount must be ≥ 0")?,
        _ => {}
    }
    if let Some(step) = input.round_total_to
        && (step <= Decimal::ZERO || step > Decimal::from(1000))
    {
        return invalid("round_total_to must be within (0, 1000]");
    }
    Ok(())
}

/// Compute all document totals. Deterministic: same input, same output, on every platform.
pub fn compute(input: &CalcInput) -> Result<CalcOutput, CalcError> {
    validate(input)?;
    let p = input.precision;
    let bp = input.base_precision.unwrap_or(p);
    let r = input.rounding;
    let n = input.lines.len();
    let m = input.taxes.len();
    let taxes = &input.taxes;
    let rate_of = |i: usize, k: usize| -> Decimal { input.lines[i].tax_rates.get(&taxes[k].key).copied().unwrap_or(taxes[k].rate) };
    let sign = |k: usize| if taxes[k].deduct { -Decimal::ONE } else { Decimal::ONE };

    // 1. Line amounts as entered.
    let mut rates = Vec::with_capacity(n);
    let mut amounts = Vec::with_capacity(n);
    for l in &input.lines {
        let factor = sub(Decimal::ONE, div(l.discount_percent, hundred(), "discount")?, "discount")?;
        let rate = mul(l.price, factor, "rate")?;
        amounts.push(round(mul(l.qty, rate, "amount")?, p, r));
        rates.push(rate.round_dp(9).normalize());
    }
    let total = sum(&amounts, "total")?;

    // 2. Back out price-inclusive taxes: net = gross / (1 + Σ inclusive factors).
    let mut net_exact = Vec::with_capacity(n);
    for i in 0..n {
        let mut f = vec![Decimal::ZERO; m];
        for k in 0..m {
            let t = &taxes[k];
            if !t.included_in_price {
                continue;
            }
            let rk = div(rate_of(i, k), hundred(), "inclusive factor")?;
            f[k] = match t.charge_type {
                ChargeType::OnNetTotal => rk,
                ChargeType::OnPreviousRowAmount => mul(rk, f[t.row_ref.unwrap_or(0)], "inclusive factor")?,
                ChargeType::OnPreviousRowTotal => {
                    let j = t.row_ref.unwrap_or(0);
                    let upto = add(Decimal::ONE, sum(&f[..=j], "inclusive factor")?, "inclusive factor")?;
                    mul(rk, upto, "inclusive factor")?
                }
                ChargeType::Actual => Decimal::ZERO,
            };
        }
        let factor = add(Decimal::ONE, sum(&f, "inclusive factor")?, "inclusive factor")?;
        net_exact.push(div(amounts[i], factor, "net amount")?);
    }

    // 3. Document discount on the net total, then distribute the discounted net over lines.
    let nt0 = round(sum(&net_exact, "net total")?, p, r);
    let discount_amount = match &input.discount {
        None => Decimal::ZERO,
        Some(DocDiscount::Percent { value }) => round(pct(nt0, *value, "discount")?, p, r),
        Some(DocDiscount::Amount { value }) => {
            let v = round(*value, p, r);
            if nt0.is_sign_negative() { -v } else { v }
        }
    };
    if discount_amount.abs() > nt0.abs() {
        return invalid("discount exceeds the net total");
    }
    let net_total = sub(nt0, discount_amount, "net total")?;
    let net = allocate(net_total, &net_exact, p, r)?;

    // 4. Taxes, row by row, each row allocated to lines so line breakups sum to the row amount.
    let mut alloc: Vec<Vec<Decimal>> = Vec::with_capacity(m);
    let mut tax_amounts: Vec<Decimal> = Vec::with_capacity(m);
    let mut row_weights: Vec<Vec<Decimal>> = Vec::with_capacity(m);
    for k in 0..m {
        let t = &taxes[k];
        let (row_total, weights) = match t.charge_type {
            ChargeType::Actual => (round(t.amount, p, r), net.clone()),
            ChargeType::OnNetTotal => {
                let w = (0..n).map(|i| pct(net[i], rate_of(i, k), "tax")).collect::<Result<Vec<_>, _>>()?;
                (round(sum(&w, "tax")?, p, r), w)
            }
            ChargeType::OnPreviousRowAmount => {
                let j = t.row_ref.unwrap_or(0);
                let w = (0..n).map(|i| pct(alloc[j][i], rate_of(i, k), "tax")).collect::<Result<Vec<_>, _>>()?;
                (round(sum(&w, "tax")?, p, r), w)
            }
            ChargeType::OnPreviousRowTotal => {
                let j = t.row_ref.unwrap_or(0);
                let mut w = Vec::with_capacity(n);
                for i in 0..n {
                    let mut base = net[i];
                    for q in 0..=j {
                        base = add(base, mul(sign(q), alloc[q][i], "tax")?, "tax")?;
                    }
                    w.push(pct(base, rate_of(i, k), "tax")?);
                }
                (round(sum(&w, "tax")?, p, r), w)
            }
        };
        alloc.push(allocate(row_total, &weights, p, r)?);
        tax_amounts.push(row_total);
        row_weights.push(weights);
    }

    // 5. Fully inclusive pricing keeps the grand total equal to what the customer saw.
    let all_inclusive = m > 0 && taxes.iter().all(|t| t.included_in_price) && discount_amount.is_zero();
    if all_inclusive {
        let signed: Vec<Decimal> = (0..m).map(|k| sign(k) * tax_amounts[k]).collect();
        let current = add(net_total, sum(&signed, "grand total")?, "grand total")?;
        let diff = sub(total, current, "grand total")?;
        if !diff.is_zero() {
            let last = m - 1;
            tax_amounts[last] = add(tax_amounts[last], diff, "inclusive adjustment")?;
            alloc[last] = allocate(tax_amounts[last], &row_weights[last], p, r)?;
        }
    }

    // 6. Running totals and grand total.
    let mut running = net_total;
    let mut totals = Vec::with_capacity(m);
    for k in 0..m {
        running = add(running, sign(k) * tax_amounts[k], "running total")?;
        totals.push(running);
    }
    let grand_total = running;
    let total_taxes = sub(grand_total, net_total, "total taxes")?;
    let rounded_total = match input.round_total_to {
        Some(step) => {
            let units = div(grand_total, step, "rounded total")?.round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero);
            round(mul(units, step, "rounded total")?, p, r)
        }
        None => grand_total,
    };
    let rounding_adjustment = sub(rounded_total, grand_total, "rounding adjustment")?;

    // 7. Base (company) currency, allocated so base breakups also reconcile.
    let cr = input.conversion_rate;
    let base_net_total = round(mul(net_total, cr, "base net total")?, bp, r);
    let base_net = allocate(base_net_total, &net, bp, r)?;
    let mut base_alloc = Vec::with_capacity(m);
    let mut base_tax = Vec::with_capacity(m);
    for k in 0..m {
        let bt = round(mul(tax_amounts[k], cr, "base tax")?, bp, r);
        base_alloc.push(allocate(bt, &alloc[k], bp, r)?);
        base_tax.push(bt);
    }
    let mut base_running = base_net_total;
    let mut base_totals = Vec::with_capacity(m);
    for k in 0..m {
        base_running = add(base_running, sign(k) * base_tax[k], "base running total")?;
        base_totals.push(base_running);
    }
    let base_grand_total = base_running;
    let base_rounded_total = if input.round_total_to.is_some() { round(mul(rounded_total, cr, "base rounded total")?, bp, r) } else { base_grand_total };

    let lines = (0..n)
        .map(|i| LineOut {
            key: input.lines[i].key.clone(),
            rate: rates[i],
            amount: amounts[i],
            net_amount: net[i],
            taxes: (0..m).map(|k| alloc[k][i]).collect(),
            base_net_amount: base_net[i],
            base_taxes: (0..m).map(|k| base_alloc[k][i]).collect(),
        })
        .collect();
    let taxes_out = (0..m)
        .map(|k| TaxOut {
            key: taxes[k].key.clone(),
            tax_amount: tax_amounts[k],
            deduct: taxes[k].deduct,
            total: totals[k],
            base_tax_amount: base_tax[k],
            base_total: base_totals[k],
        })
        .collect();
    Ok(CalcOutput {
        lines,
        taxes: taxes_out,
        total,
        discount_amount,
        net_total,
        total_taxes,
        grand_total,
        rounding_adjustment,
        rounded_total,
        base_net_total,
        base_total_taxes: sub(base_grand_total, base_net_total, "base total taxes")?,
        base_grand_total,
        base_rounding_adjustment: sub(base_rounded_total, base_grand_total, "base rounding")?,
        base_rounded_total,
    })
}

/// JSON in, JSON out. Errors are returned as `{"error": "..."}`.
pub fn compute_json(input: &str) -> String {
    let result = serde_json::from_str::<CalcInput>(input).map_err(|e| CalcError::Invalid(e.to_string())).and_then(|i| compute(&i));
    match result {
        Ok(out) => serde_json::to_string(&out).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}")),
        Err(e) => serde_json::json!({ "error": e.to_string() }).to_string(),
    }
}

#[cfg(feature = "wasm")]
mod wasm {
    use wasm_bindgen::prelude::*;

    /// Browser entry point: same engine, same results as the server.
    #[wasm_bindgen(js_name = computeTotals)]
    pub fn compute_totals(input: &str) -> String {
        super::compute_json(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rust_decimal_macros::dec;

    fn line(key: &str, qty: Decimal, price: Decimal) -> LineIn {
        LineIn { key: key.into(), qty, price, discount_percent: Decimal::ZERO, tax_rates: BTreeMap::new() }
    }
    fn tax(key: &str, ct: ChargeType, rate: Decimal) -> TaxIn {
        TaxIn { key: key.into(), charge_type: ct, rate, amount: Decimal::ZERO, row_ref: None, included_in_price: false, deduct: false }
    }
    fn input(lines: Vec<LineIn>, taxes: Vec<TaxIn>) -> CalcInput {
        CalcInput {
            precision: 2,
            base_precision: None,
            conversion_rate: Decimal::ONE,
            rounding: Rounding::HalfUp,
            lines,
            taxes,
            discount: None,
            round_total_to: None,
        }
    }

    #[test]
    fn gst_intra_state_exclusive() {
        let out = compute(&input(
            vec![line("a", dec!(3), dec!(33.33)), line("b", dec!(1), dec!(0.01))],
            vec![tax("cgst", ChargeType::OnNetTotal, dec!(9)), tax("sgst", ChargeType::OnNetTotal, dec!(9))],
        ))
        .unwrap();
        assert_eq!(out.net_total, dec!(100.00));
        assert_eq!(out.taxes[0].tax_amount, dec!(9.00));
        assert_eq!(out.taxes[1].tax_amount, dec!(9.00));
        assert_eq!(out.grand_total, dec!(118.00));
        assert_eq!(out.lines.iter().map(|l| l.taxes[0]).sum::<Decimal>(), dec!(9.00));
    }

    #[test]
    fn inclusive_keeps_entered_total() {
        let out = compute(&input(
            vec![line("a", dec!(1), dec!(10)), line("b", dec!(1), dec!(10)), line("c", dec!(1), dec!(10))],
            vec![TaxIn { included_in_price: true, ..tax("gst", ChargeType::OnNetTotal, dec!(18)) }],
        ))
        .unwrap();
        assert_eq!(out.grand_total, dec!(30.00));
        assert_eq!(out.net_total + out.taxes[0].tax_amount, dec!(30.00));
        assert_eq!(out.net_total, dec!(25.42));
        assert_eq!(out.lines.iter().map(|l| l.net_amount).sum::<Decimal>(), dec!(25.42));
    }

    #[test]
    fn cess_on_previous_row_and_tds_deduction() {
        let mut cess = tax("cess", ChargeType::OnPreviousRowAmount, dec!(10));
        cess.row_ref = Some(0);
        let mut tds = tax("tds", ChargeType::OnNetTotal, dec!(2));
        tds.deduct = true;
        let out = compute(&input(vec![line("a", dec!(1), dec!(1000))], vec![tax("gst", ChargeType::OnNetTotal, dec!(18)), cess, tds])).unwrap();
        assert_eq!(out.taxes[1].tax_amount, dec!(18.00));
        assert_eq!(out.taxes[2].tax_amount, dec!(20.00));
        assert_eq!(out.grand_total, dec!(1178.00));
    }

    #[test]
    fn actual_charge_distributed_by_net() {
        let mut freight = tax("freight", ChargeType::Actual, Decimal::ZERO);
        freight.amount = dec!(100);
        let out = compute(&input(vec![line("a", dec!(1), dec!(100)), line("b", dec!(1), dec!(200))], vec![freight])).unwrap();
        assert_eq!(out.lines[0].taxes[0], dec!(33.33));
        assert_eq!(out.lines[1].taxes[0], dec!(66.67));
        assert_eq!(out.grand_total, dec!(400.00));
    }

    #[test]
    fn item_override_exempts_line() {
        let mut exempt = line("b", dec!(1), dec!(100));
        exempt.tax_rates.insert("gst".into(), Decimal::ZERO);
        let out = compute(&input(vec![line("a", dec!(1), dec!(100)), exempt], vec![tax("gst", ChargeType::OnNetTotal, dec!(18))])).unwrap();
        assert_eq!(out.taxes[0].tax_amount, dec!(18.00));
        assert_eq!(out.lines[1].taxes[0], dec!(0.00));
    }

    #[test]
    fn discount_rounding_and_base_currency() {
        let mut i = input(vec![line("a", dec!(3), dec!(9.99))], vec![tax("vat", ChargeType::OnNetTotal, dec!(5))]);
        i.discount = Some(DocDiscount::Percent { value: dec!(10) });
        i.round_total_to = Some(dec!(1));
        i.conversion_rate = dec!(83.1234);
        let out = compute(&i).unwrap();
        assert_eq!(out.total, dec!(29.97));
        assert_eq!(out.discount_amount, dec!(3.00));
        assert_eq!(out.net_total, dec!(26.97));
        assert_eq!(out.grand_total, dec!(28.32));
        assert_eq!(out.rounded_total, dec!(28.00));
        assert_eq!(out.rounding_adjustment, dec!(-0.32));
        assert_eq!(out.base_rounded_total, dec!(2327.46));
    }

    #[test]
    fn credit_note_negative_quantities() {
        let out = compute(&input(vec![line("a", dec!(-2), dec!(50))], vec![tax("gst", ChargeType::OnNetTotal, dec!(18))])).unwrap();
        assert_eq!(out.grand_total, dec!(-118.00));
    }

    #[test]
    fn half_even_vs_half_up() {
        let mut i = input(vec![line("a", dec!(1), dec!(0.125))], vec![]);
        i.rounding = Rounding::HalfEven;
        assert_eq!(compute(&i).unwrap().grand_total, dec!(0.12));
        i.rounding = Rounding::HalfUp;
        assert_eq!(compute(&i).unwrap().grand_total, dec!(0.13));
    }

    #[test]
    fn rejects_bad_input() {
        let mut t = tax("x", ChargeType::OnPreviousRowAmount, dec!(5));
        t.row_ref = Some(0);
        assert!(compute(&input(vec![], vec![t])).is_err());
        let mut d = input(vec![line("a", dec!(1), dec!(10))], vec![]);
        d.discount = Some(DocDiscount::Amount { value: dec!(11) });
        assert!(compute(&d).is_err());
        assert!(compute_json("{not json").contains("error"));
    }

    #[test]
    fn empty_document_is_zero() {
        let out = compute(&input(vec![], vec![tax("gst", ChargeType::OnNetTotal, dec!(18))])).unwrap();
        assert_eq!(out.grand_total, dec!(0.00));
    }

    fn arb_dec(max: i64, scale: u32) -> impl Strategy<Value = Decimal> {
        (-max..=max).prop_map(move |v| Decimal::new(v, scale))
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]
        #[test]
        fn invariants_hold(
            lines in prop::collection::vec((arb_dec(10_000, 2), arb_dec(1_000_000, 3), 0i64..=50), 0..12),
            rates in prop::collection::vec((0i64..=3000, any::<bool>(), any::<bool>()), 0..5),
            disc in 0i64..=20,
            cr in 1i64..=10_000_000,
            prec in 0u32..=3,
        ) {
            let lines: Vec<LineIn> = lines.into_iter().enumerate().map(|(i, (q, p, d))| LineIn {
                key: i.to_string(), qty: q, price: p.abs(), discount_percent: Decimal::from(d), tax_rates: BTreeMap::new(),
            }).collect();
            let all_incl = rates.iter().all(|(_, inc, _)| *inc);
            let taxes: Vec<TaxIn> = rates.iter().enumerate().map(|(k, (rate, inc, ded))| {
                let incl = all_incl && *inc;
                TaxIn { key: format!("t{k}"), charge_type: ChargeType::OnNetTotal, rate: Decimal::new(*rate, 2), amount: Decimal::ZERO,
                        row_ref: None, included_in_price: incl, deduct: *ded && !incl }
            }).collect();
            let i = CalcInput { precision: prec, base_precision: Some(2), conversion_rate: Decimal::new(cr, 4), rounding: Rounding::HalfUp,
                lines, taxes, discount: if disc > 0 { Some(DocDiscount::Percent { value: Decimal::from(disc) }) } else { None }, round_total_to: None };
            let out = compute(&i).unwrap();
            prop_assert_eq!(out.lines.iter().map(|l| l.net_amount).sum::<Decimal>(), out.net_total);
            prop_assert_eq!(out.lines.iter().map(|l| l.base_net_amount).sum::<Decimal>(), out.base_net_total);
            for (k, t) in out.taxes.iter().enumerate() {
                prop_assert_eq!(out.lines.iter().map(|l| l.taxes[k]).sum::<Decimal>(), t.tax_amount);
                prop_assert_eq!(out.lines.iter().map(|l| l.base_taxes[k]).sum::<Decimal>(), t.base_tax_amount);
                prop_assert!(t.tax_amount.scale() <= prec);
            }
            let signed: Decimal = out.taxes.iter().map(|t| if t.deduct { -t.tax_amount } else { t.tax_amount }).sum();
            prop_assert_eq!(out.net_total + signed, out.grand_total);
            prop_assert!(out.net_total.scale() <= prec && out.grand_total.scale() <= prec);
            if !out.taxes.is_empty() && all_incl && disc == 0 {
                prop_assert_eq!(out.grand_total, out.total);
            }
        }

        #[test]
        fn allocation_is_exact(total in -1_000_000i64..1_000_000, weights in prop::collection::vec(-1000i64..1000, 1..20)) {
            let w: Vec<Decimal> = weights.into_iter().map(Decimal::from).collect();
            let t = Decimal::new(total, 2);
            let parts = allocate(t, &w, 2, Rounding::HalfUp).unwrap();
            prop_assert_eq!(parts.iter().copied().sum::<Decimal>(), t);
        }
    }
}
