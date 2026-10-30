//! GST e-invoicing and e-way bills (India).
//!
//! A submitted Sales Invoice to a registered business, an SEZ unit or a buyer abroad is registered
//! at the Invoice Registration Portal (IRP). Ferrum builds the IRP's JSON (schema 1.1) from the
//! invoice; a GSP connector plugin sends it (it subscribes to `india.e_invoice.requested`), or
//! someone uploads the downloaded JSON on the portal; either way the IRP's answer (IRN,
//! acknowledgement, signed QR code) is recorded on the invoice. An e-invoice is cancelled at the
//! IRP within 24 hours of its acknowledgement, and an invoice with a live e-invoice or e-way bill
//! cannot be cancelled here until they are. E-way bills go the same two ways: by IRN, or on their
//! own for invoices without one.

use crate::india::{FOREIGN, state_name, validate_gstin};
use crate::util::{d, get, id, rows, s, sys};
use async_trait::async_trait;
use chrono::{Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use ferrum_kernel::{Doc, DocHook, Error, HookCtx, Result, outbox};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json::{Map, Value, json};

/// Unit quantity codes (UQC) the GST portals accept.
pub const UQC: &[&str] = &[
    "BAG", "BAL", "BDL", "BKL", "BOU", "BOX", "BTL", "BUN", "CAN", "CBM", "CCM", "CMS", "CTN", "DOZ", "DRM", "GGK", "GMS", "GRS", "GYD", "KGS", "KLR", "KME",
    "LTR", "MLT", "MTR", "MTS", "NOS", "OTH", "PAC", "PCS", "PRS", "QTL", "ROL", "SET", "SQF", "SQM", "SQY", "TBS", "TGM", "THD", "TON", "TUB", "UGS", "UNT",
    "YDS",
];

/// The UQC for a unit of measure's name, when its own `in_uqc` is not set.
pub fn guess_uqc(name: &str) -> &'static str {
    let n = name.trim().trim_end_matches('.').to_ascii_uppercase();
    if let Some(u) = UQC.iter().find(|u| **u == n) {
        return u;
    }
    match n.as_str() {
        "NO" | "NUMBER" | "NUMBERS" | "EACH" | "EA" => "NOS",
        "PC" | "PIECE" | "PIECES" => "PCS",
        "KG" | "KILOGRAM" | "KILOGRAMS" => "KGS",
        "G" | "GM" | "GRAM" | "GRAMS" => "GMS",
        "L" | "LT" | "LITRE" | "LITER" | "LITRES" | "LITERS" => "LTR",
        "ML" | "MILLILITRE" | "MILLILITER" => "MLT",
        "M" | "METER" | "METRE" | "METERS" | "METRES" => "MTR",
        "CM" | "CENTIMETER" | "CENTIMETRE" => "CMS",
        "KM" | "KILOMETER" | "KILOMETRE" => "KME",
        "BOXES" => "BOX",
        "PACK" | "PACKS" | "PACKET" | "PACKETS" => "PAC",
        "PAIR" | "PAIRS" => "PRS",
        "DOZEN" => "DOZ",
        "TONNE" | "TONNES" | "METRIC TON" => "MTS",
        "UNIT" | "UNITS" => "UNT",
        "SQ M" | "SQUARE METER" | "SQUARE METRE" => "SQM",
        "SQ FT" | "SQUARE FEET" | "SQUARE FOOT" => "SQF",
        "SETS" => "SET",
        "ROLL" | "ROLLS" => "ROL",
        "BOTTLE" | "BOTTLES" => "BTL",
        "BAGS" => "BAG",
        _ => "OTH",
    }
}

/// What a tax row is, read from its description or account name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Head {
    Cgst,
    Sgst,
    Igst,
    Cess,
    /// Not GST: other charges.
    Other,
}

pub fn head(text: &str) -> Head {
    let t = text.to_ascii_uppercase();
    if t.contains("IGST") {
        Head::Igst
    } else if t.contains("CGST") {
        Head::Cgst
    } else if t.contains("SGST") || t.contains("UTGST") {
        Head::Sgst
    } else if t.contains("CESS") {
        Head::Cess
    } else {
        Head::Other
    }
}

/// An invoice line with its item master and unit code.
pub struct Line {
    pub row: Doc,
    pub item: Doc,
    pub uqc: String,
}

/// What an e-invoice or e-way bill is built from.
pub struct Sources {
    pub invoice: Doc,
    pub company: Doc,
    pub customer: Doc,
    pub lines: Vec<Line>,
    /// Tax rows with what they are and their account (for per-item rate overrides).
    pub taxes: Vec<(Doc, Head)>,
    /// A credit note's original invoice: its number and date.
    pub original: Option<(String, NaiveDate)>,
    /// The invoice currency's code.
    pub currency: String,
}

/// One line's values, in rupees.
struct Item {
    desc: String,
    hsn: String,
    service: bool,
    qty: Decimal,
    unit: String,
    unit_price: Decimal,
    total: Decimal,
    discount: Decimal,
    assessable: Decimal,
    gst_rate: Decimal,
    cgst: Decimal,
    sgst: Decimal,
    igst: Decimal,
    cess_rate: Decimal,
    cess: Decimal,
}

impl Item {
    fn value(&self) -> Decimal {
        self.assessable + self.cgst + self.sgst + self.igst + self.cess
    }
}

/// The invoice's values, in rupees.
struct Values {
    items: Vec<Item>,
    assessable: Decimal,
    cgst: Decimal,
    sgst: Decimal,
    igst: Decimal,
    cess: Decimal,
    other: Decimal,
    round_off: Decimal,
    total: Decimal,
    /// The total in the invoice's own currency, when that is not rupees.
    total_foreign: Option<Decimal>,
}

fn r2(v: Decimal) -> Decimal {
    v.round_dp_with_strategy(2, rust_decimal::RoundingStrategy::MidpointAwayFromZero)
}

fn r3(v: Decimal) -> Decimal {
    v.round_dp_with_strategy(3, rust_decimal::RoundingStrategy::MidpointAwayFromZero)
}

fn num(v: Decimal) -> Value {
    json!(v.normalize().to_f64().unwrap_or_default())
}

fn dmy(day: NaiveDate) -> String {
    day.format("%d/%m/%Y").to_string()
}

fn text(doc: &Doc, k: &str) -> String {
    s(doc, k).map(str::trim).unwrap_or_default().to_string()
}

/// An address text as the portals' two lines of at most 100 characters.
fn address_lines(addr: &str) -> (String, String) {
    let parts: Vec<&str> = addr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let first: String = parts.first().copied().unwrap_or_default().chars().take(100).collect();
    let rest: String = parts.iter().skip(1).copied().collect::<Vec<_>>().join(", ").chars().take(100).collect();
    (first, rest)
}

/// Whether a document number is one the IRP takes: 16 characters at most, capitals, digits, `/`
/// and `-`, not starting with 0, `/` or `-`.
pub fn valid_doc_no(no: &str) -> bool {
    let b = no.as_bytes();
    !b.is_empty()
        && b.len() <= 16
        && matches!(b[0], b'A'..=b'Z' | b'1'..=b'9')
        && b.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'/' || *c == b'-')
}

fn pin(doc: &Doc, problems: &mut Vec<String>, whose: &str) -> u32 {
    match text(doc, "in_pincode").parse::<u32>() {
        Ok(p) if (100_000..=999_999).contains(&p) => p,
        _ => {
            problems.push(format!("{whose} needs a six-digit PIN code"));
            0
        }
    }
}

/// The GST category of the buyer and whether an e-invoice applies to the sale.
fn category(customer: &Doc) -> &str {
    s(customer, "in_gst_category").unwrap_or("Unregistered")
}

impl Sources {
    fn rate_of(&self, line: &Doc, tax: &Doc) -> Decimal {
        let account = s(tax, "account").unwrap_or_default();
        line.get("item_tax_rates")
            .and_then(|m| m.get(account))
            .and_then(Value::as_str)
            .and_then(|r| r.parse::<Decimal>().ok())
            .unwrap_or_else(|| d(tax, "rate"))
    }

    /// Every line's and the invoice's values in rupees, checked to add up.
    fn values(&self, problems: &mut Vec<String>) -> Values {
        let inv = &self.invoice;
        let fx = Some(d(inv, "conversion_rate")).filter(|r| *r > Decimal::ZERO).unwrap_or(Decimal::ONE);
        for (tax, h) in &self.taxes {
            if *h != Head::Other && s(tax, "charge_type").unwrap_or("On Net Total") != "On Net Total" {
                problems.push(format!("the tax row '{}' must be charged on the net total for e-invoicing", text(tax, "description")));
            }
        }
        let mut items = Vec::new();
        for (i, l) in self.lines.iter().enumerate() {
            let row = &l.row;
            let hsn = text(&l.item, "hsn_code");
            if !(4..=8).contains(&hsn.len()) || !hsn.bytes().all(|c| c.is_ascii_digit()) {
                problems.push(format!("line {}: item {} needs an HSN/SAC code of 4 to 8 digits", i + 1, text(&l.item, "item_code")));
            }
            let qty = r3(d(row, "qty").abs());
            let assessable = r2(match row.get("base_net_amount") {
                Some(v) if !v.is_null() => d(row, "base_net_amount").abs(),
                _ => d(row, "net_amount").abs() * fx,
            });
            let list = if d(row, "price_list_rate").is_zero() { d(row, "rate") } else { d(row, "price_list_rate") };
            let mut unit_price = r3(list.abs() * fx);
            let mut total = r2(unit_price * qty);
            if total < assessable {
                // Priced up after the list price, or rounding: the line's own value stands.
                unit_price = if qty.is_zero() { assessable } else { r3(assessable / qty) };
                total = assessable.max(r2(unit_price * qty));
            }
            let mut item = Item {
                desc: text(row, "item_name").chars().take(300).collect(),
                service: hsn.starts_with("99"),
                hsn,
                qty,
                unit: l.uqc.clone(),
                unit_price,
                total,
                discount: total - assessable,
                assessable,
                gst_rate: Decimal::ZERO,
                cgst: Decimal::ZERO,
                sgst: Decimal::ZERO,
                igst: Decimal::ZERO,
                cess_rate: Decimal::ZERO,
                cess: Decimal::ZERO,
            };
            if item.desc.chars().count() < 3 {
                item.desc = format!("{:<3}", item.desc);
            }
            for (tax, h) in &self.taxes {
                let rate = self.rate_of(row, tax);
                let amount = r2(assessable * rate / Decimal::ONE_HUNDRED);
                match h {
                    Head::Cgst => (item.cgst, item.gst_rate) = (item.cgst + amount, item.gst_rate + rate),
                    Head::Sgst => (item.sgst, item.gst_rate) = (item.sgst + amount, item.gst_rate + rate),
                    Head::Igst => (item.igst, item.gst_rate) = (item.igst + amount, item.gst_rate + rate),
                    Head::Cess => (item.cess, item.cess_rate) = (item.cess + amount, item.cess_rate + rate),
                    Head::Other => {}
                }
            }
            items.push(item);
        }
        let mut heads = [Decimal::ZERO; 5];
        for (tax, h) in &self.taxes {
            let amount = match tax.get("base_tax_amount") {
                Some(v) if !v.is_null() => d(tax, "base_tax_amount"),
                _ => d(tax, "tax_amount") * fx,
            }
            .abs();
            let signed = if tax.get("deduct").and_then(Value::as_bool).unwrap_or(false) { -amount } else { amount };
            heads[*h as usize] += r2(signed);
        }
        let [cgst, sgst, igst, cess, other] = heads;
        for (label, doc_total, lines_total) in [
            ("CGST", cgst, items.iter().map(|i| i.cgst).sum::<Decimal>()),
            ("SGST", sgst, items.iter().map(|i| i.sgst).sum()),
            ("IGST", igst, items.iter().map(|i| i.igst).sum()),
            ("cess", cess, items.iter().map(|i| i.cess).sum()),
        ] {
            if (doc_total - lines_total).abs() > Decimal::ONE {
                problems.push(format!("the invoice's {label} ({doc_total}) differs from its lines' ({lines_total}) by more than ₹1"));
            }
        }
        let assessable: Decimal = items.iter().map(|i| i.assessable).sum();
        let rounded = d(inv, "base_rounded_total").abs();
        let total = r2(if rounded.is_zero() { d(inv, "base_grand_total").abs() } else { rounded });
        let round_off = r2(total - (assessable + cgst + sgst + igst + cess + other));
        if round_off.abs() > Decimal::TEN {
            problems.push(format!("the invoice total {total} does not add up to its lines and taxes (off by {round_off})"));
        }
        let foreign = self.currency != "INR";
        let own_total = if d(inv, "rounded_total").is_zero() { d(inv, "grand_total") } else { d(inv, "rounded_total") };
        Values { items, assessable, cgst, sgst, igst, cess, other, round_off, total, total_foreign: foreign.then(|| r2(own_total.abs())) }
    }

    fn party(&self, doc: &Doc, whose: &str, problems: &mut Vec<String>, name_field: &str, address_field: &str) -> Map<String, Value> {
        let mut m = Map::new();
        let name: String = text(doc, name_field).chars().take(100).collect();
        if name.chars().count() < 3 {
            problems.push(format!("{whose}'s legal name must be at least 3 characters"));
        }
        let (addr1, addr2) = address_lines(&text(doc, address_field));
        if addr1.chars().count() < 3 {
            problems.push(format!("{whose} needs an address"));
        }
        let city: String = text(doc, "in_city").chars().take(50).collect();
        if city.chars().count() < 3 {
            problems.push(format!("{whose} needs a city (at least 3 characters)"));
        }
        m.insert("LglNm".into(), json!(name));
        m.insert("Addr1".into(), json!(addr1));
        if !addr2.is_empty() {
            m.insert("Addr2".into(), json!(addr2));
        }
        m.insert("Loc".into(), json!(city));
        m
    }

    /// The IRP's JSON for the invoice (schema 1.1), or everything that must be fixed first.
    pub fn e_invoice(&self) -> Result<Value> {
        let inv = &self.invoice;
        let mut problems = Vec::new();
        let cat = category(&self.customer);
        let export = cat == "Overseas";
        if !matches!(cat, "Registered" | "SEZ" | "Overseas") {
            return Err(Error::invalid(format!("e-invoices are for sales to registered businesses, SEZ units and buyers abroad; this customer is {cat}")));
        }
        let number = text(inv, "number");
        if !valid_doc_no(&number) {
            problems.push(format!(
                "the invoice number {number} is not one the IRP takes: at most 16 characters of capitals, digits, / and - (a series such as SI/{{FY}}/{{seq:5}} is)"
            ));
        }
        let seller_gstin = text(&self.company, "in_gstin");
        if validate_gstin(&seller_gstin).is_err() {
            problems.push("the company needs its GSTIN".into());
        }
        let buyer_gstin = if export { "URP".to_string() } else { text(&self.customer, "in_gstin") };
        if !export && validate_gstin(&buyer_gstin).is_err() {
            problems.push("the customer needs a valid GSTIN".into());
        }
        let seller_state = text(&self.company, "in_state_code");
        let pos = if export { FOREIGN.to_string() } else { text(inv, "in_place_of_supply") };
        if pos != FOREIGN && state_name(&pos).is_none() {
            problems.push("the invoice needs its place of supply".into());
        }
        let buyer_state = if export {
            FOREIGN.to_string()
        } else {
            Some(text(&self.customer, "in_state_code")).filter(|c| !c.is_empty()).unwrap_or_else(|| buyer_gstin.chars().take(2).collect())
        };
        let mut seller = self.party(&self.company, "the company", &mut problems, "company_name", "in_address");
        let seller_pin = pin(&self.company, &mut problems, "the company");
        seller.insert("Gstin".into(), json!(seller_gstin));
        seller.insert("Pin".into(), json!(seller_pin));
        seller.insert("Stcd".into(), json!(seller_state));
        let mut buyer = self.party(&self.customer, "the customer", &mut problems, "customer_name", "billing_address");
        let buyer_pin = if export { 999_999 } else { pin(&self.customer, &mut problems, "the customer") };
        buyer.insert("Gstin".into(), json!(buyer_gstin));
        buyer.insert("Pos".into(), json!(pos));
        buyer.insert("Pin".into(), json!(buyer_pin));
        buyer.insert("Stcd".into(), json!(buyer_state));
        let v = self.values(&mut problems);
        if v.items.is_empty() {
            problems.push("the invoice has no lines".into());
        }
        let day = s(inv, "posting_date").and_then(|t| NaiveDate::parse_from_str(t, "%Y-%m-%d").ok());
        let credit_note = inv.get("is_return").and_then(Value::as_bool).unwrap_or(false);
        if credit_note && self.original.is_none() {
            problems.push("a credit note needs the invoice it returns".into());
        }
        if !problems.is_empty() {
            return Err(Error::invalid(format!("before e-invoicing, {}", problems.join("; "))));
        }
        let taxed = v.igst > Decimal::ZERO;
        let supply = match (cat, taxed) {
            ("SEZ", true) => "SEZWP",
            ("SEZ", false) => "SEZWOP",
            ("Overseas", true) => "EXPWP",
            ("Overseas", false) => "EXPWOP",
            _ => "B2B",
        };
        let items: Vec<Value> = v
            .items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                json!({
                    "SlNo": (i + 1).to_string(), "PrdDesc": it.desc, "IsServc": if it.service { "Y" } else { "N" }, "HsnCd": it.hsn,
                    "Qty": num(it.qty), "FreeQty": 0, "Unit": it.unit, "UnitPrice": num(it.unit_price), "TotAmt": num(it.total), "Discount": num(it.discount),
                    "PreTaxVal": 0, "AssAmt": num(it.assessable), "GstRt": num(it.gst_rate), "IgstAmt": num(it.igst), "CgstAmt": num(it.cgst), "SgstAmt": num(it.sgst),
                    "CesRt": num(it.cess_rate), "CesAmt": num(it.cess), "CesNonAdvlAmt": 0, "StateCesRt": 0, "StateCesAmt": 0, "StateCesNonAdvlAmt": 0,
                    "OthChrg": 0, "TotItemVal": num(it.value()),
                })
            })
            .collect();
        let mut values = json!({
            "AssVal": num(v.assessable), "CgstVal": num(v.cgst), "SgstVal": num(v.sgst), "IgstVal": num(v.igst), "CesVal": num(v.cess), "StCesVal": 0,
            "Discount": 0, "OthChrg": num(v.other), "RndOffAmt": num(v.round_off), "TotInvVal": num(v.total),
        });
        let mut out = json!({
            "Version": "1.1",
            "TranDtls": {"TaxSch": "GST", "SupTyp": supply, "RegRev": "N", "IgstOnIntra": "N"},
            "DocDtls": {"Typ": if credit_note { "CRN" } else { "INV" }, "No": number, "Dt": day.map(dmy).unwrap_or_default()},
            "SellerDtls": seller,
            "BuyerDtls": buyer,
            "ItemList": items,
        });
        if let Some(fc) = v.total_foreign {
            values["TotInvValFc"] = num(fc);
        }
        out["ValDtls"] = values;
        if export {
            out["ExpDtls"] = json!({"ForCur": self.currency, "RefClm": "N"});
        }
        if let Some((no, date)) = &self.original {
            out["RefDtls"] = json!({"PrecDocDtls": [{"InvNo": no, "InvDt": dmy(*date)}]});
        }
        Ok(out)
    }

    /// Transport details for an e-way bill, or what is missing.
    fn transport(&self, problems: &mut Vec<String>) -> Map<String, Value> {
        let inv = &self.invoice;
        let mode = match s(inv, "in_transport_mode").unwrap_or("Road") {
            "Rail" => "2",
            "Air" => "3",
            "Ship" => "4",
            _ => "1",
        };
        let transporter = text(inv, "in_transporter_gstin").to_uppercase();
        let vehicle: String = text(inv, "in_vehicle_no").to_uppercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let (doc_no, doc_date) =
            (text(inv, "in_transport_doc_no"), s(inv, "in_transport_doc_date").and_then(|t| NaiveDate::parse_from_str(t, "%Y-%m-%d").ok()));
        let distance = inv.get("in_distance_km").and_then(Value::as_i64).unwrap_or(0);
        if !(0..=4000).contains(&distance) {
            problems.push("the distance must be 0 to 4000 km (0: the portal works it out from the PIN codes)".into());
        }
        if !transporter.is_empty() && (transporter.len() != 15 || !transporter.chars().all(|c| c.is_ascii_alphanumeric())) {
            problems.push("the transporter ID must be a 15-character GSTIN or TRANSIN".into());
        }
        match mode {
            "1" if vehicle.is_empty() && transporter.is_empty() => problems.push("give the vehicle number, or the transporter ID who will add it".into()),
            "1" if !vehicle.is_empty() && !(7..=15).contains(&vehicle.len()) => problems.push("the vehicle number looks wrong".into()),
            "2" | "3" | "4" if doc_no.is_empty() || doc_date.is_none() => {
                problems.push("rail, air and ship transport need the transport document's number and date".into())
            }
            _ => {}
        }
        let mut m = Map::new();
        m.insert("TransMode".into(), json!(mode));
        m.insert("Distance".into(), json!(distance));
        if !transporter.is_empty() {
            m.insert("TransId".into(), json!(transporter));
        }
        if !text(inv, "in_transporter_name").is_empty() {
            m.insert("TransName".into(), json!(text(inv, "in_transporter_name").chars().take(100).collect::<String>()));
        }
        if !doc_no.is_empty() {
            m.insert("TransDocNo".into(), json!(doc_no.chars().take(15).collect::<String>()));
        }
        if let Some(day) = doc_date {
            m.insert("TransDocDt".into(), json!(dmy(day)));
        }
        if !vehicle.is_empty() {
            m.insert("VehNo".into(), json!(vehicle));
            m.insert("VehType".into(), json!(if s(inv, "in_vehicle_type") == Some("Over Dimensional Cargo") { "O" } else { "R" }));
        }
        m
    }

    /// The e-way bill: by IRN when the invoice has one (the IRP's "generate e-way bill by IRN"),
    /// otherwise the e-way bill portal's own JSON.
    pub fn e_way_bill(&self) -> Result<Value> {
        let inv = &self.invoice;
        let mut problems = Vec::new();
        if inv.get("is_return").and_then(Value::as_bool).unwrap_or(false) {
            return Err(Error::invalid("make a returned consignment's e-way bill on the portal (an inward movement)"));
        }
        let t = self.transport(&mut problems);
        if s(inv, "in_e_invoice_status") == Some("Generated") {
            if !problems.is_empty() {
                return Err(Error::invalid(format!("before the e-way bill, {}", problems.join("; "))));
            }
            let mut by_irn = t;
            by_irn.insert("Irn".into(), json!(text(inv, "in_irn")));
            return Ok(Value::Object(by_irn));
        }
        let export = category(&self.customer) == "Overseas";
        let registered = matches!(category(&self.customer), "Registered" | "SEZ");
        let seller_gstin = text(&self.company, "in_gstin");
        if validate_gstin(&seller_gstin).is_err() {
            problems.push("the company needs its GSTIN".into());
        }
        let from = self.party(&self.company, "the company", &mut problems, "company_name", "in_address");
        let from_pin = pin(&self.company, &mut problems, "the company");
        let to = self.party(&self.customer, "the customer", &mut problems, "customer_name", "billing_address");
        let to_pin = pin(&self.customer, &mut problems, "the customer");
        let number = text(inv, "number");
        if number.is_empty() || number.len() > 16 {
            problems.push(format!("the invoice number {number} is longer than the 16 characters an e-way bill takes"));
        }
        let v = self.values(&mut problems);
        if !problems.is_empty() {
            return Err(Error::invalid(format!("before the e-way bill, {}", problems.join("; "))));
        }
        let state = |code: &str| code.parse::<u32>().unwrap_or(0);
        let from_state = state(&text(&self.company, "in_state_code"));
        let to_state = if export {
            96
        } else {
            state(&Some(text(&self.customer, "in_state_code")).filter(|c| !c.is_empty()).unwrap_or_else(|| text(inv, "in_place_of_supply")))
        };
        let items: Vec<Value> = v
            .items
            .iter()
            .map(|it| {
                let (half, igst) = if it.igst > Decimal::ZERO { (Decimal::ZERO, it.gst_rate) } else { (it.gst_rate / Decimal::TWO, Decimal::ZERO) };
                json!({
                    "productName": it.desc, "productDesc": it.desc, "hsnCode": it.hsn.parse::<u64>().unwrap_or_default(), "quantity": num(it.qty), "qtyUnit": it.unit,
                    "cgstRate": num(half), "sgstRate": num(half), "igstRate": num(igst), "cessRate": num(it.cess_rate), "cessNonadvol": 0, "taxableAmount": num(it.assessable),
                })
            })
            .collect();
        let day = s(inv, "posting_date").and_then(|t| NaiveDate::parse_from_str(t, "%Y-%m-%d").ok()).map(dmy).unwrap_or_default();
        let mut bill = json!({
            "supplyType": "O", "subSupplyType": if export { "3" } else { "1" }, "docType": "INV", "docNo": number, "docDate": day,
            "fromGstin": seller_gstin, "fromTrdName": from["LglNm"], "fromAddr1": from["Addr1"], "fromAddr2": from.get("Addr2").cloned().unwrap_or(json!("")),
            "fromPlace": from["Loc"], "fromPincode": from_pin, "fromStateCode": from_state, "actFromStateCode": from_state,
            "toGstin": if registered { text(&self.customer, "in_gstin") } else { "URP".to_string() }, "toTrdName": to["LglNm"], "toAddr1": to["Addr1"],
            "toAddr2": to.get("Addr2").cloned().unwrap_or(json!("")), "toPlace": to["Loc"], "toPincode": to_pin, "toStateCode": to_state, "actToStateCode": to_state,
            "transactionType": 1, "otherValue": num(v.other + v.round_off), "totalValue": num(v.assessable), "cgstValue": num(v.cgst), "sgstValue": num(v.sgst),
            "igstValue": num(v.igst), "cessValue": num(v.cess), "cessNonAdvolValue": 0, "totInvValue": num(v.total),
            "transMode": t["TransMode"], "transDistance": t["Distance"].to_string(), "itemList": items,
        });
        for (theirs, ours) in [
            ("transporterId", "TransId"),
            ("transporterName", "TransName"),
            ("transDocNo", "TransDocNo"),
            ("transDocDate", "TransDocDt"),
            ("vehicleNo", "VehNo"),
            ("vehicleType", "VehType"),
        ] {
            if let Some(x) = t.get(ours) {
                bill[theirs] = x.clone();
            }
        }
        Ok(bill)
    }
}

/// The parts of a signed QR code (a JWT signed by the IRP) the invoice is checked against.
fn qr_claims(signed: &str) -> Option<Value> {
    use base64::Engine as _;
    let payload = signed.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let outer: Value = serde_json::from_slice(&bytes).ok()?;
    match &outer["data"] {
        Value::String(inner) => serde_json::from_str(inner).ok(),
        Value::Object(_) => Some(outer["data"].clone()),
        _ => None,
    }
}

/// An IRP timestamp (`2026-10-30 14:05:00`, India time) as UTC.
fn irp_time(t: &str) -> Option<chrono::DateTime<Utc>> {
    let naive =
        NaiveDateTime::parse_from_str(t.trim(), "%Y-%m-%d %H:%M:%S").or_else(|_| NaiveDateTime::parse_from_str(t.trim(), "%d/%m/%Y %I:%M:%S %p")).ok()?;
    chrono::FixedOffset::east_opt(5 * 3600 + 1800)?.from_local_datetime(&naive).single().map(|t| t.with_timezone(&Utc))
}

/// A field of the IRP's (or a connector's) answer, wherever it sits: at the top, under `data`, or
/// under `result`; the answer may also come as JSON text in `response`.
fn answer_field(params: &Value, key: &str) -> Option<String> {
    let parsed = params.get("response").and_then(Value::as_str).and_then(|t| serde_json::from_str::<Value>(t).ok());
    let roots = [Some(params), params.get("response"), parsed.as_ref()];
    for root in roots.into_iter().flatten() {
        for v in [root, &root["data"], &root["Data"], &root["result"]] {
            match v.get(key) {
                Some(Value::String(t)) if !t.trim().is_empty() => return Some(t.trim().to_string()),
                Some(Value::Number(n)) => return Some(n.to_string()),
                _ => {}
            }
        }
    }
    None
}

const CANCEL_WINDOW_HOURS: i64 = 24;

/// The e-invoicing and e-way bill actions on Sales Invoices, and the guard on cancelling one.
pub struct EInvoiceHook;

impl EInvoiceHook {
    async fn sources(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Sources> {
        let ctx = sys(hc.ctx);
        let company = get(hc.kernel, &ctx, hc.tx, "Company", id(doc, "company").ok_or_else(|| Error::invalid("the invoice has no company"))?).await?;
        let customer = get(hc.kernel, &ctx, hc.tx, "Customer", id(doc, "customer").ok_or_else(|| Error::invalid("the invoice has no customer"))?).await?;
        let mut lines = Vec::new();
        for row in rows(doc, "items") {
            let item = match id(&row, "item") {
                Some(i) => get(hc.kernel, &ctx, hc.tx, "Item", i).await?,
                None => Doc::new(),
            };
            let uqc = match id(&row, "uom").or_else(|| id(&item, "stock_uom")) {
                Some(u) => {
                    let uom = get(hc.kernel, &ctx, hc.tx, "Uom", u).await?;
                    s(&uom, "in_uqc")
                        .filter(|c| UQC.contains(c))
                        .map(str::to_string)
                        .unwrap_or_else(|| guess_uqc(s(&uom, "uom_name").or(s(&uom, "name")).unwrap_or_default()).into())
                }
                None => "OTH".into(),
            };
            lines.push(Line { row, item, uqc });
        }
        let mut taxes = Vec::new();
        for row in rows(doc, "taxes") {
            let account = match id(&row, "account") {
                Some(a) => get(hc.kernel, &ctx, hc.tx, "Account", a).await?,
                None => Doc::new(),
            };
            let h = match head(s(&row, "description").unwrap_or_default()) {
                Head::Other => head(s(&account, "account_name").unwrap_or_default()),
                h => h,
            };
            taxes.push((row, h));
        }
        let original = match id(doc, "return_against") {
            Some(o) => {
                let orig = get(hc.kernel, &ctx, hc.tx, "SalesInvoice", o).await?;
                s(&orig, "posting_date").and_then(|t| NaiveDate::parse_from_str(t, "%Y-%m-%d").ok()).map(|day| (text(&orig, "number"), day))
            }
            None => None,
        };
        let currency = match id(doc, "currency") {
            Some(c) => crate::util::currency(hc.kernel, &ctx, hc.tx, c).await?.0,
            None => "INR".into(),
        };
        Ok(Sources { invoice: doc.clone(), company, customer, lines, taxes, original, currency })
    }

    async fn save(hc: &mut HookCtx<'_>, doc: &Doc, values: Value) -> Result<()> {
        let inv = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
        let Value::Object(v) = values else { return Ok(()) };
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "SalesInvoice", inv, v).await
    }

    async fn emit(hc: &mut HookCtx<'_>, doc: &Doc, topic: &str, payload: Value) -> Result<()> {
        let inv = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
        let event = json!({"invoice": inv, "number": s(doc, "number"), "company": s(doc, "company"), "payload": payload});
        outbox::enqueue(hc.tx, hc.ctx.tenant, topic, Some("SalesInvoice"), Some(inv), event).await
    }
}

fn file(name: &str, content: &Value) -> Value {
    json!({"name": name, "type": "application/json", "content": serde_json::to_string_pretty(content).unwrap_or_default()})
}

fn within_window(at: Option<&str>, what: &str) -> Result<()> {
    let at = at.and_then(irp_time).ok_or_else(|| Error::invalid(format!("the {what}'s acknowledgement time is not recorded")))?;
    if Utc::now() > at + Duration::hours(CANCEL_WINDOW_HOURS) {
        return Err(Error::invalid(format!("the {what} is more than 24 hours old and can no longer be cancelled; issue a credit note instead")));
    }
    Ok(())
}

fn param(params: &Value, key: &str, max: usize) -> Result<Option<String>> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(t)) if t.trim().is_empty() => Ok(None),
        Some(Value::String(t)) if t.trim().chars().count() <= max => Ok(Some(t.trim().to_string())),
        Some(_) => Err(Error::field(key, format!("text of at most {max} characters"))),
    }
}

#[async_trait]
impl DocHook for EInvoiceHook {
    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if matches!(s(doc, "in_e_way_bill_status"), Some("Generated" | "Cancelling" | "Pending")) {
            return Err(Error::invalid("cancel the e-way bill first (within 24 hours of making it), or record that its request failed"));
        }
        if matches!(s(doc, "in_e_invoice_status"), Some("Generated" | "Cancelling" | "Pending")) {
            return Err(Error::invalid(
                "cancel the e-invoice at the IRP first (within 24 hours of its acknowledgement) and record that here, or record that its request failed; after 24 hours issue a credit note instead",
            ));
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if hc.entity.field("in_e_invoice_status").is_none() {
            return Ok(None);
        }
        let status = s(doc, "in_e_invoice_status").unwrap_or_default().to_string();
        let ewb = s(doc, "in_e_way_bill_status").unwrap_or_default().to_string();
        let number = s(doc, "number").unwrap_or("invoice").to_string();
        Ok(Some(match name {
            "e_invoice_json" => {
                let payload = Self::sources(hc, doc).await?.e_invoice()?;
                // The portal's bulk upload takes a list of invoices.
                json!({"payload": payload, "file": file(&format!("{}-e-invoice.json", number.replace('/', "-")), &json!([payload]))})
            }
            "request_e_invoice" => {
                if matches!(status.as_str(), "Generated" | "Pending" | "Cancelling") {
                    return Err(Error::invalid(format!("the e-invoice is already {}", status.to_lowercase())));
                }
                let payload = Self::sources(hc, doc).await?.e_invoice()?;
                Self::save(hc, doc, json!({"in_e_invoice_status": "Pending", "in_e_invoice_error": null})).await?;
                Self::emit(hc, doc, "india.e_invoice.requested", payload).await?;
                json!({"status": "Pending"})
            }
            "record_e_invoice" => {
                let irn = answer_field(params, "Irn").ok_or_else(|| Error::field("response", "the IRP's answer has no Irn"))?;
                if irn.len() != 64 || !irn.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(Error::field("response", "an IRN is 64 hexadecimal characters"));
                }
                if status == "Generated" {
                    return if s(doc, "in_irn") == Some(irn.as_str()) {
                        Ok(Some(json!({"status": "Generated", "irn": irn, "replayed": true})))
                    } else {
                        Err(Error::Conflict(format!("this invoice already has IRN {}", s(doc, "in_irn").unwrap_or_default())))
                    };
                }
                let qr = answer_field(params, "SignedQRCode").ok_or_else(|| Error::field("response", "the IRP's answer has no SignedQRCode"))?;
                let claims = qr_claims(&qr).ok_or_else(|| Error::field("response", "the signed QR code is not one the IRP issues"))?;
                let company = get(hc.kernel, &sys(hc.ctx), hc.tx, "Company", id(doc, "company").unwrap_or_default()).await?;
                for (claim, expected, what) in
                    [("Irn", irn.clone(), "IRN"), ("DocNo", number.clone(), "invoice number"), ("SellerGstin", text(&company, "in_gstin"), "seller GSTIN")]
                {
                    if claims[claim].as_str().map(str::trim) != Some(expected.as_str()) {
                        return Err(Error::field(
                            "response",
                            format!("the signed QR code is for another {what} ({})", claims[claim].as_str().unwrap_or("none")),
                        ));
                    }
                }
                // The IRP's signature: under the key the company names (a private IRP's, or NIC's
                // newest), otherwise under NIC's published keys for a code NIC issued.
                match s(&company, "in_irp_qr_key").filter(|k| !k.trim().is_empty()) {
                    Some(pem) => {
                        if !crate::irp::signed_by(pem, &qr)? {
                            return Err(Error::field("response", "the signed QR code does not carry the IRP's signature"));
                        }
                    }
                    None => match crate::irp::nic_signature(&qr) {
                        crate::irp::NicSignature::Valid | crate::irp::NicSignature::NotNic => {}
                        crate::irp::NicSignature::Invalid => {
                            return Err(Error::field("response", "the signed QR code does not carry NIC's signature"));
                        }
                        // NIC rotates its keys. A code straight from the IRP (the direct connection)
                        // is recorded and the gap logged; one entered by hand is not.
                        crate::irp::NicSignature::UnknownKey(kid) if hc.ctx.is_system => {
                            tracing::warn!(kid, "e-invoice QR code signed with a NIC key this release does not know: add it to nic_keys");
                        }
                        crate::irp::NicSignature::UnknownKey(kid) => {
                            return Err(Error::field(
                                "response",
                                format!(
                                    "the signed QR code names a NIC key this release does not know ({kid}): enter NIC's current QR signing key \
                                     (einvoice1.gst.gov.in/Others/Publickeys) as the company's IRP QR key"
                                ),
                            ));
                        }
                    },
                }
                let ack_no = answer_field(params, "AckNo").ok_or_else(|| Error::field("response", "the IRP's answer has no AckNo"))?;
                let ack_dt = answer_field(params, "AckDt")
                    .filter(|t| irp_time(t).is_some())
                    .ok_or_else(|| Error::field("response", "the IRP's answer has no AckDt (yyyy-MM-dd HH:mm:ss)"))?;
                let mut v = json!({"in_irn": irn, "in_ack_no": ack_no, "in_ack_date": ack_dt, "in_signed_qr_code": qr, "in_e_invoice_status": "Generated", "in_e_invoice_error": null});
                if let Some(ewb_no) = answer_field(params, "EwbNo") {
                    v["in_ewaybill_no"] = json!(ewb_no);
                    v["in_e_way_bill_date"] = json!(answer_field(params, "EwbDt"));
                    v["in_e_way_bill_valid_till"] = json!(answer_field(params, "EwbValidTill"));
                    v["in_e_way_bill_status"] = json!("Generated");
                }
                Self::save(hc, doc, v).await?;
                json!({"status": "Generated", "irn": irn})
            }
            "record_e_invoice_failure" => {
                if status != "Pending" {
                    return Ok(Some(json!({"status": status})));
                }
                let message = param(params, "message", 2000)?.unwrap_or_else(|| "the IRP refused the e-invoice".into());
                Self::save(hc, doc, json!({"in_e_invoice_status": "Failed", "in_e_invoice_error": message})).await?;
                json!({"status": "Failed"})
            }
            "cancel_e_invoice" => {
                if status != "Generated" {
                    return Err(Error::invalid("there is no e-invoice to cancel"));
                }
                if matches!(ewb.as_str(), "Generated" | "Cancelling") {
                    return Err(Error::invalid("cancel the e-way bill first"));
                }
                within_window(s(doc, "in_ack_date"), "e-invoice")?;
                let reason = match param(params, "reason", 40)?.as_deref() {
                    Some("Duplicate") => "1",
                    Some("Data entry mistake") => "2",
                    Some("Order cancelled") => "3",
                    Some("Other") => "4",
                    _ => return Err(Error::field("reason", "Duplicate, Data entry mistake, Order cancelled or Other")),
                };
                let remark = param(params, "remark", 100)?.ok_or_else(|| Error::field("remark", "say why, in at most 100 characters"))?;
                let payload = json!({"Irn": s(doc, "in_irn"), "CnlRsn": reason, "CnlRem": remark});
                Self::save(hc, doc, json!({"in_e_invoice_status": "Cancelling"})).await?;
                Self::emit(hc, doc, "india.e_invoice.cancel_requested", payload.clone()).await?;
                json!({"status": "Cancelling", "payload": payload})
            }
            "record_e_invoice_cancellation" => {
                if !matches!(status.as_str(), "Generated" | "Cancelling") {
                    return Ok(Some(json!({"status": status})));
                }
                if let Some(irn) = answer_field(params, "Irn").filter(|i| Some(i.as_str()) != s(doc, "in_irn")) {
                    return Err(Error::field("response", format!("this cancellation is for another IRN ({irn})")));
                }
                Self::save(hc, doc, json!({"in_e_invoice_status": "Cancelled", "in_e_invoice_cancelled_at": answer_field(params, "CancelDate").unwrap_or_else(|| Utc::now().to_rfc3339())})).await?;
                json!({"status": "Cancelled"})
            }
            "record_e_invoice_cancellation_failure" => {
                if status != "Cancelling" {
                    return Ok(Some(json!({"status": status})));
                }
                let message = param(params, "message", 2000)?.unwrap_or_else(|| "the IRP refused the cancellation".into());
                Self::save(hc, doc, json!({"in_e_invoice_status": "Generated", "in_e_invoice_error": message})).await?;
                json!({"status": "Generated"})
            }
            "set_transport" => {
                if matches!(ewb.as_str(), "Generated" | "Pending" | "Cancelling") {
                    return Err(Error::invalid(format!("the e-way bill is {}: cancel it before changing the transport", ewb.to_lowercase())));
                }
                let mut v = Map::new();
                let pick = |key: &str, options: &[&str]| -> Result<Option<Value>> {
                    match param(params, key, 40)? {
                        Some(o) if options.contains(&o.as_str()) => Ok(Some(json!(o))),
                        Some(_) => Err(Error::field(key, format!("one of {}", options.join(", ")))),
                        None => Ok(None),
                    }
                };
                for (key, options) in
                    [("in_transport_mode", &["Road", "Rail", "Air", "Ship"][..]), ("in_vehicle_type", &["Regular", "Over Dimensional Cargo"][..])]
                {
                    if let Some(x) = pick(key, options)? {
                        v.insert(key.into(), x);
                    }
                }
                match params.get("in_distance_km") {
                    None | Some(Value::Null) => {}
                    Some(x) => {
                        let km = x.as_i64().or_else(|| x.as_str().and_then(|t| t.trim().parse().ok())).filter(|k| (0..=4000).contains(k));
                        v.insert("in_distance_km".into(), json!(km.ok_or_else(|| Error::field("in_distance_km", "0 to 4000 km"))?));
                    }
                }
                for (key, max) in [("in_vehicle_no", 20), ("in_transporter_gstin", 15), ("in_transporter_name", 100), ("in_transport_doc_no", 15)] {
                    if params.get(key).is_some() {
                        v.insert(key.into(), json!(param(params, key, max)?));
                    }
                }
                if let Some(day) = param(params, "in_transport_doc_date", 10)? {
                    NaiveDate::parse_from_str(&day, "%Y-%m-%d").map_err(|_| Error::field("in_transport_doc_date", "a date (YYYY-MM-DD)"))?;
                    v.insert("in_transport_doc_date".into(), json!(day));
                }
                Self::save(hc, doc, Value::Object(v)).await?;
                json!({"saved": true})
            }
            "e_way_bill_json" => {
                let payload = Self::sources(hc, doc).await?.e_way_bill()?;
                let content = if status == "Generated" { payload.clone() } else { json!({"version": "1.0.0621", "billLists": [payload]}) };
                json!({"payload": payload, "by_irn": status == "Generated", "file": file(&format!("{}-e-way-bill.json", number.replace('/', "-")), &content)})
            }
            "request_e_way_bill" => {
                if matches!(ewb.as_str(), "Generated" | "Pending" | "Cancelling") {
                    return Err(Error::invalid(format!("the e-way bill is already {}", ewb.to_lowercase())));
                }
                let payload = Self::sources(hc, doc).await?.e_way_bill()?;
                Self::save(hc, doc, json!({"in_e_way_bill_status": "Pending", "in_e_way_bill_error": null})).await?;
                Self::emit(hc, doc, "india.e_way_bill.requested", json!({"by_irn": status == "Generated", "bill": payload})).await?;
                json!({"status": "Pending"})
            }
            "record_e_way_bill" => {
                let no = answer_field(params, "EwbNo")
                    .or_else(|| answer_field(params, "ewayBillNo"))
                    .ok_or_else(|| Error::field("response", "the answer has no EwbNo"))?;
                if no.len() != 12 || !no.chars().all(|c| c.is_ascii_digit()) {
                    return Err(Error::field("response", "an e-way bill number is 12 digits"));
                }
                if ewb == "Generated" {
                    return if s(doc, "in_ewaybill_no") == Some(no.as_str()) {
                        Ok(Some(json!({"status": "Generated", "replayed": true})))
                    } else {
                        Err(Error::Conflict(format!("this invoice already has e-way bill {}", s(doc, "in_ewaybill_no").unwrap_or_default())))
                    };
                }
                let made = answer_field(params, "EwbDt").or_else(|| answer_field(params, "ewayBillDate"));
                let valid = answer_field(params, "EwbValidTill").or_else(|| answer_field(params, "validUpto"));
                Self::save(
                    hc,
                    doc,
                    json!({"in_ewaybill_no": no, "in_e_way_bill_date": made, "in_e_way_bill_valid_till": valid, "in_e_way_bill_status": "Generated", "in_e_way_bill_error": null}),
                )
                .await?;
                json!({"status": "Generated", "ewaybill_no": no})
            }
            "record_e_way_bill_failure" => {
                if ewb != "Pending" {
                    return Ok(Some(json!({"status": ewb})));
                }
                let message = param(params, "message", 2000)?.unwrap_or_else(|| "the portal refused the e-way bill".into());
                Self::save(hc, doc, json!({"in_e_way_bill_status": "Failed", "in_e_way_bill_error": message})).await?;
                json!({"status": "Failed"})
            }
            "cancel_e_way_bill" => {
                if ewb != "Generated" {
                    return Err(Error::invalid("there is no e-way bill to cancel"));
                }
                within_window(s(doc, "in_e_way_bill_date"), "e-way bill")?;
                let reason = match param(params, "reason", 40)?.as_deref() {
                    Some("Duplicate") => 1,
                    Some("Order cancelled") => 2,
                    Some("Data entry mistake") => 3,
                    Some("Other") => 4,
                    _ => return Err(Error::field("reason", "Duplicate, Order cancelled, Data entry mistake or Other")),
                };
                let remark = param(params, "remark", 50)?.ok_or_else(|| Error::field("remark", "say why, in at most 50 characters"))?;
                let payload = json!({"ewbNo": s(doc, "in_ewaybill_no").and_then(|n| n.parse::<u64>().ok()), "cancelRsnCode": reason, "cancelRmrk": remark});
                Self::save(hc, doc, json!({"in_e_way_bill_status": "Cancelling"})).await?;
                Self::emit(hc, doc, "india.e_way_bill.cancel_requested", payload.clone()).await?;
                json!({"status": "Cancelling", "payload": payload})
            }
            "record_e_way_bill_cancellation" => {
                if !matches!(ewb.as_str(), "Generated" | "Cancelling") {
                    return Ok(Some(json!({"status": ewb})));
                }
                Self::save(hc, doc, json!({"in_e_way_bill_status": "Cancelled"})).await?;
                json!({"status": "Cancelled"})
            }
            _ => return Ok(None),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(v: Value) -> Doc {
        v.as_object().cloned().unwrap()
    }

    fn buyer() -> String {
        format!("27AAACL1234B1Z{}", crate::india::gstin_check_char("27AAACL1234B1Z").unwrap())
    }

    fn intra_state() -> Sources {
        let tax = |desc: &str, account: &str| {
            doc(json!({"charge_type": "On Net Total", "account": account, "description": desc, "rate": "9", "tax_amount": "153", "base_tax_amount": "153"}))
        };
        Sources {
            invoice: doc(json!({
                "number": "SI/2026-27/00042", "posting_date": "2026-10-30", "conversion_rate": "1", "in_place_of_supply": "27",
                "base_net_total": "1700", "base_grand_total": "2006", "base_rounded_total": "2006", "grand_total": "2006", "rounded_total": "2006",
            })),
            company: doc(json!({"company_name": "Test Industries Pvt Ltd", "in_gstin": "27AAPFU0939F1ZV", "in_state_code": "27",
                                "in_address": "Plot 7, MIDC\nAndheri East", "in_city": "Mumbai", "in_pincode": "400093"})),
            customer: doc(json!({"customer_name": "Local Traders", "in_gstin": buyer(), "in_gst_category": "Registered", "in_state_code": "27",
                                 "billing_address": "12 Market Road", "in_city": "Pune", "in_pincode": "411001"})),
            lines: vec![
                Line {
                    row: doc(json!({"item_name": "Steel Bracket", "qty": "10", "price_list_rate": "100", "rate": "90", "base_net_amount": "900"})),
                    item: doc(json!({"item_code": "BRK", "hsn_code": "73269099"})),
                    uqc: "NOS".into(),
                },
                Line {
                    row: doc(json!({"item_name": "Installation", "qty": "1", "price_list_rate": "800", "rate": "800", "base_net_amount": "800"})),
                    item: doc(json!({"item_code": "SVC", "hsn_code": "998719"})),
                    uqc: "OTH".into(),
                },
            ],
            taxes: vec![(tax("CGST 9%", "acc-c"), Head::Cgst), (tax("SGST 9%", "acc-s"), Head::Sgst)],
            original: None,
            currency: "INR".into(),
        }
    }

    #[test]
    fn an_intra_state_invoice_becomes_the_irp_json() {
        let out = intra_state().e_invoice().unwrap();
        assert_eq!(out["Version"], json!("1.1"));
        assert_eq!(out["TranDtls"]["SupTyp"], json!("B2B"));
        assert_eq!(out["DocDtls"], json!({"Typ": "INV", "No": "SI/2026-27/00042", "Dt": "30/10/2026"}));
        assert_eq!(
            (&out["SellerDtls"]["Addr1"], &out["SellerDtls"]["Addr2"], &out["SellerDtls"]["Pin"]),
            (&json!("Plot 7, MIDC"), &json!("Andheri East"), &json!(400_093))
        );
        assert_eq!((&out["BuyerDtls"]["Pos"], &out["BuyerDtls"]["Stcd"]), (&json!("27"), &json!("27")));
        let first = &out["ItemList"][0];
        assert_eq!(
            (&first["UnitPrice"], &first["TotAmt"], &first["Discount"], &first["AssAmt"]),
            (&json!(100.0), &json!(1000.0), &json!(100.0), &json!(900.0))
        );
        assert_eq!((&first["GstRt"], &first["CgstAmt"], &first["SgstAmt"], &first["TotItemVal"]), (&json!(18.0), &json!(81.0), &json!(81.0), &json!(1062.0)));
        assert_eq!((&out["ItemList"][1]["IsServc"], &out["ItemList"][1]["IsServc"]), (&json!("Y"), &json!("Y")));
        assert_eq!(
            out["ValDtls"],
            json!({"AssVal": 1700.0, "CgstVal": 153.0, "SgstVal": 153.0, "IgstVal": 0.0, "CesVal": 0.0, "StCesVal": 0,
                                           "Discount": 0, "OthChrg": 0.0, "RndOffAmt": 0.0, "TotInvVal": 2006.0})
        );
    }

    #[test]
    fn what_blocks_an_e_invoice_is_said_at_once() {
        let mut s = intra_state();
        s.invoice.insert("number".into(), json!("SINV-2026-27-00042"));
        s.company.remove("in_pincode");
        s.lines[0].item.insert("hsn_code".into(), json!("73"));
        s.taxes[0].0.insert("base_tax_amount".into(), json!("160"));
        let err = s.e_invoice().unwrap_err().to_string();
        for part in ["SINV-2026-27-00042", "PIN code", "HSN/SAC", "CGST (160)"] {
            assert!(err.contains(part), "{part} in {err}");
        }
        let mut b2c = intra_state();
        b2c.customer.insert("in_gst_category".into(), json!("Consumer"));
        assert!(b2c.e_invoice().unwrap_err().to_string().contains("registered businesses"));
    }

    #[test]
    fn exports_credit_notes_and_e_way_bills() {
        let mut export = intra_state();
        export.customer =
            doc(json!({"customer_name": "Globex Inc", "in_gst_category": "Overseas", "billing_address": "1 Main St, Springfield", "in_city": "Springfield"}));
        export.taxes.clear();
        for l in &mut export.lines {
            l.row.insert("price_list_rate".into(), json!("1"));
        }
        export.invoice.insert("conversion_rate".into(), json!("83"));
        export.invoice.insert("grand_total".into(), json!("20.48"));
        export.invoice.insert("rounded_total".into(), json!("0"));
        export.invoice.insert("base_rounded_total".into(), json!("0"));
        export.invoice.insert("base_grand_total".into(), json!("1700"));
        export.currency = "USD".into();
        let out = export.e_invoice().unwrap();
        assert_eq!((&out["TranDtls"]["SupTyp"], &out["BuyerDtls"]["Gstin"], &out["BuyerDtls"]["Pin"]), (&json!("EXPWOP"), &json!("URP"), &json!(999_999)));
        assert_eq!((&out["ExpDtls"]["ForCur"], &out["ValDtls"]["TotInvValFc"]), (&json!("USD"), &json!(20.48)));

        let mut credit = intra_state();
        credit.invoice.insert("is_return".into(), json!(true));
        credit.original = Some(("SI/2026-27/00041".into(), NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()));
        for l in &mut credit.lines {
            for k in ["qty", "base_net_amount"] {
                let v = d(&l.row, k);
                l.row.insert(k.into(), json!((-v).to_string()));
            }
        }
        let out = credit.e_invoice().unwrap();
        assert_eq!((&out["DocDtls"]["Typ"], &out["RefDtls"]["PrecDocDtls"][0]["InvDt"]), (&json!("CRN"), &json!("01/10/2026")));
        assert_eq!(out["ItemList"][0]["Qty"], json!(10.0), "positive quantities");

        let mut road = intra_state();
        assert!(road.e_way_bill().unwrap_err().to_string().contains("vehicle number"));
        road.invoice.insert("in_vehicle_no".into(), json!("mh 12 ab 1234"));
        road.invoice.insert("in_distance_km".into(), json!(150));
        let bill = road.e_way_bill().unwrap();
        assert_eq!(
            (&bill["vehicleNo"], &bill["transDistance"], &bill["toGstin"], &bill["toPincode"]),
            (&json!("MH12AB1234"), &json!("150"), &json!(buyer()), &json!(411_001))
        );
        assert_eq!((&bill["itemList"][0]["cgstRate"], &bill["itemList"][0]["igstRate"], &bill["totInvValue"]), (&json!(9.0), &json!(0.0), &json!(2006.0)));
        road.invoice.insert("in_e_invoice_status".into(), json!("Generated"));
        road.invoice.insert("in_irn".into(), json!("a".repeat(64)));
        let by_irn = road.e_way_bill().unwrap();
        assert_eq!((&by_irn["Irn"], &by_irn["TransMode"], &by_irn["VehType"]), (&json!("a".repeat(64)), &json!("1"), &json!("R")));
    }

    #[test]
    fn helpers() {
        assert!(valid_doc_no("SI/2026-27/00042") && !valid_doc_no("SINV-2026-27-00042") && !valid_doc_no("0001") && !valid_doc_no("si/1"));
        assert_eq!((guess_uqc("Nos"), guess_uqc("kg"), guess_uqc("Box"), guess_uqc("Litre"), guess_uqc("Hour")), ("NOS", "KGS", "BOX", "LTR", "OTH"));
        assert_eq!(
            (head("Output CGST 9%"), head("UTGST"), head("IGST Output - TI"), head("Compensation Cess"), head("Freight")),
            (Head::Cgst, Head::Sgst, Head::Igst, Head::Cess, Head::Other)
        );
        assert_eq!(address_lines("  12 Market Road \n\n Shivaji Nagar\nPune "), ("12 Market Road".into(), "Shivaji Nagar, Pune".into()));
        assert_eq!(irp_time("2026-10-30 14:05:00").unwrap().to_rfc3339(), "2026-10-30T08:35:00+00:00");
        // A signed QR code is a JWT whose payload's `data` is the invoice's summary, as JSON text.
        use base64::Engine as _;
        let data = json!({"data": json!({"Irn": "x", "DocNo": "SI/1"}).to_string(), "iss": "NIC"}).to_string();
        let jwt = format!("e30.{}.sig", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data));
        assert_eq!(qr_claims(&jwt).unwrap()["DocNo"], json!("SI/1"));
        assert_eq!(answer_field(&json!({"response": "{\"data\": {\"Irn\": \"abc\"}}"}), "Irn"), Some("abc".into()));
        assert_eq!(answer_field(&json!({"result": {"AckNo": 112010036563310_u64}}), "AckNo"), Some("112010036563310".into()));
    }
}
