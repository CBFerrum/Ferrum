//! Manufacturing Operations: workstations, operations, routings, job cards, production planning
//! and subcontracting, on top of the platform's BOMs, work orders and stock engine.
//!
//! * Routings and BOM operations cost a BOM's operating time at its workstations' hour rates.
//! * Work orders copy the BOM's operations, are scheduled forward over the workstations' working
//!   hours and capacity when submitted, and get one job card per operation. Job cards log time
//!   and completed quantity; submitting one rolls its time and cost into the work order, whose
//!   manufacture entries can never finish more than the last operation completed.
//! * Production plans explode BOMs into sub-assemblies and raw materials netted against
//!   projected stock, then make the work orders and the material requests (or purchase orders).
//! * Subcontracting orders send materials to a supplier's warehouse through stock transfers;
//!   subcontracting receipts value the goods at the materials consumed plus the service and
//!   additional costs.

mod bom;
mod calendar;
mod job_card;
mod masters;
mod production_plan;
mod reports;
mod stock_entry;
mod subcontracting;
mod work_order;

use ferrum_domain::reports::ReportSource;
use ferrum_kernel::{Ctx, Doc, Error, HookRegistry, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// This app's manifest (`app.yaml`).
pub const MANIFEST: &str = include_str!("../app.yaml");

/// This app's metadata packages, embedded at build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers this app's document hooks. Hooks on platform entities (BOM, work order, stock
/// entry) run after the platform's own and do nothing for workspaces without this package.
pub fn register(h: &mut HookRegistry) {
    h.register("WorkstationType", Arc::new(masters::WorkstationTypeHook))
        .register("Workstation", Arc::new(masters::WorkstationHook))
        .register("Routing", Arc::new(masters::RoutingHook))
        .register("Bom", Arc::new(bom::BomOperationsHook))
        .register("WorkOrder", Arc::new(work_order::WorkOrderOperationsHook))
        .register("JobCard", Arc::new(job_card::JobCardHook))
        .register("ProductionPlan", Arc::new(production_plan::ProductionPlanHook))
        .register("SubcontractingBom", Arc::new(subcontracting::SubcontractingBomHook))
        .register("SubcontractingOrder", Arc::new(subcontracting::SubcontractingOrderHook))
        .register("SubcontractingReceipt", Arc::new(subcontracting::SubcontractingReceiptHook))
        .register("StockEntry", Arc::new(stock_entry::StockEntryOpsHook));
}

/// This app's reports.
pub fn reports() -> Vec<Arc<dyn ReportSource>> {
    vec![Arc::new(reports::MfgOpsReports)]
}

/// Periodic upkeep: every minute, on one replica at a time (advisory lock); must be idempotent and cheap.
/// Everything here is event-driven, so there is nothing to do.
pub async fn maintenance(_k: &Kernel) -> Result<()> {
    Ok(())
}

/// Whether the workspace runs this package (hooks on platform entities fire for every workspace).
pub(crate) fn installed(ctx: &Ctx) -> bool {
    ctx.schema.packages.iter().any(|p| p == "mfg_ops")
}

/// Physical table of a child table field (child tables are per parent field).
pub(crate) fn table_of(ctx: &Ctx, entity: &str, field: &str) -> Result<String> {
    ctx.schema
        .entity(entity)
        .and_then(|e| e.children.get(field).map(|c| ferrum_kernel::ddl::q(&c.table)))
        .ok_or_else(|| Error::internal(format!("{entity}.{field} is not a table")))
}

/// A record read once per hook call (engine read access).
pub(crate) async fn cached(hc: &mut ferrum_kernel::HookCtx<'_>, cache: &mut std::collections::HashMap<Uuid, Doc>, entity: &str, key: Uuid) -> Result<Doc> {
    if let std::collections::hash_map::Entry::Vacant(e) = cache.entry(key) {
        e.insert(ferrum_domain::util::get(hc.kernel, hc.ctx, hc.tx, entity, key).await?);
    }
    Ok(cache[&key].clone())
}

/// A child table set by a hook: rows it made get an id (the kernel only numbers input rows), and
/// every row its position.
pub(crate) fn rows_value(rows: Vec<Doc>) -> Value {
    Value::Array(
        rows.into_iter()
            .enumerate()
            .map(|(i, mut r)| {
                if ferrum_domain::util::id(&r, "id").is_none() {
                    r.insert("id".into(), Value::String(Uuid::now_v7().to_string()));
                }
                r.insert("idx".into(), Value::from(i));
                Value::Object(r)
            })
            .collect(),
    )
}

/// `Some(decimal)` when the field holds a value (null and missing are `None`).
pub(crate) fn opt_dec(doc: &Doc, k: &str) -> Option<Decimal> {
    doc.get(k).filter(|v| !v.is_null()).and_then(|v| ferrum_kernel::value::parse_decimal(v).ok())
}

/// A transaction-scoped advisory lock: serializes work on one key (a workstation's time logs, a
/// company's schedule) across concurrent transactions without locking rows.
pub(crate) async fn xact_lock(tx: &mut Tx, key: &str) -> Result<()> {
    sqlx::query("select pg_advisory_xact_lock(hashtext($1))").bind(key).execute(&mut **tx).await?;
    Ok(())
}

pub(crate) fn param_dec(params: &Value, k: &str) -> Result<Option<Decimal>> {
    match params.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => ferrum_kernel::value::parse_decimal(v).map(Some).map_err(|m| Error::field(k, m)),
    }
}

pub(crate) fn param_uuid(params: &Value, k: &str) -> Result<Option<Uuid>> {
    match params.get(k).and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
        None => Ok(None),
        Some(v) => v.trim().parse().map(Some).map_err(|_| Error::field(k, "is not a record id")),
    }
}

/// Actions that create stock entries submit them unless `{"submit": false}` asks for a draft.
pub(crate) fn submit_param(params: &Value) -> bool {
    params.get("submit").and_then(Value::as_bool).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn packages_validate_with_the_platform() {
        let mut pkgs = ferrum_domain::packages().expect("platform packages");
        pkgs.extend(super::packages().expect("mfg_ops parses"));
        let cat = ferrum_meta::Catalog::from_packages(pkgs).expect("valid together");
        let schema = ferrum_meta::effective::build(&cat, &["mfg_ops".into()], &[], 1).expect("effective schema");
        for e in ["JobCard", "ProductionPlan", "SubcontractingOrder", "SubcontractingReceipt", "Workstation", "Routing"] {
            assert!(schema.entity(e).is_some(), "{e}");
        }
        let wo = schema.entity("WorkOrder").unwrap();
        assert!(wo.children.contains_key("mfo_operations"), "work orders carry operations");
        assert!(schema.entity("Bom").unwrap().children.contains_key("mfo_operations"));
        let names: Vec<String> = super::reports().iter().flat_map(|r| r.catalog()).map(|c| c["name"].as_str().unwrap().to_string()).collect();
        let own = cat.package("mfg_ops").unwrap();
        for module in &own.manifest.modules {
            for item in &module.items {
                match item.strip_prefix("report:") {
                    Some(r) => assert!(names.iter().any(|n| n == r), "menu report {r} exists"),
                    None => assert!(schema.entity(item).is_some(), "menu entity {item} exists"),
                }
            }
        }
    }
}
