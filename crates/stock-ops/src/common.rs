//! Helpers shared by the stock operations: item flags, warehouse accounts, batch and serial
//! positions read from the stock ledger, and small document utilities.

use ferrum_domain::ledger::Line;
use ferrum_domain::stock::Posted;
use ferrum_domain::util::Company;
use ferrum_kernel::{Ctx, Doc, Error, FieldError, Result, Tx};
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

pub const PACKAGE: &str = "stock_ops";

/// Hooks on platform entities run for every tenant; they act only where this package is installed.
pub fn installed(ctx: &Ctx) -> bool {
    ctx.schema.packages.iter().any(|p| p == PACKAGE)
}

/// Physical table of a child table field (overlay tables included).
pub fn child_table(ctx: &Ctx, entity: &str, field: &str) -> Result<String> {
    ctx.schema
        .entity(entity)
        .and_then(|e| e.children.get(field).map(|c| c.table.clone()))
        .ok_or_else(|| Error::internal(format!("{entity}.{field} is not a table")))
}

/// `Some(decimal)` when the field holds a value (null and missing are `None`).
pub fn opt_dec(doc: &Doc, k: &str) -> Option<Decimal> {
    doc.get(k).filter(|v| !v.is_null()).and_then(|v| ferrum_kernel::value::parse_decimal(v).ok())
}

pub fn rows_value(rows: Vec<Doc>) -> Value {
    Value::Array(rows.into_iter().map(Value::Object).collect())
}

/// Rows a hook builds need what the kernel gives the rows it receives: an id and a position.
pub fn finish_rows(rows: &mut [Doc]) {
    for (i, r) in rows.iter_mut().enumerate() {
        if ferrum_domain::util::id(r, "id").is_none() {
            r.insert("id".into(), Value::String(Uuid::now_v7().to_string()));
        }
        r.insert("idx".into(), Value::from(i));
    }
}

pub fn row_error(table: &str, i: usize, field: &str, msg: impl Into<String>) -> Error {
    Error::Validation(vec![FieldError::new(field, msg).in_row(table, i)])
}

pub fn validation(errors: Vec<FieldError>) -> Result<()> {
    if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
}

/// Serial numbers as written on a line (one per line or comma-separated).
pub fn serials(raw: Option<&str>) -> Vec<String> {
    ferrum_domain::stock::parse_serials(raw)
}

/// What the engine needs to know about an item.
#[derive(Debug, Clone)]
pub struct ItemInfo {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub is_stock: bool,
    pub fifo: bool,
    pub batch: bool,
    pub serial: bool,
    /// A variant template (`stk_has_variants`).
    pub template: bool,
    pub stock_uom: Option<Uuid>,
}

pub async fn items(tx: &mut Tx, tenant: Uuid, ids: &[Uuid]) -> Result<HashMap<Uuid, ItemInfo>> {
    let rows: Vec<(Uuid, String, Option<String>, Option<bool>, Option<String>, Option<bool>, Option<bool>, bool, Option<Uuid>)> = sqlx::query_as(
        "select id, item_code, item_name, is_stock_item, valuation_method, has_batch_no, has_serial_no, \
         coalesce((ext->>'stk_has_variants')::boolean, false), stock_uom from e_item where tenant_id = $1 and id = any($2)",
    )
    .bind(tenant)
    .bind(ids)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, code, name, stock, method, batch, serial, template, uom)| {
            let info = ItemInfo {
                id,
                code,
                name: name.unwrap_or_default(),
                is_stock: stock.unwrap_or(false),
                fifo: method.as_deref() == Some("FIFO"),
                batch: batch.unwrap_or(false),
                serial: serial.unwrap_or(false),
                template,
                stock_uom: uom,
            };
            (id, info)
        })
        .collect())
}

/// Stock account of each warehouse (its own, else the company's default inventory account).
pub async fn stock_accounts(tx: &mut Tx, tenant: Uuid, co: &Company, warehouses: &[Uuid]) -> Result<HashMap<Uuid, Uuid>> {
    let rows: Vec<(Uuid, Option<Uuid>)> =
        sqlx::query_as("select id, account from e_warehouse where tenant_id = $1 and id = any($2)").bind(tenant).bind(warehouses).fetch_all(&mut **tx).await?;
    let mut out = HashMap::new();
    for (w, a) in rows {
        let account = match a {
            Some(a) => a,
            None => co.require("default_inventory_account", "default inventory account")?,
        };
        out.insert(w, account);
    }
    Ok(out)
}

/// One journal line per posted move on its warehouse's stock account.
pub async fn stock_lines(tx: &mut Tx, tenant: Uuid, co: &Company, posted: &[Posted]) -> Result<Vec<Line>> {
    let whs: Vec<Uuid> = posted.iter().map(|p| p.warehouse).collect();
    let accounts = stock_accounts(tx, tenant, co, &whs).await?;
    posted
        .iter()
        .map(|p| {
            let acc = accounts.get(&p.warehouse).copied().ok_or_else(|| Error::internal("warehouse without account"))?;
            Ok(Line::new(acc, p.value, &co.currency_code))
        })
        .collect()
}

/// Company of each warehouse.
pub async fn warehouse_companies(tx: &mut Tx, tenant: Uuid, warehouses: &[Uuid]) -> Result<HashMap<Uuid, (Uuid, bool, String)>> {
    let rows: Vec<(Uuid, Uuid, Option<bool>, String)> =
        sqlx::query_as("select id, company, is_group, warehouse_name from e_warehouse where tenant_id = $1 and id = any($2)")
            .bind(tenant)
            .bind(warehouses)
            .fetch_all(&mut **tx)
            .await?;
    Ok(rows.into_iter().map(|(id, c, g, n)| (id, (c, g.unwrap_or(false), n))).collect())
}

/// Quantity and value of an item in a warehouse.
pub async fn balance(tx: &mut Tx, tenant: Uuid, item: Uuid, wh: Uuid) -> Result<(Decimal, Decimal, Decimal)> {
    let row: Option<(Decimal, Decimal, Decimal)> =
        sqlx::query_as("select qty, value, valuation_rate from stock_balance where tenant_id = $1 and item_id = $2 and warehouse_id = $3")
            .bind(tenant)
            .bind(item)
            .bind(wh)
            .fetch_optional(&mut **tx)
            .await?;
    Ok(row.unwrap_or_default())
}

/// Quantity of a batch in a warehouse, from the stock ledger.
pub async fn batch_qty(tx: &mut Tx, tenant: Uuid, item: Uuid, wh: Uuid, batch: &str) -> Result<Decimal> {
    Ok(sqlx::query_scalar("select coalesce(sum(qty), 0) from stock_move where tenant_id = $1 and item_id = $2 and warehouse_id = $3 and batch_no = $4")
        .bind(tenant)
        .bind(item)
        .bind(wh)
        .bind(batch)
        .fetch_one(&mut **tx)
        .await?)
}

/// Non-zero batch quantities of an item in a warehouse, oldest first receipt first (`None`: moves
/// without a batch).
pub async fn batch_quantities(tx: &mut Tx, tenant: Uuid, item: Uuid, wh: Uuid) -> Result<Vec<(Option<String>, Decimal)>> {
    Ok(sqlx::query_as(
        "select batch_no, sum(qty) from stock_move where tenant_id = $1 and item_id = $2 and warehouse_id = $3 \
         group by batch_no having sum(qty) <> 0 order by min(posting_ts), batch_no nulls first",
    )
    .bind(tenant)
    .bind(item)
    .bind(wh)
    .fetch_all(&mut **tx)
    .await?)
}

/// Serials in stock in a warehouse with the batch they arrived in, oldest receipt first;
/// `batch` narrows to one batch.
pub async fn serials_in_stock(tx: &mut Tx, tenant: Uuid, item: Uuid, wh: Uuid, batch: Option<&str>) -> Result<Vec<(String, Option<String>)>> {
    Ok(sqlx::query_as(
        "select s.serial, m.batch_no from serial_no s left join stock_move m on m.tenant_id = s.tenant_id and m.id = s.last_move_id \
         where s.tenant_id = $1 and s.item_id = $2 and s.warehouse_id = $3 and s.status = 'in_stock' and ($4::text is null or m.batch_no = $4) \
         order by m.posting_ts nulls first, m.seq nulls first, s.serial",
    )
    .bind(tenant)
    .bind(item)
    .bind(wh)
    .bind(batch)
    .fetch_all(&mut **tx)
    .await?)
}

/// Splits `total` over `weights` exactly (largest remainder), at the company's precision.
pub fn allocate(co: &Company, total: Decimal, weights: &[Decimal]) -> Result<Vec<Decimal>> {
    ferrum_calc::allocate(total, weights, co.precision, co.rounding).map_err(|e| Error::invalid(e.to_string()))
}

/// Splits consecutive quantities `a` over consecutive quantities `b` (equal totals): each segment is
/// `(index in a, index in b, qty)`, in order.
pub fn pair_up(a: &[Decimal], b: &[Decimal]) -> Vec<(usize, usize, Decimal)> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    let (mut left_a, mut left_b) = (a.first().copied().unwrap_or_default(), b.first().copied().unwrap_or_default());
    while i < a.len() && j < b.len() {
        let take = left_a.min(left_b);
        if take > Decimal::ZERO {
            out.push((i, j, take));
        }
        left_a -= take;
        left_b -= take;
        if left_a <= Decimal::ZERO {
            i += 1;
            left_a = a.get(i).copied().unwrap_or_default();
        }
        if left_b <= Decimal::ZERO {
            j += 1;
            left_b = b.get(j).copied().unwrap_or_default();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn pairing_covers_both_sides_in_order() {
        let segs = pair_up(&[d("5"), d("4"), d("1")], &[d("3"), d("7")]);
        assert_eq!(segs, vec![(0, 0, d("3")), (0, 1, d("2")), (1, 1, d("4")), (2, 1, d("1"))]);
        assert!(pair_up(&[], &[d("1")]).is_empty());
        let frac = pair_up(&[d("0.5"), d("0.25")], &[d("0.75")]);
        assert_eq!(frac, vec![(0, 0, d("0.5")), (1, 0, d("0.25"))]);
    }
}
