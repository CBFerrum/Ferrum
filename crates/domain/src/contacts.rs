//! Contacts and addresses (the `contacts` package, docs/plan/apps-ui.md §13): a contact has several
//! emails and phones, one primary of each; an address gets a title and its postal form, and is
//! the primary one of its type for each record it links to. What an address links to comes from
//! other packages (customers and suppliers from `parties`, leads from CRM); a customer's or
//! supplier's primary address is copied onto it, where selling and buying documents read it.

use crate::util::{b, find, id, rows, s, set, sys};
use async_trait::async_trait;
use ferrum_kernel::{Doc, DocHook, Error, FieldError, Filter, HookCtx, Op, Result};
use ferrum_meta::FieldType;
use serde_json::{Value, json};
use std::collections::HashSet;
use uuid::Uuid;

/// "First Last" from the parts that are present.
pub fn person_name(first: Option<&str>, last: Option<&str>) -> String {
    [first, last].into_iter().flatten().map(str::trim).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ")
}

/// A record by id, read with engine rights; `None` when absent.
async fn lookup(hc: &mut HookCtx<'_>, entity: &str, record: Option<Uuid>) -> Result<Option<Doc>> {
    let Some(record) = record else { return Ok(None) };
    match hc.kernel.get_tx(&sys(hc.ctx), hc.tx, entity, record, false).await {
        Ok(doc) => Ok(Some(doc)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

pub struct ContactHook;

#[async_trait]
impl DocHook for ContactHook {
    async fn before_validate(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        set(doc, "full_name", person_name(s(doc, "first_name"), s(doc, "last_name")));
        for (table, field, header) in [("emails", "email", "email"), ("phones", "phone", "phone")] {
            let mut list = rows(doc, table);
            if !list.is_empty() && !list.iter().any(|r| b(r, "is_primary")) {
                list[0].insert("is_primary".into(), json!(true));
            }
            let primary = list.iter().find(|r| b(r, "is_primary")).and_then(|r| s(r, field)).map(str::to_string);
            set(doc, header, json!(primary));
            set(doc, table, Value::Array(list.into_iter().map(Value::Object).collect()));
        }
        Ok(())
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        for (table, field) in [("emails", "email"), ("phones", "phone")] {
            let mut seen = HashSet::new();
            let mut primaries = 0;
            for (i, r) in rows(doc, table).iter().enumerate() {
                if b(r, "is_primary") {
                    primaries += 1;
                    if primaries > 1 {
                        errors.push(FieldError::new("is_primary", format!("only one {field} can be primary")).in_row(table, i));
                    }
                }
                if let Some(v) = s(r, field)
                    && !seen.insert(v.to_lowercase())
                {
                    errors.push(FieldError::new(field, format!("{v} is listed twice")).in_row(table, i));
                }
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }
}

/// Multi-line postal form: street lines, "City, State Postcode", country.
pub fn format_address(doc: &Doc) -> String {
    let mut locality = [s(doc, "city"), s(doc, "state")].into_iter().flatten().collect::<Vec<_>>().join(", ");
    if let Some(pc) = s(doc, "postal_code") {
        if !locality.is_empty() {
            locality.push(' ');
        }
        locality.push_str(pc);
    }
    let locality = Some(locality.as_str()).filter(|l| !l.is_empty());
    [s(doc, "address_line1"), s(doc, "address_line2"), locality, s(doc, "country")].into_iter().flatten().collect::<Vec<_>>().join("\n")
}

/// The records an address links to in this workspace: (link field, entity), in field order.
fn links(hc: &HookCtx<'_>) -> Vec<(String, String)> {
    hc.entity.def.fields.iter().filter(|f| f.ftype == FieldType::Link).filter_map(|f| Some((f.name.clone(), f.to.clone()?))).collect()
}

pub struct AddressHook;

#[async_trait]
impl DocHook for AddressHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if s(doc, "address_title").is_none() {
            // The title of the first record it links to, else its first line.
            let mut title = None;
            for (link, entity) in links(hc) {
                if let Some(p) = lookup(hc, &entity, id(doc, &link)).await? {
                    let field = hc.ctx.schema.entity(&entity).and_then(|e| e.def.title_field.clone());
                    title = field.and_then(|f| s(&p, &f).map(str::to_string));
                    break;
                }
            }
            set(doc, "address_title", json!(title.or_else(|| s(doc, "address_line1").map(str::to_string))));
        }
        set(doc, "display", format_address(doc));
        Ok(())
    }

    /// One primary address per linked record and address type: this one replaces the previous.
    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, _old: Option<&Doc>) -> Result<()> {
        if !b(doc, "is_primary") {
            return Ok(());
        }
        let sctx = sys(hc.ctx);
        let kind = s(doc, "address_type").unwrap_or("Billing").to_string();
        let me = id(doc, "id").map(|u| u.to_string()).unwrap_or_default();
        for (link, _) in links(hc) {
            let Some(record) = id(doc, &link) else { continue };
            let others = find(
                hc.kernel,
                hc.ctx,
                hc.tx,
                "Address",
                vec![
                    Filter::eq(&link, record.to_string()),
                    Filter::eq("address_type", kind.as_str()),
                    Filter::eq("is_primary", true),
                    Filter { field: "id".into(), op: Op::Ne, value: json!(me) },
                ],
                None,
                1000,
            )
            .await?;
            for o in others {
                let Some(oid) = id(&o, "id") else { continue };
                let mut v = Doc::new();
                v.insert("is_primary".into(), json!(false));
                hc.kernel.set_values_tx(&sctx, hc.tx, "Address", oid, v).await?;
            }
        }
        Ok(())
    }
}

/// `parties`: a customer's primary billing and shipping addresses and a supplier's primary billing
/// address are copied onto the customer or supplier.
pub struct PartyAddressHook;

#[async_trait]
impl DocHook for PartyAddressHook {
    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, _old: Option<&Doc>) -> Result<()> {
        if !b(doc, "is_primary") {
            return Ok(());
        }
        let sctx = sys(hc.ctx);
        let kind = s(doc, "address_type").unwrap_or("Billing");
        for (link, entity) in [("customer", "Customer"), ("supplier", "Supplier")] {
            let Some(party) = id(doc, link) else { continue };
            let field = match (entity, kind) {
                ("Customer", "Billing") => "billing_address",
                ("Customer", "Shipping") => "shipping_address",
                ("Supplier", "Billing") => "address",
                _ => continue,
            };
            let mut v = Doc::new();
            v.insert(field.into(), json!(s(doc, "display")));
            hc.kernel.set_values_tx(&sctx, hc.tx, entity, party, v).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postal_format() {
        let doc = |v: Value| v.as_object().cloned().unwrap();
        let full = doc(json!({"address_line1": "12 MG Road", "address_line2": "Suite 4", "city": "Bengaluru", "state": "Karnataka",
            "postal_code": "560001", "country": "India"}));
        assert_eq!(format_address(&full), "12 MG Road\nSuite 4\nBengaluru, Karnataka 560001\nIndia");
        let short = doc(json!({"address_line1": "1 High St", "postal_code": "SW1A 1AA", "address_line2": ""}));
        assert_eq!(format_address(&short), "1 High St\nSW1A 1AA");
        assert_eq!(person_name(Some(" Ada "), None), "Ada");
    }
}
