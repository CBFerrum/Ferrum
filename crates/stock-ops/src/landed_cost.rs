//! Landed cost vouchers: freight, duty and other charges added to the cost of received stock.
//!
//! The inventory engine values each move once and never reposts, so a landed cost posts forward
//! (the SAP/Oracle way). For every receipt line the voucher works out how much of the received
//! quantity is still in the receiving warehouse, exactly as the engine values it:
//! * FIFO: the units left in the receipt's own cost layer;
//! * moving average: the share of the added value a full repost would leave in the warehouse
//!   (each later issue takes its pro-rata part, a cancelled issue gives it back).
//!
//! That share is capitalized by revaluing the stock through the engine (the stock leaves and comes
//! back with the extra value; FIFO layers are re-created in order with the charge on the receipt's
//! units), and the rest is booked to the consumed cost account (cost of goods sold unless the
//! voucher names another). Value that left the warehouse by transfer is treated as consumed: it is
//! not chased into the target warehouse, which would be the reposting cascade the engine avoids.
//!
//! Cancelling is the same correction in reverse for whatever of the capitalized cost is still in
//! stock then; the part that has been sold since comes back out of the consumed cost account, so the
//! charges, the stock accounts and cost of goods sold end exactly where they would be without the
//! voucher.

use crate::common::{
    ItemInfo, allocate, balance, batch_quantities, child_table, finish_rows, installed, items, pair_up, rows_value, serials_in_stock, stock_lines, validation,
};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use ferrum_domain::ledger::{self, Entry, Line};
use ferrum_domain::stock::{self, MoveIn, Posted};
use ferrum_domain::util::{Company, b, company, d, date, get, id, money, posting_ts, round, rows, s, set, sys};
use ferrum_kernel::ddl::q;
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use uuid::Uuid;

pub const ENTITY: &str = "LandedCostVoucher";
const MANUAL: &str = "Distribute Manually";

fn receipt_entity(kind: &str) -> Option<&'static str> {
    match kind {
        "Purchase Receipt" => Some("PurchaseReceipt"),
        "Purchase Invoice" => Some("PurchaseInvoice"),
        _ => None,
    }
}

fn receipt_field(entity: &str) -> &'static str {
    if entity == "PurchaseInvoice" { "purchase_invoice" } else { "purchase_receipt" }
}

/// `(entity, link field, document id)` of a receipt or item row.
fn receipt_of(r: &Doc) -> Option<(&'static str, &'static str, Uuid)> {
    let entity = receipt_entity(s(r, "receipt_type").unwrap_or("Purchase Receipt"))?;
    let field = receipt_field(entity);
    Some((entity, field, id(r, field)?))
}

/// The stock move a receipt line posted.
#[derive(Debug, Clone, sqlx::FromRow)]
struct ReceiptMove {
    source_line_id: Uuid,
    item_id: Uuid,
    warehouse_id: Uuid,
    id: Uuid,
    seq: i64,
    qty: Decimal,
    value: Decimal,
}

async fn receipt_moves(hc: &mut HookCtx<'_>, entity: &str, doc: Uuid) -> Result<Vec<ReceiptMove>> {
    Ok(sqlx::query_as(
        "select m.source_line_id, m.item_id, m.warehouse_id, m.id, m.seq, m.qty, m.value from stock_move m \
         where m.tenant_id = $1 and m.source_entity = $2 and m.source_id = $3 and m.qty > 0 and m.reversal_of is null \
         and m.source_line_id is not null and not exists (select 1 from stock_move r where r.tenant_id = m.tenant_id and r.reversal_of = m.id) \
         order by m.seq",
    )
    .bind(hc.ctx.tenant)
    .bind(entity)
    .bind(doc)
    .fetch_all(&mut **hc.tx)
    .await?)
}

/// A later move of the warehouse, as the moving-average share walk needs it.
#[derive(Debug, Clone)]
pub(crate) struct WalkMove {
    pub id: Uuid,
    pub seq: i64,
    pub qty: Decimal,
    pub qty_after: Decimal,
    pub reversal_of: Option<Uuid>,
    /// A landed-cost revaluation: stock leaves and returns unchanged in quantity.
    pub revaluation: bool,
}

/// Moving average: how much of `initial` (value added to the warehouse at some point) is still in
/// it after `moves`, as a repost would compute it. Each issue takes its pro-rata share of what is
/// left; a reversed issue gives its share back; receipts take nothing (the value is pooled).
pub(crate) fn remaining(initial: Decimal, moves: &[WalkMove]) -> Decimal {
    let mut left = initial;
    let mut taken: HashMap<Uuid, Decimal> = HashMap::new();
    for m in moves {
        if m.revaluation {
            continue;
        }
        if m.qty < Decimal::ZERO {
            let before = m.qty_after - m.qty;
            let share = if before <= Decimal::ZERO { left } else { (left * (-m.qty / before)).round_dp(16).min(left) };
            left -= share;
            taken.insert(m.id, share);
        } else if let Some(t) = m.reversal_of.and_then(|r| taken.remove(&r)) {
            left += t;
        }
    }
    left.max(Decimal::ZERO)
}

async fn moves_after(hc: &mut HookCtx<'_>, item: Uuid, wh: Uuid, seq: i64) -> Result<Vec<WalkMove>> {
    let rows: Vec<(Uuid, i64, Decimal, Decimal, Option<Uuid>, bool)> = sqlx::query_as(
        "select id, seq, qty, qty_after, reversal_of, source_entity = 'LandedCostVoucher' from stock_move \
         where tenant_id = $1 and item_id = $2 and warehouse_id = $3 and seq > $4 order by seq",
    )
    .bind(hc.ctx.tenant)
    .bind(item)
    .bind(wh)
    .bind(seq)
    .fetch_all(&mut **hc.tx)
    .await?;
    Ok(rows.into_iter().map(|(id, seq, qty, qty_after, reversal_of, revaluation)| WalkMove { id, seq, qty, qty_after, reversal_of, revaluation }).collect())
}

/// An open FIFO layer; `origin` is the receipt its units came from (a revaluation re-creates a
/// layer with the original receipt as its source line).
#[derive(Debug, Clone, sqlx::FromRow)]
struct Layer {
    qty: Decimal,
    cost: Decimal,
    origin: Uuid,
}

async fn layers(hc: &mut HookCtx<'_>, item: Uuid, wh: Uuid) -> Result<Vec<Layer>> {
    Ok(sqlx::query_as(
        "select l.qty_remaining as qty, l.unit_cost as cost, \
         case when m.source_entity = 'LandedCostVoucher' and m.source_line_id is not null then m.source_line_id else m.id end as origin \
         from cost_layer l join stock_move m on m.tenant_id = l.tenant_id and m.id = l.source_move_id \
         where l.tenant_id = $1 and l.item_id = $2 and l.warehouse_id = $3 and l.qty_remaining > 0 order by l.posting_ts, l.seq",
    )
    .bind(hc.ctx.tenant)
    .bind(item)
    .bind(wh)
    .fetch_all(&mut **hc.tx)
    .await?)
}

/// Value to add (negative: remove) to the stock of one item in one warehouse.
#[derive(Debug, Clone)]
pub(crate) enum Delta {
    /// Moving average: the whole warehouse pool.
    Pool(Decimal),
    /// FIFO: per originating receipt move, onto that receipt's remaining units.
    Layers(Vec<(Uuid, Decimal)>),
}

/// Stock of an item in a warehouse as the engine needs it to move it: per batch and serial set.
struct Piece {
    qty: Decimal,
    batch: Option<String>,
    serials: Vec<String>,
}

async fn pieces(hc: &mut HookCtx<'_>, info: &ItemInfo, wh: Uuid, on_hand: Decimal) -> Result<Vec<Piece>> {
    let mut out: Vec<Piece> = Vec::new();
    if info.serial {
        for (serial, batch) in serials_in_stock(hc.tx, hc.ctx.tenant, info.id, wh, None).await? {
            let batch = if info.batch { batch } else { None };
            match out.iter_mut().find(|p| p.batch == batch) {
                Some(p) => {
                    p.qty += Decimal::ONE;
                    p.serials.push(serial);
                }
                None => out.push(Piece { qty: Decimal::ONE, batch, serials: vec![serial] }),
            }
        }
    } else if info.batch {
        for (batch, qty) in batch_quantities(hc.tx, hc.ctx.tenant, info.id, wh).await? {
            if batch.is_none() || qty < Decimal::ZERO {
                return Err(Error::invalid(format!("the batches of {} in this warehouse do not add up; reconcile its stock first", info.code)));
            }
            out.push(Piece { qty, batch, serials: vec![] });
        }
    } else {
        out.push(Piece { qty: on_hand, batch: None, serials: vec![] });
    }
    let total: Decimal = out.iter().map(|p| p.qty).sum();
    if total != on_hand {
        return Err(Error::invalid(format!(
            "the batch or serial records of {} do not match its stock ({} on hand); reconcile it first",
            info.code,
            on_hand.normalize()
        )));
    }
    Ok(out)
}

/// Adds `delta` to the value of the stock of `info` in `wh` through the engine: the stock is issued
/// at its book value and received back with the extra value, so its quantity, batches and serials are
/// unchanged. FIFO layers come back in their order, each at its own cost, with each receipt's share
/// on that receipt's units.
#[allow(clippy::too_many_arguments)]
async fn revalue(
    hc: &mut HookCtx<'_>,
    co: &Company,
    voucher: Uuid,
    on: NaiveDate,
    ts: DateTime<Utc>,
    info: &ItemInfo,
    wh: Uuid,
    delta: Delta,
) -> Result<Vec<Posted>> {
    let (q, _, _) = balance(hc.tx, hc.ctx.tenant, info.id, wh).await?;
    if q <= Decimal::ZERO {
        return Err(Error::invalid(format!("no stock of {} is left in the warehouse to revalue", info.code)));
    }
    let parts = pieces(hc, info, wh, q).await?;
    let open = if info.fifo { layers(hc, info.id, wh).await? } else { vec![] };
    if info.fifo && open.iter().map(|l| l.qty).sum::<Decimal>() != q {
        return Err(Error::invalid(format!("the FIFO layers of {} do not match its stock (it went negative); reconcile it first", info.code)));
    }
    let issues = parts
        .iter()
        .map(|p| MoveIn { item: info.id, warehouse: wh, qty: -p.qty, batch_no: p.batch.clone(), serials: p.serials.clone(), ..Default::default() })
        .collect();
    let issued = stock::post_moves(hc.tx, hc.ctx, co, ENTITY, voucher, on, ts, issues, "consumed").await?;
    let out_value: Decimal = issued.iter().map(|m| -m.value).sum();
    let mut receipts = Vec::new();
    match delta {
        Delta::Pool(extra) => {
            let qtys: Vec<Decimal> = parts.iter().map(|p| p.qty).collect();
            for (p, v) in parts.iter().zip(allocate(co, out_value + extra, &qtys)?) {
                receipts.push(MoveIn {
                    item: info.id,
                    warehouse: wh,
                    qty: p.qty,
                    exact_value: Some(v),
                    batch_no: p.batch.clone(),
                    serials: p.serials.clone(),
                    ..Default::default()
                });
            }
        }
        Delta::Layers(extras) => {
            let base: Vec<Decimal> = open.iter().map(|l| round(l.qty * l.cost, co.precision)).collect();
            let mut targets = base.clone();
            for (origin, extra) in extras {
                let idx: Vec<usize> = (0..open.len()).filter(|i| open[*i].origin == origin).collect();
                if idx.is_empty() {
                    if extra.is_zero() {
                        continue;
                    }
                    return Err(Error::internal(format!("no FIFO layer of {} comes from the revalued receipt", info.code)));
                }
                let shares = allocate(co, extra, &idx.iter().map(|i| open[*i].qty).collect::<Vec<_>>())?;
                for (i, sh) in idx.into_iter().zip(shares) {
                    targets[i] += sh;
                }
            }
            // Rounding of the issued value lands on the newest layer, so the totals stay exact.
            if let Some(last) = targets.last_mut() {
                *last += out_value - base.iter().copied().sum::<Decimal>();
            }
            let layer_qtys: Vec<Decimal> = open.iter().map(|l| l.qty).collect();
            let part_qtys: Vec<Decimal> = parts.iter().map(|p| p.qty).collect();
            let segments = pair_up(&layer_qtys, &part_qtys);
            let mut values: HashMap<usize, std::vec::IntoIter<Decimal>> = HashMap::new();
            for j in 0..open.len() {
                let qtys: Vec<Decimal> = segments.iter().filter(|s| s.0 == j).map(|s| s.2).collect();
                values.insert(j, allocate(co, targets[j], &qtys)?.into_iter());
            }
            let mut cursor = vec![0usize; parts.len()];
            for (j, k, y) in segments {
                let take = if info.serial { y.to_usize().unwrap_or(0) } else { 0 };
                let serials = parts[k].serials[cursor[k]..cursor[k] + take].to_vec();
                cursor[k] += take;
                receipts.push(MoveIn {
                    item: info.id,
                    warehouse: wh,
                    qty: y,
                    exact_value: values.get_mut(&j).and_then(Iterator::next),
                    batch_no: parts[k].batch.clone(),
                    serials,
                    source_line: Some(open[j].origin),
                    ..Default::default()
                });
            }
        }
    }
    if receipts.iter().any(|m| m.exact_value.is_some_and(|v| v < Decimal::ZERO)) {
        return Err(Error::invalid(format!("this would make the value of {} negative", info.code)));
    }
    let received = stock::post_moves(hc.tx, hc.ctx, co, ENTITY, voucher, on, ts, receipts, "consumed").await?;
    Ok(issued.into_iter().chain(received).collect())
}

/// One voucher line with the receipt move it applies to.
struct Share {
    line: Uuid,
    info: ItemInfo,
    wh: Uuid,
    origin: ReceiptMove,
    charge: Decimal,
    capitalized: Decimal,
    on_hand: Decimal,
}

async fn shares(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Vec<Share>> {
    let lines = rows(doc, "items");
    let ids: Vec<Uuid> = lines.iter().filter_map(|r| id(r, "item")).collect();
    let infos = items(hc.tx, hc.ctx.tenant, &ids).await?;
    let mut moves: HashMap<Uuid, Vec<ReceiptMove>> = HashMap::new();
    let mut out = Vec::new();
    for (i, r) in lines.iter().enumerate() {
        let (Some((entity, _, rid)), Some(line), Some(item)) = (receipt_of(r), id(r, "id"), id(r, "item")) else { continue };
        if let std::collections::hash_map::Entry::Vacant(e) = moves.entry(rid) {
            e.insert(receipt_moves(hc, entity, rid).await?);
        }
        let receipt_line = s(r, "receipt_line").and_then(|x| x.parse::<Uuid>().ok());
        let origin = moves[&rid].iter().find(|m| Some(m.source_line_id) == receipt_line).cloned().ok_or_else(|| {
            Error::Validation(vec![FieldError::new("items", "the receipt line no longer has stock; refresh the receipts").in_row("items", i)])
        })?;
        let info = infos.get(&item).cloned().ok_or_else(|| Error::NotFound("Item".into()))?;
        out.push(Share {
            line,
            wh: origin.warehouse_id,
            info,
            origin,
            charge: d(r, "applicable_charges"),
            capitalized: d(r, "capitalized_amount"),
            on_hand: d(r, "on_hand_qty"),
        });
    }
    Ok(out)
}

fn group(shares: &[Share]) -> BTreeMap<(Uuid, Uuid), Vec<usize>> {
    let mut by_key: BTreeMap<(Uuid, Uuid), Vec<usize>> = BTreeMap::new();
    for (i, s) in shares.iter().enumerate() {
        by_key.entry((s.info.id, s.wh)).or_default().push(i);
    }
    by_key
}

fn consumed_account(doc: &Doc, co: &Company) -> Result<Uuid> {
    match id(doc, "consumed_cost_account") {
        Some(a) => Ok(a),
        None => co.require("cogs_account", "cost of goods sold account"),
    }
}

/// Journal entry: the revaluation moves on the stock accounts, the consumed share and the charges.
async fn book(hc: &mut HookCtx<'_>, co: &Company, doc: &Doc, on: NaiveDate, posted: &[Posted], consumed: Decimal, sign: Decimal) -> Result<()> {
    let vid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let cc = co.account("default_cost_center");
    let mut entry = Entry::new(co.id, on, ENTITY, vid, s(doc, "number").map(str::to_string));
    entry.memo = Some(if sign > Decimal::ZERO { "Landed cost".into() } else { "Landed cost cancelled".into() });
    entry.lines.extend(stock_lines(hc.tx, hc.ctx.tenant, co, posted).await?);
    if !consumed.is_zero() {
        entry.lines.push(Line::new(consumed_account(doc, co)?, consumed, &co.currency_code).cost_center(cc).remarks("Landed cost of stock already consumed"));
    }
    for c in rows(doc, "charges") {
        let acc = id(&c, "expense_account").ok_or_else(|| Error::field("charges", "a charge has no account"))?;
        let line = Line::new(acc, -sign * d(&c, "amount"), &co.currency_code).cost_center(id(&c, "cost_center").or(cc));
        entry.lines.push(match s(&c, "description") {
            Some(text) => line.remarks(text),
            None => line,
        });
    }
    ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
    Ok(())
}

pub struct LandedCostHook;

#[async_trait]
impl DocHook for LandedCostHook {
    /// Lists the stock lines of the chosen receipts and spreads the charges over them.
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !d(doc, "docstatus").is_zero() {
            return Ok(());
        }
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        let manual = s(doc, "distribute_charges_based_on") == Some(MANUAL);
        let old: Vec<Doc> = rows(doc, "items");
        let mut receipts = rows(doc, "receipts");
        let mut lines: Vec<Doc> = Vec::new();
        for r in receipts.iter_mut() {
            let Some((entity, field, rid)) = receipt_of(r) else { continue };
            set(r, if field == "purchase_receipt" { "purchase_invoice" } else { "purchase_receipt" }, Value::Null);
            let rd = match get(hc.kernel, hc.ctx, hc.tx, entity, rid).await {
                Ok(x) => x,
                Err(Error::NotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            for k in ["supplier", "posting_date"] {
                set(r, k, rd.get(k).cloned().unwrap_or(Value::Null));
            }
            set(r, "grand_total", rd.get("base_grand_total").cloned().unwrap_or(Value::Null));
            if d(&rd, "docstatus") != Decimal::ONE {
                continue;
            }
            let found = receipt_moves(hc, entity, rid).await?;
            let infos = items(hc.tx, hc.ctx.tenant, &found.iter().map(|m| m.item_id).collect::<Vec<_>>()).await?;
            for m in found {
                let key = m.source_line_id.to_string();
                let prev = old.iter().find(|o| s(o, "receipt_line") == Some(key.as_str()));
                let mut row = Doc::new();
                if let Some(rowid) = prev.and_then(|p| p.get("id")) {
                    row.insert("id".into(), rowid.clone());
                }
                row.insert("item".into(), json!(m.item_id.to_string()));
                row.insert("item_name".into(), json!(infos.get(&m.item_id).map(|i| i.name.clone())));
                row.insert("warehouse".into(), json!(m.warehouse_id.to_string()));
                row.insert("receipt_type".into(), r.get("receipt_type").cloned().unwrap_or(Value::Null));
                row.insert(field.into(), json!(rid.to_string()));
                row.insert("receipt_line".into(), json!(key));
                row.insert("qty".into(), money(m.qty));
                row.insert("amount".into(), money(m.value));
                let kept = if manual { prev.and_then(|p| p.get("applicable_charges")).cloned().unwrap_or(Value::Null) } else { Value::Null };
                row.insert("applicable_charges".into(), kept);
                lines.push(row);
            }
        }
        // Charges are booked as they are distributed: at the company's precision.
        let mut charges = rows(doc, "charges");
        for c in charges.iter_mut() {
            let amount = round(d(c, "amount"), co.precision);
            c.insert("amount".into(), money(amount));
        }
        let total: Decimal = charges.iter().map(|c| d(c, "amount")).sum();
        set(doc, "charges", rows_value(charges));
        if !manual && !lines.is_empty() {
            let field = if s(doc, "distribute_charges_based_on") == Some("Qty") { "qty" } else { "amount" };
            let weights: Vec<Decimal> = lines.iter().map(|l| d(l, field)).collect();
            for (l, v) in lines.iter_mut().zip(allocate(&co, total, &weights)?) {
                l.insert("applicable_charges".into(), money(v));
            }
        }
        finish_rows(&mut lines);
        set(doc, "receipts", rows_value(receipts));
        set(doc, "items", rows_value(lines));
        set(doc, "total_charges", money(total));
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let cid = id(doc, "company");
        let on = date(doc, "posting_date")?;
        let mut errors = Vec::new();
        let mut seen = HashSet::new();
        for (i, r) in rows(doc, "receipts").iter().enumerate() {
            let Some((entity, field, rid)) = receipt_of(r) else { continue };
            let mut err = |m: String| errors.push(FieldError::new(field, m).in_row("receipts", i));
            if !seen.insert(rid) {
                err("this receipt is listed twice".into());
                continue;
            }
            let rd = match get(hc.kernel, hc.ctx, hc.tx, entity, rid).await {
                Ok(x) => x,
                Err(Error::NotFound(_)) => {
                    err("does not exist".into());
                    continue;
                }
                Err(e) => return Err(e),
            };
            let number = s(&rd, "number").unwrap_or("this receipt").to_string();
            if d(&rd, "docstatus") != Decimal::ONE {
                err(format!("{number} is not submitted"));
            }
            if id(&rd, "company") != cid {
                err(format!("{number} belongs to another company"));
            }
            if b(&rd, "is_return") {
                err(format!("{number} is a return; landed costs apply to receipts"));
            }
            if entity == "PurchaseInvoice" && !b(&rd, "update_stock") {
                err(format!("{number} did not update stock; use its purchase receipt"));
            }
            if date(&rd, "posting_date")? > on {
                err(format!("{number} is dated after this voucher"));
            }
        }
        let lines = rows(doc, "items");
        if lines.is_empty() && errors.is_empty() {
            errors.push(FieldError::new("receipts", "the selected receipts have no stock items"));
        }
        let total = d(doc, "total_charges");
        if total <= Decimal::ZERO {
            errors.push(FieldError::new("charges", "add the charges to distribute"));
        }
        if s(doc, "distribute_charges_based_on") == Some(MANUAL) {
            let given: Decimal = lines.iter().map(|l| d(l, "applicable_charges")).sum();
            if given != total {
                errors.push(FieldError::new(
                    "items",
                    format!("the applicable charges ({}) must add up to the total charges ({})", given.normalize(), total.normalize()),
                ));
            }
        }
        let mut accounts: Vec<Uuid> = rows(doc, "charges").iter().filter_map(|c| id(c, "expense_account")).collect();
        accounts.extend(id(doc, "consumed_cost_account"));
        for acc in accounts {
            let a = get(hc.kernel, hc.ctx, hc.tx, "Account", acc).await?;
            if id(&a, "company") != cid {
                errors.push(FieldError::new("charges", format!("account {} belongs to another company", s(&a, "account_name").unwrap_or_default())));
            }
        }
        validation(errors)
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let vid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let (on, ts) = (date(doc, "posting_date")?, posting_ts(doc)?);
        let mut all = shares(hc, doc).await?;
        let by_key = group(&all);
        let keys: Vec<(Uuid, Uuid)> = by_key.keys().copied().collect();
        stock::lock_balances(hc.tx, hc.ctx.tenant, co.id, &keys).await?;
        // How much of each receipt is still where it arrived.
        for ((item, wh), idx) in &by_key {
            if all[idx[0]].info.fifo {
                let open = layers(hc, *item, *wh).await?;
                for &i in idx {
                    let left: Decimal = open.iter().filter(|l| l.origin == all[i].origin.id).map(|l| l.qty).sum();
                    let fraction = (left / all[i].origin.qty).min(Decimal::ONE);
                    all[i].on_hand = left.min(all[i].origin.qty);
                    all[i].capitalized = round(all[i].charge * fraction, co.precision);
                }
            } else {
                let first = idx.iter().map(|&i| all[i].origin.seq).min().unwrap_or_default();
                let later = moves_after(hc, *item, *wh, first).await?;
                for &i in idx {
                    let seq = all[i].origin.seq;
                    let tail: Vec<WalkMove> = later.iter().filter(|m| m.seq > seq).cloned().collect();
                    let fraction = remaining(Decimal::ONE, &tail).min(Decimal::ONE);
                    all[i].on_hand = (all[i].origin.qty * fraction).round_dp(9);
                    all[i].capitalized = round(all[i].charge * fraction, co.precision);
                }
            }
        }
        let mut posted = Vec::new();
        for ((_, wh), idx) in &by_key {
            let info = all[idx[0]].info.clone();
            let total: Decimal = idx.iter().map(|&i| all[i].capitalized).sum();
            if total.is_zero() {
                continue;
            }
            let delta = if info.fifo { Delta::Layers(idx.iter().map(|&i| (all[i].origin.id, all[i].capitalized)).collect()) } else { Delta::Pool(total) };
            posted.extend(revalue(hc, &co, vid, on, ts, &info, *wh, delta).await?);
        }
        let consumed: Decimal = all.iter().map(|x| x.charge - x.capitalized).sum();
        book(hc, &co, doc, on, &posted, consumed, Decimal::ONE).await?;
        let sctx = sys(hc.ctx);
        for x in &all {
            let mut v = Doc::new();
            v.insert("on_hand_qty".into(), money(x.on_hand));
            v.insert("capitalized_amount".into(), money(x.capitalized));
            v.insert("consumed_amount".into(), money(x.charge - x.capitalized));
            hc.kernel.set_child_values_tx(&sctx, hc.tx, ENTITY, "items", x.line, v).await?;
        }
        Ok(())
    }

    /// Takes the capitalized cost still in stock back out; the rest of the charges come back out of
    /// the consumed cost account.
    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let vid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let (on, ts) = ledger::reversal_point(hc.tx, hc.ctx.tenant, co.id, doc).await?;
        let all = shares(hc, doc).await?;
        let by_key = group(&all);
        let keys: Vec<(Uuid, Uuid)> = by_key.keys().copied().collect();
        stock::lock_balances(hc.tx, hc.ctx.tenant, co.id, &keys).await?;
        let mut posted = Vec::new();
        let mut still = Decimal::ZERO;
        for ((item, wh), idx) in &by_key {
            let info = all[idx[0]].info.clone();
            let capitalized: Decimal = idx.iter().map(|&i| all[i].capitalized).sum();
            if capitalized.is_zero() {
                continue;
            }
            let delta = if info.fifo {
                let open = layers(hc, *item, *wh).await?;
                let mut out = Vec::new();
                for &i in idx {
                    let x = &all[i];
                    if x.capitalized.is_zero() || x.on_hand <= Decimal::ZERO {
                        continue;
                    }
                    let left: Decimal = open.iter().filter(|l| l.origin == x.origin.id).map(|l| l.qty).sum::<Decimal>().min(x.on_hand);
                    let back = round(x.capitalized * (left / x.on_hand), co.precision);
                    still += back;
                    out.push((x.origin.id, -back));
                }
                Delta::Layers(out)
            } else {
                let start: Option<i64> = sqlx::query_scalar(
                    "select max(seq) from stock_move where tenant_id = $1 and source_entity = $2 and source_id = $3 and item_id = $4 and warehouse_id = $5",
                )
                .bind(hc.ctx.tenant)
                .bind(ENTITY)
                .bind(vid)
                .bind(item)
                .bind(wh)
                .fetch_one(&mut **hc.tx)
                .await?;
                let later = moves_after(hc, *item, *wh, start.unwrap_or_default()).await?;
                let back = round(capitalized * remaining(Decimal::ONE, &later).min(Decimal::ONE), co.precision);
                still += back;
                Delta::Pool(-back)
            };
            let any = match &delta {
                Delta::Pool(v) => !v.is_zero(),
                Delta::Layers(v) => v.iter().any(|(_, x)| !x.is_zero()),
            };
            if any {
                posted.extend(revalue(hc, &co, vid, on, ts, &info, *wh, delta).await?);
            }
        }
        let charges: Decimal = all.iter().map(|x| x.charge).sum();
        // The ledger entry is dated like the moves (the voucher's date while its period is open).
        book(hc, &co, doc, on, &posted, -(charges - still), -Decimal::ONE).await
    }
}

/// Receipts cannot be cancelled under a submitted landed cost voucher.
pub struct ReceiptGuard;

#[async_trait]
impl DocHook for ReceiptGuard {
    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let table = child_table(hc.ctx, ENTITY, "receipts")?;
        let parent = hc.ctx.schema.entity(ENTITY).map(|e| e.table.clone()).unwrap_or_default();
        let found: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select coalesce(p.number, p.id::text) from {} r join {} p on p.tenant_id = r.tenant_id and p.id = r.parent_id \
             where r.tenant_id = $1 and p.docstatus = 1 and (r.purchase_receipt = $2 or r.purchase_invoice = $2) limit 1",
            q(&table),
            q(&parent)
        )))
        .bind(hc.ctx.tenant)
        .bind(id(doc, "id"))
        .fetch_optional(&mut **hc.tx)
        .await?;
        match found {
            Some(n) => Err(Error::invalid(format!("landed cost voucher {n} revalued this receipt; cancel it first"))),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn mv(qty: &str, after: &str) -> WalkMove {
        WalkMove { id: Uuid::now_v7(), seq: 0, qty: n(qty), qty_after: n(after), reversal_of: None, revaluation: false }
    }

    #[test]
    fn moving_average_share_follows_issues() {
        // 10 on hand after the receipt (20 units with the opening ones): selling 10 leaves half.
        assert_eq!(remaining(n("100"), &[mv("-10", "10")]), n("50"));
        // Receipts after do not dilute the share; a second issue takes its pro-rata part.
        let moves = [mv("-5", "15"), mv("10", "25"), mv("-5", "20")];
        assert_eq!(remaining(n("1"), &moves).round_dp(12), n("0.6"), "1 * 15/20 * 20/25");
        // Selling everything leaves nothing; negative stock takes the rest.
        assert_eq!(remaining(n("80"), &[mv("-20", "0")]), Decimal::ZERO);
        assert_eq!(remaining(n("80"), &[mv("-5", "-2")]), Decimal::ZERO);
    }

    #[test]
    fn cancelled_issues_give_their_share_back_and_revaluations_are_ignored() {
        let sale = mv("-5", "5");
        let reversal = WalkMove { id: Uuid::now_v7(), seq: 0, qty: n("5"), qty_after: n("10"), reversal_of: Some(sale.id), revaluation: false };
        assert_eq!(remaining(n("100"), &[sale.clone(), reversal]), n("100"));
        let reval = WalkMove { revaluation: true, ..mv("-10", "0") };
        assert_eq!(remaining(n("100"), &[reval]), n("100"));
    }
}
