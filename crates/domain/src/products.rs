//! Items (the `products` package every app shares).

use crate::util::{d, id, rows};
use async_trait::async_trait;
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use std::collections::HashSet;

/// An item's other units: each once, and the stock unit (if listed) at 1.
pub struct ItemHook;

#[async_trait]
impl DocHook for ItemHook {
    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut seen = HashSet::new();
        let mut errors = Vec::new();
        for (i, u) in rows(doc, "uoms").iter().enumerate() {
            let unit = id(u, "uom");
            if unit.is_some() && unit == id(doc, "stock_uom") && d(u, "conversion_factor") != Decimal::ONE {
                errors.push(FieldError::new("conversion_factor", "the stock unit converts at 1").in_row("uoms", i));
            }
            if !seen.insert(unit) {
                errors.push(FieldError::new("uom", "is listed twice").in_row("uoms", i));
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }
}
