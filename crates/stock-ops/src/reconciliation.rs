//! Stock reconciliation: counted quantities (and optionally valuation rates) per item, warehouse
//! and batch or serial set. On submit the differences post through the inventory engine against
//! the difference account; cancelling appends the engine's exact reversals.
//!
//! Without a valuation rate only the quantity changes: gains come in at the current valuation
//! rate and losses leave the way the item is valued (moving average or FIFO). With a rate the
//! counted stock is reset to `qty × rate`: the current stock leaves at its book value and the count
//! comes back at the given rate (one fresh FIFO layer), as ERPNext's reconciliation does.

use crate::common::{ItemInfo, batch_qty, items, opt_dec, row_error, serials, serials_in_stock, validation, warehouse_companies};
use async_trait::async_trait;
use ferrum_domain::ledger::{self, Entry, Line};
use ferrum_domain::stock::{self, MoveIn};
use ferrum_domain::util::{company, d, date, get, id, money, posting_ts, round, rows, s, set, sys};
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use uuid::Uuid;

pub const ENTITY: &str = "StockReconciliation";
const OPENING: &str = "Opening Stock";

/// The book position of one reconciliation line before it posts.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Current {
    pub qty: Decimal,
    /// Book value of the line's stock (the warehouse's for items without batches or serials).
    pub value: Decimal,
    /// The warehouse's valuation rate (the engine values batches and serials at it too).
    pub rate: Decimal,
    pub serials: Vec<String>,
}

/// The moves that take a line from `cur` to the counted state: `(issues, receipts)`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan(
    info: &ItemInfo,
    wh: Uuid,
    batch: Option<&str>,
    line: Option<Uuid>,
    cur: &Current,
    counted: Decimal,
    counted_serials: &[String],
    rate: Option<Decimal>,
    precision: u32,
) -> std::result::Result<(Vec<MoveIn>, Vec<MoveIn>), String> {
    let mv = |qty: Decimal, rate: Option<Decimal>, serials: Vec<String>, exact: Option<Decimal>| MoveIn {
        item: info.id,
        warehouse: wh,
        qty,
        rate,
        exact_value: exact,
        batch_no: batch.map(str::to_string),
        serials,
        source_line: line,
        ..Default::default()
    };
    let no_rate = || format!("{} has no valuation rate in this warehouse yet: give the valuation rate of the counted stock", info.code);
    let (mut out, mut inn) = (Vec::new(), Vec::new());
    if info.serial {
        let now: BTreeSet<&String> = cur.serials.iter().collect();
        let counted_set: BTreeSet<&String> = counted_serials.iter().collect();
        let n = |v: &Vec<String>| Decimal::from(v.len());
        match rate {
            None => {
                let gone: Vec<String> = cur.serials.iter().filter(|x| !counted_set.contains(x)).cloned().collect();
                let found: Vec<String> = counted_serials.iter().filter(|x| !now.contains(x)).cloned().collect();
                if !gone.is_empty() {
                    out.push(mv(-n(&gone), None, gone, None));
                }
                if !found.is_empty() {
                    if cur.rate <= Decimal::ZERO {
                        return Err(no_rate());
                    }
                    inn.push(mv(n(&found), Some(cur.rate), found, None));
                }
            }
            Some(r) => {
                if now != counted_set || r != cur.rate {
                    if !cur.serials.is_empty() {
                        out.push(mv(-n(&cur.serials), None, cur.serials.clone(), None));
                    }
                    if !counted_serials.is_empty() {
                        inn.push(mv(n(&counted_serials.to_vec()), Some(r), counted_serials.to_vec(), None));
                    }
                }
            }
        }
        return Ok((out, inn));
    }
    match rate {
        None => {
            let diff = counted - cur.qty;
            if diff > Decimal::ZERO {
                if cur.rate <= Decimal::ZERO {
                    return Err(no_rate());
                }
                inn.push(mv(diff, Some(cur.rate), vec![], None));
            } else if diff < Decimal::ZERO {
                out.push(mv(diff, None, vec![], None));
            }
        }
        Some(r) => {
            let target = round(counted * r, precision);
            let unchanged = cur.qty == counted && if batch.is_some() { r == cur.rate } else { cur.value == target };
            if unchanged {
                return Ok((out, inn));
            }
            if cur.qty >= Decimal::ZERO {
                if cur.qty > Decimal::ZERO {
                    out.push(mv(-cur.qty, None, vec![], None));
                }
                if counted > Decimal::ZERO {
                    inn.push(mv(counted, Some(r), vec![], None));
                }
            } else if batch.is_some() {
                return Err(format!("batch {} of {} is below zero; count it without a valuation rate first", batch.unwrap_or_default(), info.code));
            } else {
                // Negative stock (allowed by the company): one receipt brings it to the count at the rate.
                let value = target - cur.value;
                if value < Decimal::ZERO {
                    return Err(format!("{} cannot be revalued to {} at this rate from its negative balance", info.code, counted.normalize()));
                }
                inn.push(mv(counted - cur.qty, Some(r), vec![], Some(value)));
            }
        }
    }
    Ok((out, inn))
}

/// The line's current position: the warehouse balance, or its batch / serial share of it.
async fn current(hc: &mut HookCtx<'_>, info: &ItemInfo, wh: Uuid, batch: Option<&str>, bal: (Decimal, Decimal, Decimal), p: u32) -> Result<Current> {
    let (q, v, r) = bal;
    if info.serial {
        let found = serials_in_stock(hc.tx, hc.ctx.tenant, info.id, wh, batch).await?;
        let n = Decimal::from(found.len());
        let value = if n == q { v } else { round(n * r, p) };
        return Ok(Current { qty: n, value, rate: r, serials: found.into_iter().map(|(s, _)| s).collect() });
    }
    if let Some(b) = batch {
        let n = batch_qty(hc.tx, hc.ctx.tenant, info.id, wh, b).await?;
        let value = if n == q { v } else { round(n * r, p) };
        return Ok(Current { qty: n, value, rate: r, serials: vec![] });
    }
    Ok(Current { qty: q, value: v, rate: r, serials: vec![] })
}

fn batch_of(r: &Doc) -> Option<&str> {
    s(r, "batch_no").map(str::trim).filter(|b| !b.is_empty())
}

/// The read-only figures of a line: where it stood and what changes.
fn figures(r: &mut Doc, cur: &Current, p: u32, posted: Option<Decimal>) {
    let counted = d(r, "qty");
    let rate = opt_dec(r, "valuation_rate").unwrap_or(cur.rate);
    let amount = round(counted * rate, p);
    set(r, "current_qty", money(cur.qty));
    set(r, "current_valuation_rate", money(cur.rate.round_dp(9)));
    set(r, "current_amount", money(cur.value));
    set(r, "current_serial_no", if cur.serials.is_empty() { Value::Null } else { json!(cur.serials.join("\n")) });
    set(r, "amount", money(amount));
    set(r, "quantity_difference", money(counted - cur.qty));
    set(r, "amount_difference", money(posted.unwrap_or(amount - cur.value)));
}

pub struct ReconciliationHook;

#[async_trait]
impl DocHook for ReconciliationHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        if id(doc, "expense_account").is_none() {
            let account = if s(doc, "purpose") == Some(OPENING) {
                co.account("opening_balance_account").or(co.account("stock_adjustment_account"))
            } else {
                co.account("stock_adjustment_account")
            };
            set(doc, "expense_account", account.map(|a| json!(a.to_string())).unwrap_or(Value::Null));
        }
        if id(doc, "cost_center").is_none()
            && let Some(cc) = co.account("default_cost_center")
        {
            set(doc, "cost_center", cc.to_string());
        }
        if !d(doc, "docstatus").is_zero() {
            return Ok(());
        }
        // Draft: show where each line stands now and what it would change.
        let mut lines = rows(doc, "items");
        let ids: Vec<Uuid> = lines.iter().filter_map(|r| id(r, "item")).collect();
        let infos = items(hc.tx, hc.ctx.tenant, &ids).await?;
        let mut total = Decimal::ZERO;
        for r in lines.iter_mut() {
            let (Some(info), Some(wh)) = (id(r, "item").and_then(|i| infos.get(&i)), id(r, "warehouse")) else { continue };
            let bal = crate::common::balance(hc.tx, hc.ctx.tenant, info.id, wh).await?;
            let batch = batch_of(r).map(str::to_string);
            let cur = current(hc, info, wh, batch.as_deref(), bal, co.precision).await?;
            figures(r, &cur, co.precision, None);
            total += d(r, "amount_difference");
        }
        set(doc, "items", crate::common::rows_value(lines));
        set(doc, "difference_amount", money(total));
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let lines = rows(doc, "items");
        let ids: Vec<Uuid> = lines.iter().filter_map(|r| id(r, "item")).collect();
        let infos = items(hc.tx, hc.ctx.tenant, &ids).await?;
        let whs: Vec<Uuid> = lines.iter().filter_map(|r| id(r, "warehouse")).collect();
        let wh_company = warehouse_companies(hc.tx, hc.ctx.tenant, &whs).await?;
        let cid = id(doc, "company");
        let mut errors = Vec::new();
        let mut seen = HashSet::new();
        for (i, r) in lines.iter().enumerate() {
            let mut err = |f: &str, m: String| errors.push(FieldError::new(f, m).in_row("items", i));
            let Some(info) = id(r, "item").and_then(|x| infos.get(&x)) else { continue };
            if !info.is_stock {
                err("item", format!("{} is not a stock item", info.code));
            }
            if let Some(wh) = id(r, "warehouse")
                && wh_company.get(&wh).map(|w| w.0) != cid
            {
                err("warehouse", "belongs to another company".into());
            }
            let batch = batch_of(r);
            match (info.batch, batch) {
                (true, None) => err("batch_no", format!("{} is batch-tracked: count each batch on its own line", info.code)),
                (false, Some(_)) => err("batch_no", format!("{} is not batch-tracked", info.code)),
                _ => {}
            }
            let counted = serials(s(r, "serial_no"));
            if info.serial {
                let unique: HashSet<&String> = counted.iter().collect();
                if unique.len() != counted.len() {
                    err("serial_no", "a serial number is listed twice".into());
                }
                if Decimal::from(counted.len()) != d(r, "qty") {
                    err("serial_no", format!("{} is serialized: list exactly {} serial number(s)", info.code, d(r, "qty").normalize()));
                }
            } else if !counted.is_empty() {
                err("serial_no", format!("{} has no serial numbers", info.code));
            }
            if !seen.insert((info.id, id(r, "warehouse"), batch.map(str::to_string))) {
                err("item", format!("{} is counted twice for this warehouse{}", info.code, if batch.is_some() { " and batch" } else { "" }));
            }
        }
        if let Some(acc) = id(doc, "expense_account") {
            let a = get(hc.kernel, hc.ctx, hc.tx, "Account", acc).await?;
            if id(&a, "company") != cid {
                errors.push(FieldError::new("expense_account", "belongs to another company"));
            }
            let opening = s(doc, "purpose") == Some(OPENING);
            if opening && !matches!(s(&a, "root_type"), Some("Asset" | "Liability" | "Equity")) {
                errors.push(FieldError::new("expense_account", "opening stock needs a balance sheet account (e.g. Opening Balance Equity)"));
            }
            if !opening && s(&a, "account_type") == Some("Temporary") {
                errors.push(FieldError::new("expense_account", "an opening account can only be used for opening stock"));
            }
        } else {
            errors.push(FieldError::new("expense_account", "set the company's stock adjustment account or choose a difference account"));
        }
        validation(errors)
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let rid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let (on, ts) = (date(doc, "posting_date")?, posting_ts(doc)?);
        let lines = rows(doc, "items");
        let ids: Vec<Uuid> = lines.iter().filter_map(|r| id(r, "item")).collect();
        let infos = items(hc.tx, hc.ctx.tenant, &ids).await?;
        let keys: Vec<(Uuid, Uuid)> = lines.iter().filter_map(|r| Some((id(r, "item")?, id(r, "warehouse")?))).collect();
        // The counted state is compared with the balances as they are under the lock, not at save time.
        let bal = stock::lock_balances(hc.tx, hc.ctx.tenant, co.id, &keys).await?;
        let (mut issues, mut receipts) = (Vec::new(), Vec::new());
        let mut currents: HashMap<Uuid, Current> = HashMap::new();
        for (i, r) in lines.iter().enumerate() {
            let (Some(item), Some(wh), Some(line)) = (id(r, "item"), id(r, "warehouse"), id(r, "id")) else { continue };
            let info = infos.get(&item).ok_or_else(|| row_error("items", i, "item", "not found"))?;
            let batch = batch_of(r).map(str::to_string);
            let cur = current(hc, info, wh, batch.as_deref(), bal.get(&(item, wh)).copied().unwrap_or_default(), co.precision).await?;
            let counted_serials = serials(s(r, "serial_no"));
            let (out, inn) = plan(info, wh, batch.as_deref(), Some(line), &cur, d(r, "qty"), &counted_serials, opt_dec(r, "valuation_rate"), co.precision)
                .map_err(|m| row_error("items", i, "qty", m))?;
            issues.extend(out);
            receipts.extend(inn);
            currents.insert(line, cur);
        }
        let mut posted = stock::post_moves(hc.tx, hc.ctx, &co, ENTITY, rid, on, ts, issues, "consumed").await?;
        posted.extend(stock::post_moves(hc.tx, hc.ctx, &co, ENTITY, rid, on, ts, receipts, "consumed").await?);

        let offset = id(doc, "expense_account").ok_or_else(|| Error::field("expense_account", "is required"))?;
        let cc = id(doc, "cost_center").or(co.account("default_cost_center"));
        let mut entry = Entry::new(co.id, on, ENTITY, rid, s(doc, "number").map(str::to_string));
        entry.memo = s(doc, "remarks").map(str::to_string);
        let stock_side = crate::common::stock_lines(hc.tx, hc.ctx.tenant, &co, &posted).await?;
        for (line, p) in stock_side.into_iter().zip(&posted) {
            entry.lines.push(line);
            entry.lines.push(Line::new(offset, -p.value, &co.currency_code).cost_center(cc).remarks(if s(doc, "purpose") == Some(OPENING) {
                "Opening stock"
            } else {
                "Stock reconciliation"
            }));
        }
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;

        // Record what the count was compared with and what it changed.
        let sctx = sys(hc.ctx);
        let mut by_line: HashMap<Uuid, Decimal> = HashMap::new();
        for p in &posted {
            if let Some(l) = p.source_line {
                *by_line.entry(l).or_default() += p.value;
            }
        }
        let mut total = Decimal::ZERO;
        for mut r in lines {
            let Some(line) = id(&r, "id") else { continue };
            let Some(cur) = currents.get(&line) else { continue };
            let change = by_line.get(&line).copied().unwrap_or_default();
            total += change;
            figures(&mut r, cur, co.precision, Some(change));
            let mut v = Doc::new();
            for k in ["current_qty", "current_valuation_rate", "current_amount", "current_serial_no", "amount", "quantity_difference", "amount_difference"] {
                v.insert(k.into(), r.get(k).cloned().unwrap_or(Value::Null));
            }
            hc.kernel.set_child_values_tx(&sctx, hc.tx, ENTITY, "items", line, v).await?;
        }
        let mut v = Doc::new();
        v.insert("difference_amount".into(), money(total));
        hc.kernel.set_values_tx(&sctx, hc.tx, ENTITY, rid, v).await
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let rid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let (on, ts) = ledger::reversal_point(hc.tx, hc.ctx.tenant, co.id, doc).await?;
        let keys: Vec<(Uuid, Uuid)> =
            sqlx::query_as("select item_id, warehouse_id from stock_move where tenant_id = $1 and source_entity = $2 and source_id = $3")
                .bind(hc.ctx.tenant)
                .bind(ENTITY)
                .bind(rid)
                .fetch_all(&mut **hc.tx)
                .await?;
        stock::lock_balances(hc.tx, hc.ctx.tenant, co.id, &keys).await?;
        stock::reverse_moves(hc.tx, hc.ctx, &co, ENTITY, rid, on, ts).await?;
        ledger::reverse(hc.tx, hc.ctx, ENTITY, rid, on, co.precision).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn info(serial: bool, batch: bool) -> ItemInfo {
        ItemInfo { id: Uuid::nil(), code: "X".into(), name: String::new(), is_stock: true, fifo: false, batch, serial, template: false, stock_uom: None }
    }

    fn qtys(m: &[MoveIn]) -> Vec<Decimal> {
        m.iter().map(|x| x.qty).collect()
    }

    #[test]
    fn quantity_only_counts_move_the_difference() {
        let cur = Current { qty: n("10"), value: n("100"), rate: n("10"), serials: vec![] };
        let wh = Uuid::nil();
        let (out, inn) = plan(&info(false, false), wh, None, None, &cur, n("12"), &[], None, 2).unwrap();
        assert!(out.is_empty());
        assert_eq!((qtys(&inn), inn[0].rate), (vec![n("2")], Some(n("10"))));
        let (out, inn) = plan(&info(false, false), wh, None, None, &cur, n("7"), &[], None, 2).unwrap();
        assert_eq!((qtys(&out), inn.len()), (vec![n("-3")], 0));
        let (out, inn) = plan(&info(false, false), wh, None, None, &cur, n("10"), &[], None, 2).unwrap();
        assert!(out.is_empty() && inn.is_empty());
        let empty = Current::default();
        assert!(plan(&info(false, false), wh, None, None, &empty, n("1"), &[], None, 2).is_err(), "no rate to value a gain");
    }

    #[test]
    fn a_rate_resets_the_line() {
        let cur = Current { qty: n("10"), value: n("100"), rate: n("10"), serials: vec![] };
        let wh = Uuid::nil();
        let (out, inn) = plan(&info(false, false), wh, None, None, &cur, n("12"), &[], Some(n("11")), 2).unwrap();
        assert_eq!((qtys(&out), qtys(&inn), inn[0].rate), (vec![n("-10")], vec![n("12")], Some(n("11"))));
        let (out, inn) = plan(&info(false, false), wh, None, None, &cur, n("10"), &[], Some(n("10")), 2).unwrap();
        assert!(out.is_empty() && inn.is_empty(), "already at the target");
        let (out, inn) = plan(&info(false, false), wh, None, None, &cur, n("0"), &[], Some(n("5")), 2).unwrap();
        assert_eq!((qtys(&out), inn.len()), (vec![n("-10")], 0), "counting zero empties the line");
        let negative = Current { qty: n("-2"), value: n("-20"), rate: n("10"), serials: vec![] };
        let (out, inn) = plan(&info(false, false), wh, None, None, &negative, n("5"), &[], Some(n("10")), 2).unwrap();
        assert!(out.is_empty());
        assert_eq!((qtys(&inn), inn[0].exact_value), (vec![n("7")], Some(n("70"))), "from -2 (-20) to 5 x 10");
    }

    #[test]
    fn serial_counts_move_the_serials_that_differ() {
        let cur = Current { qty: n("3"), value: n("300"), rate: n("100"), serials: vec!["A".into(), "B".into(), "C".into()] };
        let counted: Vec<String> = vec!["A".into(), "B".into(), "D".into()];
        let (out, inn) = plan(&info(true, false), Uuid::nil(), None, None, &cur, n("3"), &counted, None, 2).unwrap();
        assert_eq!((out[0].serials.clone(), out[0].qty), (vec!["C".to_string()], n("-1")));
        assert_eq!((inn[0].serials.clone(), inn[0].qty, inn[0].rate), (vec!["D".to_string()], n("1"), Some(n("100"))));
        let (out, inn) = plan(&info(true, false), Uuid::nil(), None, None, &cur, n("3"), &counted, Some(n("90")), 2).unwrap();
        assert_eq!((out[0].serials.len(), inn[0].serials.len()), (3, 3), "a rate re-receives every counted serial");
    }

    #[test]
    fn batch_lines_stay_in_their_batch() {
        let cur = Current { qty: n("5"), value: n("100"), rate: n("20"), serials: vec![] };
        let (out, _) = plan(&info(false, true), Uuid::nil(), Some("B1"), None, &cur, n("4"), &[], None, 2).unwrap();
        assert_eq!((out[0].qty, out[0].batch_no.as_deref()), (n("-1"), Some("B1")));
    }
}
