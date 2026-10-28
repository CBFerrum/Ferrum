//! Contacts (several emails and phones, one primary of each) and addresses (one primary per party
//! and address type; a customer's or supplier's primary address is copied onto the party, where
//! selling and buying documents read it).

use crate::{lookup, person_name};
use async_trait::async_trait;
use ferrum_domain::util::{b, find, id, rows, s, set, sys};
use ferrum_kernel::{Doc, DocHook, Error, FieldError, Filter, HookCtx, Op, Result};
use serde_json::{Value, json};
use std::collections::HashSet;

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
pub(crate) fn format_address(doc: &Doc) -> String {
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

/// Party links of an address: (link field, party entity, party name field, party address field).
const PARTIES: [(&str, &str, &str, Option<&str>); 3] =
    [("customer", "Customer", "customer_name", None), ("supplier", "Supplier", "supplier_name", Some("address")), ("lead", "Lead", "lead_name", None)];

pub struct AddressHook;

#[async_trait]
impl DocHook for AddressHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if s(doc, "address_title").is_none() {
            let mut title = None;
            for (link, entity, name_field, _) in PARTIES {
                if let Some(p) = lookup(hc, entity, id(doc, link)).await? {
                    title = s(&p, name_field).map(str::to_string);
                    break;
                }
            }
            set(doc, "address_title", json!(title.or_else(|| s(doc, "address_line1").map(str::to_string))));
        }
        set(doc, "display", format_address(doc));
        Ok(())
    }

    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, _old: Option<&Doc>) -> Result<()> {
        if !b(doc, "is_primary") {
            return Ok(());
        }
        let sctx = sys(hc.ctx);
        let kind = s(doc, "address_type").unwrap_or("Billing").to_string();
        let me = id(doc, "id").map(|u| u.to_string()).unwrap_or_default();
        for (link, entity, _, party_field) in PARTIES {
            let Some(party) = id(doc, link) else { continue };
            // One primary address per party and type: this one replaces the previous one.
            let others = find(
                hc.kernel,
                hc.ctx,
                hc.tx,
                "Address",
                vec![
                    Filter::eq(link, party.to_string()),
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
            let field = match (entity, kind.as_str()) {
                ("Customer", "Billing") => Some("billing_address"),
                ("Customer", "Shipping") => Some("shipping_address"),
                (_, "Billing") => party_field,
                _ => None,
            };
            if let Some(field) = field {
                let mut v = Doc::new();
                v.insert(field.into(), json!(s(doc, "display")));
                hc.kernel.set_values_tx(&sctx, hc.tx, entity, party, v).await?;
            }
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
    }
}
