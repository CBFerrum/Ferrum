//! India GST: GSTIN validation (format, state and checksum), place of supply, automatic choice
//! between intra-state (CGST+SGST), inter-state (IGST) and export templates, and HSN checks.

use crate::pricing::Side;
use crate::util::{b, get, id, rows, s, set, sys};
use async_trait::async_trait;
use ferrum_kernel::{Doc, DocHook, Error, FieldError, Filter, HookCtx, Result};
use serde_json::Value;

pub const STATES: &[(&str, &str)] = &[
    ("01", "Jammu and Kashmir"),
    ("02", "Himachal Pradesh"),
    ("03", "Punjab"),
    ("04", "Chandigarh"),
    ("05", "Uttarakhand"),
    ("06", "Haryana"),
    ("07", "Delhi"),
    ("08", "Rajasthan"),
    ("09", "Uttar Pradesh"),
    ("10", "Bihar"),
    ("11", "Sikkim"),
    ("12", "Arunachal Pradesh"),
    ("13", "Nagaland"),
    ("14", "Manipur"),
    ("15", "Mizoram"),
    ("16", "Tripura"),
    ("17", "Meghalaya"),
    ("18", "Assam"),
    ("19", "West Bengal"),
    ("20", "Jharkhand"),
    ("21", "Odisha"),
    ("22", "Chhattisgarh"),
    ("23", "Madhya Pradesh"),
    ("24", "Gujarat"),
    ("26", "Dadra and Nagar Haveli and Daman and Diu"),
    ("27", "Maharashtra"),
    ("29", "Karnataka"),
    ("30", "Goa"),
    ("31", "Lakshadweep"),
    ("32", "Kerala"),
    ("33", "Tamil Nadu"),
    ("34", "Puducherry"),
    ("35", "Andaman and Nicobar Islands"),
    ("36", "Telangana"),
    ("37", "Andhra Pradesh"),
    ("38", "Ladakh"),
    ("97", "Other Territory"),
    ("99", "Centre Jurisdiction"),
];

/// Place-of-supply code used for exports.
pub const FOREIGN: &str = "96";

pub fn state_name(code: &str) -> Option<&'static str> {
    STATES.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// The 15th character of a GSTIN: a Luhn mod-36 check over the first 14.
pub fn gstin_check_char(first14: &str) -> Option<char> {
    let mut sum = 0u32;
    for (i, c) in first14.bytes().enumerate() {
        let v = CHARS.iter().position(|x| *x == c)? as u32;
        let p = v * if i % 2 == 0 { 1 } else { 2 };
        sum += p / 36 + p % 36;
    }
    Some(CHARS[((36 - sum % 36) % 36) as usize] as char)
}

/// Validates a normalized (trimmed, upper-case) GSTIN.
pub fn validate_gstin(g: &str) -> std::result::Result<(), String> {
    if g.len() != 15 || !g.bytes().all(|c| c.is_ascii_digit() || c.is_ascii_uppercase()) {
        return Err("a GSTIN has 15 letters and digits".into());
    }
    let state = &g[..2];
    if state_name(state).is_none() {
        return Err(format!("'{state}' is not a valid GST state code"));
    }
    let pan = &g[2..12];
    let pan_ok = pan[..5].bytes().all(|c| c.is_ascii_uppercase()) && pan[5..9].bytes().all(|c| c.is_ascii_digit()) && pan.as_bytes()[9].is_ascii_uppercase();
    // Government (UIN/TAN-based) registrations do not embed a PAN; only regular ones are checked.
    if !pan_ok && g.as_bytes()[13] == b'Z' {
        return Err("characters 3-12 must be the holder's PAN".into());
    }
    match gstin_check_char(&g[..14]) {
        Some(c) if g.ends_with(c) => Ok(()),
        _ => Err("the GSTIN check digit does not match; please re-check the number".into()),
    }
}

fn normalize(doc: &mut Doc, field: &str) -> Option<String> {
    let v = s(doc, field).map(|x| x.trim().to_uppercase()).filter(|x| !x.is_empty());
    set(doc, field, v.clone().map(Value::String).unwrap_or(Value::Null));
    v
}

/// GSTIN, state code and category on Company, Customer and Supplier.
pub struct PartyGstHook;

#[async_trait]
impl DocHook for PartyGstHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if hc.entity.field("in_gstin").is_none() {
            return Ok(());
        }
        let gstin = normalize(doc, "in_gstin");
        normalize(doc, "in_state_code");
        if let Some(g) = &gstin
            && s(doc, "in_state_code").is_none()
            && g.len() >= 2
        {
            set(doc, "in_state_code", g[..2].to_string());
        }
        if hc.entity.field("in_gst_category").is_some() {
            match (gstin.is_some(), s(doc, "in_gst_category")) {
                (true, None | Some("Unregistered") | Some("Consumer")) => set(doc, "in_gst_category", "Registered"),
                // The GSTIN is the registration: without one the party is unregistered.
                (false, Some("Registered")) => set(doc, "in_gst_category", "Unregistered"),
                _ => {}
            }
        }
        Ok(())
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        let category = s(doc, "in_gst_category");
        if let Some(g) = s(doc, "in_gstin") {
            if let Err(m) = validate_gstin(g) {
                errors.push(FieldError::new("in_gstin", m));
            } else if s(doc, "in_state_code").is_some_and(|sc| sc != &g[..2]) {
                errors.push(FieldError::new("in_state_code", "does not match the GSTIN"));
            }
            if category == Some("Overseas") {
                errors.push(FieldError::new("in_gstin", "overseas parties have no GSTIN"));
            }
        } else if category == Some("SEZ") {
            errors.push(FieldError::new("in_gstin", "is required for SEZ units"));
        }
        if let Some(sc) = s(doc, "in_state_code")
            && state_name(sc).is_none()
        {
            errors.push(FieldError::new("in_state_code", format!("'{sc}' is not a GST state code")));
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }
}

fn state_of(doc: &Doc) -> Option<String> {
    s(doc, "in_state_code").map(str::to_string).or_else(|| s(doc, "in_gstin").filter(|g| g.len() >= 2).map(|g| g[..2].to_string()))
}

/// Picks the GST tax template matching the place of supply on sales and purchase documents.
/// Registered ahead of the pricing hooks so the chosen template's rows are the ones calculated.
pub struct GstTemplateHook(pub Side);

impl GstTemplateHook {
    fn party(&self) -> (&'static str, &'static str) {
        match self.0 {
            Side::Selling => ("customer", "Customer"),
            Side::Buying => ("supplier", "Supplier"),
        }
    }

    fn kind(&self) -> &'static str {
        match self.0 {
            Side::Selling => "Sales",
            Side::Buying => "Purchase",
        }
    }
}

#[async_trait]
impl DocHook for GstTemplateHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if doc.get("docstatus").and_then(Value::as_i64).unwrap_or(0) != 0 {
            return Ok(());
        }
        let (Some(cid), Some(pid)) = (id(doc, "company"), id(doc, self.party().0)) else { return Ok(()) };
        let company = get(hc.kernel, hc.ctx, hc.tx, "Company", cid).await?;
        let Some(home) = state_of(&company) else { return Ok(()) };
        let party = get(hc.kernel, hc.ctx, hc.tx, self.party().1, pid).await?;
        let category = s(&party, "in_gst_category").unwrap_or("Unregistered");
        let has_pos = hc.entity.field("in_place_of_supply").is_some();
        let given = if has_pos { normalize(doc, "in_place_of_supply") } else { None };
        let pos = match given {
            Some(p) => p,
            None if category == "Overseas" => FOREIGN.to_string(),
            None => state_of(&party).unwrap_or_else(|| home.clone()),
        };
        if pos != FOREIGN && state_name(&pos).is_none() {
            return Err(Error::field("in_place_of_supply", format!("'{pos}' is not a GST state code")));
        }
        if has_pos {
            set(doc, "in_place_of_supply", pos.clone());
        }
        let supply = match (self.0, category) {
            (Side::Selling, "Overseas") => "Export",
            // Imports carry no GST on the supplier's invoice (it is paid at customs).
            (Side::Buying, "Overseas") => return Ok(()),
            (_, "SEZ") => "Inter-State",
            _ if pos == home => "Intra-State",
            _ => "Inter-State",
        };
        if let Some(current) = id(doc, "tax_template") {
            let t = get(hc.kernel, hc.ctx, hc.tx, "TaxTemplate", current).await?;
            if s(&t, "in_supply_type").is_none_or(|x| x == supply) {
                return Ok(());
            }
        }
        let sctx = sys(hc.ctx);
        let candidates = hc
            .kernel
            .find_tx(
                &sctx,
                hc.tx,
                "TaxTemplate",
                &[Filter::eq("company", cid.to_string()), Filter::eq("kind", self.kind()), Filter::eq("in_supply_type", supply)],
                None,
                50,
            )
            .await?;
        let chosen = candidates.iter().find(|t| b(t, "is_default")).or(candidates.first());
        match chosen {
            Some(t) => {
                set(doc, "tax_template", s(t, "id").unwrap_or_default().to_string());
                doc.remove("taxes");
            }
            None if id(doc, "tax_template").is_some() => {
                return Err(Error::field(
                    "tax_template",
                    format!("this is an {supply} supply but the template is for another supply type; create a {supply} template"),
                ));
            }
            None => {}
        }
        Ok(())
    }

    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        // Outward supplies must carry HSN/SAC codes once the company is GST-registered.
        let (Side::Selling, Some(cid)) = (self.0, id(doc, "company")) else { return Ok(()) };
        let company = get(hc.kernel, hc.ctx, hc.tx, "Company", cid).await?;
        if s(&company, "in_gstin").is_none() {
            return Ok(());
        }
        let mut missing = Vec::new();
        for r in rows(doc, "items") {
            let Some(item) = id(&r, "item") else { continue };
            let it = get(hc.kernel, hc.ctx, hc.tx, "Item", item).await?;
            if s(&it, "hsn_code").is_none() {
                missing.push(s(&it, "item_code").unwrap_or("?").to_string());
            }
        }
        if !missing.is_empty() {
            missing.sort();
            missing.dedup();
            return Err(Error::invalid(format!("set the HSN/SAC code on these items before invoicing: {}", missing.join(", "))));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_of_known_gstins() {
        assert_eq!(gstin_check_char("27AAPFU0939F1Z"), Some('V'));
        assert!(validate_gstin("27AAPFU0939F1ZV").is_ok());
        assert!(validate_gstin("27AAPFU0939F1ZX").is_err());
        assert!(validate_gstin("00AAPFU0939F1ZV").is_err());
        assert!(validate_gstin("27AAPFU0939F1Z").is_err());
    }

    #[test]
    fn every_generated_check_char_validates() {
        for base in ["29ABCDE1234F1Z", "07AAACR5055K1Z", "33AAACI1195H1Z"] {
            let c = gstin_check_char(base).unwrap();
            assert!(validate_gstin(&format!("{base}{c}")).is_ok(), "{base}{c}");
        }
    }
}
