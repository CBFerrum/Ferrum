//! Inventory engine: append-only moves, locked balances, moving-average or FIFO valuation.
//!
//! Each move is valued once, from the balance state at posting time; nothing is ever re-valued
//! later (no repost cascades). Balances are locked in a fixed (item, warehouse) order so
//! concurrent postings cannot deadlock, and quantity checks happen under that lock, so stock can
//! never go negative unless the company allows it.

use crate::ledger::{Entry, Line};
use crate::util::{Company, round};
use chrono::{DateTime, NaiveDate, Utc};
use ferrum_kernel::{Ctx, Error, Result, Tx};
use rust_decimal::Decimal;
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

#[derive(Debug, Clone, Default)]
pub struct MoveIn {
    pub item: Uuid,
    pub warehouse: Uuid,
    /// Signed: positive receives, negative issues.
    pub qty: Decimal,
    /// Incoming unit cost (company currency) for receipts; ignored for issues.
    pub rate: Option<Decimal>,
    /// Forces the move's value (used by reversals so they cancel exactly).
    pub exact_value: Option<Decimal>,
    /// FIFO: the layer to retire when reversing a receipt.
    pub reverse_layer_of: Option<Uuid>,
    pub reversal_of: Option<Uuid>,
    pub batch_no: Option<String>,
    pub serials: Vec<String>,
    pub source_line: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct Posted {
    pub move_id: Uuid,
    pub item: Uuid,
    pub warehouse: Uuid,
    pub qty: Decimal,
    pub rate: Decimal,
    /// Signed value in company currency (negative for issues).
    pub value: Decimal,
    pub source_line: Option<Uuid>,
}

#[derive(sqlx::FromRow, Clone)]
struct ItemInfo {
    id: Uuid,
    item_code: String,
    is_stock_item: bool,
    valuation_method: Option<String>,
    has_serial_no: bool,
    has_batch_no: bool,
}

#[derive(sqlx::FromRow)]
struct Bal {
    item_id: Uuid,
    warehouse_id: Uuid,
    qty: Decimal,
    value: Decimal,
    valuation_rate: Decimal,
}

pub fn parse_serials(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default().split(['\n', ',']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// Locks the balance rows of `keys` in a deterministic order (creating missing ones), so
/// concurrent postings cannot deadlock. Documents that post in several steps (transfers,
/// manufacturing) call this first with every key they will touch; later locks are re-entrant.
pub async fn lock_balances(tx: &mut Tx, tenant: Uuid, company: Uuid, keys: &[(Uuid, Uuid)]) -> Result<BTreeMap<(Uuid, Uuid), (Decimal, Decimal, Decimal)>> {
    let mut keys = keys.to_vec();
    keys.sort();
    keys.dedup();
    let (ki, kw): (Vec<Uuid>, Vec<Uuid>) = keys.iter().copied().unzip();
    sqlx::query(
        "insert into stock_balance (tenant_id, item_id, warehouse_id, company_id) select $1, i, w, $2 from unnest($3::uuid[], $4::uuid[]) as t(i, w) \
         order by i, w on conflict do nothing",
    )
    .bind(tenant)
    .bind(company)
    .bind(&ki)
    .bind(&kw)
    .execute(&mut **tx)
    .await?;
    let rows: Vec<Bal> = sqlx::query_as(
        "select item_id, warehouse_id, qty, value, valuation_rate from stock_balance where tenant_id = $1 \
         and (item_id, warehouse_id) in (select * from unnest($2::uuid[], $3::uuid[])) order by item_id, warehouse_id for update",
    )
    .bind(tenant)
    .bind(&ki)
    .bind(&kw)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows.into_iter().map(|b| ((b.item_id, b.warehouse_id), (b.qty, b.value, b.valuation_rate))).collect())
}

#[allow(clippy::too_many_arguments)]
pub async fn post_moves(
    tx: &mut Tx,
    ctx: &Ctx,
    company: &Company,
    source_entity: &str,
    source_id: Uuid,
    posting_date: NaiveDate,
    ts: DateTime<Utc>,
    moves: Vec<MoveIn>,
    serial_out_status: &str,
) -> Result<Vec<Posted>> {
    if moves.is_empty() {
        return Ok(vec![]);
    }
    let p = company.precision;
    let item_ids: Vec<Uuid> = moves.iter().map(|m| m.item).collect();
    let items: HashMap<Uuid, ItemInfo> = sqlx::query_as::<_, ItemInfo>(
        "select id, item_code, is_stock_item, valuation_method, has_serial_no, has_batch_no from e_item where tenant_id = $1 and id = any($2)",
    )
    .bind(ctx.tenant)
    .bind(&item_ids)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|i| (i.id, i))
    .collect();
    let wh_ids: Vec<Uuid> = moves.iter().map(|m| m.warehouse).collect();
    let warehouses: HashMap<Uuid, (Uuid, bool, String)> =
        sqlx::query_as::<_, (Uuid, Uuid, bool, String)>("select id, company, is_group, warehouse_name from e_warehouse where tenant_id = $1 and id = any($2)")
            .bind(ctx.tenant)
            .bind(&wh_ids)
            .fetch_all(&mut **tx)
            .await?
            .into_iter()
            .map(|(id, c, g, n)| (id, (c, g, n)))
            .collect();
    for m in &moves {
        let item = items.get(&m.item).ok_or_else(|| Error::invalid("item not found"))?;
        if !item.is_stock_item {
            return Err(Error::invalid(format!("{} is not a stock item", item.item_code)));
        }
        let (wc, group, name) = warehouses.get(&m.warehouse).ok_or_else(|| Error::invalid("warehouse not found"))?;
        if *wc != company.id {
            return Err(Error::invalid(format!("warehouse {name} belongs to another company")));
        }
        if *group {
            return Err(Error::invalid(format!("{name} is a group warehouse; choose a leaf warehouse")));
        }
        if m.qty.is_zero() {
            return Err(Error::invalid(format!("quantity of {} is zero", item.item_code)));
        }
        if item.has_serial_no && (m.qty.fract() != Decimal::ZERO || Decimal::from(m.serials.len()) != m.qty.abs()) {
            return Err(Error::invalid(format!("{} is serialized: give exactly {} serial number(s) (one per line)", item.item_code, m.qty.abs())));
        }
        if item.has_batch_no && m.batch_no.as_deref().unwrap_or_default().is_empty() {
            return Err(Error::invalid(format!("{} is batch-tracked: a batch number is required", item.item_code)));
        }
    }

    let keys: Vec<(Uuid, Uuid)> = moves.iter().map(|m| (m.item, m.warehouse)).collect();
    let mut bal = lock_balances(tx, ctx.tenant, company.id, &keys).await?;

    let mut posted = Vec::with_capacity(moves.len());
    for m in moves {
        let item = &items[&m.item];
        let fifo = item.valuation_method.as_deref() == Some("FIFO");
        let (q, v, r) = bal.get(&(m.item, m.warehouse)).copied().unwrap_or_default();
        let move_id = Uuid::now_v7();
        let (rate, value, nq, nv, nr);
        if m.qty > Decimal::ZERO {
            let unit = m.rate.unwrap_or(r);
            if unit < Decimal::ZERO {
                return Err(Error::invalid(format!("negative rate for {}", item.item_code)));
            }
            value = m.exact_value.map(|x| x.abs()).unwrap_or_else(|| round(m.qty * unit, p));
            rate = (value / m.qty).round_dp(12);
            nq = q + m.qty;
            nv = v + value;
            nr = if nq > Decimal::ZERO { (nv / nq).round_dp(12).max(Decimal::ZERO) } else { rate };
        } else {
            let need = -m.qty;
            if q < need && !company.allow_negative_stock {
                let wname = &warehouses[&m.warehouse].2;
                return Err(Error::invalid(format!(
                    "not enough stock of {} in {wname}: available {}, required {}",
                    item.item_code,
                    q.normalize(),
                    need.normalize()
                )));
            }
            if let Some(batch) = m.batch_no.as_deref().filter(|b| !b.is_empty()) {
                let in_batch: Decimal = sqlx::query_scalar(
                    "select coalesce(sum(qty), 0) from stock_move where tenant_id = $1 and item_id = $2 and warehouse_id = $3 and batch_no = $4",
                )
                .bind(ctx.tenant)
                .bind(m.item)
                .bind(m.warehouse)
                .bind(batch)
                .fetch_one(&mut **tx)
                .await?;
                if in_batch < need && !company.allow_negative_stock {
                    return Err(Error::invalid(format!("batch {batch} of {} has only {} available", item.item_code, in_batch.normalize())));
                }
            }
            let mut out = if let Some(x) = m.exact_value {
                if let Some(layer_move) = m.reverse_layer_of.filter(|_| fifo) {
                    let n = sqlx::query("update cost_layer set qty_remaining = 0 where tenant_id = $1 and source_move_id = $2 and qty_remaining = qty_in")
                        .bind(ctx.tenant)
                        .bind(layer_move)
                        .execute(&mut **tx)
                        .await?
                        .rows_affected();
                    if n == 0 {
                        return Err(Error::invalid(format!(
                            "stock of {} from this receipt has already been used; cancel the documents that consumed it first",
                            item.item_code
                        )));
                    }
                }
                x.abs()
            } else if fifo {
                consume_layers(tx, ctx.tenant, m.item, m.warehouse, need, r, p).await?
            } else if q == need {
                v
            } else {
                round(need * r, p)
            };
            nq = q - need;
            if nq.is_zero() {
                // Take the residue with the last unit so stock value and the ledger stay equal.
                out = v;
            }
            value = -out;
            rate = if need.is_zero() { Decimal::ZERO } else { (out / need).round_dp(12) };
            nv = v - out;
            nr = if nq > Decimal::ZERO { (nv / nq).round_dp(12).max(Decimal::ZERO) } else { r };
        }
        let serial_text = (!m.serials.is_empty()).then(|| m.serials.join("\n"));
        sqlx::query(
            "insert into stock_move (tenant_id, id, company_id, item_id, warehouse_id, posting_date, posting_ts, qty, valuation_rate, value, \
             qty_after, value_after, batch_no, serial_no, source_entity, source_id, source_line_id, reversal_of) \
             values ($1, $2, $3, $4, $5, $6, $7, $8::numeric, $9::numeric, $10::numeric, $11::numeric, $12::numeric, $13, $14, $15, $16, $17, $18)",
        )
        .bind(ctx.tenant)
        .bind(move_id)
        .bind(company.id)
        .bind(m.item)
        .bind(m.warehouse)
        .bind(posting_date)
        .bind(ts)
        .bind(m.qty.to_string())
        .bind(rate.to_string())
        .bind(value.to_string())
        .bind(nq.to_string())
        .bind(nv.to_string())
        .bind(&m.batch_no)
        .bind(&serial_text)
        .bind(source_entity)
        .bind(source_id)
        .bind(m.source_line)
        .bind(m.reversal_of)
        .execute(&mut **tx)
        .await?;
        if fifo && m.qty > Decimal::ZERO {
            sqlx::query(
                "insert into cost_layer (tenant_id, id, item_id, warehouse_id, posting_ts, qty_in, qty_remaining, unit_cost, source_move_id) \
                 values ($1, $2, $3, $4, $5, $6::numeric, $6::numeric, $7::numeric, $8)",
            )
            .bind(ctx.tenant)
            .bind(Uuid::now_v7())
            .bind(m.item)
            .bind(m.warehouse)
            .bind(ts)
            .bind(m.qty.to_string())
            .bind(rate.to_string())
            .bind(move_id)
            .execute(&mut **tx)
            .await?;
        }
        for serial in &m.serials {
            let n = if m.qty > Decimal::ZERO {
                sqlx::query(
                    "insert into serial_no (tenant_id, item_id, serial, warehouse_id, status, last_move_id) values ($1, $2, $3, $4, 'in_stock', $5) \
                     on conflict (tenant_id, item_id, serial) do update set warehouse_id = excluded.warehouse_id, status = 'in_stock', \
                     last_move_id = excluded.last_move_id, updated_at = now() where serial_no.status <> 'in_stock'",
                )
                .bind(ctx.tenant)
                .bind(m.item)
                .bind(serial)
                .bind(m.warehouse)
                .bind(move_id)
                .execute(&mut **tx)
                .await?
                .rows_affected()
            } else {
                sqlx::query(
                    "update serial_no set status = $5, warehouse_id = null, last_move_id = $6, updated_at = now() \
                     where tenant_id = $1 and item_id = $2 and serial = $3 and warehouse_id = $4 and status = 'in_stock'",
                )
                .bind(ctx.tenant)
                .bind(m.item)
                .bind(serial)
                .bind(m.warehouse)
                .bind(serial_out_status)
                .bind(move_id)
                .execute(&mut **tx)
                .await?
                .rows_affected()
            };
            if n == 0 {
                return Err(Error::invalid(if m.qty > Decimal::ZERO {
                    format!("serial {serial} of {} is already in stock", item.item_code)
                } else {
                    format!("serial {serial} of {} is not in stock in this warehouse", item.item_code)
                }));
            }
        }
        bal.insert((m.item, m.warehouse), (nq, nv, nr));
        posted.push(Posted { move_id, item: m.item, warehouse: m.warehouse, qty: m.qty, rate, value, source_line: m.source_line });
    }
    for ((item, wh), (q, v, r)) in &bal {
        sqlx::query(
            "update stock_balance set qty = $4::numeric, value = $5::numeric, valuation_rate = $6::numeric, updated_at = now() \
             where tenant_id = $1 and item_id = $2 and warehouse_id = $3",
        )
        .bind(ctx.tenant)
        .bind(item)
        .bind(wh)
        .bind(q.to_string())
        .bind(v.to_string())
        .bind(r.to_string())
        .execute(&mut **tx)
        .await?;
    }
    Ok(posted)
}

async fn consume_layers(tx: &mut Tx, tenant: Uuid, item: Uuid, wh: Uuid, need: Decimal, fallback_rate: Decimal, p: u32) -> Result<Decimal> {
    let layers: Vec<(Uuid, Decimal, Decimal)> = sqlx::query_as(
        "select id, qty_remaining, unit_cost from cost_layer where tenant_id = $1 and item_id = $2 and warehouse_id = $3 and qty_remaining > 0 \
         order by posting_ts, seq for update",
    )
    .bind(tenant)
    .bind(item)
    .bind(wh)
    .fetch_all(&mut **tx)
    .await?;
    let mut left = need;
    let mut value = Decimal::ZERO;
    for (id, remaining, cost) in layers {
        if left.is_zero() {
            break;
        }
        let take = left.min(remaining);
        value += take * cost;
        left -= take;
        sqlx::query("update cost_layer set qty_remaining = qty_remaining - $3::numeric where tenant_id = $1 and id = $2")
            .bind(tenant)
            .bind(id)
            .bind(take.to_string())
            .execute(&mut **tx)
            .await?;
    }
    // Negative stock (when allowed) is valued at the last known rate.
    value += left * fallback_rate;
    Ok(round(value, p))
}

#[derive(sqlx::FromRow)]
struct MoveRow {
    id: Uuid,
    item_id: Uuid,
    warehouse_id: Uuid,
    qty: Decimal,
    value: Decimal,
    batch_no: Option<String>,
    serial_no: Option<String>,
    source_line_id: Option<Uuid>,
}

/// Appends exact reversals for every not-yet-reversed move of a source document.
///
/// The last units of an item carry whatever value is left, so reversing a moving-average receipt
/// after part of it was issued can take out less value than it brought in. That difference is
/// booked to the stock adjustment account (as `StockValueAdjustment`, so a ledger reversal of the
/// document never undoes it), keeping the stock accounts equal to the stock ledger.
pub async fn reverse_moves(
    tx: &mut Tx,
    ctx: &Ctx,
    company: &Company,
    source_entity: &str,
    source_id: Uuid,
    posting_date: NaiveDate,
    ts: DateTime<Utc>,
) -> Result<Vec<Posted>> {
    let rows: Vec<MoveRow> = sqlx::query_as(
        "select m.id, m.item_id, m.warehouse_id, m.qty, m.value, m.batch_no, m.serial_no, m.source_line_id from stock_move m \
         where m.tenant_id = $1 and m.source_entity = $2 and m.source_id = $3 and m.reversal_of is null \
         and not exists (select 1 from stock_move r where r.tenant_id = m.tenant_id and r.reversal_of = m.id) order by m.seq desc",
    )
    .bind(ctx.tenant)
    .bind(source_entity)
    .bind(source_id)
    .fetch_all(&mut **tx)
    .await?;
    let originals: Vec<Decimal> = rows.iter().map(|r| r.value).collect();
    let moves: Vec<MoveIn> = rows
        .into_iter()
        .map(|r| MoveIn {
            item: r.item_id,
            warehouse: r.warehouse_id,
            qty: -r.qty,
            rate: (r.qty < Decimal::ZERO).then(|| (r.value.abs() / r.qty.abs()).round_dp(12)),
            exact_value: Some(r.value.abs()),
            reverse_layer_of: (r.qty > Decimal::ZERO).then_some(r.id),
            reversal_of: Some(r.id),
            batch_no: r.batch_no,
            serials: parse_serials(r.serial_no.as_deref()),
            source_line: r.source_line_id,
        })
        .collect();
    // Reversed issues put serials back in stock; reversed receipts take them out.
    let posted = post_moves(tx, ctx, company, source_entity, source_id, posting_date, ts, moves, "consumed").await?;
    let mut entry = Entry::new(company.id, posting_date, "StockValueAdjustment", source_id, None);
    entry.memo = Some(format!("Stock value difference on cancelling {source_entity}"));
    for (p, original) in posted.iter().zip(originals) {
        let diff = p.value + original;
        if diff.is_zero() {
            continue;
        }
        let account: Option<Uuid> = sqlx::query_scalar("select account from e_warehouse where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(p.warehouse)
            .fetch_one(&mut **tx)
            .await?;
        let account = match account {
            Some(a) => a,
            None => company.require("default_inventory_account", "default inventory account")?,
        };
        let adjust = company.require("stock_adjustment_account", "stock adjustment account")?;
        entry.lines.push(Line::new(account, diff, &company.currency_code));
        entry.lines.push(Line::new(adjust, -diff, &company.currency_code).remarks("Stock value difference on cancellation"));
    }
    if !entry.lines.is_empty() {
        crate::ledger::post(tx, ctx, entry, company.precision).await?;
    }
    Ok(posted)
}

/// Current valuation rate of an item in a warehouse (falls back to any warehouse of the company).
pub async fn valuation_rate(tx: &mut Tx, tenant: Uuid, company: Uuid, item: Uuid, wh: Option<Uuid>) -> Result<Decimal> {
    if let Some(w) = wh {
        let r: Option<Decimal> = sqlx::query_scalar("select valuation_rate from stock_balance where tenant_id = $1 and item_id = $2 and warehouse_id = $3")
            .bind(tenant)
            .bind(item)
            .bind(w)
            .fetch_optional(&mut **tx)
            .await?;
        if let Some(r) = r.filter(|r| *r > Decimal::ZERO) {
            return Ok(r);
        }
    }
    let r: Option<Decimal> = sqlx::query_scalar(
        "select case when sum(qty) > 0 then sum(value) / sum(qty) else null end from stock_balance where tenant_id = $1 and company_id = $2 and item_id = $3",
    )
    .bind(tenant)
    .bind(company)
    .bind(item)
    .fetch_one(&mut **tx)
    .await?;
    Ok(r.unwrap_or(Decimal::ZERO).round_dp(12))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_parsing() {
        assert_eq!(parse_serials(Some("A1\nA2, A3\n\n")), vec!["A1", "A2", "A3"]);
        assert!(parse_serials(None).is_empty());
    }
}
