//! The shipped packages must always load, validate and merge.

use ferrum_meta::{Catalog, effective};
use std::path::Path;

#[test]
fn shipped_packages_are_valid() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages");
    let cat = Catalog::load_dir(&dir).unwrap_or_else(|e| panic!("{e}"));
    let all: Vec<String> = cat.packages().map(|p| p.name().to_string()).collect();
    let schema = effective::build(&cat, &all, &[], 1).unwrap_or_else(|e| panic!("{e}"));
    assert!(schema.warnings.is_empty(), "{:?}", schema.warnings);
    for p in &all {
        effective::build(&cat, std::slice::from_ref(p), &[], 1).unwrap_or_else(|e| panic!("package {p} alone: {e}"));
    }
    assert!(schema.entity("SalesInvoice").is_some());
    assert!(schema.entity("Customer").unwrap().storage_of("in_gstin").is_some());
}
