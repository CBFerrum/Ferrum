//! Stock reserved for submitted sales orders: per item and warehouse, what their open lines have
//! yet to deliver (`stock_balance.reserved_qty`). It is recomputed from the orders whenever one
//! changes — never added to or subtracted from — so it cannot drift, and each balance is locked
//! before it is summed (in the same item-then-warehouse order as stock postings), so concurrent
//! orders for the same item never lose each other's quantities.

use ferrum_kernel::{Result, Tx};
use rust_decimal::Decimal;
use uuid::Uuid;

/// Refreshes the reservations of every stock item and warehouse on sales order `order`.
pub async fn refresh_for_order(tx: &mut Tx, tenant: Uuid, order: Uuid) -> Result<()> {
    let mut pairs: Vec<(Uuid, Uuid, Uuid)> = sqlx::query_as(
        "select distinct l.item, coalesce(l.warehouse, o.set_warehouse), o.company from e_sales_order o \
         join e_sales_order__items l on l.tenant_id = o.tenant_id and l.parent_id = o.id \
         join e_item i on i.tenant_id = l.tenant_id and i.id = l.item \
         where o.tenant_id = $1 and o.id = $2 and i.is_stock_item and coalesce(l.warehouse, o.set_warehouse) is not null",
    )
    .bind(tenant)
    .bind(order)
    .fetch_all(&mut **tx)
    .await?;
    pairs.sort();
    pairs.dedup_by_key(|p| (p.0, p.1));
    for (item, warehouse, company) in pairs {
        refresh(tx, tenant, item, warehouse, company).await?;
    }
    Ok(())
}

async fn refresh(tx: &mut Tx, tenant: Uuid, item: Uuid, warehouse: Uuid, company: Uuid) -> Result<()> {
    // The balance must exist to be locked; a concurrent first insert waits for this one.
    sqlx::query(
        "insert into stock_balance (tenant_id, item_id, warehouse_id, company_id) values ($1, $2, $3, $4) \
         on conflict (tenant_id, item_id, warehouse_id) do nothing",
    )
    .bind(tenant)
    .bind(item)
    .bind(warehouse)
    .bind(company)
    .execute(&mut **tx)
    .await?;
    sqlx::query("select 1 from stock_balance where tenant_id = $1 and item_id = $2 and warehouse_id = $3 for update")
        .bind(tenant)
        .bind(item)
        .bind(warehouse)
        .execute(&mut **tx)
        .await?;
    // A new statement after the lock: it sees every order committed while this one waited.
    let reserved: Decimal = sqlx::query_scalar(
        "select coalesce(sum(greatest(l.qty - coalesce(l.delivered_qty, 0), 0)), 0) from e_sales_order__items l \
         join e_sales_order o on o.tenant_id = l.tenant_id and o.id = l.parent_id \
         where l.tenant_id = $1 and l.item = $2 and coalesce(l.warehouse, o.set_warehouse) = $3 \
         and o.docstatus = 1 and o.status not in ('Closed', 'Completed')",
    )
    .bind(tenant)
    .bind(item)
    .bind(warehouse)
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query("update stock_balance set reserved_qty = $4, updated_at = now() where tenant_id = $1 and item_id = $2 and warehouse_id = $3")
        .bind(tenant)
        .bind(item)
        .bind(warehouse)
        .bind(reserved)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
