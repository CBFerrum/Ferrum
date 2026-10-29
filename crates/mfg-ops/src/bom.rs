//! BOM operations: a routing's operations copied into the BOM, each costed at its workstation's
//! hour rate for the BOM quantity. With operations, the BOM's operating cost is their total, so it
//! flows into `total_cost` and, through the platform's manufacture entries, into finished goods.

use crate::calendar::op_minutes;
use crate::masters::op_defaults;
use crate::{installed, rows_value};
use async_trait::async_trait;
use ferrum_domain::util::{b, company, d, get, id, money, round, rows, s, set, sys};
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub(crate) fn per_batch(r: &Doc) -> bool {
    s(r, "time_basis") == Some("Per Batch")
}

/// The definition fields of routing/BOM operation rows (also what work orders copy).
const OP_FIELDS: &[&str] = &["operation", "workstation", "time_basis", "time_in_mins", "batch_size", "setup_time", "description"];

pub(crate) fn copy_op(r: &Doc) -> Doc {
    OP_FIELDS.iter().filter_map(|k| r.get(*k).filter(|v| !v.is_null()).map(|v| (k.to_string(), v.clone()))).collect()
}

pub(crate) async fn workstation(hc: &mut HookCtx<'_>, cache: &mut HashMap<Uuid, Doc>, ws: Uuid) -> Result<Doc> {
    crate::cached(hc, cache, "Workstation", ws).await
}

/// Costs a BOM's operations for its quantity (the costing lot: setup time is spread over it) at
/// the workstations' current hour rates. `None` when the BOM has no operations.
async fn cost_operations(hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<Option<Decimal>> {
    let mut ops = rows(doc, "mfo_operations");
    if ops.is_empty() {
        return Ok(None);
    }
    op_defaults(hc, &mut ops, &mut HashMap::new()).await?;
    let precision = match id(doc, "company") {
        Some(c) => company(hc.kernel, hc.ctx, hc.tx, c).await?.precision,
        None => 2,
    };
    let qty = d(doc, "quantity").max(Decimal::ZERO);
    let qty = if qty.is_zero() { Decimal::ONE } else { qty };
    let mut cache = HashMap::new();
    let mut total = Decimal::ZERO;
    for r in ops.iter_mut() {
        let rate = match id(r, "workstation") {
            Some(ws) => d(&workstation(hc, &mut cache, ws).await?, "hour_rate"),
            None => Decimal::ZERO,
        };
        let minutes = op_minutes(qty, per_batch(r), d(r, "time_in_mins"), d(r, "batch_size"), d(r, "setup_time"));
        let cost = round(minutes * rate / Decimal::from(60), precision);
        set(r, "hour_rate", money(rate));
        set(r, "operating_cost", money(cost));
        total += cost;
    }
    set(doc, "mfo_operations", rows_value(ops));
    Ok(Some(total))
}

pub struct BomOperationsHook;

#[async_trait]
impl DocHook for BomOperationsHook {
    /// Runs after the platform costed the materials: copies a newly chosen routing's operations,
    /// costs them and makes them the operating cost.
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !installed(hc.ctx) || !d(doc, "docstatus").is_zero() {
            return Ok(());
        }
        let copied_from = s(doc, "mfo_operations_routing").map(str::to_string);
        match id(doc, "mfo_routing") {
            Some(r) if copied_from.as_deref() != Some(r.to_string().as_str()) || rows(doc, "mfo_operations").is_empty() => {
                let routing = get(hc.kernel, hc.ctx, hc.tx, "Routing", r).await?;
                if b(&routing, "disabled") {
                    return Err(Error::field("mfo_routing", "this routing is disabled"));
                }
                set(doc, "mfo_operations", rows_value(rows(&routing, "operations").iter().map(copy_op).collect()));
                set(doc, "mfo_operations_routing", r.to_string());
            }
            Some(_) => {}
            None if copied_from.is_some() => set(doc, "mfo_operations_routing", Value::Null),
            None => {}
        }
        if let Some(total) = cost_operations(hc, doc).await? {
            set(doc, "operating_cost", money(total));
            set(doc, "total_cost", money(d(doc, "raw_material_cost") + total));
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let ops = rows(doc, "mfo_operations");
        let mut errors = Vec::new();
        let mut cache = HashMap::new();
        for (i, r) in ops.iter().enumerate() {
            match id(r, "workstation") {
                None => errors.push(FieldError::new("workstation", "is required (the operation has no default workstation)").in_row("mfo_operations", i)),
                Some(ws) => {
                    let w = workstation(hc, &mut cache, ws).await?;
                    if id(&w, "company") != id(doc, "company") {
                        errors.push(FieldError::new("workstation", "belongs to another company").in_row("mfo_operations", i));
                    }
                }
            }
        }
        let used: HashSet<Uuid> = ops.iter().filter_map(|r| id(r, "operation")).collect();
        for (i, r) in rows(doc, "items").iter().enumerate() {
            if let Some(op) = id(r, "mfo_operation")
                && !used.contains(&op)
            {
                errors.push(FieldError::new("mfo_operation", "is not one of this BOM's operations").in_row("items", i));
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    /// Re-costs the operations at today's hour rates (also on submitted BOMs, like the
    /// platform's "Update Cost" does for materials).
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "mfo_update_operating_cost" {
            return Ok(None);
        }
        let bid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let mut fresh = doc.clone();
        let total = cost_operations(hc, &mut fresh).await?.unwrap_or_default();
        let sctx = sys(hc.ctx);
        for r in rows(&fresh, "mfo_operations") {
            let Some(line) = id(&r, "id") else { continue };
            let mut v = Doc::new();
            v.insert("hour_rate".into(), r.get("hour_rate").cloned().unwrap_or(Value::Null));
            v.insert("operating_cost".into(), r.get("operating_cost").cloned().unwrap_or(Value::Null));
            hc.kernel.set_child_values_tx(&sctx, hc.tx, "Bom", "mfo_operations", line, v).await?;
        }
        let total_cost = d(doc, "raw_material_cost") + total;
        let mut v = Doc::new();
        v.insert("operating_cost".into(), money(total));
        v.insert("total_cost".into(), money(total_cost));
        hc.kernel.set_values_tx(&sctx, hc.tx, "Bom", bid, v).await?;
        Ok(Some(json!({"operating_cost": money(total), "total_cost": money(total_cost)})))
    }
}
