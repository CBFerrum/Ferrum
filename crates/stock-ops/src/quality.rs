//! Quality inspection: templates of parameters with acceptance criteria (numeric range, accepted
//! values, a CEL formula over the readings), inspections of a receipt, delivery or stock entry line
//! whose status follows from the readings, and the gate that refuses to submit a receipt or a
//! delivery whose items need an accepted inspection first.

use crate::common::{finish_rows, installed, items, rows_value, validation};
use async_trait::async_trait;
use ferrum_domain::flow::created;
use ferrum_domain::trade::create_for_user;
use ferrum_domain::util::{b, d, get, id, money, rows, s, set};
use ferrum_kernel::ddl::q;
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use ferrum_meta::{Expr, Scope};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json::{Value, json};
use std::collections::HashSet;
use uuid::Uuid;

pub const ENTITY: &str = "QualityInspection";

/// `(reference type label, entity, link field on the inspection)`.
const REFERENCES: &[(&str, &str, &str)] = &[
    ("Purchase Receipt", "PurchaseReceipt", "purchase_receipt"),
    ("Purchase Invoice", "PurchaseInvoice", "purchase_invoice"),
    ("Delivery Note", "DeliveryNote", "delivery_note"),
    ("Sales Invoice", "SalesInvoice", "sales_invoice"),
    ("Stock Entry", "StockEntry", "stock_entry"),
];

fn by_label(label: &str) -> Option<(&'static str, &'static str, &'static str)> {
    REFERENCES.iter().copied().find(|r| r.0 == label)
}

fn by_entity(entity: &str) -> Option<(&'static str, &'static str, &'static str)> {
    REFERENCES.iter().copied().find(|r| r.1 == entity)
}

/// The item flag that makes an inspection mandatory before submitting `entity` (none for stock entries).
fn required_flag(entity: &str) -> Option<&'static str> {
    match entity {
        "PurchaseReceipt" | "PurchaseInvoice" => Some("stk_inspection_required_before_purchase"),
        "DeliveryNote" | "SalesInvoice" => Some("stk_inspection_required_before_delivery"),
        _ => None,
    }
}

fn default_type(label: &str) -> &'static str {
    match label {
        "Purchase Receipt" | "Purchase Invoice" => "Incoming",
        "Delivery Note" | "Sales Invoice" => "Outgoing",
        _ => "In Process",
    }
}

fn values(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default().split(['\n', ',']).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string).collect()
}

fn opt(doc: &Doc, k: &str) -> Option<Decimal> {
    doc.get(k).filter(|v| !v.is_null()).and_then(|v| ferrum_kernel::value::parse_decimal(v).ok())
}

/// Whether one parameter's readings pass its criteria (`None` while nothing was read).
pub(crate) fn judge(row: &Doc) -> std::result::Result<Option<bool>, String> {
    let readings = values(s(row, "readings"));
    if readings.is_empty() {
        return Ok(None);
    }
    let numeric = row.get("numeric").and_then(Value::as_bool).unwrap_or(true);
    let mut ok = true;
    let view = if numeric {
        let mut nums = Vec::with_capacity(readings.len());
        for r in &readings {
            nums.push(r.parse::<Decimal>().map_err(|_| format!("'{r}' is not a number"))?);
        }
        let (min, max) = (opt(row, "min_value"), opt(row, "max_value"));
        ok &= nums.iter().all(|v| min.is_none_or(|m| *v >= m) && max.is_none_or(|m| *v <= m));
        let mean = nums.iter().copied().sum::<Decimal>() / Decimal::from(nums.len());
        let f = |x: Decimal| x.to_f64().unwrap_or(0.0);
        json!({
            "readings": nums.iter().map(|x| f(*x)).collect::<Vec<_>>(),
            "mean": f(mean),
            "min": f(nums.iter().copied().min().unwrap_or_default()),
            "max": f(nums.iter().copied().max().unwrap_or_default()),
            "count": nums.len(),
            "value": readings[0],
        })
    } else {
        let accepted = values(s(row, "accepted_values"));
        if !accepted.is_empty() {
            ok &= readings.iter().all(|r| accepted.contains(r));
        }
        json!({"readings": readings, "count": readings.len(), "value": readings[0]})
    };
    if let Some(src) = s(row, "formula") {
        let expr = Expr::compile(src).map_err(|e| format!("acceptance formula: {e}"))?;
        let user = json!({});
        ok &= expr.eval_bool(&Scope { doc: &view, old: None, parent: None, user: &user }).map_err(|e| format!("acceptance formula: {e}"))?;
    }
    Ok(Some(ok))
}

/// Checks the criteria of template parameters and inspection readings.
fn criteria_errors(rows: &[Doc], table: &str, errors: &mut Vec<FieldError>) {
    let mut seen = HashSet::new();
    for (i, r) in rows.iter().enumerate() {
        let name = s(r, "parameter").unwrap_or_default().trim().to_lowercase();
        if !seen.insert(name) {
            errors.push(FieldError::new("parameter", "is listed twice").in_row(table, i));
        }
        if let (Some(lo), Some(hi)) = (opt(r, "min_value"), opt(r, "max_value"))
            && lo > hi
        {
            errors.push(FieldError::new("max_value", "is below the minimum").in_row(table, i));
        }
        if let Some(src) = s(r, "formula")
            && let Err(e) = Expr::compile(src)
        {
            errors.push(FieldError::new("formula", e).in_row(table, i));
        }
        let numeric = r.get("numeric").and_then(Value::as_bool).unwrap_or(true);
        if !numeric && values(s(r, "accepted_values")).is_empty() && s(r, "formula").is_none() {
            errors.push(FieldError::new("accepted_values", "list the accepted values or give a formula").in_row(table, i));
        }
    }
}

pub struct TemplateHook;

#[async_trait]
impl DocHook for TemplateHook {
    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        criteria_errors(&rows(doc, "parameters"), "parameters", &mut errors);
        validation(errors)
    }
}

/// The inspected document and its lines of the inspected item: `(doc, [(line id, qty)])`.
async fn reference(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Option<(Doc, Vec<(String, Decimal)>)>> {
    let Some((_, entity, field)) = s(doc, "reference_type").and_then(by_label) else { return Ok(None) };
    let Some(rid) = id(doc, field) else { return Ok(None) };
    let rd = ferrum_domain::util::get(hc.kernel, hc.ctx, hc.tx, entity, rid).await?;
    let item = id(doc, "item");
    let lines = rows(&rd, "items")
        .into_iter()
        .filter(|r| id(r, "item") == item && item.is_some())
        .map(|r| (s(&r, "id").unwrap_or_default().to_string(), d(&r, "qty").abs()))
        .collect();
    Ok(Some((rd, lines)))
}

pub struct InspectionHook;

#[async_trait]
impl DocHook for InspectionHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        // Only one reference link is kept: the one the reference type names.
        let kind = s(doc, "reference_type").and_then(by_label);
        for (_, _, field) in REFERENCES {
            if kind.is_some_and(|k| k.2 != *field) && doc.get(*field).is_some_and(|v| !v.is_null()) {
                set(doc, field, Value::Null);
            }
        }
        if s(doc, "inspection_type").is_none()
            && let Some((label, _, _)) = kind
        {
            set(doc, "inspection_type", default_type(label));
        }
        if rows(doc, "readings").is_empty()
            && let Some(t) = id(doc, "template")
        {
            let tpl = get(hc.kernel, hc.ctx, hc.tx, "QualityInspectionTemplate", t).await?;
            let mut out: Vec<Doc> = rows(&tpl, "parameters")
                .into_iter()
                .map(|p| {
                    let mut r = Doc::new();
                    for k in ["parameter", "numeric", "min_value", "max_value", "accepted_values", "formula"] {
                        r.insert(k.into(), p.get(k).cloned().unwrap_or(Value::Null));
                    }
                    r
                })
                .collect();
            finish_rows(&mut out);
            set(doc, "readings", rows_value(out));
        }
        if b(doc, "manual_inspection") {
            return Ok(());
        }
        let mut readings = rows(doc, "readings");
        let mut verdicts = Vec::new();
        for r in readings.iter_mut() {
            let v = judge(r).ok().flatten();
            r.insert("status".into(), v.map(|ok| json!(if ok { "Accepted" } else { "Rejected" })).unwrap_or(Value::Null));
            verdicts.push(v);
        }
        let status = if verdicts.is_empty() || verdicts.iter().any(Option::is_none) {
            if verdicts.contains(&Some(false)) { json!("Rejected") } else { Value::Null }
        } else if verdicts.iter().all(|v| *v == Some(true)) {
            json!("Accepted")
        } else {
            json!("Rejected")
        };
        set(doc, "readings", rows_value(readings));
        set(doc, "status", status);
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        let readings = rows(doc, "readings");
        criteria_errors(&readings, "readings", &mut errors);
        for (i, r) in readings.iter().enumerate() {
            if let Err(m) = judge(r) {
                errors.push(FieldError::new("readings", m).in_row("readings", i));
            }
        }
        if let Some((rd, lines)) = reference(hc, doc).await? {
            let field = s(doc, "reference_type").and_then(by_label).map(|r| r.2).unwrap_or("reference_type");
            if id(&rd, "company") != id(doc, "company") {
                errors.push(FieldError::new(field, "belongs to another company"));
            }
            if d(&rd, "docstatus") == Decimal::TWO {
                errors.push(FieldError::new(field, "is cancelled"));
            }
            let inspected: Decimal = match s(doc, "reference_line") {
                Some(line) => match lines.iter().find(|(l, _)| l == line) {
                    Some((_, q)) => *q,
                    None => {
                        errors.push(FieldError::new("reference_line", "is not a line of this item on the document"));
                        Decimal::ZERO
                    }
                },
                None => {
                    if lines.is_empty() {
                        errors.push(FieldError::new("item", "is not on the referenced document"));
                    }
                    lines.iter().map(|(_, q)| *q).sum()
                }
            };
            if !inspected.is_zero() && d(doc, "sample_size") > inspected {
                errors.push(FieldError::new("sample_size", format!("cannot exceed the inspected quantity ({})", inspected.normalize())));
            }
        }
        validation(errors)
    }

    async fn before_submit(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if b(doc, "manual_inspection") {
            if s(doc, "status").is_none() {
                return Err(Error::field("status", "set Accepted or Rejected for a manual inspection"));
            }
            return Ok(());
        }
        let readings = rows(doc, "readings");
        if readings.is_empty() {
            return Err(Error::field("readings", "add the readings (or a template), or mark the inspection as manual"));
        }
        let mut errors = Vec::new();
        for (i, r) in readings.iter().enumerate() {
            if values(s(r, "readings")).is_empty() {
                errors.push(
                    FieldError::new("readings", format!("enter the readings of {}", s(r, "parameter").unwrap_or("this parameter"))).in_row("readings", i),
                );
            }
        }
        validation(errors)
    }

    /// An accepted inspection that let a document be submitted stays while that document does.
    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if s(doc, "status") != Some("Accepted") {
            return Ok(());
        }
        let Some((rd, _)) = reference(hc, doc).await? else { return Ok(()) };
        let entity = s(doc, "reference_type").and_then(by_label).map(|r| r.1).unwrap_or_default();
        let Some(flag) = required_flag(entity) else { return Ok(()) };
        let item = get(hc.kernel, hc.ctx, hc.tx, "Item", id(doc, "item").unwrap_or_default()).await?;
        if d(&rd, "docstatus") == Decimal::ONE && b(&item, flag) {
            return Err(Error::invalid(format!(
                "{} {} was submitted on this inspection; cancel it first",
                s(doc, "reference_type").unwrap_or_default(),
                s(&rd, "number").unwrap_or_default()
            )));
        }
        Ok(())
    }
}

/// Refuses to submit receipts and deliveries whose items need an accepted inspection first, and
/// creates the missing inspections on request.
pub struct InspectionGate;

impl InspectionGate {
    /// Whether the document moves stock (returns are not inspected).
    fn moves_stock(entity: &str, doc: &Doc) -> bool {
        !b(doc, "is_return") && (matches!(entity, "PurchaseReceipt" | "DeliveryNote") || b(doc, "update_stock"))
    }
}

#[async_trait]
impl DocHook for InspectionGate {
    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let entity = hc.entity.def.name.clone();
        let (Some(flag), Some((_, _, field))) = (required_flag(&entity), by_entity(&entity)) else { return Ok(()) };
        if !installed(hc.ctx) || !Self::moves_stock(&entity, doc) {
            return Ok(());
        }
        let lines = rows(doc, "items");
        let ids: Vec<Uuid> = lines.iter().filter_map(|r| id(r, "item")).collect();
        let required: Vec<(Uuid, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select id, item_code from e_item where tenant_id = $1 and id = any($2) and coalesce((ext->>'{flag}')::boolean, false)"
        )))
        .bind(hc.ctx.tenant)
        .bind(&ids)
        .fetch_all(&mut **hc.tx)
        .await?;
        if required.is_empty() {
            return Ok(());
        }
        let did = id(doc, "id");
        let table = hc.ctx.schema.entity(ENTITY).map(|e| e.table.clone()).unwrap_or_default();
        let when = if flag.ends_with("purchase") { "purchase" } else { "delivery" };
        let mut errors = Vec::new();
        for (i, r) in lines.iter().enumerate() {
            let Some((item, code)) = id(r, "item").and_then(|x| required.iter().find(|(r, _)| *r == x)) else { continue };
            let line = s(r, "id").unwrap_or_default();
            // The latest inspection of this line (or of the item on the whole document) decides.
            let status: Option<Option<String>> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "select status from {} where tenant_id = $1 and docstatus = 1 and {} = $2 and item = $3 \
                 and (reference_line = $4 or coalesce(reference_line, '') = '') \
                 order by (reference_line = $4) desc nulls last, report_date desc, created_at desc limit 1",
                q(&table),
                q(field)
            )))
            .bind(hc.ctx.tenant)
            .bind(did)
            .bind(item)
            .bind(line)
            .fetch_optional(&mut **hc.tx)
            .await?;
            match status.flatten().as_deref() {
                Some("Accepted") => {}
                Some(_) => errors.push(FieldError::new("item", format!("{code} was rejected by its quality inspection")).in_row("items", i)),
                None => errors.push(FieldError::new("item", format!("{code} needs an accepted quality inspection before {when}")).in_row("items", i)),
            }
        }
        validation(errors)
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "make_quality_inspections" || !installed(hc.ctx) {
            return Ok(None);
        }
        let entity = hc.entity.def.name.clone();
        let Some((label, _, field)) = by_entity(&entity) else { return Ok(None) };
        let did = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let lines = rows(doc, "items");
        let ids: Vec<Uuid> = lines.iter().filter_map(|r| id(r, "item")).collect();
        let infos = items(hc.tx, hc.ctx.tenant, &ids).await?;
        let table = hc.ctx.schema.entity(ENTITY).map(|e| e.table.clone()).unwrap_or_default();
        let done: Vec<Option<String>> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select reference_line from {} where tenant_id = $1 and docstatus < 2 and {} = $2",
            q(&table),
            q(field)
        )))
        .bind(hc.ctx.tenant)
        .bind(did)
        .fetch_all(&mut **hc.tx)
        .await?;
        let mut made = Vec::new();
        for r in &lines {
            let Some(info) = id(r, "item").and_then(|i| infos.get(&i)) else { continue };
            let item = get(hc.kernel, hc.ctx, hc.tx, "Item", info.id).await?;
            let needed = match required_flag(&entity) {
                Some(flag) => b(&item, flag),
                None => id(&item, "stk_quality_inspection_template").is_some(),
            };
            let line = s(r, "id").unwrap_or_default().to_string();
            if !needed || !info.is_stock || done.iter().any(|l| l.as_deref() == Some(line.as_str())) {
                continue;
            }
            let mut input = Doc::new();
            input.insert("company".into(), doc.get("company").cloned().unwrap_or(Value::Null));
            input.insert("reference_type".into(), json!(label));
            input.insert(field.into(), json!(did.to_string()));
            input.insert("reference_line".into(), json!(line));
            input.insert("item".into(), json!(info.id.to_string()));
            input.insert("sample_size".into(), money(d(r, "qty").abs()));
            for k in ["batch_no", "serial_no"] {
                if let Some(v) = r.get(k).filter(|v| !v.is_null()) {
                    input.insert(k.into(), v.clone());
                }
            }
            if let Some(t) = item.get("stk_quality_inspection_template").filter(|v| !v.is_null()) {
                input.insert("template".into(), t.clone());
            }
            let qi = create_for_user(hc, ENTITY, input).await?;
            made.push(created(ENTITY, &qi));
        }
        Ok(Some(json!({"created": made})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(v: Value) -> Doc {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn numeric_readings_need_every_value_in_range() {
        let r = row(json!({"numeric": true, "min_value": "9.5", "max_value": "10.5", "readings": "9.8, 10.1\n10.5"}));
        assert_eq!(judge(&r), Ok(Some(true)));
        let r = row(json!({"numeric": true, "min_value": "9.5", "max_value": "10.5", "readings": "9.8, 10.6"}));
        assert_eq!(judge(&r), Ok(Some(false)));
        assert!(judge(&row(json!({"numeric": true, "readings": "ten"}))).is_err());
        assert_eq!(judge(&row(json!({"numeric": true, "readings": ""}))), Ok(None));
    }

    #[test]
    fn formulas_see_the_statistics() {
        let r = row(json!({"numeric": true, "readings": "9, 11", "formula": "doc.mean == 10.0 && doc.count == 2 && doc.max <= 11.0"}));
        assert_eq!(judge(&r), Ok(Some(true)));
        let r = row(json!({"numeric": true, "readings": "9, 12", "formula": "doc.mean <= 10.0"}));
        assert_eq!(judge(&r), Ok(Some(false)));
    }

    #[test]
    fn text_readings_must_be_accepted_values() {
        let r = row(json!({"numeric": false, "accepted_values": "Red\nMaroon", "readings": "Red"}));
        assert_eq!(judge(&r), Ok(Some(true)));
        let r = row(json!({"numeric": false, "accepted_values": "Red", "readings": "Red, Blue"}));
        assert_eq!(judge(&r), Ok(Some(false)));
    }
}
