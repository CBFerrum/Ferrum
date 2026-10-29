//! Workstation types and workstations (hour rate from its components, working hours), and the
//! defaults operations give to routing rows.

use crate::calendar::{Calendar, parse_hhmm, weekday_index};
use crate::{opt_dec, rows_value};
use async_trait::async_trait;
use ferrum_domain::util::{d, get, id, money, rows, s, set};
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use std::collections::HashMap;
use uuid::Uuid;

const RATE_PARTS: [&str; 4] = ["hour_rate_electricity", "hour_rate_consumable", "hour_rate_rent", "hour_rate_labour"];

fn sum_rate(doc: &mut Doc) {
    let total: Decimal = RATE_PARTS.iter().map(|k| d(doc, k)).sum();
    set(doc, "hour_rate", money(total));
}

pub struct WorkstationTypeHook;

#[async_trait]
impl DocHook for WorkstationTypeHook {
    async fn before_validate(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        sum_rate(doc);
        Ok(())
    }
}

/// A workstation's working calendar (an empty table means around the clock).
pub(crate) fn calendar_of(ws: &Doc) -> Calendar {
    let hours: Vec<(usize, u32, u32)> = rows(ws, "working_hours")
        .iter()
        .filter_map(|r| Some((weekday_index(s(r, "weekday")?)?, parse_hhmm(s(r, "start_time")?)?, parse_hhmm(s(r, "end_time")?)?)))
        .collect();
    let holidays = rows(ws, "holidays").iter().filter_map(|r| s(r, "holiday_date").and_then(|x| ferrum_kernel::value::parse_date(x).ok())).collect::<Vec<_>>();
    Calendar::new(&hours, holidays)
}

/// Jobs a workstation runs at once (at least one).
pub(crate) fn capacity_of(ws: &Doc) -> usize {
    ws.get("production_capacity").and_then(serde_json::Value::as_i64).unwrap_or(1).max(1) as usize
}

pub struct WorkstationHook;

#[async_trait]
impl DocHook for WorkstationHook {
    /// Rate components left empty come from the workstation type ("fetch if empty"), so rates
    /// typed on the workstation are never overwritten by its type.
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if let Some(t) = id(doc, "workstation_type")
            && RATE_PARTS.iter().any(|k| opt_dec(doc, k).is_none())
        {
            let ty = get(hc.kernel, hc.ctx, hc.tx, "WorkstationType", t).await?;
            for k in RATE_PARTS {
                if opt_dec(doc, k).is_none() {
                    set(doc, k, money(d(&ty, k)));
                }
            }
        }
        sum_rate(doc);
        Ok(())
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        let mut by_day: HashMap<usize, Vec<(u32, u32, usize)>> = HashMap::new();
        for (i, r) in rows(doc, "working_hours").iter().enumerate() {
            let (Some(day), Some(start), Some(end)) =
                (s(r, "weekday").and_then(weekday_index), s(r, "start_time").and_then(parse_hhmm), s(r, "end_time").and_then(parse_hhmm))
            else {
                continue; // the row's own rules report the format
            };
            if end <= start {
                errors.push(FieldError::new("end_time", "must be after the start time").in_row("working_hours", i));
                continue;
            }
            by_day.entry(day).or_default().push((start, end, i));
        }
        for spans in by_day.values_mut() {
            spans.sort_unstable();
            for w in spans.windows(2) {
                if w[1].0 < w[0].1 {
                    errors.push(FieldError::new("start_time", "overlaps another shift on the same day").in_row("working_hours", w[1].2));
                }
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }
}

/// Fills an operation row's workstation and batch size from the operation's defaults.
pub(crate) async fn op_defaults(hc: &mut HookCtx<'_>, ops: &mut [Doc], cache: &mut HashMap<Uuid, Doc>) -> Result<()> {
    for r in ops.iter_mut() {
        let Some(op) = id(r, "operation") else { continue };
        let needs_ws = id(r, "workstation").is_none();
        let needs_batch = opt_dec(r, "batch_size").is_none_or(|b| b <= Decimal::ZERO);
        if !needs_ws && !needs_batch {
            continue;
        }
        let o = crate::cached(hc, cache, "Operation", op).await?;
        if needs_ws && let Some(ws) = id(&o, "workstation") {
            set(r, "workstation", ws.to_string());
        }
        if needs_batch {
            let b = d(&o, "batch_size");
            set(r, "batch_size", money(if b > Decimal::ZERO { b } else { Decimal::ONE }));
        }
    }
    Ok(())
}

pub struct RoutingHook;

#[async_trait]
impl DocHook for RoutingHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let mut ops = rows(doc, "operations");
        op_defaults(hc, &mut ops, &mut HashMap::new()).await?;
        set(doc, "operations", rows_value(ops));
        Ok(())
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let errors: Vec<FieldError> = rows(doc, "operations")
            .iter()
            .enumerate()
            .filter(|(_, r)| id(r, "workstation").is_none())
            .map(|(i, _)| FieldError::new("workstation", "is required (the operation has no default workstation)").in_row("operations", i))
            .collect();
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    /// BOMs keep their routing in an extension field, which has no foreign key to stop this.
    async fn before_delete(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let used: i64 = sqlx::query_scalar("select count(*) from e_bom where tenant_id = $1 and ext->>'mfo_routing' = $2")
            .bind(hc.ctx.tenant)
            .bind(s(doc, "id"))
            .fetch_one(&mut **hc.tx)
            .await?;
        if used > 0 {
            return Err(Error::invalid(format!("{used} BOM(s) use this routing; disable it instead")));
        }
        Ok(())
    }
}
