//! Warranty claims: the warranty status follows from when the serial number was delivered (the
//! stock ledger knows) and the item's warranty period; an active maintenance contract covering the
//! item means "Under AMC". A claim can send an engineer through a breakdown maintenance visit.

use crate::{child_table, int, linked};
use async_trait::async_trait;
use chrono::{Duration, NaiveDate, Utc};
use ferrum_domain::flow::created;
use ferrum_domain::util::{get, id, s, set};
use ferrum_kernel::value::parse_date;
use ferrum_kernel::{Doc, DocHook, HookCtx, Result};
use serde_json::{Value, json};
use uuid::Uuid;

pub struct ClaimHook;

/// When the serial left stock to a customer: the latest movement of the serial (in posting
/// order; cancelled moves and their reversals excluded), if that movement is a delivery note or
/// invoice issue. A serial returned or still in stock has no delivery.
async fn serial_delivery(hc: &mut HookCtx<'_>, item: Uuid, serial: &str) -> Result<Option<(NaiveDate, String)>> {
    let last: Option<(bool, NaiveDate, String, Uuid)> = sqlx::query_as(
        "select m.qty < 0 and m.source_entity in ('DeliveryNote', 'SalesInvoice'), m.posting_date, m.source_entity, m.source_id \
         from stock_move m where m.tenant_id = $1 and m.item_id = $2 and m.serial_no is not null \
         and $3 = any(string_to_array(m.serial_no, chr(10))) and m.reversal_of is null \
         and not exists (select 1 from stock_move r where r.tenant_id = m.tenant_id and r.reversal_of = m.id) \
         order by m.seq desc limit 1",
    )
    .bind(hc.ctx.tenant)
    .bind(item)
    .bind(serial)
    .fetch_optional(&mut **hc.tx)
    .await?;
    let Some((true, day, entity, source)) = last else { return Ok(None) };
    let number = s(&get(hc.kernel, hc.ctx, hc.tx, &entity, source).await?, "number").unwrap_or_default().to_string();
    let label = hc.ctx.schema.entity(&entity).map_or(entity.clone(), |e| e.def.label.clone());
    Ok(Some((day, format!("{label} {number}"))))
}

/// Whether a submitted maintenance schedule of the customer covers the item (and serial) on `on`.
async fn under_amc(hc: &mut HookCtx<'_>, customer: Uuid, item: Uuid, serial: Option<&str>, on: NaiveDate) -> Result<bool> {
    let lines = child_table(hc.ctx, "MaintenanceSchedule", "items")?;
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select exists (select 1 from e_maintenance_schedule s join {lines} i on i.tenant_id = s.tenant_id and i.parent_id = s.id \
         where s.tenant_id = $1 and s.customer = $2 and s.docstatus = 1 and i.item = $3 \
         and (coalesce(i.serial_no, '') = '' or i.serial_no = $4) and i.start_date <= $5 and i.end_date >= $5)"
    )))
    .bind(hc.ctx.tenant)
    .bind(customer)
    .bind(item)
    .bind(serial)
    .bind(on)
    .fetch_one(&mut **hc.tx)
    .await?)
}

#[async_trait]
impl DocHook for ClaimHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let today = Utc::now().date_naive();
        match s(doc, "status") {
            Some("Closed") if s(doc, "resolution_date").is_none() => set(doc, "resolution_date", today.to_string()),
            Some("Closed") => {}
            _ => set(doc, "resolution_date", Value::Null),
        }
        let (Some(item), Some(customer)) = (id(doc, "item"), id(doc, "customer")) else { return Ok(()) };
        let Some(item_doc) = linked(hc.kernel, hc.ctx, hc.tx, "Item", item).await? else { return Ok(()) };
        let serial = s(doc, "serial_no").map(str::to_string);
        let delivery = match &serial {
            Some(sn) => serial_delivery(hc, item, sn).await?,
            None => None,
        };
        // The stock ledger is the authority on serialized deliveries; otherwise the entered date counts.
        match delivery {
            Some((day, reference)) => {
                set(doc, "delivery_date", day.to_string());
                set(doc, "delivery_reference", reference);
            }
            None => set(doc, "delivery_reference", Value::Null),
        }
        let claimed = s(doc, "claim_date").and_then(|v| parse_date(v).ok()).unwrap_or(today);
        let delivered = s(doc, "delivery_date").and_then(|v| parse_date(v).ok());
        let days = int(&item_doc, "sup_warranty_days");
        let expiry = delivered.filter(|_| days > 0).map(|d| d + Duration::days(days));
        set(doc, "warranty_expiry_date", expiry.map_or(Value::Null, |e| json!(e.to_string())));
        let status = if expiry.is_some_and(|e| claimed <= e) {
            Some("Under Warranty")
        } else if under_amc(hc, customer, item, serial.as_deref(), claimed).await? {
            Some("Under AMC")
        } else if delivered.is_some() || days <= 0 {
            Some("Out of Warranty")
        } else {
            None // delivery date unknown: cannot tell yet
        };
        set(doc, "warranty_status", status.map_or(Value::Null, |v| json!(v)));
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "make_maintenance_visit" {
            return Ok(None);
        }
        let mut input = Doc::new();
        input.insert("customer".into(), doc.get("customer").cloned().unwrap_or(Value::Null));
        input.insert("maintenance_type".into(), json!("Breakdown"));
        input.insert("warranty_claim".into(), doc.get("id").cloned().unwrap_or(Value::Null));
        input.insert(
            "work_done".into(),
            json!([{"item": doc.get("item"), "serial_no": doc.get("serial_no"), "work_done": s(doc, "complaint").unwrap_or("Warranty repair")}]),
        );
        // Nothing read-only is set, so the user's own permissions apply as they are.
        let visit = hc.kernel.insert_tx(hc.ctx, hc.tx, "MaintenanceVisit", input).await?;
        Ok(Some(created("MaintenanceVisit", &visit)))
    }
}
