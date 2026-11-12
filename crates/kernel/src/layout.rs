//! The one renderer of print layouts (docs/builder/decisions.md BD-22; the contract is `tools/PRINT-FORMATS-SPEC.md`
//! §4-5): a layout's sections, columns and blocks over a record become HTML, every string escaped by [`esc`], the
//! only inline styles numbers and colours the layout's check accepted, images only `data:` URLs the caller inlined
//! from the workspace's own files. It runs no template a tenant wrote. Letter heads' headers and footers are layouts
//! too (their own blocks only).
//!
//! The data it reads is loaded by the caller (`documents.rs`): the record as its reader may read it with its links,
//! the company and settings as the workspace, the images. Rendering itself is synchronous string building.

use ferrum_meta::layout::{
    As, BarKind, Block, BlockKind, Borders, DateStyle, Format, ImageSrc, LabelPosition, Layout, LineStyle, PageSetup, PathAt, Paths, ShowCurrency, Size, TOKEN,
    Weight,
};
use ferrum_meta::{EffEntity, EntityKind, FieldDef, FieldType, Schema, Scope};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

/// The page's own CSS (§5.2). Served at `GET /api/print/layout.css` to read, never to be re-implemented.
pub const BASE_CSS: &str = r#"
.pf,.pf *,.pf-letterhead *,.pf-letterhead-foot *{box-sizing:border-box}
html{-webkit-print-color-adjust:exact;print-color-adjust:exact}
body.pf{margin:0;color:var(--pf-color,#111);line-height:var(--pf-lh,1.45);background:#fff;font-variant-numeric:lining-nums}
.pf-font-sans{font-family:"Noto Sans","Noto Sans Devanagari","Noto Sans Arabic",system-ui,-apple-system,"Segoe UI",sans-serif}
.pf-font-serif{font-family:"Noto Serif","Noto Serif Devanagari","Noto Naskh Arabic",Georgia,serif}
.pf-font-mono{font-family:"Noto Sans Mono",ui-monospace,Menlo,Consolas,monospace}
.pf-size-xs,.pf-s-xs{font-size:8pt}.pf-size-sm,.pf-s-sm{font-size:9pt}.pf-size-md,.pf-s-md{font-size:10.5pt}
.pf-size-lg,.pf-s-lg{font-size:12pt}.pf-size-xl,.pf-s-xl{font-size:14pt}.pf-size-2xl,.pf-s-2xl{font-size:18pt}
.pf-doc{position:relative;z-index:1}
.pf-section{margin:0;break-inside:auto}
.pf-section-title{font-size:1.1em;margin:0 0 2mm;color:var(--pf-accent,#111);font-weight:600}
.pf-border{border:1px solid #c9ccd1;padding:2mm}
.pf-keep,.pf-signature,.pf-totals,tr{break-inside:avoid}
.pf-break-before{break-before:page}
.pf-break{break-after:page;height:0}
.pf-cols{display:grid;align-items:start}
.pf-col{min-width:0}
.pf-block{margin:0}
.pf-a-start{text-align:start}.pf-a-center{text-align:center}.pf-a-end{text-align:end}
.pf-w-bold{font-weight:700}.pf-w-normal{font-weight:400}
.pf-field{padding:.3mm 0}
.pf-field.pf-inline .pf-label{color:#555;margin-inline-end:1.5mm}
.pf-field.pf-inline .pf-label::after{content:":"}
.pf-field.pf-above .pf-label{display:block;color:#555;font-size:.85em;text-transform:none}
.pf-heading h1,.pf-heading h2,.pf-heading h3{margin:0;color:var(--pf-accent,#111);line-height:1.2}
.pf-heading h1{font-size:18pt;font-weight:700;letter-spacing:.01em}
.pf-heading h2{font-size:13pt;font-weight:600}
.pf-heading h3{font-size:9pt;font-weight:600;text-transform:uppercase;letter-spacing:.06em;color:#555;margin-bottom:1mm}
.pf-heading:not(:first-child){padding-top:3mm}
.pf-text p{margin:0 0 1.5mm}
.pf-image{margin:0}
.pf-image img{display:block;max-width:100%}
.pf-a-center.pf-image img{margin-inline:auto}.pf-a-end.pf-image img{margin-inline-start:auto}
.pf-table{width:100%;border-collapse:collapse;table-layout:fixed}
.pf-table th,.pf-table td{padding:1.1mm 1.6mm;vertical-align:top;overflow-wrap:anywhere}
.pf-table thead{display:table-header-group}
.pf-table tfoot{display:table-footer-group}
.pf-table th{font-weight:600;color:#333;font-size:.9em;text-align:start;border-bottom:1.5px solid var(--pf-accent,#111)}
.pf-table th.pf-a-end{text-align:end}.pf-table th.pf-a-center{text-align:center}
.pf-borders-rows tbody td{border-bottom:1px solid #e1e3e6}
.pf-borders-all th,.pf-borders-all td{border:1px solid #c9ccd1}
.pf-borders-none th{border-bottom:1px solid #c9ccd1}
.pf-striped tbody tr:nth-child(even) td{background:#f6f7f8}
.pf-rn{width:auto;color:#666;text-align:end}
.pf-num,.pf-table td.pf-a-end,.pf-totals td{font-variant-numeric:tabular-nums}
.pf-secondary{color:#666;font-size:.86em;margin-top:.5mm}
.pf-total-row td{font-weight:700;border-top:1.5px solid var(--pf-accent,#111)}
.pf-totals{border-collapse:collapse;margin-inline-start:auto}
.pf-totals.pf-a-start{margin-inline-start:0}.pf-totals.pf-a-center{margin-inline:auto}
.pf-totals th{text-align:start;font-weight:400;color:#444;padding:.9mm 2mm .9mm 0}
.pf-totals td{text-align:end;padding:.9mm 0}
.pf-totals .pf-emphasis th,.pf-totals .pf-emphasis td{font-weight:700;font-size:1.12em;color:var(--pf-accent,#111);border-top:1.5px solid var(--pf-accent,#111);padding-top:1.6mm}
.pf-signature{display:block}
.pf-a-end.pf-signature{margin-inline-start:auto}.pf-a-center.pf-signature{margin-inline:auto}
.pf-signature img{display:block;max-height:18mm;max-width:100%;margin-bottom:1mm}
.pf-sign-line{border-bottom:1px solid #333;height:14mm}
.pf-sign-label{margin-top:1mm;font-size:.9em;text-align:center}
.pf-barcode svg{display:block;height:100%;width:auto;max-width:100%}
.pf-a-center.pf-barcode svg{margin-inline:auto}.pf-a-end.pf-barcode svg{margin-inline-start:auto}
.pf-copy-label{text-transform:uppercase;letter-spacing:.06em;color:#444}
.pf-divider{border:0;border-top-style:solid;border-top-color:#c9ccd1;margin:2mm 0}
.pf-t-1{border-top-width:1px}.pf-t-2{border-top-width:2px}.pf-t-3{border-top-width:3px}.pf-dashed{border-top-style:dashed}
.pf-letterhead{margin-bottom:4mm}.pf-letterhead-foot{margin-top:6mm}
.pf-watermark{position:fixed;inset:40% 0 auto 0;text-align:center;transform:rotate(-30deg);font-size:72pt;font-weight:700;color:rgba(0,0,0,.08);z-index:0;pointer-events:none;white-space:nowrap}
.pf-design .pf-block,.pf-design .pf-section,.pf-design .pf-col{outline:1px dashed transparent;min-height:4px}
.pf-design .pf-slot{min-height:6px}
.pf-design .pf-hidden{opacity:.35}
.pf-ghost{height:2.4mm;overflow:hidden;border-top:1px dashed #b8c2cc;font:6.5pt/2.2mm system-ui,sans-serif;color:#8a94a0;white-space:nowrap}
.pf-ghost span{padding-inline-start:.5mm}
.pf-table th.pf-ghost-col,.pf-table td.pf-ghost-col{padding:0;border:0;border-inline-start:1px dashed #b8c2cc;overflow:hidden}
.pf-placeholder{color:#9aa0a6;font-style:italic}
@media screen{body.pf{padding:var(--pf-screen-pad,0)}}
"#;

/// HTML-escapes text: the one function every string from a record, a layout or a letter head goes through.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Whether a language is written right to left.
pub fn rtl(lang: &str) -> bool {
    matches!(lang.split(['-', '_']).next().unwrap_or_default().to_ascii_lowercase().as_str(), "ar" | "he" | "fa" | "ur")
}

/// Whether the print is the designer's canvas (slots, placeholders, dimmed hidden blocks, `data-b` marks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Print,
    /// The canvas: what a condition hides on this record is a collapsed ghost the designer can still select.
    Design,
    /// The canvas showing every part at full size, what a condition hides dimmed ("show hidden parts").
    DesignAll,
}

/// Where a layout is rendered: a document's body, a letter head placed in the body, or a letter head the PDF
/// service repeats in the page's margin (where `print.page` and `print.pages` are filled in).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Body,
    LetterHead,
    Margin,
}

/// What a layout is rendered over.
#[derive(Clone, Copy)]
pub struct Data<'a> {
    pub schema: &'a Schema,
    pub ent: &'a EffEntity,
    /// The record as its reader may read it, `_titles` of its links included.
    pub doc: &'a Value,
    pub links: &'a Map<String, Value>,
    pub company: &'a Value,
    pub settings: &'a Value,
    /// `copy`, `now`, `language`, `title`, `entity`.
    pub print: &'a Map<String, Value>,
    pub language: &'a str,
    pub currency: Option<&'a str>,
    pub currencies: &'a Value,
    /// `/files/<id>` → its `data:` URL, as the caller inlined them.
    pub images: &'a HashMap<String, String>,
    pub mode: Mode,
    pub part: Part,
}

/// A layout rendered: its HTML and what it could not print (said to a designer's preview).
pub struct Rendered {
    pub html: String,
    pub warnings: Vec<String>,
}

/// A value found by a path: the value, the field it is (when it is one), its link's title.
struct Got<'s> {
    v: Value,
    field: Option<&'s FieldDef>,
    title: Option<String>,
    name: String,
}

fn empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => false,
    }
}

/// A record as conditions read it: as CEL reads a record, an empty text a `""` (so `doc.terms != ""` says what it
/// means).
fn cel_ready(ent: Option<&EffEntity>, v: &Value, schema: &Schema) -> Value {
    let Some(ent) = ent else { return v.clone() };
    let mut out = ent.cel_view(v, schema);
    if let Value::Object(m) = &mut out {
        for f in ent.def.fields.iter().filter(|f| !f.ftype.is_numeric() && !matches!(f.ftype, FieldType::Check | FieldType::Table | FieldType::Json)) {
            if let Some(x) = m.get_mut(&f.name).filter(|x| x.is_null()) {
                *x = Value::String(String::new());
            }
        }
        for s in ["number", "status"] {
            if let Some(x) = m.get_mut(s).filter(|x| x.is_null()) {
                *x = Value::String(String::new());
            }
        }
    }
    out
}

const SYSTEM: [(&str, &str, FieldType); 6] = [
    ("number", "Number", FieldType::Data),
    ("docstatus", "Document Status", FieldType::Int),
    ("status", "Status", FieldType::Data),
    ("created_at", "Created", FieldType::Datetime),
    ("updated_at", "Updated", FieldType::Datetime),
    ("created_by", "Created By", FieldType::Data),
];
const PRINT: [(&str, &str); 10] = [
    ("copy", "Copy"),
    ("now", "Printed at"),
    ("time", "Printed at, the workspace's time"),
    ("letterhead", "Wears a letter head"),
    ("letterhead_name", "Letter head"),
    ("language", "Language"),
    ("title", "Title"),
    ("entity", "Document type"),
    ("page", "Page"),
    ("pages", "Pages"),
];

struct Renderer<'a> {
    d: &'a Data<'a>,
    cel: [(&'static str, Value); 4],
    user: Value,
    warnings: Vec<String>,
    company: Option<&'a EffEntity>,
}

impl<'a> Renderer<'a> {
    fn new(d: &'a Data<'a>) -> Self {
        let company = d.schema.entity("Company");
        let links: Map<String, Value> = d
            .links
            .iter()
            .map(|(k, v)| {
                let target = d.ent.field(k).and_then(|f| f.to.as_deref()).and_then(|t| d.schema.entity(t));
                (k.clone(), cel_ready(target.map(|t| &**t), v, d.schema))
            })
            .collect();
        let cel = [
            ("doc", cel_ready(Some(d.ent), d.doc, d.schema)),
            ("links", Value::Object(links)),
            ("company", if d.company.is_null() { json!({}) } else { cel_ready(company.map(|c| &**c), d.company, d.schema) }),
            ("print", Value::Object(d.print.clone())),
        ];
        Renderer { d, cel, user: json!({"settings": d.settings}), warnings: Vec::new(), company: company.map(|c| &**c) }
    }

    /// A condition: absent is true; one that cannot be evaluated (a value missing) is false.
    fn when(&self, c: &Option<String>) -> bool {
        let Some(c) = c else { return true };
        let Ok(e) = ferrum_meta::Expr::compile(c) else { return false };
        let extra: Vec<(&str, &Value)> = self.cel.iter().skip(1).map(|(k, v)| (*k, v)).collect();
        e.eval_bool_with(&Scope { doc: &self.cel[0].1, old: None, parent: None, user: &self.user }, &extra).unwrap_or(false)
    }

    fn lookup(&self, path: &str, row: Option<(&Value, &'a EffEntity)>) -> Got<'a> {
        let parts: Vec<&str> = path.split('.').collect();
        let none = || Got { v: Value::Null, field: None, title: None, name: String::new() };
        let schema = self.d.schema;
        let (obj, ent, name): (&Value, Option<&EffEntity>, &str) = match parts.as_slice() {
            ["doc", f] => (self.d.doc, Some(self.d.ent), f),
            ["links", l, f] => {
                let target = self.d.ent.field(l).filter(|x| x.ftype == FieldType::Link).and_then(|x| x.to.as_deref()).and_then(|t| schema.entity(t));
                match (self.d.links.get(*l), target) {
                    (Some(o), Some(t)) => (o, Some(&**t), f),
                    _ => return none(),
                }
            }
            ["company", f] => (self.d.company, self.company, f),
            ["settings", e, f] => match self.d.settings.get(*e) {
                Some(o) => (o, schema.entity(e).map(|x| &**x), f),
                None => return none(),
            },
            ["row", f] => match row {
                Some((r, e)) => (r, Some(e), f),
                None => return none(),
            },
            ["print", f] => {
                let v = match (*f, self.d.part) {
                    ("page", Part::Margin) | ("pages", Part::Margin) => Value::String(format!("\u{0}{f}")),
                    ("page", _) | ("pages", _) => Value::Null,
                    _ => self.d.print.get(*f).cloned().unwrap_or(Value::Null),
                };
                return Got { v, field: None, title: None, name: f.to_string() };
            }
            _ => return none(),
        };
        let field = ent.and_then(|e| e.field(name));
        if field.is_some_and(|f| f.ftype == FieldType::Secret) {
            return none();
        }
        let title = obj.get("_titles").and_then(|t| t.get(name)).and_then(Value::as_str).map(str::to_string);
        Got { v: obj.get(name).cloned().unwrap_or(Value::Null), field, title, name: name.to_string() }
    }

    /// The first of `paths` that is not empty.
    fn first(&self, paths: &Paths, row: Option<(&Value, &'a EffEntity)>) -> Option<Got<'a>> {
        paths.list().iter().map(|p| self.lookup(p, row)).find(|g| !empty(&g.v))
    }

    fn label_of(&self, paths: &Paths, row_ent: Option<&EffEntity>) -> String {
        let p = paths.list().first().map(String::as_str).unwrap_or_default();
        let parts: Vec<&str> = p.split('.').collect();
        let schema = self.d.schema;
        let field = |e: Option<&EffEntity>, n: &str| {
            e.and_then(|e| e.field(n)).map(|f| f.label()).or_else(|| SYSTEM.iter().find(|s| s.0 == n).map(|s| s.1.to_string()))
        };
        let l = match parts.as_slice() {
            ["doc", f] => field(Some(self.d.ent), f),
            ["row", f] => field(row_ent, f),
            ["company", f] => field(self.company, f),
            ["links", l, f] => field(self.d.ent.field(l).and_then(|x| x.to.as_deref()).and_then(|t| schema.entity(t)).map(|x| &**x), f),
            ["settings", e, f] => field(schema.entity(e).map(|x| &**x), f),
            ["print", f] => PRINT.iter().find(|x| x.0 == *f).map(|x| x.1.to_string()),
            _ => None,
        };
        l.unwrap_or_else(|| ferrum_meta::humanize(parts.last().copied().unwrap_or_default()))
    }

    /// Whether `print.page` and `print.pages` say nothing here: anywhere but a part the PDF service repeats.
    fn no_page_numbers(&self) -> bool {
        self.d.part != Part::Margin
    }

    fn locale(&self) -> &'static crate::docfmt::Locale {
        crate::docfmt::locale(self.d.language)
    }

    fn currency(&self, code: Option<&str>) -> Option<crate::docfmt::Currency> {
        let code = code.or(self.d.currency)?.trim().to_ascii_uppercase();
        let known = self.d.currencies.get(&code).and_then(crate::docfmt::Currency::from_record);
        Some(known.unwrap_or_else(|| crate::docfmt::Currency::iso(&code)))
    }

    /// A value as it prints, escaped (a line break a `<br>`); None when there is nothing to print.
    fn show(&mut self, g: &Got<'_>, fmt: Option<&Format>) -> Option<(String, bool)> {
        if empty(&g.v) {
            return None;
        }
        if let Some(which) = g.v.as_str().and_then(|s| s.strip_prefix('\u{0}')) {
            let class = if which == "page" { "pageNumber" } else { "totalPages" };
            return Some((format!("<span class=\"{class}\"></span>"), true));
        }
        let fmt = fmt.cloned().unwrap_or_default();
        let ftype = g.field.map(|f| f.ftype).or_else(|| SYSTEM.iter().find(|s| s.0 == g.name).map(|s| s.2));
        let auto = match fmt.as_.unwrap_or(As::Auto) {
            As::Auto => {
                if let Some(t) = &g.title {
                    return Some((esc(t), false));
                }
                match ftype {
                    Some(FieldType::Currency) => As::Amount,
                    Some(FieldType::Date) => As::Date,
                    Some(FieldType::Datetime) => As::Datetime,
                    Some(FieldType::Check) => As::Check,
                    Some(FieldType::Int | FieldType::Decimal) => As::Number,
                    Some(FieldType::Percent) => As::Percent,
                    Some(FieldType::Text | FieldType::Markdown | FieldType::Code) => As::Multiline,
                    Some(FieldType::Attach | FieldType::Json | FieldType::Table | FieldType::Secret) => return None,
                    _ => As::Text,
                }
            }
            a => a,
        };
        let raw = || match &g.v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let abs = match &fmt.sign {
            Some(ferrum_meta::layout::Sign::Always(_)) => true,
            Some(ferrum_meta::layout::Sign::When(w)) => self.when(&Some(w.abs_when.clone())),
            None => false,
        };
        let decimal = || crate::value::parse_decimal(&g.v).ok().map(|d| if abs { d.abs() } else { d });
        let places = |d: Decimal| fmt.decimals.map(u32::from).or_else(|| g.field.and_then(|f| f.precision)).unwrap_or_else(|| d.normalize().scale()).min(12);
        let numeric = matches!(auto, As::Number | As::Amount | As::Percent);
        let s = match auto {
            As::Text | As::Auto => esc(&g.title.clone().unwrap_or_else(raw)),
            As::Multiline => esc(&raw()).replace("\r\n", "\n").replace('\n', "<br>"),
            As::Number => esc(&crate::docfmt::number(decimal()?, places(decimal()?), self.locale())),
            As::Percent => {
                let d = decimal()?;
                format!("{}%", esc(&crate::docfmt::number(d, fmt.decimals.map(u32::from).unwrap_or_else(|| d.normalize().scale().min(4)), self.locale())))
            }
            As::Amount => {
                let d = decimal()?;
                match self.currency(fmt.currency.as_deref()) {
                    Some(mut c) => {
                        if let Some(p) = fmt.decimals {
                            c.precision = p.into();
                        }
                        let show = match fmt.show.unwrap_or(ShowCurrency::Symbol) {
                            ShowCurrency::Symbol => crate::docfmt::Show::Symbol,
                            ShowCurrency::Code => crate::docfmt::Show::Code,
                            ShowCurrency::None => crate::docfmt::Show::Nothing,
                        };
                        esc(&crate::docfmt::amount(d, &c, self.locale(), show))
                    }
                    None => esc(&crate::docfmt::number(d, fmt.decimals.map(u32::from).unwrap_or(2), self.locale())),
                }
            }
            As::Date | As::Datetime => {
                let v = raw();
                // Day, the month's name and year unless the format says otherwise: never a bare numeric order nobody chose.
                let style = match fmt.date {
                    Some(DateStyle::Short) => "short",
                    Some(DateStyle::Long) => "long",
                    _ => "medium",
                };
                let day = crate::docfmt::date(&v, style, self.locale()).unwrap_or_else(|_| v.clone());
                let time = (auto == As::Datetime).then(|| v.get(11..16).filter(|t| t.as_bytes().get(2) == Some(&b':'))).flatten();
                esc(&match time {
                    Some(t) => format!("{day} {t}"),
                    None => day,
                })
            }
            As::Words => {
                let d = decimal()?;
                let Some(c) = self.currency(fmt.currency.as_deref()) else {
                    self.warnings.push(format!("{}: an amount in words needs a currency", g.name));
                    return None;
                };
                match crate::docfmt::words(d, &c, self.d.language, None).or_else(|_| crate::docfmt::words(d, &c, "en", None)) {
                    Ok(w) => esc(&w),
                    Err(e) => {
                        self.warnings.push(format!("{}: {e}", g.name));
                        return None;
                    }
                }
            }
            As::Check => {
                let yes = matches!(&g.v, Value::Bool(true)) || matches!(g.v.as_str(), Some("1" | "true")) || g.v.as_i64() == Some(1);
                let (y, n) = match self.d.language.split(['-', '_']).next().unwrap_or_default() {
                    "hi" => ("हाँ", "नहीं"),
                    "ar" => ("نعم", "لا"),
                    "de" => ("Ja", "Nein"),
                    "fr" => ("Oui", "Non"),
                    "es" | "it" => ("Sí", "No"),
                    "pt" => ("Sim", "Não"),
                    _ => ("Yes", "No"),
                };
                (if yes { y } else { n }).to_string()
            }
        };
        Some((s, numeric))
    }

    /// `text` with its `{path}` tokens filled in, escaped.
    fn tokens(&mut self, text: &str) -> String {
        let mut out = String::new();
        let mut at = 0;
        for c in TOKEN.captures_iter(text) {
            let m = c.get(0).expect("a match");
            out.push_str(&esc(&text[at..m.start()]));
            let g = self.lookup(&c[1], None);
            let fmt = c.get(2).and_then(|a| As::parse(a.as_str())).map(|a| Format { as_: Some(a), ..Default::default() });
            // A field's own format (an amount, a date) unless the token names one.
            let got = Got { v: g.v.clone(), field: g.field, title: g.title.clone(), name: g.name.clone() };
            out.push_str(&self.show(&got, fmt.as_ref()).map(|s| s.0).unwrap_or_default());
            at = m.end();
        }
        out.push_str(&esc(&text[at..]));
        out
    }

    fn image(&mut self, id: &str, src: &ImageSrc) -> Option<String> {
        let url = match src {
            ImageSrc::File { file } => file.clone(),
            ImageSrc::Value { value } => self.lookup(value, None).v.as_str()?.to_string(),
        };
        let found = self.d.images.get(&url).cloned();
        if found.is_none() && url.starts_with("/files/") {
            self.warnings.push(format!("{id}: the image {url} is not printed (not one of this workspace's files you may read, or larger than 1 MB)"));
        }
        found
    }

    fn style(margin_top: Option<u8>, color: Option<&str>) -> String {
        let mut s = String::new();
        if let Some(m) = margin_top.filter(|m| *m > 0) {
            let _ = write!(s, "margin-top:{m}mm;");
        }
        if let Some(c) = color {
            let _ = write!(s, "color:{c};");
        }
        if s.is_empty() { s } else { format!(" style=\"{s}\"") }
    }

    fn mark(&self, attr: &str, id: &str) -> String {
        if self.d.mode != Mode::Print { format!(" {attr}=\"{}\"", esc(id)) } else { String::new() }
    }

    /// A part a condition hides on this record, on the canvas: a thin dashed marker with its id and kind.
    fn ghost(&self, kind: &str, id: &str) -> String {
        format!(
            "<div class=\"pf-ghost\" data-b=\"{id}\" data-ghost=\"{kind}\" aria-hidden=\"true\"><span>{kind} · {id}</span></div>",
            id = esc(id),
            kind = esc(kind)
        )
    }

    fn placeholder(&self, text: &str) -> String {
        format!("<span class=\"pf-placeholder\">{}</span>", esc(text))
    }

    fn sections(&mut self, l: &Layout, out: &mut String) {
        for s in &l.sections {
            let shown = self.when(&s.when);
            if !shown && self.d.mode == Mode::Print {
                continue;
            }
            if !shown && self.d.mode == Mode::Design {
                out.push_str(&self.ghost("section", &s.id));
                continue;
            }
            let mut class = String::from("pf-section");
            for (on, c) in [(s.border, " pf-border"), (s.keep_together, " pf-keep"), (s.break_before, " pf-break-before"), (!shown, " pf-hidden")] {
                if on {
                    class.push_str(c);
                }
            }
            let mut style = String::new();
            if let Some(m) = s.margin_top_mm.filter(|m| *m > 0) {
                let _ = write!(style, "margin-top:{m}mm;");
            }
            if let Some(b) = &s.background {
                let _ = write!(style, "background:{b};");
            }
            if let Some(p) = s.padding_mm {
                let _ = write!(style, "padding:{p}mm;");
            }
            let style = if style.is_empty() { style } else { format!(" style=\"{style}\"") };
            let _ = write!(out, "<section class=\"{class}\"{}{style}>", self.mark("data-b", &s.id));
            if let Some(t) = &s.title {
                let _ = write!(out, "<h2 class=\"pf-section-title\">{}</h2>", esc(t));
            }
            let widths: Vec<u8> = s.widths.clone().unwrap_or_else(|| vec![1; s.columns.len()]);
            let template: Vec<String> = widths.iter().map(|w| format!("minmax(0,{w}fr)")).collect();
            let _ = write!(out, "<div class=\"pf-cols\" style=\"grid-template-columns:{};column-gap:{}mm\">", template.join(" "), s.gap_mm.unwrap_or(6));
            for c in &s.columns {
                let _ = write!(out, "<div class=\"pf-col\"{}>", self.mark("data-b", &c.id));
                for (i, b) in c.blocks.iter().enumerate() {
                    if self.d.mode != Mode::Print {
                        let _ = write!(out, "<div class=\"pf-slot\" data-slot=\"{}:{i}\"></div>", esc(&c.id));
                    }
                    self.block(b, out);
                }
                if self.d.mode != Mode::Print {
                    let _ = write!(out, "<div class=\"pf-slot\" data-slot=\"{}:{}\"></div>", esc(&c.id), c.blocks.len());
                }
                out.push_str("</div>");
            }
            out.push_str("</div></section>");
        }
    }

    fn block(&mut self, b: &Block, out: &mut String) {
        let shown = self.when(&b.when);
        let design = self.d.mode != Mode::Print;
        if !shown && !design {
            return;
        }
        if !shown && self.d.mode == Mode::Design {
            out.push_str(&self.ghost(b.kind.name(), &b.id));
            return;
        }
        let align = b.align.map(|a| format!(" pf-a-{}", a.name())).unwrap_or_default();
        let hidden = if shown { "" } else { " pf-hidden" };
        let data = self.mark("data-b", &b.id);
        let id = b.id.as_str();
        match &b.kind {
            BlockKind::Field { value, label, label_position, weight, size, color, format, hide_empty } => {
                let row = None;
                let got = self.first(value, row);
                let shownv = got.as_ref().and_then(|g| {
                    let g = Got { v: g.v.clone(), field: g.field, title: g.title.clone(), name: g.name.clone() };
                    self.show(&g, format.as_ref())
                });
                let label = match label {
                    Some(Some(l)) => Some(l.clone()),
                    Some(None) => None,
                    None => Some(self.label_of(value, None)),
                };
                let v = match shownv {
                    Some((v, _)) => v,
                    None if design => self.placeholder(&label.clone().unwrap_or_else(|| value.list().first().cloned().unwrap_or_default())),
                    None if hide_empty.unwrap_or(true) => return,
                    None => String::new(),
                };
                let pos = if *label_position == Some(LabelPosition::Above) { "pf-above" } else { "pf-inline" };
                let w = if *weight == Some(Weight::Bold) { " pf-w-bold" } else { "" };
                let s = size.map(|s| format!(" pf-s-{}", s.name())).unwrap_or_default();
                let _ = write!(out, "<div class=\"pf-block pf-field {pos}{align}{w}{s}{hidden}\"{data}{}>", Self::style(b.margin_top_mm, color.as_deref()));
                if let Some(l) = &label {
                    let _ = write!(out, "<span class=\"pf-label\">{}</span>", esc(l));
                }
                let _ = write!(out, "<span class=\"pf-value\">{v}</span></div>");
            }
            BlockKind::Heading { text, level, size, color } => {
                // "Page {print.page} of {print.pages}" says something only where the PDF service numbers pages.
                if self.no_page_numbers() && TOKEN.captures_iter(text).any(|c| is_page(&c[1])) {
                    return;
                }
                let lv = level.unwrap_or(1).clamp(1, 3);
                let s = size.map(|s| format!(" style=\"font-size:{}pt\"", pt(s))).unwrap_or_default();
                let t = self.tokens(text);
                let _ = write!(
                    out,
                    "<div class=\"pf-block pf-heading pf-h{lv}{align}{hidden}\"{data}{}><h{lv}{s}>{t}</h{lv}></div>",
                    Self::style(b.margin_top_mm, color.as_deref())
                );
            }
            BlockKind::Text { content, size, color } => {
                let s = size.map(|s| format!(" pf-s-{}", s.name())).unwrap_or_default();
                let _ = write!(out, "<div class=\"pf-block pf-text{align}{s}{hidden}\"{data}{}>", Self::style(b.margin_top_mm, color.as_deref()));
                for p in content {
                    // A paragraph numbering pages is left out where pages are not numbered (the HTML, a letter head in
                    // the body), never printed half ("Page  of ").
                    if self.no_page_numbers() && p.spans.iter().flat_map(|sp| sp.v.iter().flat_map(|v| v.list())).any(|v| is_page(v)) {
                        continue;
                    }
                    let a = p.align.map(|a| a.name()).unwrap_or("start");
                    let _ = write!(out, "<p class=\"pf-a-{a}\">");
                    for sp in &p.spans {
                        let mut t = match (&sp.t, &sp.v) {
                            (Some(t), _) => esc(t).replace('\n', "<br>"),
                            (None, Some(v)) => {
                                let got = self.first(v, None);
                                match got.map(|g| Got { v: g.v.clone(), field: g.field, title: g.title.clone(), name: g.name.clone() }) {
                                    Some(g) => self.show(&g, sp.format.as_ref()).map(|x| x.0).unwrap_or_default(),
                                    None if design => self.placeholder(v.list().first().map(String::as_str).unwrap_or_default()),
                                    None => String::new(),
                                }
                            }
                            _ => String::new(),
                        };
                        for (on, tag) in [(sp.u, "u"), (sp.i, "i"), (sp.b, "b")] {
                            if on {
                                t = format!("<{tag}>{t}</{tag}>");
                            }
                        }
                        out.push_str(&t);
                    }
                    out.push_str("</p>");
                }
                out.push_str("</div>");
            }
            BlockKind::Image { src, width_mm, height_mm, fit, alt } => {
                let Some(url) = self.image(id, src) else {
                    if design {
                        let _ = write!(out, "<figure class=\"pf-block pf-image{align}{hidden}\"{data}>{}</figure>", self.placeholder("Image"));
                    }
                    return;
                };
                let mut st = format!("height:{}mm;", num(height_mm.unwrap_or(20.0)));
                if let Some(w) = width_mm {
                    let _ = write!(st, "width:{}mm;", num(*w));
                }
                let _ = write!(st, "object-fit:{};", if *fit == Some(ferrum_meta::layout::Fit::Cover) { "cover" } else { "contain" });
                let _ = write!(
                    out,
                    "<figure class=\"pf-block pf-image{align}{hidden}\"{data}{}><img src=\"{}\" alt=\"{}\" style=\"{st}\"></figure>",
                    Self::style(b.margin_top_mm, None),
                    esc(&url),
                    esc(alt.as_deref().unwrap_or_default())
                );
            }
            BlockKind::Table { .. } => self.table(b, &align, hidden, &data, out),
            BlockKind::Totals { rows, width_pct } => {
                let a = b.align.map(|a| a.name()).unwrap_or("end");
                let mut body = String::new();
                for r in rows {
                    if !self.when(&r.when) {
                        continue;
                    }
                    let got = self.first(&r.value, None).map(|g| Got { v: g.v.clone(), field: g.field, title: g.title.clone(), name: g.name.clone() });
                    let shownv = got.as_ref().and_then(|g| {
                        if r.hide_zero && crate::value::parse_decimal(&g.v).is_ok_and(|d| d.is_zero()) {
                            return None;
                        }
                        Some(g)
                    });
                    let v = match shownv {
                        Some(g) => match self.show(g, r.format.as_ref()) {
                            Some(v) => v.0,
                            None => continue,
                        },
                        None if r.hide_zero || got.is_none() => {
                            if design {
                                self.placeholder(&r.label)
                            } else {
                                continue;
                            }
                        }
                        None => continue,
                    };
                    let label = self.tokens(&r.label);
                    let _ = write!(body, "<tr{}><th>{label}</th><td>{v}</td></tr>", if r.emphasis { " class=\"pf-emphasis\"" } else { "" });
                }
                if body.is_empty() && !design {
                    return;
                }
                let _ = write!(
                    out,
                    "<table class=\"pf-block pf-totals pf-a-{a}{hidden}\"{data} style=\"width:{}%;{}\">{body}</table>",
                    num(width_pct.unwrap_or(45.0)),
                    b.margin_top_mm.filter(|m| *m > 0).map(|m| format!("margin-top:{m}mm")).unwrap_or_default()
                );
            }
            BlockKind::Signature { label, image, width_mm, line } => {
                let img = image.as_ref().and_then(|src| self.image(id, src)).map(|u| format!("<img src=\"{}\" alt=\"\">", esc(&u))).unwrap_or_default();
                let l = self.tokens(label);
                let line = if line.unwrap_or(true) { "<div class=\"pf-sign-line\"></div>" } else { "" };
                let _ = write!(
                    out,
                    "<div class=\"pf-block pf-signature{align}{hidden}\"{data} style=\"width:{}mm;{}\">{img}{line}<div class=\"pf-sign-label\">{l}</div></div>",
                    num(width_mm.unwrap_or(60.0)),
                    b.margin_top_mm.filter(|m| *m > 0).map(|m| format!("margin-top:{m}mm")).unwrap_or_default()
                );
            }
            BlockKind::Barcode { value, kind, height_mm, show_text } => {
                let text = self.first(value, None).and_then(|g| match &g.v {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                });
                let Some(text) = text else {
                    if design {
                        let _ = write!(out, "<div class=\"pf-block pf-barcode{align}{hidden}\"{data}>{}</div>", self.placeholder("Barcode"));
                    }
                    return;
                };
                let (svg, h) = match kind {
                    BarKind::Qr => {
                        if text.len() > 1000 {
                            self.warnings.push(format!("{id}: a QR code holds at most 1000 bytes; this value has {}", text.len()));
                            return;
                        }
                        match qrcode::QrCode::new(text.as_bytes()) {
                            Ok(code) => {
                                let svg = code.render::<qrcode::render::svg::Color>().min_dimensions(150, 150).quiet_zone(true).build();
                                (svg[svg.find("<svg").unwrap_or(0)..].to_string(), height_mm.unwrap_or(25.0))
                            }
                            Err(e) => {
                                self.warnings.push(format!("{id}: {e}"));
                                return;
                            }
                        }
                    }
                    k => {
                        let k = if *k == BarKind::Ean13 { "ean13" } else { "code128" };
                        if k == "code128" && text.chars().count() > 80 {
                            self.warnings.push(format!("{id}: a Code 128 barcode holds at most 80 characters"));
                            return;
                        }
                        match crate::docfmt::barcode(&text, k, 2, 60, show_text.unwrap_or(true)) {
                            Ok(svg) => (svg, height_mm.unwrap_or(12.0)),
                            Err(e) => {
                                self.warnings.push(format!("{id}: {e}"));
                                return;
                            }
                        }
                    }
                };
                let _ = write!(
                    out,
                    "<div class=\"pf-block pf-barcode{align}{hidden}\"{data} style=\"height:{}mm;{}\">{svg}</div>",
                    num(h),
                    b.margin_top_mm.filter(|m| *m > 0).map(|m| format!("margin-top:{m}mm")).unwrap_or_default()
                );
            }
            BlockKind::CopyLabel { size, weight } => {
                let copy = self.d.print.get("copy").and_then(Value::as_str).filter(|c| !c.is_empty()).map(esc);
                let text = match copy {
                    Some(c) => c,
                    None if design => self.placeholder("Copy label"),
                    None => return,
                };
                let s = format!(" pf-s-{}", size.unwrap_or(Size::Sm).name());
                let w = if *weight == Some(Weight::Normal) { "" } else { " pf-w-bold" };
                let _ = write!(out, "<div class=\"pf-block pf-copy-label{align}{s}{w}{hidden}\"{data}{}>{text}</div>", Self::style(b.margin_top_mm, None));
            }
            BlockKind::Divider { thickness, style, color } => {
                let mut st = String::new();
                if let Some(c) = color {
                    let _ = write!(st, "border-top-color:{c};");
                }
                if let Some(m) = b.margin_top_mm.filter(|m| *m > 0) {
                    let _ = write!(st, "margin-top:{m}mm;");
                }
                let st = if st.is_empty() { st } else { format!(" style=\"{st}\"") };
                let ls = if *style == Some(LineStyle::Dashed) { "pf-dashed" } else { "pf-solid" };
                let _ = write!(out, "<hr class=\"pf-block pf-divider pf-t-{} {ls}{hidden}\"{data}{st}>", thickness.unwrap_or(1).clamp(1, 3));
            }
            BlockKind::Spacer { height_mm } => {
                let _ = write!(out, "<div class=\"pf-block pf-spacer{hidden}\"{data} style=\"height:{}mm\"></div>", num(*height_mm));
            }
            BlockKind::PageBreak {} => {
                let _ = write!(out, "<div class=\"pf-break{hidden}\"{data}></div>");
            }
        }
    }

    fn table(&mut self, b: &Block, align: &str, hidden: &str, data: &str, out: &mut String) {
        let BlockKind::Table { value, columns, header, row_numbers, borders, striped, size, hide_zero_rows, totals, totals_label, empty_text, hide_empty } =
            &b.kind
        else {
            return;
        };
        let design = self.d.mode != Mode::Print;
        let schema: &'a Schema = self.d.schema;
        let Some(child) = table_rows_of(schema, self.d.ent, value) else { return };
        // The record's lines as its reader may read them, a linked record's as they may, the company's as the workspace.
        let parts: Vec<&str> = value.split('.').collect();
        let holder = match parts.as_slice() {
            ["doc", t] => self.d.doc.get(*t),
            ["links", l, t] => self.d.links.get(*l).and_then(|r| r.get(*t)),
            ["company", t] => self.d.company.get(*t),
            _ => None,
        };
        let all: Vec<Value> = holder.and_then(Value::as_array).cloned().unwrap_or_default();
        let rows: Vec<&Value> = all
            .iter()
            .filter(|r| match hide_zero_rows {
                Some(p) => {
                    !self.lookup(p, Some((r, child))).v.is_null()
                        && !crate::value::parse_decimal(&self.lookup(p, Some((r, child))).v).is_ok_and(|d| d.is_zero())
                }
                None => true,
            })
            .collect();
        if rows.is_empty() && hide_empty.unwrap_or(true) && !design {
            return;
        }
        // Columns hidden by their condition (read once per document) are left out; dimmed on the canvas.
        let cols: Vec<(&ferrum_meta::layout::TableColumn, bool)> = columns.iter().map(|c| (c, self.when(&c.when))).filter(|(_, on)| *on || design).collect();
        let numeric = |c: &ferrum_meta::layout::TableColumn| {
            let f = c.format.as_ref().and_then(|f| f.as_);
            matches!(f, Some(As::Number | As::Amount | As::Percent))
                || (matches!(f, None | Some(As::Auto))
                    && c.value.list().first().and_then(|p| p.strip_prefix("row.")).and_then(|n| child.field(n)).is_some_and(|f| f.ftype.is_numeric()))
        };
        // On the canvas a column hidden on this record is a ghost: no width, a hairline in the header.
        let ghosts = self.d.mode == Mode::Design;
        let ghost = |on: bool| !on && ghosts;
        let given: f64 = cols.iter().filter(|(_, on)| !ghost(*on)).filter_map(|(c, _)| c.width_pct).sum();
        let free = cols.iter().filter(|(c, on)| !ghost(*on) && c.width_pct.is_none()).count();
        let rn = if *row_numbers { 5.0 } else { 0.0 };
        let share = if free > 0 { ((100.0 - rn - given).max(free as f64 * 3.0)) / free as f64 } else { 0.0 };
        let classes = format!(
            "pf-block pf-table pf-borders-{}{}{}{align}{hidden}",
            match borders.unwrap_or(Borders::Rows) {
                Borders::Rows => "rows",
                Borders::All => "all",
                Borders::None => "none",
            },
            if *striped { " pf-striped" } else { "" },
            size.map(|s| format!(" pf-s-{}", s.name())).unwrap_or_default()
        );
        let _ = write!(out, "<table class=\"{classes}\"{data}{}><colgroup>", Self::style(b.margin_top_mm, None));
        if *row_numbers {
            out.push_str("<col style=\"width:5%\">");
        }
        for (c, on) in &cols {
            let w = if ghost(*on) { 0.0 } else { c.width_pct.unwrap_or(share) };
            let _ = write!(out, "<col style=\"width:{}%\">", num(w));
        }
        out.push_str("</colgroup>");
        let calign = |c: &ferrum_meta::layout::TableColumn| c.align.map(|a| a.name()).unwrap_or(if numeric(c) { "end" } else { "start" });
        if header.unwrap_or(true) {
            out.push_str("<thead><tr>");
            if *row_numbers {
                out.push_str("<th class=\"pf-rn\">#</th>");
            }
            for (c, on) in &cols {
                if ghost(*on) {
                    let _ = write!(out, "<th class=\"pf-ghost-col\" data-b=\"{}\" data-ghost=\"column\" aria-hidden=\"true\"></th>", esc(&c.id));
                    continue;
                }
                let label = c.label.clone().unwrap_or_else(|| self.label_of(&c.value, Some(child)));
                let _ =
                    write!(out, "<th class=\"pf-a-{}{}\"{}>{}</th>", calign(c), if *on { "" } else { " pf-hidden" }, self.mark("data-b", &c.id), esc(&label));
            }
            out.push_str("</tr></thead>");
        }
        out.push_str("<tbody>");
        let mut sums: Vec<Decimal> = vec![Decimal::ZERO; cols.len()];
        let mut fields: Vec<Option<&FieldDef>> = vec![None; cols.len()];
        for (i, r) in rows.iter().enumerate() {
            out.push_str("<tr>");
            if *row_numbers {
                let _ = write!(out, "<td class=\"pf-rn\">{}</td>", esc(&crate::docfmt::number(Decimal::from(i + 1), 0, self.locale())));
            }
            for (j, (c, on)) in cols.iter().enumerate() {
                if ghost(*on) {
                    out.push_str("<td class=\"pf-ghost-col\"></td>");
                    continue;
                }
                let got = self.first(&c.value, Some((r, child))).map(|g| Got { v: g.v.clone(), field: g.field, title: g.title.clone(), name: g.name.clone() });
                if let Some(g) = &got {
                    fields[j] = fields[j].or(g.field);
                    if totals.contains(&c.id)
                        && let Ok(d) = crate::value::parse_decimal(&g.v)
                    {
                        sums[j] += d;
                    }
                }
                let zero = c.hide_zero && got.as_ref().is_some_and(|g| crate::value::parse_decimal(&g.v).is_ok_and(|d| d.is_zero()));
                let v = got.filter(|_| !zero).and_then(|g| self.show(&g, c.format.as_ref())).map(|x| x.0).unwrap_or_default();
                // A second line only when it says something the first does not (an item's name falling back to its
                // description prints it once).
                let second = c
                    .secondary
                    .as_ref()
                    .and_then(|s| self.first(s, Some((r, child))).map(|g| Got { v: g.v.clone(), field: g.field, title: g.title.clone(), name: g.name.clone() }))
                    .and_then(|g| self.show(&g, None))
                    .filter(|x| x.0 != v)
                    .map(|x| format!("<div class=\"pf-secondary\">{}</div>", x.0))
                    .unwrap_or_default();
                let _ = write!(out, "<td class=\"pf-a-{}{}\">{v}{second}</td>", calign(c), if *on { "" } else { " pf-hidden" });
            }
            out.push_str("</tr>");
        }
        if rows.is_empty() {
            let span = cols.len() + usize::from(*row_numbers);
            let t = empty_text.clone().unwrap_or_default();
            let t = if t.is_empty() && design { self.placeholder("No lines") } else { esc(&t) };
            let _ = write!(out, "<tr><td colspan=\"{span}\">{t}</td></tr>");
        }
        out.push_str("</tbody>");
        if !totals.is_empty() && !rows.is_empty() {
            out.push_str("<tfoot><tr class=\"pf-total-row\">");
            let mut label = Some(esc(totals_label.as_deref().unwrap_or("Total")));
            if *row_numbers {
                out.push_str("<td></td>");
            }
            for (j, (c, on)) in cols.iter().enumerate() {
                if ghost(*on) {
                    out.push_str("<td class=\"pf-ghost-col\"></td>");
                } else if totals.contains(&c.id) {
                    let g = Got { v: Value::String(sums[j].to_string()), field: fields[j], title: None, name: c.id.clone() };
                    let v = self.show(&g, c.format.as_ref()).map(|x| x.0).unwrap_or_default();
                    let _ = write!(out, "<td class=\"pf-a-{}\">{v}</td>", calign(c));
                } else {
                    let _ = write!(out, "<td class=\"pf-a-start\">{}</td>", label.take().unwrap_or_default());
                }
            }
            out.push_str("</tr></tfoot>");
        }
        out.push_str("</table>");
    }
}

/// A size of the scale in points.
pub fn pt(s: Size) -> &'static str {
    match s {
        Size::Xs => "8",
        Size::Sm => "9",
        Size::Md => "10.5",
        Size::Lg => "12",
        Size::Xl => "14",
        Size::Xxl => "18",
    }
}

fn is_page(path: &str) -> bool {
    path == "print.page" || path == "print.pages"
}

/// The entity of the rows a table block prints (`doc.<table>`, `links.<link>.<table>`, `company.<table>`), when its
/// path names a table field there.
pub fn table_rows_of<'a>(schema: &'a Schema, ent: &'a EffEntity, value: &str) -> Option<&'a EffEntity> {
    let parts: Vec<&str> = value.split('.').collect();
    let (parent, name): (&EffEntity, &str) = match parts.as_slice() {
        ["doc", t] => (ent, t),
        ["links", l, t] => (ent.field(l).filter(|f| f.ftype == FieldType::Link).and_then(|f| f.to.as_deref()).and_then(|e| schema.entity(e))?, t),
        ["company", t] => (schema.entity("Company")?, t),
        _ => return None,
    };
    parent.field(name).filter(|f| f.ftype == FieldType::Table).and_then(|f| f.of.as_deref()).and_then(|c| schema.entity(c)).map(|c| &**c)
}

/// A number written for CSS: at most two decimals, no exponent.
fn num(v: f64) -> String {
    let s = format!("{:.2}", v);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// A layout's sections rendered over `d`: the inside of one copy (`<div class="pf-doc">`'s content).
pub fn render(l: &Layout, d: &Data<'_>) -> Rendered {
    let mut r = Renderer::new(d);
    let mut html = String::new();
    r.sections(l, &mut html);
    Rendered { html, warnings: r.warnings }
}

/// How tall a letter head's part comes out on a PDF page, in millimetres, erring slightly tall: each section's tallest
/// column of the blocks that print (their resolved values; an empty field, an image not inlined, a block whose
/// condition fails take nothing), lines from the font sizes of `margin_page` (9 pt, line height 1.35), a value's text
/// wrapped by its column's width at an average glyph width, images and spacers by their declared heights.
pub fn estimate_mm(l: &Layout, d: &Data<'_>, width_mm: f64) -> f64 {
    const PT: f64 = 25.4 / 72.0;
    let mut r = Renderer::new(d);
    // Lines `text` takes at `size_pt` in `col_mm` (each line break a line of its own).
    let lines = |text: &str, size_pt: f64, bold: bool, col_mm: f64| -> f64 {
        let glyph = size_pt * PT * if bold { 0.58 } else { 0.53 };
        let per_line = (col_mm / glyph).max(1.0);
        text.split('\n').map(|t| (t.chars().count() as f64 / per_line).ceil().max(1.0)).sum()
    };
    let plain = |html: &str| -> String {
        let html = html.replace("<br>", "\n");
        let mut out = String::new();
        let mut tag = false;
        for c in html.chars() {
            match c {
                '<' => tag = true,
                '>' => tag = false,
                c if !tag => out.push(c),
                _ => {}
            }
        }
        out.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'")
    };
    let (base, lh) = (9.0, 1.35);
    let mut total = 0.0;
    for s in &l.sections {
        if !r.when(&s.when) {
            continue;
        }
        let widths: Vec<f64> = s.widths.clone().unwrap_or_else(|| vec![1; s.columns.len()]).into_iter().map(f64::from).collect();
        let gap = s.gap_mm.unwrap_or(6) as f64;
        let inner = width_mm - gap * (s.columns.len().saturating_sub(1)) as f64 - if s.border { 4.0 } else { 0.0 };
        let sum: f64 = widths.iter().sum::<f64>().max(1.0);
        let mut tallest: f64 = 0.0;
        for (c, w) in s.columns.iter().zip(widths.iter()) {
            let col = inner * w / sum;
            let mut h = 0.0;
            for (i, b) in c.blocks.iter().enumerate() {
                if !r.when(&b.when) {
                    continue;
                }
                let top = b.margin_top_mm.unwrap_or(0) as f64;
                let size = |sz: &Option<Size>| sz.map(|z| pt(z).parse::<f64>().unwrap_or(base)).unwrap_or(base);
                let taken = match &b.kind {
                    BlockKind::Field { value, label, label_position, weight, size: sz, format, hide_empty, .. } => {
                        let got = r.first(value, None);
                        let shown = got.and_then(|g| r.show(&g, format.as_ref())).map(|x| plain(&x.0));
                        let text = match shown {
                            Some(t) => t,
                            None if hide_empty.unwrap_or(true) => continue,
                            None => String::new(),
                        };
                        let label = match label {
                            Some(Some(l)) => l.clone(),
                            Some(None) => String::new(),
                            None => r.label_of(value, None),
                        };
                        let z = size(sz);
                        let bold = *weight == Some(Weight::Bold);
                        if *label_position == Some(LabelPosition::Above) && !label.is_empty() {
                            lines(&label, z * 0.85, false, col) * z * 0.85 * lh * PT + lines(&text, z, bold, col) * z * lh * PT + 0.6
                        } else {
                            let joined = if label.is_empty() { text } else { format!("{label}: {text}") };
                            lines(&joined, z, bold, col) * z * lh * PT + 0.6
                        }
                    }
                    BlockKind::Heading { text, level, size: sz, .. } => {
                        if r.no_page_numbers() && TOKEN.captures_iter(text).any(|c| is_page(&c[1])) {
                            continue;
                        }
                        let z = match (sz, level.unwrap_or(1)) {
                            (Some(_), _) => size(sz),
                            (None, 1) => 18.0,
                            (None, 2) => 13.0,
                            _ => 9.0,
                        };
                        let after = if level.unwrap_or(1) >= 3 { 1.0 } else { 0.0 };
                        let before = if i > 0 { 3.0 } else { 0.0 };
                        let t = plain(&r.tokens(text));
                        before + lines(&t, z, true, col) * z * 1.2 * PT + after
                    }
                    BlockKind::Text { content, size: sz, .. } => {
                        let z = size(sz);
                        let mut h = 0.0;
                        for p in content {
                            if r.no_page_numbers() && p.spans.iter().flat_map(|sp| sp.v.iter().flat_map(|v| v.list())).any(|v| is_page(v)) {
                                continue;
                            }
                            let mut t = String::new();
                            for sp in &p.spans {
                                match (&sp.t, &sp.v) {
                                    (Some(x), _) => t.push_str(x),
                                    (None, Some(v)) => {
                                        let got = r.first(v, None);
                                        // A page number is a few digits.
                                        if v.list().iter().any(|p| is_page(p)) {
                                            t.push_str("999");
                                        } else if let Some(x) = got.and_then(|g| r.show(&g, sp.format.as_ref())) {
                                            t.push_str(&plain(&x.0));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            h += lines(&t, z, false, col) * z * lh * PT + 1.5;
                        }
                        h
                    }
                    BlockKind::Image { src, height_mm, .. } => match r.image(&b.id, src) {
                        Some(_) => height_mm.unwrap_or(20.0),
                        None => continue,
                    },
                    BlockKind::Divider { thickness, .. } => 4.0 + thickness.unwrap_or(1) as f64 * 0.27,
                    BlockKind::Spacer { height_mm } => *height_mm,
                    _ => continue,
                };
                h += top + taken;
            }
            tallest = tallest.max(h);
        }
        let pad = s.padding_mm.map(f64::from).unwrap_or(if s.border { 2.0 } else { 0.0 }) * 2.0;
        let title = if s.title.is_some() { 1.1 * base * 1.2 * PT + 2.0 } else { 0.0 };
        total += s.margin_top_mm.unwrap_or(0) as f64 + pad + title + tallest;
    }
    if total <= 0.0 { 0.0 } else { (total * 1.06 + 2.0).ceil() }
}

/// A layout's own watermark, when its condition holds.
pub fn watermark(l: &Layout, d: &Data<'_>) -> Option<String> {
    let w = l.watermark.as_ref()?;
    let mut r = Renderer::new(d);
    r.when(&w.when).then(|| r.tokens(&w.text)).filter(|t| !t.trim().is_empty())
}

/// Paper and margins as the page's CSS, for a browser printing the HTML (the PDF service is told them as fields).
pub fn page_css(p: &PageSetup) -> String {
    let (w, h) = p.sheet_mm();
    let m = p.margin_mm;
    // On screen (a preview, the canvas) the page's margins are its padding, so it looks as it prints.
    format!(
        "@page{{size:{}mm {}mm;margin:{}mm {}mm {}mm {}mm}}@media screen{{body.pf{{padding:{}mm {}mm {}mm {}mm}}}}",
        num(w),
        num(h),
        m.top,
        m.right,
        m.bottom,
        m.left,
        m.top,
        m.right,
        m.bottom,
        m.left
    )
}

/// The parts of a whole page.
pub struct Page<'a> {
    pub language: &'a str,
    pub title: &'a str,
    pub font: &'a str,
    pub size: Size,
    pub color: &'a str,
    pub accent: &'a str,
    pub line_height: f64,
    pub page_css: String,
    pub design: bool,
    pub watermark: Option<String>,
    /// Each copy: its label and its inside (letter head included where it is placed in the body).
    pub copies: Vec<(Option<String>, String)>,
}

/// A whole HTML page (§5.2): the renderer's CSS and the page's, the watermark, one `pf-doc` per copy.
pub fn page(p: &Page<'_>) -> String {
    let dir = if rtl(p.language) { "rtl" } else { "ltr" };
    let mut out = format!(
        "<!doctype html>\n<html lang=\"{}\" dir=\"{dir}\"><head><meta charset=\"utf-8\"><title>{}</title><style>{BASE_CSS}{}</style></head>\
         <body class=\"pf pf-font-{} pf-size-{}{}\" style=\"--pf-color:{};--pf-accent:{};--pf-lh:{}\">",
        esc(p.language),
        esc(p.title),
        p.page_css,
        p.font,
        p.size.name(),
        if p.design { " pf-design" } else { "" },
        p.color,
        p.accent,
        num(p.line_height)
    );
    if let Some(w) = &p.watermark {
        let _ = write!(out, "<div class=\"pf-watermark\" aria-hidden=\"true\">{w}</div>");
    }
    for (i, (label, inside)) in p.copies.iter().enumerate() {
        if i > 0 {
            out.push_str("<div class=\"pf-break\"></div>");
        }
        let copy = label.as_deref().map(|l| format!(" data-copy=\"{}\"", esc(l))).unwrap_or_default();
        let _ = write!(out, "<div class=\"pf-doc\"{copy}>{inside}</div>");
    }
    out.push_str("</body></html>");
    out
}

/// What Chromium leaves between the paper's edge and a repeated header's (or footer's) box, measured on Gotenberg 8: a
/// header's first line starts about 5 mm below the edge with no padding of ours. Our padding is the page margin less it.
const CHROMIUM_EDGE_MM: u32 = 5;

/// A letter head's part as the PDF service repeats it: a page of its own in the page's side margins, our CSS, the
/// space above (a header) or below (a footer) the page's own margin. One box holds it all, with its classes: Chromium
/// keeps a header's body's children, not the body itself (its classes, its size), and lays them out in a row.
pub fn margin_page(inside: &str, language: &str, font: &str, side_mm: (u32, u32), pad_mm: (u32, u32)) -> String {
    let dir = if rtl(language) { "rtl" } else { "ltr" };
    format!(
        "<!doctype html><html lang=\"{}\" dir=\"{dir}\"><head><meta charset=\"utf-8\"><style>{BASE_CSS}\
         html,body{{margin:0;padding:0;width:100%}}.pf-margin{{width:100%;padding:{}mm {}mm {}mm {}mm;color:#111;line-height:1.35}}</style></head>\
         <body><div class=\"pf pf-margin pf-font-{font} pf-size-sm\" dir=\"{dir}\">{inside}</div></body></html>",
        esc(language),
        pad_mm.0.saturating_sub(CHROMIUM_EDGE_MM),
        side_mm.1,
        pad_mm.1.saturating_sub(CHROMIUM_EDGE_MM),
        side_mm.0
    )
}

// ── checking against the entity ──

fn field_ok(e: Option<&EffEntity>, name: &str, system: bool) -> Result<(), String> {
    let Some(e) = e else { return Err("is not there".into()) };
    if system && SYSTEM.iter().any(|s| s.0 == name) {
        return Ok(());
    }
    match e.field(name) {
        Some(f) if f.ftype == FieldType::Secret => Err(format!("{} is a secret field, never printed", f.name)),
        Some(_) => Ok(()),
        None => Err(format!("{} has no field {name}", e.def.name)),
    }
}

/// Whether `path` is one `ent`'s layout may read at `at`.
pub fn check_path(schema: &Schema, ent: Option<&EffEntity>, path: &str, at: PathAt, row: Option<&EffEntity>) -> Result<(), String> {
    let parts: Vec<&str> = path.split('.').collect();
    let entity = |n: &str| schema.entity(n).map(|e| &**e);
    match (parts.as_slice(), at) {
        (["doc", f], PathAt::Body | PathAt::Row) => field_ok(ent, f, true),
        (["row", f], PathAt::Row) => field_ok(row, f, false),
        (["row", ..], _) => Err("row.* is read in a table's columns only".into()),
        (["links", l, f], PathAt::Body | PathAt::Row) => {
            let link = ent.and_then(|e| e.field(l)).filter(|x| x.ftype == FieldType::Link).ok_or_else(|| format!("{l} is not a link field here"))?;
            field_ok(link.to.as_deref().and_then(entity), f, true)
        }
        (["company", f], _) => field_ok(entity("Company"), f, false),
        (["settings", e, f], _) => {
            let s = entity(e).filter(|s| s.def.kind == EntityKind::Settings).ok_or_else(|| format!("{e} is not a settings entity"))?;
            field_ok(Some(s), f, false)
        }
        (["print", "page" | "pages"], PathAt::LetterHead) => Ok(()),
        (["print", "page" | "pages"], _) => Err("print.page and print.pages are a letter head's".into()),
        (["print", f], _) if PRINT.iter().any(|p| p.0 == *f) => Ok(()),
        (["doc" | "links", ..], PathAt::LetterHead) => Err("a letter head reads company.*, settings.* and print.*".into()),
        _ => Err("is not a path this layout may read".into()),
    }
}

/// Every problem of `l` for `ent` (`None`: a letter head): its shape and limits, then each path against the fields.
/// Each as `(id, message)`.
pub fn check(schema: &Schema, ent: Option<&EffEntity>, l: &Layout) -> Vec<(String, String)> {
    let letterhead = ent.is_none();
    let mut out = l.problems(letterhead);
    let at = if letterhead { PathAt::LetterHead } else { PathAt::Body };
    let n = l.named();
    for (id, p, where_) in &n.paths {
        if *where_ == PathAt::Row {
            continue;
        }
        if let Err(e) = check_path(schema, ent, p, at, None) {
            out.push((id.to_string(), format!("{p}: {e}")));
        }
    }
    for b in l.blocks() {
        if let BlockKind::Table { value, columns, hide_zero_rows, .. } = &b.kind {
            let Some(child) = ent.and_then(|e| table_rows_of(schema, e, value)) else {
                out.push((b.id.clone(), format!("{value} is not a table of the record, of a record it links to or of the company")));
                continue;
            };
            for (id, p) in columns
                .iter()
                .flat_map(|c| c.value.list().iter().chain(c.secondary.iter().flat_map(|s| s.list())).map(move |p| (c.id.as_str(), p)))
                .chain(hide_zero_rows.iter().map(|h| (b.id.as_str(), h)))
            {
                let r = if p.starts_with("doc.") {
                    Err("a table's column reads row.*, not doc.*".into())
                } else {
                    check_path(schema, ent, p, PathAt::Row, Some(child))
                };
                if let Err(e) = r {
                    out.push((id.to_string(), format!("{p}: {e}")));
                }
            }
        }
    }
    out
}

/// The link fields and settings entities a layout reads, for its loader.
pub fn reads(l: &Layout) -> (BTreeSet<String>, BTreeSet<String>) {
    let text = serde_json::to_string(l).unwrap_or_default();
    let names = |prefix: &str| -> BTreeSet<String> {
        text.match_indices(prefix)
            .map(|(i, _)| text[i + prefix.len()..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect::<String>())
            .filter(|n| !n.is_empty())
            .collect()
    };
    (names("links."), names("settings."))
}

// ── the designer's catalogue ──

fn describe(f: &FieldDef, path: String) -> Value {
    let mut v = json!({"path": path, "label": f.label(), "type": f.ftype});
    if let Some(to) = &f.to {
        v["to"] = json!(to);
    }
    if let Some(p) = f.precision {
        v["precision"] = json!(p);
    }
    if f.ftype == FieldType::Select && !f.options.is_empty() {
        v["options"] = json!(f.options);
    }
    v
}

/// A field a format may print: any but a secret (a field the form hides, an e-invoice's signed QR, prints all the same).
fn printable(f: &FieldDef) -> bool {
    f.ftype != FieldType::Secret
}

/// What a designer may place for `ent` (§6.2 `GET /api/admin/documents/fields/{Entity}`): no secret fields.
pub fn catalogue(schema: &Schema, ent: &EffEntity) -> Value {
    let values = |e: &EffEntity, root: &str| -> Vec<Value> {
        e.def.fields.iter().filter(|f| printable(f) && f.ftype != FieldType::Table).map(|f| describe(f, format!("{root}.{}", f.name))).collect()
    };
    // The tables of `e` a table block may print, at `root.<table>`, each with its columns as `row.*`.
    let tables_of = |e: &EffEntity, root: &str| -> Vec<Value> {
        e.def
            .fields
            .iter()
            .filter(|f| printable(f) && f.ftype == FieldType::Table)
            .filter_map(|f| {
                let c = schema.entity(f.of.as_deref()?)?;
                Some(json!({"path": format!("{root}.{}", f.name), "label": f.label(), "of": c.def.name, "columns": values(c, "row")}))
            })
            .collect()
    };
    // A record's own fields, then the system's it does not have a field of its own for (`status` once).
    let system_of = |e: &EffEntity, root: &str| -> Vec<Value> {
        SYSTEM.iter().filter(|(n, ..)| e.field(n).is_none()).map(|(n, l, t)| json!({"path": format!("{root}.{n}"), "label": l, "type": t})).collect()
    };
    let system = system_of(ent, "doc");
    let links: Vec<Value> = ent
        .def
        .fields
        .iter()
        .filter(|f| printable(f) && f.ftype == FieldType::Link)
        .filter_map(|f| {
            let t = schema.entity(f.to.as_deref()?)?;
            let root = format!("links.{}", f.name);
            let mut fields = values(t, &root);
            fields.extend(system_of(t, &root));
            Some(json!({"path": root, "field": f.name, "to": t.def.name, "label": f.label(), "fields": fields, "tables": tables_of(t, &root)}))
        })
        .collect();
    let computed: Vec<Value> = ent
        .def
        .fields
        .iter()
        .filter(|f| printable(f) && f.ftype == FieldType::Currency)
        .map(|f| json!({"path": format!("doc.{}", f.name), "format": "words", "label": format!("{} in words", f.label())}))
        .collect();
    let links_company = ent.def.fields.iter().any(|f| f.ftype == FieldType::Link && f.to.as_deref() == Some("Company")) || ent.def.name == "Company";
    let (company, company_tables) = match schema.entity("Company") {
        Some(c) if links_company => (values(c, "company"), tables_of(c, "company")),
        _ => (vec![], vec![]),
    };
    let print: Vec<Value> = PRINT.iter().take(8).map(|(n, l)| json!({"path": format!("print.{n}"), "label": l})).collect();
    let settings: Vec<Value> = schema
        .entities
        .values()
        .filter(|e| e.def.kind == EntityKind::Settings && !e.def.company_scoped)
        .flat_map(|e| values(e, &format!("settings.{}", e.def.name)))
        .collect();
    json!({
        "entity": {"name": ent.def.name, "label": ent.def.label, "submittable": ent.def.submittable},
        "fields": values(ent, "doc"), "system": system, "tables": tables_of(ent, "doc"), "links": links, "company": company,
        "company_tables": company_tables, "computed": computed, "print": print, "settings": settings,
        // What a value's format and a table's column may say, for the designer's pickers.
        "options": {
            "format": {"as": ["auto", "text", "multiline", "number", "amount", "percent", "date", "datetime", "words", "check"],
                       "show": ["symbol", "code", "none"], "date": ["short", "medium", "long"], "sign": ["abs", {"abs_when": "<condition>"}]},
            "table_column": {"hide_zero": "a cell whose value is zero is left blank",
                             "secondary": "printed under the value, only when it differs from it"},
            "letter_head_pages": ["print.page", "print.pages"],
        },
    })
}

/// The kinds a stored image may be, by its first bytes (its stored content type is not trusted): png, jpeg, gif,
/// webp, svg.
pub fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_ascii_lowercase();
        let t = head.trim_start_matches('\u{feff}').trim_start();
        (t.starts_with("<svg") || (t.starts_with("<?xml") && head.contains("<svg"))).then_some("image/svg+xml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_and_numbers() {
        assert_eq!(esc("<script>\"'&"), "&lt;script&gt;&quot;&#39;&amp;");
        assert_eq!((num(20.0), num(12.5), num(0.333)), ("20".into(), "12.5".into(), "0.33".into()));
        assert!(rtl("ar") && rtl("fa-IR") && !rtl("en"));
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(image_type(b"<svg xmlns='x'/>"), Some("image/svg+xml"));
        assert_eq!(image_type(b"<html><script>"), None);
    }
}
