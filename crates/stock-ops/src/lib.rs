//! Stock Operations: stock reconciliation, landed costs, pick lists, packing, quality inspection,
//! item variants and product bundles.
//!
//! Every quantity and value change goes through the platform's inventory engine
//! (`ferrum_domain::stock`) and every financial effect through its ledger, so valuation, locking
//! and append-only reversal behave exactly as for the platform's own documents. Hooks on platform
//! entities run for every tenant and act only where this package is installed.

mod bundle;
mod common;
mod landed_cost;
mod packing;
mod pick_list;
mod quality;
mod reconciliation;
mod reports;
mod variants;

use ferrum_domain::reports::ReportSource;
use ferrum_kernel::{DocHook, HookRegistry, Kernel, Result};
use std::sync::Arc;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// This app's manifest (`app.yaml`).
pub const MANIFEST: &str = include_str!("../app.yaml");

/// This app's metadata packages, embedded at build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers this app's document hooks. They run after the platform's (registered first), so on a
/// delivery note the engine has posted the note's own lines before the bundle components move.
pub fn register(h: &mut HookRegistry) {
    let guard: Arc<dyn DocHook> = Arc::new(variants::TemplateGuard);
    for entity in [
        "SalesOrder",
        "DeliveryNote",
        "SalesInvoice",
        "PurchaseOrder",
        "PurchaseReceipt",
        "PurchaseInvoice",
        "StockEntry",
        "StockReconciliation",
        "PickList",
        "PackingSlip",
        "ProductBundle",
    ] {
        h.register(entity, guard.clone());
    }
    let gate: Arc<dyn DocHook> = Arc::new(quality::InspectionGate);
    for entity in ["PurchaseReceipt", "PurchaseInvoice", "DeliveryNote", "SalesInvoice", "StockEntry"] {
        h.register(entity, gate.clone());
    }
    let packed: Arc<dyn DocHook> = Arc::new(bundle::PackedStock);
    let progress: Arc<dyn DocHook> = Arc::new(pick_list::PickProgress);
    let receipt_guard: Arc<dyn DocHook> = Arc::new(landed_cost::ReceiptGuard);
    h.register("Item", Arc::new(variants::ItemHook))
        .register("ItemAttribute", Arc::new(variants::AttributeHook))
        .register("ProductBundle", Arc::new(bundle::BundleHook))
        .register("DeliveryNote", packed.clone())
        .register("SalesInvoice", packed)
        .register("DeliveryNote", progress.clone())
        .register("StockEntry", progress)
        .register("DeliveryNote", Arc::new(packing::DeliveryPacking))
        .register("PackingSlip", Arc::new(packing::PackingSlipHook))
        .register("SalesOrder", Arc::new(pick_list::OrderPicking))
        .register("PickList", Arc::new(pick_list::PickListHook))
        .register("QualityInspectionTemplate", Arc::new(quality::TemplateHook))
        .register("QualityInspection", Arc::new(quality::InspectionHook))
        .register("StockReconciliation", Arc::new(reconciliation::ReconciliationHook))
        .register("LandedCostVoucher", Arc::new(landed_cost::LandedCostHook))
        .register("PurchaseReceipt", receipt_guard.clone())
        .register("PurchaseInvoice", receipt_guard);
}

/// This app's reports.
pub fn reports() -> Vec<Arc<dyn ReportSource>> {
    vec![Arc::new(reports::StockOpsReports)]
}

/// Periodic upkeep: nothing here needs it (every state change happens inside its document's
/// transaction), so this stays a cheap no-op.
pub async fn maintenance(_k: &Kernel) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn packages_validate_with_the_platform() {
        let mut pkgs = ferrum_domain::packages().expect("platform packages");
        pkgs.extend(super::packages().expect("stock_ops parses"));
        let cat = ferrum_meta::Catalog::from_packages(pkgs).expect("catalog validates");
        let schema = ferrum_meta::effective::build(&cat, &["stock_ops".into()], &[], 1).expect("effective schema");
        let item = schema.entity("Item").expect("item");
        assert!(item.children.contains_key("stk_attributes"));
        assert!(schema.entity("DeliveryNote").unwrap().children.contains_key("stk_packed_items"));
        assert!(schema.entity("SalesOrder").unwrap().def.actions.iter().any(|a| a.name == "make_pick_list"));
        let catalog = super::reports::StockOpsReports;
        let names: Vec<String> =
            ferrum_domain::reports::ReportSource::catalog(&catalog).iter().map(|r| r["name"].as_str().unwrap_or_default().to_string()).collect();
        assert_eq!(names.len(), 6);
        for r in ferrum_domain::reports::ReportSource::catalog(&catalog) {
            assert!(schema.entity(r["entity"].as_str().unwrap()).is_some(), "report entity of {}", r["name"]);
        }
    }
}
