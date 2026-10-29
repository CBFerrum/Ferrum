//! Re-order levels per item and warehouse, and the upkeep that raises material requests for them.
//!
//! The projected quantity is what is on hand (stock balance), plus what submitted purchase orders
//! still bring and running inward requests still ask for, minus what running issue requests will
//! take and what is reserved (open sales order quantity, or the balance's own reservation when
//! larger). When it falls below the level, a submitted request for the largest of the re-order
//! quantity, the shortfall and the item's minimum order quantity is raised — unless a running
//! request for that item and warehouse still has quantity to order: never two open requests for
//! the same need.

use crate::material_request::days_after;
use crate::{installed, settings};
use async_trait::async_trait;
use ferrum_domain::util::{b, get, id, money, rows, s};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, HookCtx, Kernel, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

/// Validates an item's re-order levels: one row per warehouse, a different source for transfers,
/// stock items only.
pub struct ItemReorderHook;

#[async_trait]
impl DocHook for ItemReorderHook {
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) || hc.entity.field("proc_reorder_levels").is_none() {
            return Ok(());
        }
        let levels = rows(doc, "proc_reorder_levels");
        if levels.is_empty() {
            return Ok(());
        }
        if !b(doc, "is_stock_item") {
            return Err(Error::field("proc_reorder_levels", "only stock items can be re-ordered automatically"));
        }
        let mut errors = Vec::new();
        let mut seen = Vec::new();
        for (i, r) in levels.iter().enumerate() {
            let Some(wh) = id(r, "warehouse") else { continue };
            if seen.contains(&wh) {
                errors.push(FieldError::new("warehouse", "has a re-order level already").in_row("proc_reorder_levels", i));
            }
            seen.push(wh);
            if s(r, "request_type") != Some("Material Transfer") {
                continue;
            }
            match id(r, "from_warehouse") {
                Some(from) if from == wh => {
                    errors.push(FieldError::new("from_warehouse", "must differ from the warehouse being replenished").in_row("proc_reorder_levels", i));
                }
                Some(from) => {
                    // The raised request lives in one company: both warehouses must be its.
                    let company = |w: Doc| id(&w, "company");
                    let to = company(get(hc.kernel, hc.ctx, hc.tx, "Warehouse", wh).await?);
                    if company(get(hc.kernel, hc.ctx, hc.tx, "Warehouse", from).await?) != to {
                        errors.push(FieldError::new("from_warehouse", "belongs to another company").in_row("proc_reorder_levels", i));
                    }
                }
                None => {}
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }
}

#[derive(Debug, sqlx::FromRow)]
struct Level {
    item: Uuid,
    item_code: String,
    warehouse: Uuid,
    company: Uuid,
    request_type: Option<String>,
    from_warehouse: Option<Uuid>,
    reorder_level: Decimal,
    reorder_qty: Decimal,
    min_order_qty: Option<Decimal>,
    lead_time_days: Option<i64>,
    whole: bool,
    on_hand: Decimal,
    ordered: Decimal,
    requested: Decimal,
    reserved: Decimal,
    open_request: bool,
}

const LEVELS: &str = "\
select r.parent_id as item, coalesce(i.item_code, '') as item_code, r.warehouse, w.company, r.request_type, r.from_warehouse, \
  coalesce(r.reorder_level, 0) as reorder_level, coalesce(r.reorder_qty, 0) as reorder_qty, \
  i.min_order_qty, i.lead_time_days, coalesce(u.whole_number, false) as whole, coalesce(b.qty, 0) as on_hand, \
  coalesce((select sum(l.qty - coalesce(l.received_qty, 0)) from e_purchase_order__items l \
            join e_purchase_order o on o.tenant_id = l.tenant_id and o.id = l.parent_id \
            where l.tenant_id = r.tenant_id and l.item = r.parent_id and l.warehouse = r.warehouse \
              and o.docstatus = 1 and o.status <> 'Closed' and l.qty > coalesce(l.received_qty, 0)), 0) as ordered, \
  coalesce((select sum(case when m.material_request_type = 'Material Issue' then -1 else 1 end * (l.stock_qty - coalesce(l.ordered_qty, 0))) \
            from e_material_request__items l join e_material_request m on m.tenant_id = l.tenant_id and m.id = l.parent_id \
            where l.tenant_id = r.tenant_id and l.item = r.parent_id and l.warehouse = r.warehouse \
              and m.docstatus = 1 and m.status <> 'Stopped' and l.stock_qty > coalesce(l.ordered_qty, 0)), 0) as requested, \
  greatest(coalesce(b.reserved_qty, 0), \
           coalesce((select sum(l.qty - coalesce(l.delivered_qty, 0)) from e_sales_order__items l \
                     join e_sales_order o on o.tenant_id = l.tenant_id and o.id = l.parent_id \
                     where l.tenant_id = r.tenant_id and l.item = r.parent_id and l.warehouse = r.warehouse \
                       and o.docstatus = 1 and o.status <> 'Closed' and l.qty > coalesce(l.delivered_qty, 0)), 0)) as reserved, \
  exists (select 1 from e_material_request__items l join e_material_request m on m.tenant_id = l.tenant_id and m.id = l.parent_id \
          where l.tenant_id = r.tenant_id and l.item = r.parent_id and l.warehouse = r.warehouse \
            and m.docstatus = 1 and m.status <> 'Stopped' and m.material_request_type <> 'Material Issue' \
            and l.stock_qty > coalesce(l.ordered_qty, 0)) as open_request \
from e_item__proc_reorder_levels r \
join e_item i on i.tenant_id = r.tenant_id and i.id = r.parent_id \
join e_warehouse w on w.tenant_id = r.tenant_id and w.id = r.warehouse \
left join e_uom u on u.tenant_id = i.tenant_id and u.id = i.stock_uom \
left join stock_balance b on b.tenant_id = r.tenant_id and b.item_id = r.parent_id and b.warehouse_id = r.warehouse \
where r.tenant_id = $1 and coalesce(i.is_stock_item, false) and not coalesce(i.disabled, false) and not coalesce(w.disabled, false) \
order by w.company, r.request_type, r.from_warehouse, i.item_code";

impl Level {
    fn projected(&self) -> Decimal {
        self.on_hand + self.ordered + self.requested - self.reserved
    }

    /// What to request, if anything: enough to reach the level, at least the re-order and minimum
    /// order quantities, in whole units for whole-number UOMs.
    fn shortfall(&self) -> Option<Decimal> {
        let projected = self.projected();
        if self.open_request || projected >= self.reorder_level {
            return None;
        }
        let qty = self.reorder_qty.max(self.reorder_level - projected).max(self.min_order_qty.unwrap_or_default());
        Some(if self.whole { qty.ceil() } else { qty })
    }
}

/// Upkeep: raises the requests the re-order levels call for, one per company, type and source.
pub(crate) async fn auto_reorder(k: &Kernel, ctx: &Ctx) -> Result<()> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    if !settings(&mut tx, ctx.tenant).await?.auto_reorder {
        tx.rollback().await?;
        return Ok(());
    }
    let levels: Vec<Level> = sqlx::query_as(LEVELS).bind(ctx.tenant).fetch_all(&mut *tx).await?;
    tx.rollback().await?;
    let today = ctx.today();
    let mut groups: Vec<((Uuid, String, Option<Uuid>), Vec<(Level, Decimal)>)> = Vec::new();
    for l in levels {
        let Some(qty) = l.shortfall() else { continue };
        let kind = l.request_type.clone().unwrap_or_else(|| "Purchase".into());
        let source = if kind == "Material Transfer" { l.from_warehouse } else { None };
        let key = (l.company, kind, source);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, g)) => g.push((l, qty)),
            None => groups.push((key, vec![(l, qty)])),
        }
    }
    let mut first_error = None;
    for ((company, kind, source), lines) in groups {
        let lead = |l: &Level| l.lead_time_days.unwrap_or(0).max(0);
        let by = lines.iter().map(|(l, _)| lead(l)).max().unwrap_or(0);
        let items: Vec<Value> = lines
            .iter()
            .map(|(l, qty)| {
                json!({"item": l.item.to_string(), "qty": money(*qty), "warehouse": l.warehouse.to_string(),
                       "from_warehouse": source.map(|w| w.to_string()), "schedule_date": days_after(today, lead(l))})
            })
            .collect();
        let codes: Vec<&str> = lines.iter().map(|(l, _)| l.item_code.as_str()).collect();
        let input = json!({
            "company": company.to_string(), "material_request_type": kind, "transaction_date": today.to_string(),
            "schedule_date": days_after(today, by), "set_from_warehouse": source.map(|w| w.to_string()), "auto_reorder": true,
            "remarks": "Raised automatically: the projected quantity fell below the re-order level.", "items": items,
        });
        let outcome = async {
            let mut tx = k.db.tenant_tx(ctx.tenant).await?;
            let mr = k.insert_tx(ctx, &mut tx, "MaterialRequest", input.as_object().cloned().unwrap_or_default()).await?;
            let mr_id = id(&mr, "id").ok_or_else(|| Error::internal("no id"))?;
            let mr = k.submit_tx(ctx, &mut tx, "MaterialRequest", mr_id, None).await?;
            let managers: Vec<Uuid> = sqlx::query_scalar(
                "select r.user_id from user_role r join app_user u on u.tenant_id = r.tenant_id and u.id = r.user_id \
                 where r.tenant_id = $1 and r.role = 'Purchase Manager' and u.is_active",
            )
            .bind(ctx.tenant)
            .fetch_all(&mut *tx)
            .await?;
            let title = format!("{kind} request {} raised automatically", s(&mr, "number").unwrap_or_default());
            let body = format!("Below re-order level: {}", codes.join(", "));
            for user in managers {
                ferrum_kernel::notify::send(&mut tx, ctx.tenant, user, &title, Some(&body), Some("MaterialRequest"), Some(mr_id)).await?;
            }
            tx.commit().await?;
            Ok::<(), Error>(())
        }
        .await;
        if let Err(e) = outcome {
            tracing::warn!(tenant = %ctx.tenant, items = ?codes, "auto-reorder request failed: {e}");
            first_error.get_or_insert(e);
        }
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(on_hand: &str, ordered: &str, requested: &str, reserved: &str) -> Level {
        Level {
            item: Uuid::nil(),
            item_code: "X".into(),
            warehouse: Uuid::nil(),
            company: Uuid::nil(),
            request_type: None,
            from_warehouse: None,
            reorder_level: "50".parse().unwrap(),
            reorder_qty: "100".parse().unwrap(),
            min_order_qty: None,
            lead_time_days: None,
            whole: false,
            on_hand: on_hand.parse().unwrap(),
            ordered: ordered.parse().unwrap(),
            requested: requested.parse().unwrap(),
            reserved: reserved.parse().unwrap(),
            open_request: false,
        }
    }

    fn n(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn requests_only_below_the_level() {
        assert_eq!(level("50", "0", "0", "0").shortfall(), None, "at the level is not below it");
        assert_eq!(level("20", "0", "0", "0").shortfall(), Some(n("100")));
        assert_eq!(level("20", "40", "0", "0").shortfall(), None, "incoming orders count");
        assert_eq!(level("60", "0", "0", "30").shortfall(), Some(n("100")), "reservations count");
        let mut deep = level("0", "0", "0", "300");
        assert_eq!(deep.shortfall(), Some(n("350")), "enough to reach the level");
        deep.open_request = true;
        assert_eq!(deep.shortfall(), None, "never a second open request");
        let mut moq = level("0", "0", "0", "0");
        moq.min_order_qty = Some(n("250"));
        moq.whole = true;
        moq.reorder_qty = n("100.5");
        assert_eq!(moq.shortfall(), Some(n("250")));
        moq.min_order_qty = None;
        assert_eq!(moq.shortfall(), Some(n("101")), "whole units round up");
    }
}
