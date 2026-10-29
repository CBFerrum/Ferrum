//! Item variants: attributes (a list of values with abbreviations, or a numeric range with an
//! increment), templates that vary by some of them, and `make_variants`, which creates the missing
//! variant items (code and name from the template's plus the abbreviations, every other field
//! copied from the template). Templates themselves are never transacted.

use crate::common::{child_table, installed, validation};
use async_trait::async_trait;
use ferrum_domain::trade::create_for_user;
use ferrum_domain::util::{b, get, id, rows, s, set};
use ferrum_kernel::ddl::q;
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use uuid::Uuid;

/// Most values a numeric attribute may produce, and most variants one call may create.
const MAX_VALUES: usize = 1000;
const MAX_COMBINATIONS: usize = 500;

/// `(table field, item field)` of the documents whose lines must not name a template.
fn line_fields(entity: &str) -> &'static [(&'static str, &'static str)] {
    match entity {
        "PickList" => &[("locations", "item")],
        "ProductBundle" => &[("items", "item")],
        _ => &[("items", "item")],
    }
}

/// Refuses templates on stock, sales and purchase lines.
pub struct TemplateGuard;

#[async_trait]
impl DocHook for TemplateGuard {
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let entity = hc.entity.def.name.clone();
        let mut refs: Vec<(String, usize, Uuid)> = Vec::new();
        for (table, field) in line_fields(&entity) {
            for (i, r) in rows(doc, table).iter().enumerate() {
                if let Some(item) = id(r, field) {
                    refs.push((table.to_string(), i, item));
                }
            }
        }
        if refs.is_empty() {
            return Ok(());
        }
        let ids: Vec<Uuid> = refs.iter().map(|r| r.2).collect();
        let templates: HashMap<Uuid, String> = sqlx::query_as::<_, (Uuid, String)>(
            "select id, item_code from e_item where tenant_id = $1 and id = any($2) and coalesce((ext->>'stk_has_variants')::boolean, false)",
        )
        .bind(hc.ctx.tenant)
        .bind(&ids)
        .fetch_all(&mut **hc.tx)
        .await?
        .into_iter()
        .collect();
        let errors = refs
            .iter()
            .filter_map(|(t, i, item)| {
                templates.get(item).map(|code| FieldError::new("item", format!("{code} is a template; choose one of its variants")).in_row(t, *i))
            })
            .collect();
        validation(errors)
    }
}

/// The values an attribute offers on a template: `(value, abbreviation)`.
#[derive(Debug, Clone)]
struct Choices {
    attribute: Uuid,
    name: String,
    numeric: bool,
    values: Vec<(String, String)>,
}

/// `from..=to` by `step`, as canonical decimal strings.
pub(crate) fn numeric_values(from: Decimal, to: Decimal, step: Decimal) -> std::result::Result<Vec<String>, String> {
    if step <= Decimal::ZERO {
        return Err("the increment must be positive".into());
    }
    if to < from {
        return Err("the range ends before it starts".into());
    }
    let mut out = Vec::new();
    let mut v = from;
    while v <= to {
        if out.len() >= MAX_VALUES {
            return Err(format!("the range gives more than {MAX_VALUES} values; use a larger increment"));
        }
        out.push(v.normalize().to_string());
        v += step;
    }
    Ok(out)
}

/// Whether `v` lies on the range's grid.
fn on_grid(v: Decimal, from: Decimal, to: Decimal, step: Decimal) -> bool {
    v >= from && v <= to && (step <= Decimal::ZERO || ((v - from) % step).is_zero())
}

fn dec(doc: &Doc, k: &str) -> Option<Decimal> {
    crate::common::opt_dec(doc, k)
}

/// The choices of each attribute row of a template (a row may narrow a numeric range).
async fn choices(hc: &mut HookCtx<'_>, template: &Doc) -> Result<Vec<Choices>> {
    let mut out = Vec::new();
    for r in rows(template, "stk_attributes") {
        let Some(aid) = id(&r, "attribute") else { continue };
        let a = get(hc.kernel, hc.ctx, hc.tx, "ItemAttribute", aid).await?;
        let name = s(&a, "attribute_name").unwrap_or_default().to_string();
        let numeric = b(&a, "numeric_values");
        let values = if numeric {
            let from = dec(&r, "from_range").or(dec(&a, "from_range")).unwrap_or_default();
            let to = dec(&r, "to_range").or(dec(&a, "to_range")).unwrap_or_default();
            let step = dec(&r, "increment").or(dec(&a, "increment")).unwrap_or_default();
            numeric_values(from, to, step).map_err(|m| Error::field("stk_attributes", format!("{name}: {m}")))?.into_iter().map(|v| (v.clone(), v)).collect()
        } else {
            rows(&a, "values").iter().map(|v| (s(v, "attribute_value").unwrap_or_default().to_string(), s(v, "abbr").unwrap_or_default().to_string())).collect()
        };
        out.push(Choices { attribute: aid, name, numeric, values });
    }
    Ok(out)
}

/// Same value for matching: numbers compare numerically.
fn canonical(numeric: bool, v: &str) -> String {
    if numeric { v.trim().parse::<Decimal>().map(|d| d.normalize().to_string()).unwrap_or_else(|_| v.trim().to_string()) } else { v.trim().to_string() }
}

/// Every combination of the given value lists, in order (first attribute slowest).
pub(crate) fn combinations(lists: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = vec![vec![]];
    for l in lists {
        let mut next = Vec::with_capacity(out.len() * l.len());
        for prefix in &out {
            for v in l {
                let mut c = prefix.clone();
                c.push(*v);
                next.push(c);
            }
        }
        out = next;
    }
    out
}

/// Variants of a template: id → (item code, attribute → canonical value).
async fn variants_of(hc: &mut HookCtx<'_>, template: Uuid) -> Result<HashMap<Uuid, (String, BTreeMap<Uuid, String>)>> {
    let found: Vec<(Uuid, String)> = sqlx::query_as("select id, item_code from e_item where tenant_id = $1 and ext->>'stk_variant_of' = $2")
        .bind(hc.ctx.tenant)
        .bind(template.to_string())
        .fetch_all(&mut **hc.tx)
        .await?;
    let table = child_table(hc.ctx, "Item", "stk_attributes")?;
    let ids: Vec<Uuid> = found.iter().map(|(i, _)| *i).collect();
    let attrs: Vec<(Uuid, Uuid, Option<String>, Option<bool>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select parent_id, attribute, attribute_value, numeric_values from {} where tenant_id = $1 and parent_id = any($2)",
        q(&table)
    )))
    .bind(hc.ctx.tenant)
    .bind(&ids)
    .fetch_all(&mut **hc.tx)
    .await?;
    let mut out: HashMap<Uuid, (String, BTreeMap<Uuid, String>)> = found.into_iter().map(|(i, c)| (i, (c, BTreeMap::new()))).collect();
    for (parent, attr, value, numeric) in attrs {
        if let Some(v) = out.get_mut(&parent) {
            v.1.insert(attr, canonical(numeric.unwrap_or(false), value.as_deref().unwrap_or_default()));
        }
    }
    Ok(out)
}

pub struct ItemHook;

#[async_trait]
impl DocHook for ItemHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !installed(hc.ctx) || !b(doc, "stk_has_variants") {
            return Ok(());
        }
        // A template varies by its attributes; it has no values of its own.
        let mut attrs = rows(doc, "stk_attributes");
        for r in attrs.iter_mut() {
            r.insert("attribute_value".into(), Value::Null);
        }
        set(doc, "stk_attributes", crate::common::rows_value(attrs));
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let me = id(doc, "id").unwrap_or_default();
        let template = b(doc, "stk_has_variants");
        let variant_of = id(doc, "stk_variant_of");
        let attrs = rows(doc, "stk_attributes");
        let mut errors = Vec::new();
        let mut seen = HashSet::new();
        for (i, r) in attrs.iter().enumerate() {
            if !seen.insert(id(r, "attribute")) {
                errors.push(FieldError::new("attribute", "is listed twice").in_row("stk_attributes", i));
            }
        }
        let existing = variants_of(hc, me).await?;
        if template {
            if variant_of.is_some() {
                errors.push(FieldError::new("stk_has_variants", "a variant cannot be a template itself"));
            }
            if attrs.is_empty() {
                errors.push(FieldError::new("stk_attributes", "a template needs the attributes its variants differ in"));
            }
            let mine: HashSet<Uuid> = attrs.iter().filter_map(|r| id(r, "attribute")).collect();
            if let Some((code, values)) = existing.values().next()
                && values.keys().copied().collect::<HashSet<_>>() != mine
            {
                errors.push(FieldError::new("stk_attributes", format!("variants exist (e.g. {code}); their attributes cannot change")));
            }
        } else if !existing.is_empty() {
            errors.push(FieldError::new("stk_has_variants", format!("{} variant(s) exist; it must stay a template", existing.len())));
        }
        if let Some(t) = variant_of {
            let tpl = get(hc.kernel, hc.ctx, hc.tx, "Item", t).await?;
            if !b(&tpl, "stk_has_variants") {
                errors.push(FieldError::new("stk_variant_of", "is not a template"));
            }
            let options = choices(hc, &tpl).await?;
            let given: HashMap<Uuid, String> =
                attrs.iter().filter_map(|r| Some((id(r, "attribute")?, s(r, "attribute_value").unwrap_or_default().to_string()))).collect();
            let mut key = BTreeMap::new();
            for c in &options {
                match given.get(&c.attribute) {
                    None => errors.push(FieldError::new("stk_attributes", format!("{} is missing", c.name))),
                    Some(v) => {
                        let v = canonical(c.numeric, v);
                        if !c.values.iter().any(|(x, _)| canonical(c.numeric, x) == v) {
                            errors.push(FieldError::new("stk_attributes", format!("{v} is not a value of {}", c.name)));
                        }
                        key.insert(c.attribute, v);
                    }
                }
            }
            if given.len() != options.len() {
                errors.push(FieldError::new("stk_attributes", "a variant has exactly the attributes of its template"));
            }
            let siblings = variants_of(hc, t).await?;
            if let Some((code, _)) = siblings.iter().find(|(sid, (_, k))| **sid != me && *k == key).map(|(_, v)| v) {
                errors.push(FieldError::new("stk_attributes", format!("variant {code} already has these attributes")));
            }
        } else if !template && !attrs.is_empty() {
            errors.push(FieldError::new("stk_attributes", "attributes belong to templates and their variants"));
        }
        if b(doc, "is_stock_item") {
            let bundle: Option<Uuid> = sqlx::query_scalar("select id from e_product_bundle where tenant_id = $1 and item = $2 limit 1")
                .bind(hc.ctx.tenant)
                .bind(me)
                .fetch_optional(&mut **hc.tx)
                .await?;
            if bundle.is_some() {
                errors.push(FieldError::new("is_stock_item", "this item is a product bundle; a bundle keeps no stock of its own"));
            }
        }
        validation(errors)
    }

    async fn before_delete(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) || !b(doc, "stk_has_variants") {
            return Ok(());
        }
        let n = variants_of(hc, id(doc, "id").unwrap_or_default()).await?.len();
        if n > 0 {
            return Err(Error::invalid(format!("{n} variant(s) of this template exist; delete them first")));
        }
        Ok(())
    }

    /// Creates the variants that do not exist yet: every combination, or the values chosen in
    /// `{"attributes": {"<attribute name>": ["value", ...]}}`. Calling it again creates nothing new.
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if name != "make_variants" || !installed(hc.ctx) {
            return Ok(None);
        }
        if !b(doc, "stk_has_variants") {
            return Err(Error::invalid("only templates have variants"));
        }
        let tid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let options = choices(hc, doc).await?;
        if options.is_empty() {
            return Err(Error::field("stk_attributes", "a template needs the attributes its variants differ in"));
        }
        let chosen = params.get("attributes").and_then(Value::as_object);
        let mut lists = Vec::new();
        for c in &options {
            let wanted = chosen.and_then(|m| m.get(&c.name).or_else(|| m.get(&c.attribute.to_string())));
            let idx: Vec<usize> = match wanted {
                None => (0..c.values.len()).collect(),
                Some(Value::Array(vals)) => {
                    let mut idx = Vec::new();
                    for v in vals {
                        let text = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
                        let key = canonical(c.numeric, &text);
                        let pos = c
                            .values
                            .iter()
                            .position(|(x, _)| canonical(c.numeric, x) == key)
                            .ok_or_else(|| Error::field("attributes", format!("{text} is not a value of {}", c.name)))?;
                        if !idx.contains(&pos) {
                            idx.push(pos);
                        }
                    }
                    idx
                }
                Some(_) => return Err(Error::field("attributes", format!("give the values of {} as a list", c.name))),
            };
            if idx.is_empty() {
                return Err(Error::field("attributes", format!("{} has no values to combine", c.name)));
            }
            lists.push(idx);
        }
        let combos = combinations(&lists);
        if combos.len() > MAX_COMBINATIONS {
            return Err(Error::invalid(format!("{} combinations; create at most {MAX_COMBINATIONS} at a time by choosing values", combos.len())));
        }
        let existing = variants_of(hc, tid).await?;
        let code = s(doc, "item_code").unwrap_or_default().to_string();
        let item_name = s(doc, "item_name").unwrap_or(&code).to_string();
        let (mut made, mut kept) = (Vec::new(), Vec::new());
        for combo in combos {
            let key: BTreeMap<Uuid, String> = options.iter().zip(&combo).map(|(c, i)| (c.attribute, canonical(c.numeric, &c.values[*i].0))).collect();
            if let Some((vid, (vcode, _))) = existing.iter().find(|(_, (_, k))| *k == key) {
                kept.push(json!({"id": vid, "item_code": vcode}));
                continue;
            }
            let abbrs: Vec<&str> = options.iter().zip(&combo).map(|(c, i)| c.values[*i].1.as_str()).collect();
            let vcode = format!("{code}-{}", abbrs.join("-"));
            let taken: Option<Uuid> = sqlx::query_scalar("select id from e_item where tenant_id = $1 and item_code = $2")
                .bind(hc.ctx.tenant)
                .bind(&vcode)
                .fetch_optional(&mut **hc.tx)
                .await?;
            if taken.is_some() {
                return Err(Error::Conflict(format!("item code {vcode} is taken by an item that is not this variant")));
            }
            let mut input = ferrum_kernel::doc::copy_for_new(hc.entity, &hc.ctx.schema, doc);
            for k in ["barcode", "last_purchase_rate", "stk_has_variants"] {
                input.remove(k);
            }
            input.insert("item_code".into(), json!(vcode));
            input.insert("item_name".into(), json!(format!("{item_name}-{}", abbrs.join("-"))));
            input.insert("stk_variant_of".into(), json!(tid.to_string()));
            input.insert(
                "stk_attributes".into(),
                Value::Array(
                    options.iter().zip(&combo).map(|(c, i)| json!({"attribute": c.attribute.to_string(), "attribute_value": c.values[*i].0})).collect(),
                ),
            );
            let v = create_for_user(hc, "Item", input).await?;
            made.push(json!({"id": v.get("id"), "item_code": vcode}));
        }
        Ok(Some(json!({"created": made, "existing": kept})))
    }
}

/// Validates attribute values and keeps values that variants use.
pub struct AttributeHook;

#[async_trait]
impl DocHook for AttributeHook {
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        let numeric = b(doc, "numeric_values");
        let values = rows(doc, "values");
        if numeric {
            let (from, to, step) =
                (dec(doc, "from_range").unwrap_or_default(), dec(doc, "to_range").unwrap_or_default(), dec(doc, "increment").unwrap_or_default());
            if let Err(m) = numeric_values(from, to, step) {
                errors.push(FieldError::new("increment", m));
            }
        } else {
            if values.is_empty() {
                errors.push(FieldError::new("values", "list the attribute's values"));
            }
            let (mut seen_v, mut seen_a) = (HashSet::new(), HashSet::new());
            for (i, r) in values.iter().enumerate() {
                if !seen_v.insert(s(r, "attribute_value").unwrap_or_default().trim().to_lowercase()) {
                    errors.push(FieldError::new("attribute_value", "is listed twice").in_row("values", i));
                }
                if !seen_a.insert(s(r, "abbr").unwrap_or_default().trim().to_lowercase()) {
                    errors.push(FieldError::new("abbr", "is used twice").in_row("values", i));
                }
            }
        }
        // Values that variants carry must stay valid.
        if let Some(aid) = id(doc, "id")
            && hc.ctx.schema.entity("Item").is_some_and(|e| e.children.contains_key("stk_attributes"))
        {
            let table = child_table(hc.ctx, "Item", "stk_attributes")?;
            let used: Vec<(Option<String>, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "select a.attribute_value, i.item_code from {} a join e_item i on i.tenant_id = a.tenant_id and i.id = a.parent_id \
                 where a.tenant_id = $1 and a.attribute = $2 and coalesce(a.attribute_value, '') <> ''",
                q(&table)
            )))
            .bind(hc.ctx.tenant)
            .bind(aid)
            .fetch_all(&mut **hc.tx)
            .await?;
            for (value, code) in used {
                let v = value.unwrap_or_default();
                let ok = if numeric {
                    v.parse::<Decimal>().is_ok_and(|x| {
                        on_grid(
                            x,
                            dec(doc, "from_range").unwrap_or_default(),
                            dec(doc, "to_range").unwrap_or_default(),
                            dec(doc, "increment").unwrap_or_default(),
                        )
                    })
                } else {
                    values.iter().any(|r| s(r, "attribute_value") == Some(v.as_str()))
                };
                if !ok {
                    errors.push(FieldError::new("values", format!("variant {code} uses {v}; keep that value")));
                    break;
                }
            }
        }
        validation(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn numeric_ranges_step_exactly() {
        assert_eq!(numeric_values(n("1"), n("2"), n("0.5")).unwrap(), vec!["1", "1.5", "2"]);
        assert_eq!(numeric_values(n("0.1"), n("0.3"), n("0.1")).unwrap(), vec!["0.1", "0.2", "0.3"], "no float drift");
        assert!(numeric_values(n("1"), n("2"), n("0")).is_err());
        assert!(numeric_values(n("0"), n("5000"), n("1")).is_err(), "too many values");
        assert!(on_grid(n("1.5"), n("1"), n("2"), n("0.5")) && !on_grid(n("1.25"), n("1"), n("2"), n("0.5")));
    }

    #[test]
    fn combinations_take_every_choice_once() {
        let c = combinations(&[vec![0, 1], vec![0, 1, 2]]);
        assert_eq!(c.len(), 6);
        assert_eq!(c[0], vec![0, 0]);
        assert_eq!(c[5], vec![1, 2]);
        assert_eq!(canonical(true, "2.50"), "2.5");
        assert_eq!(canonical(false, " Red "), "Red");
    }
}
