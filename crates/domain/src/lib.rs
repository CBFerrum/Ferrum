//! ERP domain engines and hooks: double-entry ledger with receivables/payables, inventory
//! valuation, selling and buying flows, stock entries, manufacturing, India GST and reports.
//!
//! Everything here plugs into the kernel through `DocHook`s registered by entity name, so an
//! entity whose package is not installed simply never triggers its hooks.

pub mod accounting;
pub mod flow;
pub mod india;
pub mod ledger;
pub mod manufacturing;
pub mod pricing;
pub mod reports;
pub mod setup;
pub mod stock;
pub mod stock_entry;
pub mod trade;
pub mod util;

use chrono::Datelike;
use ferrum_kernel::{HookRegistry, Kernel, Result};
use pricing::Side;
use std::sync::Arc;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// The platform's metadata packages (core, accounting, buying, selling, inventory, manufacturing,
/// india), embedded at build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers every domain hook. Order matters per entity: GST template selection runs before
/// the pricing/posting hooks of the same document.
pub fn register(h: &mut HookRegistry) {
    h.register("Company", Arc::new(india::PartyGstHook))
        .register("Customer", Arc::new(india::PartyGstHook))
        .register("Supplier", Arc::new(india::PartyGstHook))
        .register("Company", Arc::new(setup::CompanyHook))
        .register("FiscalYear", Arc::new(setup::FiscalYearHook))
        .register("JournalEntry", Arc::new(accounting::JournalEntryHook))
        .register("PaymentEntry", Arc::new(accounting::PaymentEntryHook))
        .register("StockEntry", Arc::new(stock_entry::StockEntryHook))
        .register("Bom", Arc::new(manufacturing::BomHook))
        .register("WorkOrder", Arc::new(manufacturing::WorkOrderHook))
        .register("MrpRun", Arc::new(manufacturing::MrpRunHook));
    for (side, order, shipment, invoice) in
        [(Side::Selling, "SalesOrder", "DeliveryNote", "SalesInvoice"), (Side::Buying, "PurchaseOrder", "PurchaseReceipt", "PurchaseInvoice")]
    {
        for entity in [order, shipment, invoice] {
            h.register(entity, Arc::new(india::GstTemplateHook(side)));
        }
        h.register(order, Arc::new(trade::OrderHook(side)))
            .register(shipment, Arc::new(trade::ShipmentHook(side)))
            .register(invoice, Arc::new(trade::InvoiceHook(side)));
    }
}

/// Periodic domain upkeep, safe to run on every replica at once: yearly journal partitions.
pub async fn maintenance(k: &Kernel) -> Result<()> {
    let year = chrono::Utc::now().year();
    let mut tx = k.db.system_tx().await?;
    let installed: bool = sqlx::query_scalar("select to_regproc('ferrum_ensure_journal_partitions') is not null").fetch_one(&mut *tx).await?;
    // One replica at a time; the others skip this round.
    let locked: bool = sqlx::query_scalar("select pg_try_advisory_xact_lock(hashtext('ferrum:journal_partitions'))").fetch_one(&mut *tx).await?;
    if installed && locked {
        let created: i32 = sqlx::query_scalar("select ferrum_ensure_journal_partitions($1, $2)").bind(year - 1).bind(year + 1).fetch_one(&mut *tx).await?;
        if created > 0 {
            tracing::info!(created, "journal partitions created");
        }
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_platform_packages_form_a_valid_catalog() {
        let pkgs = super::packages().expect("embedded packages parse");
        let names: Vec<&str> = pkgs.iter().map(|p| p.name()).collect();
        assert_eq!(names, ["accounting", "buying", "core", "india", "inventory", "manufacturing", "selling"]);
        ferrum_meta::Catalog::from_packages(pkgs).expect("they validate together");
    }
}
