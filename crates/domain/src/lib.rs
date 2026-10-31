//! The business records and helpers every app shares: contacts and addresses, customers and
//! suppliers, items and price lists, projects (the `core`, `contacts`, `parties`, `products` and
//! `project_records` packages, embedded here), their hooks, the small helpers apps use on
//! documents (`util`, `flow`) and the report framework (`reports`). ERP's engine and packages are
//! ferrum-app-erp's (docs/plan/erp-repo.md).
//!
//! Everything plugs into the kernel through `DocHook`s registered by entity name, each for the
//! workspaces with its package.

pub mod contacts;
pub mod flow;
pub mod products;
pub mod reports;
pub mod util;

use ferrum_kernel::{HookRegistry, Kernel, Result};
use std::sync::Arc;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// The shared metadata packages (core, contacts, parties, products, project_records), embedded at
/// build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers the shared packages' hooks, each for the workspaces with its package.
pub fn register(h: &mut HookRegistry) {
    let only = |p: &str| vec![p.to_string()];
    h.scoped(&only("contacts"), |h| {
        h.register("Contact", Arc::new(contacts::ContactHook)).register("Address", Arc::new(contacts::AddressHook));
    });
    h.scoped(&only("parties"), |h| {
        h.register("Address", Arc::new(contacts::PartyAddressHook));
    });
    h.scoped(&only("products"), |h| {
        h.register("Item", Arc::new(products::ItemHook));
    });
}

/// Periodic upkeep of the shared packages: none. Kept for the apps' tests that still call it.
pub async fn maintenance(_k: &Kernel) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_shared_packages_form_a_valid_catalog() {
        let pkgs = super::packages().expect("embedded packages parse");
        let names: Vec<&str> = pkgs.iter().map(|p| p.name()).collect();
        assert_eq!(names, ["contacts", "core", "parties", "products", "project_records"]);
        // Their forms also place the fields ERP's packages add (a company's default accounts, an
        // item's stock settings), so that every placed field exists is checked with ERP's packages
        // (ferrum-app-erp's tests) and with every compiled app (the server's).
        ferrum_meta::Catalog::from_packages(pkgs).expect("they validate together");
    }
}
