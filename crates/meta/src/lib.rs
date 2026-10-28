//! Ferrum metadata: packages, entities, fields, layered overlays and CEL rules.
//!
//! Pure (no I/O beyond reading package directories) so it is unit-testable and reusable by tools.

pub mod catalog;
pub mod effective;
pub mod expr;
pub mod types;

pub use catalog::Catalog;
pub use effective::{ChildInfo, EffEntity, Exprs, Schema, SchemaWarning, Storage};
pub use expr::{Expr, Scope};
pub use types::*;

use sha2::{Digest, Sha256};
use std::sync::LazyLock;

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum MetaError {
    #[error("{0}")]
    Invalid(String),
    #[error("cannot read {path}: {msg}")]
    Io { path: String, msg: String },
    #[error("invalid YAML in {path}: {msg}")]
    Yaml { path: String, msg: String },
}

pub(crate) fn invalid<T>(msg: impl Into<String>) -> Result<T, MetaError> {
    Err(MetaError::Invalid(msg.into()))
}

/// Columns every entity table has; user fields may not use these names.
pub const RESERVED_FIELDS: &[&str] = &[
    "id",
    "tenant_id",
    "version",
    "docstatus",
    "number",
    "created_at",
    "created_by",
    "updated_at",
    "updated_by",
    "ext",
    "parent_id",
    "idx",
    "amended_from",
    "workflow_state",
    "entity",
];

static IDENT: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^[a-z][a-z0-9_]{0,62}$").unwrap());
static ENTITY: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^[A-Z][A-Za-z0-9]{0,62}$").unwrap());
static PACKAGE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^[a-z][a-z0-9_-]{0,62}$").unwrap());

/// Lower-case SQL-safe identifier (fields, columns, actions).
pub fn is_ident(s: &str) -> bool {
    IDENT.is_match(s)
}

pub fn is_entity_name(s: &str) -> bool {
    ENTITY.is_match(s)
}

pub fn is_package_name(s: &str) -> bool {
    PACKAGE.is_match(s)
}

/// `SalesInvoice` → `sales_invoice`.
pub fn snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let chars: Vec<char> = name.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            let prev_lower = i > 0 && (chars[i - 1].is_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if i > 0 && (prev_lower || (next_lower && chars[i - 1].is_uppercase())) {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(*c);
        }
    }
    out
}

/// Postgres identifiers are limited to 63 bytes; long names keep a readable prefix + hash.
pub fn fit_ident(name: &str) -> String {
    if name.len() <= 63 {
        return name.to_string();
    }
    let hash = hex::encode(&Sha256::digest(name.as_bytes())[..4]);
    format!("{}_{hash}", &name[..54])
}

pub fn table_for(entity: &str) -> String {
    fit_ident(&format!("e_{}", snake(entity)))
}

pub fn child_table(parent_table: &str, field: &str) -> String {
    fit_ident(&format!("{parent_table}__{field}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn naming() {
        assert_eq!(snake("SalesInvoice"), "sales_invoice");
        assert_eq!(snake("BOMItem"), "bom_item");
        assert_eq!(snake("GLAccount"), "gl_account");
        assert_eq!(snake("Item2Price"), "item2_price");
        assert_eq!(table_for("SalesInvoice"), "e_sales_invoice");
        assert!(fit_ident(&"a".repeat(80)).len() <= 63);
        assert!(is_ident("customer_name") && !is_ident("Customer") && !is_ident("1a") && !is_ident("a-b"));
    }
}
