//! The shared packages shipped here must always load, validate and merge (ERP's are checked in its
//! repository).

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
    for e in ["Company", "Contact", "Customer", "Item", "Project"] {
        assert!(schema.entity(e).is_some(), "{e}");
    }
}
