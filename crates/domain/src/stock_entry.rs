//! Stock entries: receipts, issues, transfers, opening stock and (with the manufacturing package)
//! manufacture entries that consume materials and receive the output at its actual cost.

use crate::ledger::{self, Entry, Line};
use crate::stock::{self, MoveIn, Posted, parse_serials};
use crate::util::{Company, company, d, date, get, id, money, posting_ts, round, rows, s, set, sys};
use async_trait::async_trait;
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use std::collections::HashMap;
use uuid::Uuid;

const RECEIPT: &str = "Material Receipt";
const ISSUE: &str = "Material Issue";
const TRANSFER: &str = "Material Transfer";
const OPENING: &str = "Opening Stock";
pub const MANUFACTURE: &str = "Manufacture";

fn purpose(doc: &Doc) -> &str {
    s(doc, "purpose").unwrap_or(RECEIPT)
}

/// A manufacture line is an output when it only has a target warehouse.
fn is_output(r: &Doc) -> bool {
    id(r, "target_warehouse").is_some() && id(r, "source_warehouse").is_none()
}

async fn stock_account(hc: &mut HookCtx<'_>, co: &Company, warehouse: Uuid, cache: &mut HashMap<Uuid, Uuid>) -> Result<Uuid> {
    if let Some(a) = cache.get(&warehouse) {
        return Ok(*a);
    }
    let w = get(hc.kernel, hc.ctx, hc.tx, "Warehouse", warehouse).await?;
    let a = match id(&w, "account") {
        Some(a) => a,
        None => co.require("default_inventory_account", "default inventory account")?,
    };
    cache.insert(warehouse, a);
    Ok(a)
}

pub struct StockEntryHook;

#[async_trait]
impl DocHook for StockEntryHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let p = purpose(doc).to_string();
        let precision = match id(doc, "company") {
            Some(c) => company(hc.kernel, hc.ctx, hc.tx, c).await?.precision,
            None => 2,
        };
        if p == MANUFACTURE && !hc.ctx.schema.packages.iter().any(|x| x == "manufacturing") {
            return Err(Error::field("purpose", "manufacture entries need the manufacturing package"));
        }
        let mut lines = rows(doc, "items");
        let mut total = Decimal::ZERO;
        for r in lines.iter_mut() {
            if p == MANUFACTURE {
                set(r, "mfg_is_finished_item", is_output(r));
            } else {
                r.remove("mfg_is_finished_item");
            }
            if matches!(p.as_str(), RECEIPT | OPENING) && r.get("basic_rate").is_some_and(|v| !v.is_null()) {
                let amount = round(d(r, "qty") * d(r, "basic_rate"), precision);
                set(r, "amount", money(amount));
            }
            total += d(r, "amount");
        }
        set(doc, "items", serde_json::Value::Array(lines.into_iter().map(serde_json::Value::Object).collect()));
        if d(doc, "docstatus").is_zero() {
            set(doc, "total_amount", money(total));
        }
        Ok(())
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let p = purpose(doc);
        let mut errors = Vec::new();
        let lines = rows(doc, "items");
        for (i, r) in lines.iter().enumerate() {
            let (src, tgt) = (id(r, "source_warehouse"), id(r, "target_warehouse"));
            let mut need = |field: &str, ok: bool, msg: &str| {
                if !ok {
                    errors.push(FieldError::new(field, msg).in_row("items", i));
                }
            };
            match p {
                RECEIPT | OPENING => {
                    need("target_warehouse", tgt.is_some(), "is required for receipts");
                    need("source_warehouse", src.is_none(), "must be empty for receipts");
                    need("basic_rate", r.get("basic_rate").is_some_and(|v| !v.is_null()), "is required for receipts");
                }
                ISSUE => {
                    need("source_warehouse", src.is_some(), "is required for issues");
                    need("target_warehouse", tgt.is_none(), "must be empty for issues");
                }
                TRANSFER => {
                    need("source_warehouse", src.is_some(), "is required for transfers");
                    need("target_warehouse", tgt.is_some(), "is required for transfers");
                    need("target_warehouse", src.is_none() || src != tgt, "must differ from the source warehouse");
                }
                MANUFACTURE => {
                    need("source_warehouse", src.is_some() != tgt.is_some(), "set either a source (consumed) or a target (produced) warehouse");
                }
                other => errors.push(FieldError::new("purpose", format!("unknown purpose {other}"))),
            }
        }
        if p == MANUFACTURE {
            if !lines.iter().any(is_output) {
                errors.push(FieldError::new("items", "a manufacture entry needs at least one produced line (target warehouse only)"));
            }
            if !lines.iter().any(|r| id(r, "source_warehouse").is_some()) {
                errors.push(FieldError::new("items", "a manufacture entry needs at least one consumed line (source warehouse only)"));
            }
            if id(doc, "mfg_work_order").is_none() {
                errors.push(FieldError::new("mfg_work_order", "manufacture entries are created from a work order"));
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let eid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let p = purpose(doc).to_string();
        let on = date(doc, "posting_date")?;
        let ts = posting_ts(doc)?;
        let lines = rows(doc, "items");

        let mut keys = Vec::new();
        for r in &lines {
            let item = id(r, "item").ok_or_else(|| Error::field("items", "a line has no item"))?;
            for w in [id(r, "source_warehouse"), id(r, "target_warehouse")].into_iter().flatten() {
                keys.push((item, w));
            }
        }
        stock::lock_balances(hc.tx, hc.ctx.tenant, co.id, &keys).await?;

        // Issues first: transfers and manufacture value their receipts from what was issued.
        let issues: Vec<MoveIn> = lines
            .iter()
            .filter_map(|r| {
                id(r, "source_warehouse").map(|w| MoveIn {
                    item: id(r, "item").unwrap_or_default(),
                    warehouse: w,
                    qty: -d(r, "qty"),
                    batch_no: s(r, "batch_no").map(str::to_string),
                    serials: parse_serials(s(r, "serial_no")),
                    source_line: id(r, "id"),
                    ..Default::default()
                })
            })
            .collect();
        let issued = stock::post_moves(hc.tx, hc.ctx, &co, "StockEntry", eid, on, ts, issues, "consumed").await?;
        let issued_value: Decimal = issued.iter().map(|m| -m.value).sum();
        let issued_by_line: HashMap<Uuid, Decimal> = issued.iter().filter_map(|m| m.source_line.map(|l| (l, -m.value))).collect();

        let outputs: Vec<&Doc> = lines.iter().filter(|r| id(r, "target_warehouse").is_some()).collect();
        let additional = if p == MANUFACTURE { d(doc, "mfg_additional_cost") } else { Decimal::ZERO };
        // Manufacture: the output carries consumed value + additional cost, split by quantity.
        let shares = if p == MANUFACTURE {
            let weights: Vec<Decimal> = outputs.iter().map(|r| d(r, "qty")).collect();
            ferrum_calc::allocate(round(issued_value + additional, co.precision), &weights, co.precision, co.rounding)
                .map_err(|e| Error::invalid(e.to_string()))?
        } else {
            vec![]
        };
        let receipts: Vec<MoveIn> = outputs
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let line = id(r, "id");
                let (rate, exact) = match p.as_str() {
                    TRANSFER => (None, line.and_then(|l| issued_by_line.get(&l).copied())),
                    MANUFACTURE => (None, shares.get(i).copied()),
                    _ => (Some(d(r, "basic_rate")), None),
                };
                MoveIn {
                    item: id(r, "item").unwrap_or_default(),
                    warehouse: id(r, "target_warehouse").unwrap_or_default(),
                    qty: d(r, "qty"),
                    rate: rate.or(Some(Decimal::ZERO)),
                    exact_value: exact,
                    batch_no: s(r, "batch_no").map(str::to_string),
                    serials: parse_serials(s(r, "serial_no")),
                    source_line: line,
                    ..Default::default()
                }
            })
            .collect();
        let received = stock::post_moves(hc.tx, hc.ctx, &co, "StockEntry", eid, on, ts, receipts, "consumed").await?;

        // Ledger: every move hits its warehouse's stock account; the purpose decides the offset.
        let mut entry = Entry::new(co.id, on, "StockEntry", eid, s(doc, "number").map(str::to_string));
        entry.memo = s(doc, "remarks").map(str::to_string);
        let mut accounts = HashMap::new();
        let line_by_id: HashMap<Uuid, &Doc> = lines.iter().filter_map(|r| id(r, "id").map(|i| (i, r))).collect();
        let all: Vec<&Posted> = issued.iter().chain(received.iter()).collect();
        for m in &all {
            let acc = stock_account(hc, &co, m.warehouse, &mut accounts).await?;
            let row = m.source_line.and_then(|l| line_by_id.get(&l).copied());
            let cc = row.and_then(|r| id(r, "cost_center")).or(co.account("default_cost_center"));
            entry.lines.push(Line::new(acc, m.value, &co.currency_code));
            let offset = match p.as_str() {
                RECEIPT | ISSUE => match row.and_then(|r| id(r, "expense_account")) {
                    Some(a) => Some(a),
                    None => Some(co.require("stock_adjustment_account", "stock adjustment account")?),
                },
                OPENING => Some(match co.account("opening_balance_account") {
                    Some(a) => a,
                    None => co.require("stock_adjustment_account", "stock adjustment account")?,
                }),
                _ => None,
            };
            if let Some(o) = offset {
                entry.lines.push(Line::new(o, -m.value, &co.currency_code).cost_center(cc));
            }
        }
        if p == MANUFACTURE && additional > Decimal::ZERO {
            let absorbed = match co.account("overhead_absorbed_account") {
                Some(a) => a,
                None => co.require("stock_adjustment_account", "stock adjustment account")?,
            };
            entry
                .lines
                .push(Line::new(absorbed, -additional, &co.currency_code).cost_center(co.account("default_cost_center")).remarks("Operating cost absorbed"));
        }
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;

        // Write the actual rates back to the lines (a transfer line takes its receipt side).
        let sctx = sys(hc.ctx);
        let mut by_line: HashMap<Uuid, &Posted> = HashMap::new();
        for m in issued.iter().chain(received.iter()) {
            if let Some(l) = m.source_line {
                by_line.insert(l, m);
            }
        }
        for (line, m) in by_line {
            let mut v = Doc::new();
            v.insert("basic_rate".into(), money(m.rate.round_dp(9)));
            v.insert("amount".into(), money(m.value.abs()));
            hc.kernel.set_child_values_tx(&sctx, hc.tx, "StockEntry", "items", line, v).await?;
        }
        let total: Decimal = if p == ISSUE { issued_value } else { received.iter().map(|m| m.value).sum() };
        let mut v = Doc::new();
        v.insert("total_amount".into(), money(total));
        hc.kernel.set_values_tx(&sctx, hc.tx, "StockEntry", eid, v).await?;

        if p == MANUFACTURE {
            let wo = id(doc, "mfg_work_order").ok_or_else(|| Error::field("mfg_work_order", "is required"))?;
            let produced: Vec<(Uuid, Decimal, Decimal)> = received.iter().map(|m| (m.item, m.qty, m.value)).collect();
            crate::manufacturing::record_production(hc, wo, &produced, Decimal::ONE).await?;
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let eid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let (on, ts) = ledger::reversal_point(hc.tx, hc.ctx.tenant, co.id, doc).await?;
        let keys: Vec<(Uuid, Uuid)> =
            sqlx::query_as("select item_id, warehouse_id from stock_move where tenant_id = $1 and source_entity = 'StockEntry' and source_id = $2")
                .bind(hc.ctx.tenant)
                .bind(eid)
                .fetch_all(&mut **hc.tx)
                .await?;
        stock::lock_balances(hc.tx, hc.ctx.tenant, co.id, &keys).await?;
        let reversed = stock::reverse_moves(hc.tx, hc.ctx, &co, "StockEntry", eid, on, ts).await?;
        ledger::reverse(hc.tx, hc.ctx, "StockEntry", eid, on, co.precision).await?;
        if purpose(doc) == MANUFACTURE
            && let Some(wo) = id(doc, "mfg_work_order")
        {
            // Reversed outputs are the negative moves of the reversal.
            let produced: Vec<(Uuid, Decimal, Decimal)> = reversed.iter().filter(|m| m.qty < Decimal::ZERO).map(|m| (m.item, -m.qty, -m.value)).collect();
            crate::manufacturing::record_production(hc, wo, &produced, -Decimal::ONE).await?;
        }
        Ok(())
    }
}
