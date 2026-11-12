//! Documents from data (docs/builder/decisions.md BD-21): templates a workspace edits that render a record, with its
//! lines and what it links to, into a page (HTML, a PDF where the server makes PDFs), text, CSV or XML.
//!
//! * **Templates.** A package ships defaults (`documents:` in its YAML); a workspace edits a copy, sees the default,
//!   resets it, and is told when a release changed the default it edited, as message templates are (BD-20). Several
//!   per entity; one may be its default; `roles` say who may pick one.
//! * **Reading.** A person printing gets the record as they may read it (the print path's rule). A document the
//!   system generates for someone outside ([`generate`], the `generate` effect) is rendered as the workspace holds the
//!   record, its secret fields left out (BD-20's rule for messages).
//! * **Files.** Text, CSV and XML are written in a stated encoding with stated line ends, each escaping what it prints
//!   for its own syntax; downloading one needs export permission and is on the record's audit trail.
//! * **Keeping.** [`generate`] stores the file on the record in the caller's transaction, replaces the one the
//!   template made before when asked, and gives its id and SHA-256 digest to later effects (a `message` attaches it, an
//!   `invoke` hands it to the Sign app).
//! * **Queries.** A template may be about a saved report's rows instead of one record ([`DocumentQuery`]): one file
//!   for the whole result, or one per record a column of the rows links to, kept on that record (a statement per
//!   customer). The rows come through [`QueryRows`], which the server wires to the analytics engine: read as the person
//!   who asks, or as the workspace when an effect or a schedule runs it (as generated documents are, above). At most
//!   [`MAX_QUERY_ROWS`] rows and [`MAX_GROUPS`] files a run: a person asking for more is refused with the numbers; an
//!   effect's run goes on in a job of its own after the last record it did, never dropping the rest.
//! * **Print formats** (BD-28). A page is a template or a layout ([`crate::layout`]); it has paper and margins, a
//!   letter head (the record's company's, else the workspace's), copies with labels, a watermark on drafts and
//!   cancelled records; the workspace picks each entity's default (`document_default`); images are the workspace's own
//!   files, inlined as `data:` URLs. One path renders every page ([`render_page`]), for a person printing and for a
//!   document generated for outside alike.

use crate::ctx::Ctx;
use crate::db::Tx;
use crate::error::{Error, FieldError, Result};
use crate::layout::{self, Mode, Part};
use crate::pdf::PdfPage;
use crate::{Action, Kernel, audit};
use ferrum_meta::layout::{Font, ImageSrc, Layout, PageSetup, PaperSize, Size};
use ferrum_meta::{DocumentBody, DocumentDef, DocumentOutput, DocumentQuery, FieldType};
use minijinja::value::Value as JValue;
use minijinja::{AutoEscape, Error as JError, ErrorKind, UndefinedBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use uuid::Uuid;

/// The largest body (or header, or footer) a template keeps.
pub const MAX_BODY: usize = 256 * 1024;
/// Records one call renders together (labels), copies of each, and labels in all: more is refused, never cut; the
/// caller asks for the next records in another call.
pub const MAX_RECORDS: usize = 200;
pub const MAX_COPIES: u32 = 100;
pub const MAX_LABELS: usize = 1000;
/// Saved versions kept per workspace template.
pub const MAX_VERSIONS: i64 = 50;
/// An image inlined into a print, and all of them in one render.
pub const MAX_IMAGE: usize = 1 << 20;
pub const MAX_IMAGES_BYTES: usize = 4 << 20;
/// Rows a document about a query reads, and files one run of a grouped one makes: more is refused when a person
/// asks, and done in the next job when an effect asked.
pub const MAX_QUERY_ROWS: usize = 10_000;
pub const MAX_GROUPS: usize = 100;
/// The job that renders a grouped query's files for an effect, [`MAX_GROUPS`] at a time.
pub const GROUPS_KIND: &str = "platform.document.groups";

/// Where a document about a query gets its rows: the analytics engine's report path, wired by the server.
#[async_trait::async_trait]
pub trait QueryRows: Send + Sync {
    /// The rows of the saved report labelled `report`, with `params` (`filters`, `from`, `to`, `limit`), read as `ctx`
    /// may read them: `{columns: [{name, type, to?}], rows: [{..}], meta: {entity, truncated}}`.
    async fn rows(&self, k: &Kernel, ctx: &Ctx, report: &str, params: &Value) -> Result<Value>;
    /// What a template about `report` is checked against, read in the caller's transaction (a pack's install, which
    /// may have just made the report) as `ctx` may run it: `{columns, meta: {entity}}`, however many rows it has.
    async fn shape(&self, k: &Kernel, tx: &mut Tx, ctx: &Ctx, report: &str) -> Result<Value>;
}

/// What separates two records' pages when a page template renders several (each starts a page).
const PAGE_BREAK: &str = "\n<div style=\"break-after:page\"></div>\n";

fn admin_only(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("document templates are for administrators".into())) }
}

fn digest(def: &DocumentDef) -> String {
    hex::encode(Sha256::digest(serde_json::to_vec(def).unwrap_or_default()))
}

/// A template's entity tag: its digest, quoted.
pub fn etag(def: &DocumentDef) -> String {
    format!("\"{}\"", digest(def))
}

// ── rendering ──

/// The environment bodies render in: the print formats' engine and helpers, each output escaping what it prints for
/// its own syntax (a page as HTML, CSV as a quoted field where needed, XML as text); text as written, a field's
/// fixed width and padding the template's (`pad`). Nothing prints as `none`.
fn environment(output: DocumentOutput, strict: bool) -> minijinja::Environment<'static> {
    let mut env = crate::print::environment();
    if strict {
        env.set_undefined_behavior(UndefinedBehavior::Strict);
    }
    env.add_function("display", crate::print::display);
    if output == DocumentOutput::Page {
        return env;
    }
    // A file's last line ends as its template's does.
    env.set_keep_trailing_newline(true);
    env.set_auto_escape_callback(move |_| match output {
        DocumentOutput::Csv => AutoEscape::Custom("csv"),
        DocumentOutput::Xml => AutoEscape::Custom("xml"),
        _ => AutoEscape::None,
    });
    env.set_formatter(move |out, state, v| {
        if v.is_undefined() && state.undefined_behavior() == UndefinedBehavior::Strict {
            return Err(JError::new(ErrorKind::UndefinedError, "a value that is not there is printed"));
        }
        if v.is_none() || v.is_undefined() {
            return Ok(());
        }
        let text = if v.is_safe() {
            v.to_string()
        } else {
            match output {
                DocumentOutput::Csv => crate::docfmt::csv(&serde_json::to_value(v).unwrap_or(Value::Null)),
                DocumentOutput::Xml => crate::docfmt::xml(&v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())),
                _ => v.to_string(),
            }
        };
        std::fmt::Write::write_str(out, &text).map_err(|e| JError::new(ErrorKind::WriteFailure, e.to_string()))
    });
    env
}

/// Extras a page template reads besides its variables: `image(value)` (a workspace file inlined), `letterhead`.
#[derive(Default, Clone)]
struct Extras {
    images: Arc<HashMap<String, String>>,
    letterhead: Option<(String, String)>,
}

fn render_raw(output: DocumentOutput, src: &str, vars: &Value, strict: bool, extras: &Extras) -> std::result::Result<String, JError> {
    let mut env = environment(output, strict);
    let images = extras.images.clone();
    // Our own `data:` URL (base64 of a checked image), safe as it is in an attribute.
    env.add_function("image", move |v: JValue| -> JValue { JValue::from_safe_string(v.as_str().and_then(|s| images.get(s).cloned()).unwrap_or_default()) });
    let ctx = match &extras.letterhead {
        Some((h, f)) => {
            let lh: BTreeMap<&str, JValue> = [("header", JValue::from_safe_string(h.clone())), ("footer", JValue::from_safe_string(f.clone()))].into();
            minijinja::context! { letterhead => lh, ..JValue::from_serialize(vars) }
        }
        None => JValue::from_serialize(vars),
    };
    env.render_str(src, ctx)
}

/// What a template's error says, with where it is: `line N: …`.
fn where_(e: &JError) -> String {
    let what = e.detail().map(str::to_string).unwrap_or_else(|| e.kind().to_string());
    format!("line {}: {what}", e.line().unwrap_or(1))
}

#[cfg(test)]
fn render(output: DocumentOutput, src: &str, vars: &Value, strict: bool) -> Result<String> {
    render_x(output, src, vars, strict, &Extras::default())
}

#[cfg(test)]
fn render_x(output: DocumentOutput, src: &str, vars: &Value, strict: bool, extras: &Extras) -> Result<String> {
    if src.len() > MAX_BODY {
        return Err(Error::field("template", "a body is larger than 256 KB"));
    }
    render_raw(output, src, vars, strict, extras).map_err(|e| Error::field("template", e.to_string()))
}

/// A body, its header and its footer, rendered.
struct Pages {
    body: String,
    header: Option<String>,
    footer: Option<String>,
}

fn render_all(def: &DocumentDef, b: &DocumentBody, vars: &Value, strict: bool, lang: &str) -> Result<Pages> {
    render_all_x(def, b, vars, strict, &Extras::default(), lang)
}

/// A body, its header and its footer rendered; what goes wrong said at its part and line, as a save says it
/// (`en body, line 5: …`).
fn render_all_x(def: &DocumentDef, b: &DocumentBody, vars: &Value, strict: bool, x: &Extras, lang: &str) -> Result<Pages> {
    let one = |part: &str, src: &str| -> Result<String> {
        if src.len() > MAX_BODY {
            return Err(Error::field("template", format!("{lang} {part}, line 1: larger than 256 KB")));
        }
        render_raw(def.output, src, vars, strict, x).map_err(|e| Error::field("template", format!("{lang} {part}, {}", where_(&e))))
    };
    Ok(Pages {
        body: one("body", &b.body)?,
        header: b.header.as_deref().map(|s| one("header", s)).transpose()?,
        footer: b.footer.as_deref().map(|s| one("footer", s)).transpose()?,
    })
}

/// The names after `prefix` in a template's text (`settings.Company` → `Company`).
fn named(text: &str, prefix: &str) -> BTreeSet<String> {
    text.match_indices(prefix)
        .map(|(i, _)| text[i + prefix.len()..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect::<String>())
        .filter(|n| !n.is_empty())
        .collect()
}

fn all_text(def: &DocumentDef) -> String {
    let mut parts: Vec<String> = def.languages.values().flat_map(|b| [Some(&b.body), b.header.as_ref(), b.footer.as_ref()]).flatten().cloned().collect();
    if let Some(l) = &def.layout {
        parts.push(serde_json::to_string(l).unwrap_or_default());
    }
    parts.join("\n")
}

/// The bodies for the first language of `wanted` there is one for (or its base, `pt` for `pt-BR`), else English, else
/// the first; and the tag numbers and words are written in: the one asked for when it is of the language chosen.
fn bodies<'a>(def: &'a DocumentDef, wanted: &[Option<&str>]) -> (String, &'a DocumentBody) {
    let base = |l: &str| l.to_ascii_lowercase().split(['-', '_']).next().unwrap_or_default().to_string();
    let find = |l: &str| {
        let l = l.to_ascii_lowercase();
        def.languages.iter().find(|(k, _)| k.to_ascii_lowercase() == l).or_else(|| def.languages.iter().find(|(k, _)| k.to_ascii_lowercase() == base(&l)))
    };
    let asked = wanted.iter().flatten().find(|l| find(l).is_some()).copied();
    let (k, b) = asked.and_then(find).or_else(|| find("en")).or_else(|| def.languages.iter().next()).expect("a template has a language");
    let tag = asked.filter(|a| base(a) == base(k)).unwrap_or(k);
    (tag.to_string(), b)
}

/// Who a document is rendered for: a person printing it (what they may read), or someone outside the workspace
/// (the record as the workspace holds it, its secret fields left out).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reader {
    Person,
    Outside,
}

/// The variables a body reads for record `id`: `doc` (with `_titles`), `entity`, `links` (the link fields it reads
/// as `links.<field>`), `settings`, `values`, `language`, `currency` (the record's), `currencies` (the workspace's
/// currency records, when it prints amounts), `title` (the record's) and `now`.
#[allow(clippy::too_many_arguments)]
async fn record_vars(
    k: &Kernel,
    tx: &mut Tx,
    ctx: &Ctx,
    reader: Reader,
    def: &DocumentDef,
    id: Uuid,
    language: &str,
    values: &Map<String, Value>,
) -> Result<Value> {
    let ent = k.entity(ctx, &def.entity)?;
    let sys = Ctx::system(ctx.tenant, ctx.schema.clone());
    let view = if reader == Reader::Person { ctx } else { &sys };
    let mut doc = k.get_tx(view, tx, &def.entity, id, false).await?;
    k.add_titles(view, tx, &ent, std::slice::from_mut(&mut doc)).await?;
    if reader == Reader::Outside {
        crate::messages::without_secrets(&ctx.schema, &ent, &mut doc);
    }
    let text = all_text(def);
    let read = named(&text, "links.");
    let mut links = Map::new();
    for f in ent.def.fields.iter().filter(|f| f.ftype == FieldType::Link && read.contains(&f.name)) {
        let (Some(target), Some(linked)) = (f.to.as_deref(), doc.get(&f.name).and_then(Value::as_str).and_then(|v| v.parse::<Uuid>().ok())) else {
            continue;
        };
        // One they may not read is not there, as on a print.
        let Ok(mut record) = k.get_tx(view, tx, target, linked, false).await else { continue };
        let tent = k.entity(ctx, target)?;
        k.add_titles(view, tx, &tent, std::slice::from_mut(&mut record)).await?;
        if reader == Reader::Outside {
            crate::messages::without_secrets(&ctx.schema, &tent, &mut record);
        }
        links.insert(f.name.clone(), Value::Object(record));
    }
    let settings = crate::settings::load(k, ctx, tx, &named(&text, "settings.")).await?;
    let currency = doc
        .get("_titles")
        .and_then(|t| t.get("currency"))
        .or_else(|| doc.get("currency"))
        .and_then(Value::as_str)
        .filter(|c| c.len() == 3 && c.chars().all(|c| c.is_ascii_uppercase()))
        .map(str::to_string);
    let currencies = currencies(k, tx, ctx, if def.layout.is_some() { "amount" } else { &text }).await?;
    let title = crate::collab::record_title(ctx, &def.entity, &doc);
    Ok(json!({
        "doc": doc, "entity": crate::print::field_meta(&ent, &ctx.schema), "links": links, "settings": settings, "values": values,
        "language": language, "currency": currency, "currencies": currencies, "title": title,
        "now": chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string(),
    }))
}

/// The workspace's currency records by code (`{code, symbol, precision}`), when a template prints amounts.
async fn currencies(k: &Kernel, tx: &mut Tx, ctx: &Ctx, text: &str) -> Result<Value> {
    if ctx.schema.entity("Currency").is_none() || !["amount", "words", "currencies"].iter().any(|w| text.contains(w)) {
        return Ok(json!({}));
    }
    let sys = Ctx::system(ctx.tenant, ctx.schema.clone());
    let rows = k.find_tx(&sys, tx, "Currency", &[], None, 500).await?;
    Ok(Value::Object(
        rows.into_iter()
            .filter_map(|r| {
                let code = r.get("code")?.as_str()?.to_string();
                Some((code, json!({"code": r.get("code"), "symbol": r.get("symbol"), "precision": r.get("precision")})))
            })
            .collect(),
    ))
}

/// A record of `entity` with every field there (one line in each table), each link's title, and a record of
/// every entity it links to: for checking a template (`filled` false: empty values, an unknown field refused), or for
/// a designer's preview (`filled`: a dash, 0, today).
fn sample(ctx: &Ctx, entity: &str, filled: bool) -> Value {
    fn fields(ent: &ferrum_meta::EffEntity, ctx: &Ctx, lines: bool, filled: bool) -> Map<String, Value> {
        let text = if filled { json!("—") } else { json!("") };
        let mut doc: Map<String, Value> = ["id", "number", "created_at", "updated_at", "created_by", "version", "docstatus", "status"]
            .into_iter()
            .map(|f| (f.to_string(), text.clone()))
            .collect();
        if filled {
            doc.insert("docstatus".into(), json!(1));
        }
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let mut titles = Map::new();
        for f in ent.def.fields.iter().filter(|f| f.ftype != FieldType::Secret) {
            let v = match f.ftype {
                FieldType::Table if lines => {
                    json!([f.of.as_deref().and_then(|c| ctx.schema.entity(c)).map(|c| Value::Object(fields(c, ctx, false, filled))).unwrap_or(json!({}))])
                }
                FieldType::Table => json!([]),
                FieldType::Check => json!(false),
                FieldType::Date | FieldType::Datetime if filled => json!(today),
                FieldType::Attach | FieldType::Json if filled => Value::Null,
                t if t.is_numeric() => json!(0),
                _ => text.clone(),
            };
            if f.ftype == FieldType::Link {
                titles.insert(f.name.clone(), text.clone());
            }
            doc.insert(f.name.clone(), v);
        }
        doc.insert("_titles".into(), Value::Object(titles));
        doc
    }
    let Some(ent) = ctx.schema.entity(entity) else { return json!({"doc": {}, "links": {}}) };
    let links: Map<String, Value> = ent
        .def
        .fields
        .iter()
        .filter(|f| f.ftype == FieldType::Link)
        .filter_map(|f| Some((f.name.clone(), Value::Object(fields(ctx.schema.entity(f.to.as_deref()?)?, ctx, false, filled)))))
        .collect();
    json!({"doc": fields(ent, ctx, true, filled), "links": links, "entity": crate::print::field_meta(ent, &ctx.schema)})
}

/// A sample record that reads as a document (a designer's canvas, a preview with no record): every field filled from
/// its type and name, the same each time: three lines whose amounts add up, a tax line, totals, today's dates, the
/// company's currency.
fn rich_sample(ctx: &Ctx, entity: &str, currency: &str) -> Value {
    use rust_decimal::Decimal;
    let Some(ent) = ctx.schema.entity(entity) else { return json!({"doc": {}, "links": {}}) };
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let has = |n: &str, words: &[&str]| words.iter().any(|w| n.contains(w));
    let money = |d: Decimal| json!(d.round_dp(2).to_string());
    // Three lines of goods and a service, and what they add up to.
    let (qty, rate) = ([2, 5, 1], [1500, 250, 4000]);
    let subtotal: Decimal = qty.iter().zip(rate).map(|(q, r)| Decimal::from(q * r)).sum();
    let tax = subtotal * Decimal::new(18, 2);
    let names = ["Steel bracket", "Assembly service", "Packing crate"];
    fn text(f: &ferrum_meta::FieldDef, line: Option<usize>, names: &[&str; 3]) -> Value {
        let n = f.name.as_str();
        let has = |w: &[&str]| w.iter().any(|x| n.contains(x));
        if has(&["email"]) {
            return json!("accounts@example.com");
        }
        if has(&["phone", "mobile"]) {
            return json!("555 0100");
        }
        if has(&["address", "display", "billing", "shipping"]) {
            return json!("12 Sample Street\nSample City 100001");
        }
        if let Some(i) = line.filter(|_| has(&["item", "description", "name"])) {
            return json!(names[i % 3]);
        }
        if has(&["name"]) {
            return json!(format!("Sample {}", f.label().trim_end_matches(" Name")));
        }
        if has(&["uom", "unit"]) {
            return json!("Nos");
        }
        json!(format!("Sample {}", f.label()))
    }
    // A line (`line`: its index) or a record (a document, what it links to).
    let row = |c: &ferrum_meta::EffEntity, i: usize, taxes: bool, line: bool| -> Map<String, Value> {
        let mut r = Map::new();
        let mut titles = Map::new();
        let (q, rt) = (Decimal::from(qty[i]), Decimal::from(rate[i]));
        for f in c.def.fields.iter().filter(|f| f.ftype != FieldType::Secret && f.ftype != FieldType::Table) {
            let n = f.name.as_str();
            let v = match f.ftype {
                FieldType::Percent if taxes || has(n, &["tax", "gst", "vat"]) => json!("18"),
                FieldType::Percent => json!("0"),
                FieldType::Currency | FieldType::Decimal | FieldType::Int if taxes => {
                    if has(n, &["rate"]) {
                        json!("18")
                    } else {
                        money(tax)
                    }
                }
                FieldType::Int | FieldType::Decimal if has(n, &["qty", "quantity"]) => json!(q.to_string()),
                FieldType::Currency | FieldType::Decimal if has(n, &["rate", "price"]) => money(rt),
                FieldType::Currency => money(q * rt),
                t if t.is_numeric() => json!("0"),
                FieldType::Date | FieldType::Datetime => json!(today),
                FieldType::Check => json!(false),
                FieldType::Select => f.options.first().map(|o| json!(o)).unwrap_or(Value::Null),
                FieldType::Link => {
                    let label = f.to.as_deref().and_then(|t| ctx.schema.entity(t)).map(|t| t.def.label.clone()).unwrap_or_default();
                    titles.insert(f.name.clone(), if f.to.as_deref() == Some("Currency") { json!(currency) } else { json!(format!("Sample {label}")) });
                    json!("")
                }
                FieldType::Attach | FieldType::Json => Value::Null,
                _ if taxes && has(n, &["description", "account", "name"]) => json!("Tax 18%"),
                _ => text(f, line.then_some(i), &names),
            };
            r.insert(f.name.clone(), v);
        }
        r.insert("_titles".into(), Value::Object(titles));
        r
    };
    let mut doc = row(ent, 0, false, false);
    for (n, v) in [("id", json!("")), ("number", json!("SAMPLE-0001")), ("created_at", json!(today)), ("updated_at", json!(today)), ("docstatus", json!(1))] {
        doc.insert(n.into(), v);
    }
    doc.insert("status".into(), json!(if ent.is_submittable() { "Submitted" } else { "" }));
    for f in ent.def.fields.iter().filter(|f| f.ftype == FieldType::Currency) {
        let n = f.name.as_str();
        let v = if has(n, &["tax"]) {
            tax
        } else if has(n, &["grand", "rounded", "outstanding", "balance", "payable", "due", "receivable"]) {
            subtotal + tax
        } else if has(n, &["discount", "round", "adjust", "paid", "advance", "change", "write"]) {
            Decimal::ZERO
        } else {
            subtotal
        };
        doc.insert(f.name.clone(), money(v));
    }
    for f in ent.def.fields.iter().filter(|f| f.ftype == FieldType::Table) {
        let Some(c) = f.of.as_deref().and_then(|c| ctx.schema.entity(c)) else { continue };
        let taxes = has(&f.name, &["tax"]);
        let rows: Vec<Value> = (0..if taxes { 1 } else { 3 }).map(|i| Value::Object(row(c, i, taxes, true))).collect();
        doc.insert(f.name.clone(), Value::Array(rows));
    }
    let links: Map<String, Value> = ent
        .def
        .fields
        .iter()
        .filter(|f| f.ftype == FieldType::Link)
        .filter_map(|f| Some((f.name.clone(), Value::Object(row(ctx.schema.entity(f.to.as_deref()?)?, 0, false, false)))))
        .collect();
    json!({"doc": doc, "links": links, "entity": crate::print::field_meta(ent, &ctx.schema)})
}

fn layout_errors(problems: Vec<(String, String)>) -> Error {
    Error::Validation(problems.into_iter().map(|(id, m)| FieldError::new(&if id.is_empty() { "layout".into() } else { format!("layout.{id}") }, m)).collect())
}

/// What is wrong with a template, said as a field error: its shape, its entity, its key (one a package owns must be
/// that package's document), and each body rendered against a record with every field, refusing an unknown one
/// (`<language> <body|header|footer>, line <n>: …`); a layout checked against the entity's fields (`layout.<id>`).
async fn check_template(k: &Kernel, tx: &mut Tx, ctx: &Ctx, def: &DocumentDef) -> Result<()> {
    if let Some(p) = def.problem() {
        return Err(Error::field("template", p));
    }
    let Some(ent) = ctx.schema.entity(&def.entity) else {
        return Err(Error::field("entity", format!("{} is not an entity here", def.entity)));
    };
    let owner = ctx.schema.packages.iter().find(|p| def.key.strip_prefix(p.as_str()).is_some_and(|r| r.starts_with('.')));
    if let Some(p) = owner
        && !ctx.schema.documents.iter().any(|d| d.key == def.key)
    {
        return Err(Error::field("key", format!("{p} has no document {}; a workspace's own template has a key no package's name starts", def.key)));
    }
    if def.layout_file.is_some() {
        return Err(Error::field("layout_file", "a layout file is a package's; a workspace's layout is in the template itself"));
    }
    if let Some(l) = &def.layout {
        if serde_json::to_vec(l).map(|v| v.len()).unwrap_or(0) > ferrum_meta::layout::MAX_LAYOUT_BYTES {
            return Err(Error::field("layout", "a layout is at most 256 KB"));
        }
        let problems = layout::check(&ctx.schema, Some(ent), l);
        return if problems.is_empty() { Ok(()) } else { Err(layout_errors(problems)) };
    }
    let mut vars = match &def.query {
        Some(q) => query_sample(k, tx, ctx, def, q).await?,
        None => sample(ctx, &def.entity, false),
    };
    vars["settings"] = crate::settings::load(k, ctx, tx, &named(&all_text(def), "settings.")).await?;
    vars["values"] = Value::Object(def.values.iter().map(|v| (v.clone(), json!(""))).collect());
    for (name, v) in [
        ("language", json!("en")),
        ("currency", json!("USD")),
        ("currencies", json!({})),
        ("now", json!("")),
        ("title", json!("")),
        ("checking", json!(true)),
        ("print", json!({"copy": "", "language": "en", "title": "", "entity": "", "now": "", "time": "", "letterhead": false, "letterhead_name": ""})),
        ("company", ctx.schema.entity("Company").map(|c| Value::Object(sample_of(c, ctx))).unwrap_or(json!({}))),
    ] {
        vars[name] = v;
    }
    let x = Extras { letterhead: Some((String::new(), String::new())), ..Default::default() };
    let rec = if def.query.is_none() { ctx.schema.entity(&def.entity) } else { None };
    let has = |f: &str| match f == "_titles" || ["id", "number", "created_at", "updated_at", "created_by", "version", "docstatus", "status"].contains(&f) {
        true => Some(false),
        false => rec.and_then(|e| e.def.fields.iter().find(|d| d.name == f)).map(|d| d.ftype == FieldType::Secret),
    };
    for (lang, b) in &def.languages {
        vars["language"] = json!(lang);
        for (part, src) in [("body", Some(&b.body)), ("header", b.header.as_ref()), ("footer", b.footer.as_ref())] {
            let Some(src) = src else { continue };
            if src.len() > MAX_BODY {
                return Err(Error::field("template", format!("{lang} {part}, line 1: larger than 256 KB")));
            }
            render_raw(def.output, src, &vars, true, &x).map_err(|e| {
                // In plain words where the body names what is not there (BD-21), at the line the engine named.
                let said = crate::messages::undefined_said(
                    e.to_string(),
                    src,
                    &def.values,
                    rec.map(|r| (r.def.label.as_str(), &has as &dyn Fn(&str) -> Option<bool>)),
                );
                let plain = said.rsplit_once(" (line ").map(|(m, _)| m.to_string()).filter(|_| said != e.to_string());
                match plain {
                    Some(m) => Error::field("template", format!("{lang} {part}, line {}: {m}", e.line().unwrap_or(1))),
                    None => Error::field("template", format!("{lang} {part}, {}", where_(&e))),
                }
            })?;
        }
    }
    Ok(())
}

/// What a document about a query is checked against: the report's columns as one row, and a record of the entity its
/// grouping column links to as `group`. The report is run once, as whoever saves it, to learn them.
async fn query_sample(k: &Kernel, tx: &mut Tx, ctx: &Ctx, def: &DocumentDef, q: &DocumentQuery) -> Result<Value> {
    let source = k.hooks.query_rows().ok_or_else(|| Error::invalid("documents about a query need the analytics engine, which this server does not have"))?;
    // In the caller's transaction, and however many rows there are: that is no matter of the template's.
    let out = source.shape(k, tx, ctx, &q.report).await?;
    let columns = reads(def, q, &out)?;
    let row: Map<String, Value> = columns
        .iter()
        .filter_map(|c| {
            let v = match serde_json::from_value::<FieldType>(c["type"].clone()) {
                Ok(t) if t.is_numeric() => json!(0),
                Ok(FieldType::Check) => json!(false),
                _ => json!(""),
            };
            Some((c["name"].as_str()?.to_string(), v))
        })
        .collect();
    let group = match group_target(ctx, &columns, q) {
        Ok(Some(target)) => sample(ctx, &target, false)["doc"].clone(),
        Ok(None) => Value::Null,
        Err(e) => return Err(Error::field("query", e)),
    };
    Ok(json!({"rows": [row], "columns": columns, "group": group}))
}

/// The entity `q.group_by` links to, among `columns` (a report's): none when it does not group.
fn group_target(ctx: &Ctx, columns: &[Value], q: &DocumentQuery) -> std::result::Result<Option<String>, String> {
    let Some(g) = &q.group_by else { return Ok(None) };
    let col = columns.iter().find(|c| c["name"] == json!(g)).ok_or_else(|| format!("report {} has no column {g}", q.report))?;
    match (col["type"].as_str(), col["to"].as_str()) {
        (Some("link"), Some(to)) if ctx.schema.entity(to).is_some() => Ok(Some(to.to_string())),
        _ => Err(format!("{g} of report {} links to no record: a document is grouped by a column that does", q.report)),
    }
}

/// The report's columns and rows for `def`, read as `ctx` may: refused when the report reads another entity than the
/// template says, and when it has more than [`MAX_QUERY_ROWS`] rows, with the numbers.
async fn query_rows(k: &Kernel, ctx: &Ctx, def: &DocumentDef, q: &DocumentQuery, params: &Value) -> Result<(Vec<Value>, Vec<Map<String, Value>>)> {
    let source = k.hooks.query_rows().ok_or_else(|| Error::invalid("documents about a query need the analytics engine, which this server does not have"))?;
    let mut params = if params.is_object() { params.clone() } else { json!({}) };
    params.as_object_mut().map(|m| m.entry("limit").or_insert(json!(MAX_QUERY_ROWS)));
    let out = source.rows(k, ctx, &q.report, &params).await?;
    let columns = reads(def, q, &out)?;
    let rows: Vec<Map<String, Value>> = out["rows"].as_array().into_iter().flatten().filter_map(|r| r.as_object().cloned()).collect();
    if out["meta"]["truncated"] == json!(true) || rows.len() > MAX_QUERY_ROWS {
        return Err(Error::invalid(format!(
            "report {} gives more than {MAX_QUERY_ROWS} rows, the most one document reads; narrow it with its filters or dates",
            q.report
        )));
    }
    Ok((columns, rows))
}

/// A report's columns, refused when it reads another entity than the template says.
fn reads(def: &DocumentDef, q: &DocumentQuery, out: &Value) -> Result<Vec<Value>> {
    if out["meta"]["entity"].as_str() != Some(def.entity.as_str()) {
        return Err(Error::field("entity", format!("report {} reads {}, not {}", q.report, out["meta"]["entity"].as_str().unwrap_or("nothing"), def.entity)));
    }
    Ok(out["columns"].as_array().cloned().unwrap_or_default())
}

fn sample_of(ent: &ferrum_meta::EffEntity, _ctx: &Ctx) -> Map<String, Value> {
    ent.def.fields.iter().filter(|f| f.ftype != FieldType::Secret).map(|f| (f.name.clone(), if f.ftype.is_numeric() { json!(0) } else { json!("") })).collect()
}

// ── templates ──

type All = BTreeMap<String, (DocumentDef, &'static str, bool)>;

/// Every template the workspace has: the packages' defaults with its own and edited copies over them, each with
/// where it is from (`package`, `workspace`, `edited`) and, for an edited one, whether a release changed the default
/// since.
async fn every(tx: &mut Tx, ctx: &Ctx) -> Result<All> {
    let own: Vec<(String, Value, Option<String>)> =
        sqlx::query_as("select key, def, base from document_template where tenant_id = $1").bind(ctx.tenant).fetch_all(&mut **tx).await?;
    let mut out: All = ctx.schema.documents.iter().map(|d| (d.key.clone(), (d.clone(), "package", false))).collect();
    for (key, def, base) in own {
        let Ok(d) = serde_json::from_value::<DocumentDef>(def) else { continue };
        let default = ctx.schema.documents.iter().find(|m| m.key == key);
        let changed = default.is_some_and(|m| base.as_deref() != Some(digest(m).as_str()));
        out.insert(key, (d, if default.is_some() { "edited" } else { "workspace" }, changed));
    }
    Ok(out)
}

/// The template `key` as the workspace has it.
async fn effective(tx: &mut Tx, ctx: &Ctx, key: &str) -> Result<DocumentDef> {
    every(tx, ctx).await?.remove(key).map(|t| t.0).ok_or_else(|| Error::NotFound(format!("document template {key}")))
}

/// Whether `ctx` may pick `def`: anyone who may print its record when it names no roles, else those roles'
/// holders and administrators.
fn may_use(ctx: &Ctx, def: &DocumentDef) -> bool {
    def.roles.is_empty() || ctx.is_admin || ctx.roles.iter().any(|r| def.roles.contains(r))
}

/// The entity's default for `ctx` (D9): the workspace's choice (`document_default`), a workspace template flagged
/// `default`, a package's `default` (an add-on's over the package it depends on, then by key); each only when it is
/// enabled and `ctx` may use it. None: the print format.
async fn default_for<'a>(tx: &mut Tx, ctx: &Ctx, all: &'a All, entity: &str) -> Result<Option<&'a DocumentDef>> {
    let chosen: Option<String> = sqlx::query_scalar("select key from document_default where tenant_id = $1 and entity = $2")
        .bind(ctx.tenant)
        .bind(entity)
        .fetch_optional(&mut **tx)
        .await?;
    let ok = |d: &DocumentDef| d.entity == entity && d.enabled && d.query.is_none() && may_use(ctx, d);
    if let Some((d, ..)) = chosen.as_deref().and_then(|k| all.get(k)).filter(|(d, ..)| ok(d)) {
        return Ok(Some(d));
    }
    let flagged = || all.values().filter(|(d, ..)| d.default && ok(d));
    if let Some((d, ..)) = flagged().find(|(_, source, _)| *source == "workspace") {
        return Ok(Some(d));
    }
    // Among packages' (and their edited copies'), an add-on's beats what it extends: the default of a package that
    // depends, directly or not, on another's package wins; otherwise the first by key.
    Ok(shipped_default(&ctx.schema, &flagged().map(|t| &t.0).collect::<Vec<_>>()))
}

/// [`Kernel::save_document_template`] in the caller's transaction: `created`, `changed` or `unchanged`.
pub(crate) async fn save_template(k: &Kernel, tx: &mut Tx, ctx: &Ctx, def: &DocumentDef) -> Result<&'static str> {
    check_template(k, tx, ctx, def).await?;
    // A package's layout is not edited in place: a workspace duplicates it (its paper, roles and the like may change).
    if let Some(pkg) = ctx.schema.documents.iter().find(|m| m.key == def.key)
        && (pkg.layout.is_some() || def.layout.is_some())
        && pkg.layout != def.layout
    {
        return Err(Error::field("key", "duplicate this format to change it: a package's layout is not edited in place"));
    }
    let was: Option<Value> = sqlx::query_scalar("select def from document_template where tenant_id = $1 and key = $2")
        .bind(ctx.tenant)
        .bind(&def.key)
        .fetch_optional(&mut **tx)
        .await?;
    let now = serde_json::to_value(def).map_err(Error::internal)?;
    if was.as_ref() == Some(&now) {
        return Ok("unchanged");
    }
    let by = (!ctx.user.is_nil()).then_some(ctx.user);
    let base = ctx.schema.documents.iter().find(|m| m.key == def.key).map(digest);
    sqlx::query(
        "insert into document_template (tenant_id, key, def, base, updated_by, created_by) values ($1, $2, $3, $4, $5, $5) \
         on conflict (tenant_id, key) do update set def = excluded.def, base = excluded.base, updated_by = excluded.updated_by, updated_at = now()",
    )
    .bind(ctx.tenant)
    .bind(&def.key)
    .bind(&now)
    .bind(&base)
    .bind(by)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "insert into document_template_version (tenant_id, key, n, def, saved_by) \
         select $1, $2, coalesce(max(n), 0) + 1, $3, $4 from document_template_version where tenant_id = $1 and key = $2",
    )
    .bind(ctx.tenant)
    .bind(&def.key)
    .bind(&now)
    .bind(by)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "delete from document_template_version where tenant_id = $1 and key = $2 and n <= (select max(n) from document_template_version where tenant_id = $1 and key = $2) - $3",
    )
    .bind(ctx.tenant)
    .bind(&def.key)
    .bind(MAX_VERSIONS)
    .execute(&mut **tx)
    .await?;
    audit::record(tx, ctx, "DocumentTemplate", Uuid::nil(), "save", None, json!({"key": def.key})).await?;
    // Marking a template the default makes it the workspace's choice; a template read back with its flag as it was
    // (a package's default, saved again) changes nothing: `PUT …/defaults` is the way to choose.
    let was_default = match &was {
        Some(v) => v.get("default").and_then(Value::as_bool).unwrap_or(false),
        None => ctx.schema.documents.iter().find(|m| m.key == def.key).is_some_and(|m| m.default),
    };
    if def.default && !was_default && def.output == DocumentOutput::Page {
        choose_default(tx, ctx, &def.entity, Some(&def.key)).await?;
    }
    Ok(if was.is_some() { "changed" } else { "created" })
}

/// Among packages' default documents for one entity (in key order), the one that prints: a package's whose package no
/// other's depends on, directly or not (an add-on's over what it extends), the first by key among those.
fn shipped_default<'a>(schema: &ferrum_meta::Schema, shipped: &[&'a DocumentDef]) -> Option<&'a DocumentDef> {
    let owner = |key: &str| schema.packages.iter().find(|p| key.strip_prefix(p.as_str()).is_some_and(|r| r.starts_with('.'))).cloned().unwrap_or_default();
    let beaten = |d: &DocumentDef| shipped.iter().any(|o| owner(&o.key) != owner(&d.key) && schema.package_depends_on(&owner(&o.key), &owner(&d.key)));
    shipped.iter().find(|d| !beaten(d)).or(shipped.first()).copied()
}

/// Sets (or with `None` removes) the workspace's default for `entity`. Audited.
async fn choose_default(tx: &mut Tx, ctx: &Ctx, entity: &str, key: Option<&str>) -> Result<()> {
    match key {
        Some(key) => {
            sqlx::query(
                "insert into document_default (tenant_id, entity, key, updated_by) values ($1, $2, $3, $4) \
                 on conflict (tenant_id, entity) do update set key = excluded.key, updated_by = excluded.updated_by, updated_at = now()",
            )
            .bind(ctx.tenant)
            .bind(entity)
            .bind(key)
            .bind((!ctx.user.is_nil()).then_some(ctx.user))
            .execute(&mut **tx)
            .await?;
        }
        None => {
            sqlx::query("delete from document_default where tenant_id = $1 and entity = $2").bind(ctx.tenant).bind(entity).execute(&mut **tx).await?;
        }
    }
    audit::record(tx, ctx, "DocumentTemplate", Uuid::nil(), "default", None, json!({"entity": entity, "key": key})).await
}

/// The workspace's own and edited templates, for a pack.
pub(crate) async fn for_pack(tx: &mut Tx, tenant: Uuid) -> Result<Vec<DocumentDef>> {
    let defs: Vec<Value> = sqlx::query_scalar("select def from document_template where tenant_id = $1 order by key").bind(tenant).fetch_all(&mut **tx).await?;
    Ok(defs.into_iter().filter_map(|d| serde_json::from_value(d).ok()).collect())
}

/// The workspace's chosen defaults (entity → key), for a pack.
pub(crate) async fn defaults_for_pack(tx: &mut Tx, tenant: Uuid) -> Result<BTreeMap<String, String>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("select entity, key from document_default where tenant_id = $1 order by entity").bind(tenant).fetch_all(&mut **tx).await?;
    Ok(rows.into_iter().collect())
}

/// A pack's default for `entity`: `created`, `changed` or `unchanged`.
pub(crate) async fn install_default(tx: &mut Tx, ctx: &Ctx, entity: &str, key: &str) -> Result<&'static str> {
    let was: Option<String> = sqlx::query_scalar("select key from document_default where tenant_id = $1 and entity = $2")
        .bind(ctx.tenant)
        .bind(entity)
        .fetch_optional(&mut **tx)
        .await?;
    if was.as_deref() == Some(key) {
        return Ok("unchanged");
    }
    let all = every(tx, ctx).await?;
    if !all.get(key).is_some_and(|(d, ..)| d.entity == entity && d.output == DocumentOutput::Page) {
        return Err(Error::invalid(format!("{key} is not a page format of {entity} here")));
    }
    choose_default(tx, ctx, entity, Some(key)).await?;
    Ok(if was.is_some() { "changed" } else { "created" })
}

// ── output ──

/// A rendered document: its bytes, content type, file name and language.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub bytes: Vec<u8>,
    pub content_type: String,
    pub file_name: String,
    pub output: DocumentOutput,
    pub language: String,
}

/// A header or footer as the PDF service takes one: a page of its own, small type, in the page's side margins.
fn margin_page(html: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><style>html,body{{margin:0;font:9px system-ui,sans-serif;color:#444;width:100%}}\
         body{{padding:0 0.4in;-webkit-print-color-adjust:exact}}</style></head><body>{html}</body></html>"
    )
}

/// The bytes of a text, CSV or XML document in the template's encoding and line ends.
fn produce_file(def: &DocumentDef, pages: Pages) -> Result<(Vec<u8>, String, &'static str)> {
    let bytes = crate::docfmt::encode(&pages.body, def.encoding.as_deref(), def.crlf).map_err(|e| Error::field("template", e))?;
    let (ext, ctype) = def.output.file_type();
    Ok((bytes, format!("{ctype}; charset={}", def.encoding.as_deref().unwrap_or("utf-8")), ext))
}

/// A file name: the one given, else the template's label (or key) and the record's title, kept to letters, digits,
/// spaces and `-_.`, with the output's extension.
fn file_name(def: &DocumentDef, title: &str, given: Option<&str>, ext: &str) -> String {
    let fallback = || {
        let label = def.label.clone().unwrap_or_else(|| def.key.rsplit('.').next().unwrap_or_default().replace('_', " "));
        format!("{label} {title}")
    };
    let raw = given.map(str::trim).filter(|g| !g.is_empty()).map(str::to_string).unwrap_or_else(fallback);
    let mut name: String =
        raw.chars().map(|c| if c.is_alphanumeric() || " -_.".contains(c) { c } else { '_' }).take(120).collect::<String>().trim().to_string();
    if name.is_empty() {
        name = "document".into();
    }
    if name.to_lowercase().ends_with(&format!(".{ext}")) { name } else { format!("{name}.{ext}") }
}

/// The language a person reads in: theirs, else the workspace's.
async fn languages(tx: &mut Tx, ctx: &Ctx, person: bool) -> Result<(Option<String>, Option<String>)> {
    let theirs: Option<String> = if person && !ctx.user.is_nil() {
        sqlx::query_scalar("select value->>'language' from ui_pref where tenant_id = $1 and user_id = $2 and scope = 'locale'")
            .bind(ctx.tenant)
            .bind(ctx.user)
            .fetch_optional(&mut **tx)
            .await?
            .flatten()
    } else {
        None
    };
    let (_, workspace) = crate::messages::workspace(tx, ctx.tenant).await?;
    Ok((theirs, workspace))
}

/// Renders a text, CSV or XML `def` for record `id` as `reader` sees it.
#[allow(clippy::too_many_arguments)]
async fn render_record(
    k: &Kernel,
    tx: &mut Tx,
    ctx: &Ctx,
    reader: Reader,
    def: &DocumentDef,
    id: Uuid,
    language: Option<&str>,
    values: &Map<String, Value>,
) -> Result<(Pages, Value, String)> {
    let (theirs, workspace) = languages(tx, ctx, reader == Reader::Person).await?;
    let (tag, body) = bodies(def, &[language, theirs.as_deref(), workspace.as_deref()]);
    let vars = record_vars(k, tx, ctx, reader, def, id, &tag, values).await?;
    Ok((render_all(def, body, &vars, false, &tag)?, vars, tag))
}

/// Refused unless `ctx` may have this template's output for `entity`: reading it for a page (as the print path asks),
/// exporting it for a file of data; and the template's roles.
fn may_render(ctx: &Ctx, def: &DocumentDef) -> Result<()> {
    if !may_use(ctx, def) {
        return Err(Error::Forbidden(format!("document {} is for {} only", def.label.as_deref().unwrap_or(&def.key), def.roles.join(", "))));
    }
    let action = if def.output == DocumentOutput::Page { Action::Read } else { Action::Export };
    ctx.require(&def.entity, action).map(drop)
}

// ── pages: print settings, letter heads, images, one renderer for both kinds ──

/// The workspace's Print Settings as a page reads them (their defaults while the record is not saved).
#[derive(Debug, Clone)]
struct PrintSet {
    page: PageSetup,
    font: Font,
    letter_head: Option<Uuid>,
    repeat_header: bool,
    repeat_footer: bool,
    allow_draft: bool,
    allow_cancelled: bool,
    draft: String,
    cancelled: String,
    copies: Vec<String>,
}

async fn print_settings(k: &Kernel, tx: &mut Tx, ctx: &Ctx) -> Result<PrintSet> {
    let all = crate::settings::load(k, ctx, tx, &BTreeSet::from(["PrintSettings".to_string()])).await?;
    let s = all.get("PrintSettings").cloned().unwrap_or(json!({}));
    let text = |f: &str, d: &str| match s.get(f) {
        Some(Value::String(v)) => v.clone(),
        Some(Value::Null) | None if s.as_object().is_none_or(|m| !m.contains_key(f)) => d.to_string(),
        _ => String::new(),
    };
    let int = |f: &str, d: u32| s.get(f).and_then(Value::as_f64).map(|v| v.clamp(0.0, 50.0) as u32).unwrap_or(d);
    let check = |f: &str| s.get(f).and_then(Value::as_bool).unwrap_or(true);
    let page = PageSetup {
        size: PaperSize::parse(&text("paper", "A4")).filter(|p| *p != PaperSize::Custom).unwrap_or(PaperSize::A4),
        width_mm: None,
        height_mm: None,
        orientation: if text("orientation", "portrait") == "landscape" { ferrum_meta::layout::Orientation::Landscape } else { Default::default() },
        margin_mm: ferrum_meta::layout::Margins {
            top: int("margin_top_mm", 15),
            right: int("margin_right_mm", 12),
            bottom: int("margin_bottom_mm", 15),
            left: int("margin_left_mm", 12),
        },
    };
    Ok(PrintSet {
        page,
        font: Font::parse(&text("font", "sans")).unwrap_or(Font::Sans),
        letter_head: s.get("letter_head").and_then(Value::as_str).and_then(|v| v.parse().ok()),
        repeat_header: check("repeat_header"),
        repeat_footer: check("repeat_footer"),
        allow_draft: check("allow_print_draft"),
        allow_cancelled: check("allow_print_cancelled"),
        draft: text("draft_watermark", "Draft"),
        cancelled: text("cancelled_watermark", "Cancelled"),
        copies: text("copies", "").lines().map(str::trim).filter(|l| !l.is_empty()).take(5).map(|l| l.chars().take(60).collect()).collect(),
    })
}

/// A letter head as a page wears it.
#[derive(Debug, Clone)]
struct LetterHead {
    id: Uuid,
    name: String,
    company: Option<Uuid>,
    header: Option<Layout>,
    footer: Option<Layout>,
    /// The room its header and footer take on a PDF page; none: as much as their blocks need, worked out as it prints.
    header_mm: Option<f64>,
    footer_mm: Option<f64>,
}

/// Letter head `id`, read as the workspace; a disabled one, or one whose parts no longer pass the check, is none.
async fn letter_head(k: &Kernel, tx: &mut Tx, ctx: &Ctx, id: Uuid) -> Result<Option<LetterHead>> {
    if ctx.schema.entity("LetterHead").is_none() {
        return Ok(None);
    }
    let sys = Ctx::system(ctx.tenant, ctx.schema.clone());
    let Ok(r) = k.get_tx(&sys, tx, "LetterHead", id, false).await else { return Ok(None) };
    if r.get("disabled").and_then(Value::as_bool) == Some(true) {
        return Ok(None);
    }
    let part = |f: &str| {
        let l = Layout::from_letterhead(r.get(f).unwrap_or(&Value::Null)).ok().flatten()?;
        layout::check(&ctx.schema, None, &l).is_empty().then_some(l)
    };
    let mm = |f: &str, most: f64| r.get(f).and_then(Value::as_f64).map(|v| v.clamp(0.0, most));
    Ok(Some(LetterHead {
        id,
        name: r.get("letter_head_name").and_then(Value::as_str).unwrap_or_default().to_string(),
        company: r.get("company").and_then(Value::as_str).and_then(|v| v.parse().ok()),
        header: part("header"),
        footer: part("footer"),
        header_mm: mm("header_height_mm", 120.0),
        footer_mm: mm("footer_height_mm", 80.0),
    }))
}

/// A letter head as its form holds it before it is saved, checked as a save checks it.
fn draft_letter_head(ctx: &Ctx, d: &Value) -> Result<LetterHead> {
    let doc = d.as_object().cloned().ok_or_else(|| Error::field("letterhead", "a letter head is {header, footer, …}"))?;
    let problems = letter_head_problems(&ctx.schema, &doc);
    if !problems.is_empty() {
        return Err(Error::Validation(problems));
    }
    let part = |f: &str| Layout::from_letterhead(doc.get(f).unwrap_or(&Value::Null)).ok().flatten();
    let uuid = |f: &str| doc.get(f).and_then(Value::as_str).and_then(|v| v.parse::<Uuid>().ok());
    let mm = |f: &str, most: f64| doc.get(f).and_then(Value::as_f64).map(|v| v.clamp(0.0, most));
    Ok(LetterHead {
        id: uuid("id").unwrap_or_default(),
        name: doc.get("letter_head_name").and_then(Value::as_str).unwrap_or_default().to_string(),
        company: uuid("company"),
        header: part("header"),
        footer: part("footer"),
        header_mm: mm("header_height_mm", 120.0),
        footer_mm: mm("footer_height_mm", 80.0),
    })
}

/// What a LetterHead record holds wrong, as field errors on `header` and `footer` (`<block id>: …`).
pub(crate) fn letter_head_problems(schema: &ferrum_meta::Schema, doc: &crate::Doc) -> Vec<FieldError> {
    let mut out = Vec::new();
    for f in ["header", "footer"] {
        match Layout::from_letterhead(doc.get(f).unwrap_or(&Value::Null)) {
            Ok(None) => {}
            Ok(Some(l)) => {
                if serde_json::to_vec(&l).map(|v| v.len()).unwrap_or(0) > ferrum_meta::layout::MAX_LAYOUT_BYTES {
                    out.push(FieldError::new(f, "a letter head's part is at most 256 KB"));
                }
                for (id, m) in layout::check(schema, None, &l) {
                    out.push(FieldError::new(f, if id.is_empty() { m } else { format!("{id}: {m}") }));
                }
            }
            Err(e) => out.push(FieldError::new(f, e)),
        }
    }
    out
}

/// Whose file an image is read as: the person printing (a record's attachment), or the workspace (a format's own
/// image, the company's logo, a letter head's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadAs {
    Person,
    Workspace,
}

/// An image a page wants inlined.
struct Want {
    url: String,
    by: ReadAs,
    /// Only a file attached to this record (a letter head's images).
    on: Option<(&'static str, Uuid)>,
}

/// The images `wants` names, read and inlined as `data:` URLs: only this workspace's stored files, each read as its
/// reader may (a record's attachment as the person printing, a format's or the company's as the workspace), png,
/// jpeg, gif, webp or svg by their bytes, at most 1 MB each, 4 MB and 20 in all. What is not inlined is said.
async fn inline(k: &Kernel, tx: &mut Tx, ctx: &Ctx, wants: Vec<Want>) -> Result<(HashMap<String, String>, Vec<String>)> {
    let (mut out, mut warn, mut total) = (HashMap::new(), Vec::new(), 0usize);
    let mut seen = BTreeSet::new();
    for w in wants {
        if !seen.insert(w.url.clone()) {
            continue;
        }
        let Some(id) = w.url.strip_prefix("/files/").and_then(|s| s.parse::<Uuid>().ok()).filter(|_| ferrum_meta::layout::is_uuid(&w.url[7..])) else {
            warn.push(format!("{} is not one of this workspace's files", w.url.chars().take(80).collect::<String>()));
            continue;
        };
        if out.len() >= ferrum_meta::layout::MAX_IMAGES {
            warn.push(format!("{}: at most {} images print", w.url, ferrum_meta::layout::MAX_IMAGES));
            continue;
        }
        let row: Option<(Option<String>, Option<Uuid>, Option<Uuid>, i64, String, bool)> =
            sqlx::query_as("select entity, record_id, created_by, size_bytes, storage_key, shared from file_object where tenant_id = $1 and id = $2")
                .bind(ctx.tenant)
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?;
        let Some((entity, record, by, size, key, shared)) = row else {
            warn.push(format!("{}: no such file here", w.url));
            continue;
        };
        let allowed = match (w.on, &entity, record) {
            (Some((e, r)), Some(fe), Some(fr)) => fe == e && fr == r,
            (Some(_), ..) => false,
            // As the person would download it: the same rule, an outside person's included (shared, their own, or
            // named in a field of its record as they read it: BD-22).
            (None, ..) if w.by == ReadAs::Person => k.may_download_file(ctx, id, entity.as_deref(), record, by, shared).await.is_ok(),
            (None, ..) => true,
        };
        if !allowed {
            warn.push(format!("{}: not a file this print may show", w.url));
            continue;
        }
        if size as usize > MAX_IMAGE || total + size as usize > MAX_IMAGES_BYTES {
            warn.push(format!("{}: images print up to 1 MB each and 4 MB in all", w.url));
            continue;
        }
        let bytes = match k.file_store()?.get(&key).await {
            Ok(b) => b,
            Err(_) => {
                warn.push(format!("{}: the file could not be read", w.url));
                continue;
            }
        };
        let Some(kind) = layout::image_type(&bytes) else {
            warn.push(format!("{}: not a png, jpeg, gif, webp or svg image", w.url));
            continue;
        };
        total += bytes.len();
        out.insert(w.url, format!("data:{kind};base64,{}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes)));
    }
    Ok((out, warn))
}

/// The value at a path of the loaded data (`doc.x`, `links.l.x`, `company.x`, `settings.E.x`), as text.
fn text_at(vars: &Value, company: &Value, path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('.').collect();
    let v = match parts.as_slice() {
        ["doc", f] => vars["doc"].get(f),
        ["links", l, f] => vars["links"].get(l).and_then(|r| r.get(f)),
        ["company", f] => company.get(f),
        ["settings", e, f] => vars["settings"].get(e).and_then(|r| r.get(f)),
        _ => None,
    };
    v.and_then(Value::as_str).map(str::to_string)
}

/// The images a layout shows, with who reads each.
fn layout_wants(l: &Layout, vars: &Value, company: &Value, on: Option<(&'static str, Uuid)>, out: &mut Vec<Want>) {
    for b in l.blocks() {
        let src = match &b.kind {
            ferrum_meta::layout::BlockKind::Image { src, .. } => Some(src),
            ferrum_meta::layout::BlockKind::Signature { image, .. } => image.as_ref(),
            _ => None,
        };
        match src {
            Some(ImageSrc::File { file }) => out.push(Want { url: file.clone(), by: ReadAs::Workspace, on }),
            Some(ImageSrc::Value { value }) => {
                if let Some(url) = text_at(vars, company, value) {
                    let by = if value.starts_with("doc.") || value.starts_with("links.") { ReadAs::Person } else { ReadAs::Workspace };
                    out.push(Want { url, by, on: None });
                }
            }
            None => {}
        }
    }
}

/// What a page is asked for.
#[derive(Debug, Clone, Default)]
pub struct PrintAsk {
    /// A template's key; None: the entity's default (D9); `standard`: the entity's print format.
    pub template: Option<String>,
    /// `none` or a LetterHead's id; None: the format's, else the record's company's, else the workspace's.
    pub letterhead: Option<String>,
    pub language: Option<String>,
    /// One copy per label (at most 5); none: one unlabelled copy.
    pub copies: Vec<String>,
    pub pdf: bool,
    /// A letter head not saved yet (`{id?, company?, header, footer, header_height_mm?, footer_height_mm?}`), worn in
    /// place of any other: a designer's preview of their edits.
    pub letterhead_draft: Option<Value>,
}

/// How a page renders (beyond [`PrintAsk`]).
#[derive(Debug, Clone, Copy)]
struct How {
    reader: Reader,
    design: bool,
    /// The canvas with every part at full size (what a condition hides dimmed), not collapsed.
    design_all: bool,
}

/// A page rendered, before it becomes bytes.
struct Printed {
    html: String,
    /// The letter head's parts the PDF service repeats (and a template's own header and footer).
    header: Option<String>,
    footer: Option<String>,
    pdf: PdfPage,
    title: String,
    language: String,
    warnings: Vec<String>,
    /// Each copy's inside, to put several records' pages together (labels).
    parts: Vec<String>,
}

/// The legacy print format of `entity` as a template (`standard`): its `print_template`, else the one that prints
/// any record.
fn standard(ctx: &Ctx, entity: &str) -> Result<DocumentDef> {
    let ent = ctx.schema.entity(entity).ok_or_else(|| Error::NotFound(format!("entity {entity}")))?;
    let body = ent.print_template.clone().unwrap_or_else(|| crate::print::DEFAULT_TEMPLATE.to_string());
    Ok(DocumentDef {
        key: "standard".into(),
        label: Some(ent.def.label.clone()),
        entity: entity.into(),
        languages: BTreeMap::from([("en".into(), DocumentBody { body, ..Default::default() })]),
        ..Default::default()
    })
}

/// Puts a letter head and a watermark into a template's page: after `<body…>` and before `</body>` (or around it).
fn wear(html: &str, head: Option<&str>, foot: Option<&str>, watermark: Option<&str>) -> String {
    if head.is_none() && foot.is_none() && watermark.is_none() {
        return html.to_string();
    }
    let mut top = format!("<style>{}</style>", layout::BASE_CSS);
    if let Some(w) = watermark {
        top.push_str(&format!("<div class=\"pf-watermark\" aria-hidden=\"true\">{w}</div>"));
    }
    if let Some(h) = head {
        top.push_str(&format!("<header class=\"pf-letterhead\">{h}</header>"));
    }
    let bottom = foot.map(|f| format!("<footer class=\"pf-letterhead-foot\">{f}</footer>")).unwrap_or_default();
    let lower = html.to_ascii_lowercase();
    let open = lower.find("<body").and_then(|i| lower[i..].find('>').map(|j| i + j + 1));
    let close = lower.rfind("</body>");
    match (open, close) {
        (Some(o), Some(c)) if c >= o => format!("{}{top}{}{bottom}{}", &html[..o], &html[o..c], &html[c..]),
        (Some(o), _) => format!("{}{top}{}{bottom}", &html[..o], &html[o..]),
        _ => format!("{top}{html}{bottom}"),
    }
}

/// Renders page format `def` for record `id` (None: the sample record) as `how` says, with the letter head, copies,
/// language and paper `ask` names: the one path every page takes (a person printing, a preview, a designer's canvas,
/// an e-mail's attachment, a document generated for outside).
async fn render_page(k: &Kernel, tx: &mut Tx, ctx: &Ctx, def: &DocumentDef, id: Option<Uuid>, ask: &PrintAsk, how: How) -> Result<Printed> {
    let ent = k.entity(ctx, &def.entity)?;
    let (theirs, workspace) = languages(tx, ctx, how.reader == Reader::Person).await?;
    let wanted = [ask.language.as_deref(), theirs.as_deref(), workspace.as_deref()];
    let (tag, body) = match &def.layout {
        Some(_) => (wanted.iter().flatten().next().copied().unwrap_or("en").to_string(), None),
        None => {
            let (t, b) = bodies(def, &wanted);
            (t, Some(b))
        }
    };
    let mut vars = match id {
        Some(id) => record_vars(k, tx, ctx, how.reader, def, id, &tag, &Map::new()).await?,
        None => {
            // The sample prints in the company's currency (the first company's), else the workspace's first one.
            let sys = Ctx::system(ctx.tenant, ctx.schema.clone());
            let currencies = currencies(k, tx, ctx, "amount").await?;
            let mut code = None;
            if let Some(cent) = ctx.schema.entity("Company") {
                let mut first = k.find_tx(&sys, tx, "Company", &[], None, 1).await?;
                k.add_titles(&sys, tx, cent, &mut first).await?;
                code = first.first().and_then(|c| c.get("_titles")?.get("default_currency")?.as_str().map(str::to_string));
            }
            let code = code.or_else(|| currencies.as_object().and_then(|m| m.keys().next().cloned())).unwrap_or_else(|| "USD".into());
            let mut v = rich_sample(ctx, &def.entity, &code);
            v["settings"] = crate::settings::load(k, ctx, tx, &named(&all_text(def), "settings.")).await?;
            v["currencies"] = currencies;
            for (n, x) in [("language", json!(tag)), ("currency", json!(code)), ("values", json!({})), ("title", json!("Sample")), ("now", json!(""))] {
                v[n] = x;
            }
            v
        }
    };
    let settings = print_settings(k, tx, ctx).await?;
    // Drafts and cancelled records wear a watermark.
    let mut warnings = Vec::new();
    let status = vars["doc"].get("docstatus").and_then(Value::as_i64).unwrap_or(0);
    let mut watermark = match (ent.is_submittable(), status) {
        (true, 0) if id.is_some() => Some(settings.draft.clone()),
        (true, 2) => Some(settings.cancelled.clone()),
        _ => None,
    }
    .filter(|w| !w.trim().is_empty())
    .map(|w| layout::esc(&w));
    // The record's company (its first link to Company), and the letter head.
    let company_field = ent.def.fields.iter().find(|f| f.ftype == FieldType::Link && f.to.as_deref() == Some("Company"));
    let mut company_id: Option<Uuid> = company_field.and_then(|f| vars["doc"].get(&f.name)).and_then(Value::as_str).and_then(|v| v.parse().ok());
    if def.entity == "Company" {
        company_id = id;
    }
    let sys = Ctx::system(ctx.tenant, ctx.schema.clone());
    let read_company = async |k: &Kernel, tx: &mut Tx, cid: Option<Uuid>| -> Result<Value> {
        let Some(cid) = cid else { return Ok(Value::Null) };
        let Some(cent) = ctx.schema.entity("Company") else { return Ok(Value::Null) };
        match k.get_tx(&sys, tx, "Company", cid, false).await {
            Ok(mut c) => {
                k.add_titles(&sys, tx, cent, std::slice::from_mut(&mut c)).await?;
                crate::messages::without_secrets(&ctx.schema, cent, &mut c);
                Ok(Value::Object(c))
            }
            Err(_) => Ok(Value::Null),
        }
    };
    let mut company = read_company(k, tx, company_id).await?;
    let pick = |s: &str| -> Option<Option<Uuid>> {
        match s {
            "none" => Some(None),
            "default" => None,
            id => Some(id.parse().ok()),
        }
    };
    let chosen: Option<Uuid> = match ask.letterhead.as_deref().and_then(pick).or_else(|| pick(&def.letterhead)) {
        Some(explicit) => explicit,
        None => company.get("letter_head").and_then(Value::as_str).and_then(|v| v.parse().ok()).or(settings.letter_head),
    };
    let lh = match (&ask.letterhead_draft, chosen) {
        (Some(d), _) => Some(draft_letter_head(ctx, d)?),
        (None, Some(l)) => letter_head(k, tx, ctx, l).await?,
        (None, None) => None,
    };
    if company.is_null()
        && let Some(c) = lh.as_ref().and_then(|l| l.company)
    {
        company = read_company(k, tx, Some(c)).await?;
    }
    // Images: a layout's, the letter head's, and what a template's `image()` may name.
    let mut wants = Vec::new();
    if let Some(l) = &def.layout {
        layout_wants(l, &vars, &company, None, &mut wants);
    }
    for part in lh.iter().flat_map(|l| [&l.header, &l.footer]).flatten() {
        layout_wants(part, &vars, &company, Some(("LetterHead", lh.as_ref().map(|l| l.id).unwrap_or_default())), &mut wants);
    }
    if def.layout.is_none() && all_text(def).contains("image(") {
        let files = |e: &ferrum_meta::EffEntity, r: &Value| -> Vec<String> {
            e.def.fields.iter().filter(|f| f.ftype == FieldType::Attach).filter_map(|f| r.get(&f.name)?.as_str().map(str::to_string)).collect()
        };
        let person = if how.reader == Reader::Person { ReadAs::Person } else { ReadAs::Workspace };
        wants.extend(files(&ent, &vars["doc"]).into_iter().map(|url| Want { url, by: person, on: None }));
        for (f, r) in vars["links"].as_object().cloned().unwrap_or_default() {
            if let Some(t) = ent.field(&f).and_then(|x| x.to.as_deref()).and_then(|t| ctx.schema.entity(t)) {
                wants.extend(files(t, &r).into_iter().map(|url| Want { url, by: person, on: None }));
            }
        }
        if let Some(c) = ctx.schema.entity("Company") {
            wants.extend(files(c, &company).into_iter().map(|url| Want { url, by: ReadAs::Workspace, on: None }));
        }
    }
    let (images, missing) = inline(k, tx, ctx, wants).await?;
    warnings.extend(missing);
    let page = def.page.clone().unwrap_or_else(|| settings.page.clone());
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
    // The time of printing where the workspace is.
    let zone: chrono_tz::Tz = ctx.schema.timezone.parse().unwrap_or(chrono_tz::UTC);
    let local = chrono::Utc::now().with_timezone(&zone).format("%Y-%m-%d %H:%M").to_string();
    let title = vars["title"].as_str().unwrap_or_default().to_string();
    let doc_title = format!("{} {}", ent.def.label, vars["doc"].get("number").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or(&title));
    let print = |copy: Option<&str>| -> Map<String, Value> {
        json!({"copy": copy.unwrap_or(""), "now": now, "time": local, "language": tag, "title": title, "entity": ent.def.label,
               "letterhead": lh.is_some(), "letterhead_name": lh.as_ref().map(|l| l.name.clone()).unwrap_or_default()})
        .as_object()
        .cloned()
        .unwrap_or_default()
    };
    let links = vars["links"].as_object().cloned().unwrap_or_default();
    let currency = vars["currency"].as_str().map(str::to_string);
    let mode = match (how.design, how.design_all) {
        (false, _) => Mode::Print,
        (true, false) => Mode::Design,
        (true, true) => Mode::DesignAll,
    };
    let one = print(None);
    let base = layout::Data {
        schema: &ctx.schema,
        ent: &ent,
        doc: &vars["doc"],
        links: &links,
        company: &company,
        settings: &vars["settings"],
        print: &one,
        language: &tag,
        currency: currency.as_deref(),
        currencies: &vars["currencies"],
        images: &images,
        mode,
        part: Part::Body,
    };
    // The letter head: repeated by the PDF service in the page's margins, else placed in the body.
    let lh_part = |l: &Option<Layout>, part: Part, warnings: &mut Vec<String>| -> Option<String> {
        let r = layout::render(l.as_ref()?, &layout::Data { part, ..base });
        warnings.extend(r.warnings);
        Some(r.html).filter(|h| !h.is_empty())
    };
    let template_has = |part: fn(&DocumentBody) -> &Option<String>| body.is_some_and(|b| part(b).is_some());
    let repeat_head = ask.pdf && settings.repeat_header && !template_has(|b| &b.header);
    let repeat_foot = ask.pdf && settings.repeat_footer && !template_has(|b| &b.footer);
    let (lh_head, lh_foot) = match &lh {
        Some(l) => (
            lh_part(&l.header, if repeat_head { Part::Margin } else { Part::LetterHead }, &mut warnings),
            lh_part(&l.footer, if repeat_foot { Part::Margin } else { Part::LetterHead }, &mut warnings),
        ),
        None => (None, None),
    };
    let (in_head, in_foot) = (lh_head.as_ref().filter(|_| !repeat_head), lh_foot.as_ref().filter(|_| !repeat_foot));
    let side = (page.margin_mm.left, page.margin_mm.right);
    let font = def.layout.as_ref().and_then(|l| l.style.as_ref()).and_then(|s| s.font).unwrap_or(settings.font).name();
    let mut margin_head = lh_head.as_ref().filter(|_| repeat_head).map(|h| layout::margin_page(h, &tag, font, side, (page.margin_mm.top, 0)));
    let mut margin_foot = lh_foot.as_ref().filter(|_| repeat_foot).map(|f| layout::margin_page(f, &tag, font, side, (0, page.margin_mm.bottom)));
    // A letter head's height as given, else as tall as its blocks come out on this print (`auto`).
    let width = page.sheet_mm().0 - (page.margin_mm.left + page.margin_mm.right) as f64;
    let room = |given: Option<f64>, part: &Option<Layout>, most: f64| {
        given.unwrap_or_else(|| part.as_ref().map_or(0.0, |l| layout::estimate_mm(l, &layout::Data { part: Part::Margin, ..base }, width).min(most)))
    };
    let (head_mm, foot_mm) = (
        lh.as_ref().filter(|_| margin_head.is_some()).map_or(0.0, |l| room(l.header_mm, &l.header, 120.0)),
        lh.as_ref().filter(|_| margin_foot.is_some()).map_or(0.0, |l| room(l.footer_mm, &l.footer, 80.0)),
    );
    let copies: Vec<Option<String>> = if ask.copies.is_empty() { vec![None] } else { ask.copies.iter().cloned().map(Some).collect() };
    let mut parts = Vec::new();
    let html = match (&def.layout, body) {
        (Some(l), _) => {
            // A stored layout is not trusted: checked again as it renders.
            let problems = layout::check(&ctx.schema, Some(&ent), l);
            if !problems.is_empty() {
                return Err(layout_errors(problems));
            }
            let first = print(copies[0].as_deref());
            if let Some(w) = layout::watermark(l, &layout::Data { print: &first, ..base }) {
                watermark = Some(match watermark {
                    Some(d) => format!("{d} · {w}"),
                    None => w,
                });
            }
            for c in &copies {
                let p = print(c.as_deref());
                let r = layout::render(l, &layout::Data { print: &p, ..base });
                warnings.extend(r.warnings);
                let mut inside = String::new();
                if let Some(h) = in_head {
                    inside.push_str(&format!("<header class=\"pf-letterhead\">{h}</header>"));
                }
                inside.push_str(&r.html);
                if let Some(f) = in_foot {
                    inside.push_str(&format!("<footer class=\"pf-letterhead-foot\">{f}</footer>"));
                }
                parts.push(inside);
            }
            let style = l.style.clone().unwrap_or_default();
            let page_css = if def.page.is_some() || !ask.pdf { layout::page_css(&page) } else { String::new() };
            layout::page(&layout::Page {
                language: &tag,
                title: &doc_title,
                font,
                size: style.size.unwrap_or(Size::Md),
                color: style.color.as_deref().unwrap_or("#111111"),
                accent: style.accent.as_deref().unwrap_or("#111111"),
                line_height: style.line_height.unwrap_or(1.45),
                page_css: if ask.pdf { String::new() } else { page_css },
                design: how.design,
                watermark: watermark.clone(),
                copies: copies.iter().cloned().zip(parts.iter().cloned()).collect(),
            })
        }
        (None, Some(b)) => {
            vars["company"] = company.clone();
            let names_letterhead = all_text(def).contains("letterhead.");
            let x = Extras { images: Arc::new(images.clone()), letterhead: Some((lh_head.clone().unwrap_or_default(), lh_foot.clone().unwrap_or_default())) };
            let mut out = Vec::new();
            let mut own = (None, None);
            for c in &copies {
                vars["print"] = Value::Object(print(c.as_deref()));
                let pages = render_all_x(def, b, &vars, false, &x, &tag)?;
                own = (pages.header, pages.footer);
                let (h, f) = if names_letterhead { (None, None) } else { (in_head.map(String::as_str), in_foot.map(String::as_str)) };
                out.push(wear(&pages.body, h, f, watermark.as_deref()));
            }
            parts = out.clone();
            if names_letterhead {
                // The template places the letter head itself.
                margin_head = None;
                margin_foot = None;
            }
            if let Some(h) = own.0 {
                margin_head = Some(margin_page(&h));
            }
            if let Some(f) = own.1 {
                margin_foot = Some(margin_page(&f));
            }
            let mut html = out.join(PAGE_BREAK);
            if let Some(p) = def.page.as_ref().filter(|_| !ask.pdf) {
                html = format!("<style>{}</style>{html}", layout::page_css(p));
            }
            html
        }
        (None, None) => return Err(Error::internal("a template without a body")),
    };
    let pdf = match (&def.page, &def.layout) {
        (Some(p), _) => PdfPage::from_setup(p, head_mm, foot_mm),
        (None, Some(_)) => PdfPage::from_setup(&page, head_mm, foot_mm),
        (None, None) => {
            let mut p = PdfPage::from_css(margin_head.is_some(), margin_foot.is_some());
            if head_mm > 0.0 {
                p.margin_in[0] = p.margin_in[0].max((page.margin_mm.top as f64 + head_mm) / 25.4);
            }
            if foot_mm > 0.0 {
                p.margin_in[2] = p.margin_in[2].max((page.margin_mm.bottom as f64 + foot_mm) / 25.4);
            }
            p
        }
    };
    Ok(Printed { html, header: margin_head, footer: margin_foot, pdf, title, language: tag, warnings, parts })
}

/// Whether Print Settings let `ctx` print this record: refused for a draft or a cancelled record they turn off.
fn may_print_status(ent: &ferrum_meta::EffEntity, doc: &Value, s: &PrintSet) -> std::result::Result<(), String> {
    if !ent.is_submittable() {
        return Ok(());
    }
    match doc.get("docstatus").and_then(Value::as_i64).unwrap_or(0) {
        0 if !s.allow_draft => Err("printing a draft is turned off in Print settings".into()),
        2 if !s.allow_cancelled => Err("printing a cancelled record is turned off in Print settings".into()),
        _ => Ok(()),
    }
}

fn check_ask(ask: &PrintAsk) -> Result<()> {
    if ask.copies.len() > 5 || ask.copies.iter().any(|c| c.trim().is_empty() || c.chars().count() > 60) {
        return Err(Error::field("copy", "at most 5 copies, each named in 1 to 60 characters"));
    }
    if let Some(l) = ask.letterhead.as_deref().filter(|l| *l != "none" && *l != "default" && !ferrum_meta::layout::is_uuid(l)) {
        return Err(Error::field("letterhead", format!("none or a letter head's id, not {}", l.chars().take(40).collect::<String>())));
    }
    if let Some(l) = ask.language.as_deref().filter(|l| l.is_empty() || l.len() > 20 || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')) {
        return Err(Error::field("language", format!("{} is not a language tag", l.chars().take(40).collect::<String>())));
    }
    Ok(())
}

// ── generating and keeping ──

/// A document to generate and keep on its record ([`generate`]).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generate {
    /// A template's key.
    pub template: String,
    pub record: Uuid,
    /// The file's name; else the template's label and the record's number.
    #[serde(default)]
    pub name: Option<String>,
    /// Its language; else the workspace's.
    #[serde(default)]
    pub language: Option<String>,
    /// Drop the file this template made for the record before.
    #[serde(default)]
    pub replace: bool,
    /// What the template reads as `values`.
    #[serde(default)]
    pub values: Map<String, Value>,
}

/// A generated, stored document: its file (`/files/{file}`) and its SHA-256 digest, what a signing step takes.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Generated {
    pub file: Uuid,
    pub url: String,
    pub name: String,
    pub content_type: String,
    pub size: usize,
    pub sha256: String,
    pub template: String,
    pub entity: String,
    pub record: Uuid,
}

/// Renders template `g.template` for its record as the workspace holds it (secret fields left out: it may go outside),
/// a page as a PDF where the server makes PDFs (HTML otherwise), and stores it attached to the record, in the
/// caller's transaction: it is there, and the one it replaces gone, only if that commits. On the record's audit trail.
pub async fn generate(k: &Kernel, tx: &mut Tx, ctx: &Ctx, g: &Generate) -> Result<Generated> {
    let def = effective(tx, ctx, &g.template).await?;
    if def.query.is_some() {
        return Err(Error::invalid(format!("document {} is about a query: it is made from the report's rows", def.key)));
    }
    // Whoever generates it may read the record; what the document holds is the workspace's view.
    k.get_tx(ctx, tx, &def.entity, g.record, false).await?;
    let pdf = k.pdf_enabled();
    let (bytes, content_type, ext, title) = if def.output == DocumentOutput::Page && def.values.is_empty() {
        let ask = PrintAsk { language: g.language.clone(), pdf, ..Default::default() };
        let p = render_page(k, tx, ctx, &def, Some(g.record), &ask, How { reader: Reader::Outside, design: false, design_all: false }).await?;
        // ponytail: the PDF service is called inside the caller's transaction (up to its 60 s); a job if that ever matters.
        let (b, t, e) = page_bytes(k, p.html, p.header, p.footer, &p.pdf, pdf).await?;
        (b, t, e, p.title)
    } else {
        let (pages, vars, _) = render_record(k, tx, ctx, Reader::Outside, &def, g.record, g.language.as_deref(), &g.values).await?;
        let title = vars["title"].as_str().unwrap_or_default().to_string();
        let (b, t, e) = produce(k, &def, pages, pdf).await?;
        (b, t, e, title)
    };
    let name = file_name(&def, &title, g.name.as_deref(), ext);
    keep(k, tx, ctx, &def.key, (&def.entity, g.record), name, bytes, content_type, g.replace).await
}

/// Stores `bytes` as template `key`'s file on record `record` of `entity` in the caller's transaction (the one the
/// template made for it before dropped with `replace`), on the record's audit trail.
#[allow(clippy::too_many_arguments)]
async fn keep(
    k: &Kernel,
    tx: &mut Tx,
    ctx: &Ctx,
    template: &str,
    (entity, record): (&str, Uuid),
    name: String,
    bytes: Vec<u8>,
    content_type: String,
    replace: bool,
) -> Result<Generated> {
    k.check_storage_quota(ctx.tenant, bytes.len() as u64).await?;
    let id = Uuid::now_v7();
    let key = format!("tenants/{}/{}", ctx.tenant, id);
    // ponytail: the object is written before the transaction commits; one it rolls back leaves an object no row names.
    k.file_store()?.put(&key, &bytes).await?;
    if replace {
        // One a mail still waits to attach stays on the record, no longer this template's: the mail goes with it.
        sqlx::query(
            "update file_object f set document = null where f.tenant_id = $1 and f.entity = $2 and f.record_id = $3 and f.document = $4 \
             and exists (select 1 from outbox o where o.tenant_id = $1 and o.topic = $5 and o.processed_at is null and not o.dead and o.payload->'files' ? f.id::text)",
        )
        .bind(ctx.tenant)
        .bind(entity)
        .bind(record)
        .bind(template)
        .bind(crate::outbox::MAIL_TOPIC)
        .execute(&mut **tx)
        .await?;
        let gone: Vec<String> =
            sqlx::query_scalar("delete from file_object where tenant_id = $1 and entity = $2 and record_id = $3 and document = $4 returning storage_key")
                .bind(ctx.tenant)
                .bind(entity)
                .bind(record)
                .bind(template)
                .fetch_all(&mut **tx)
                .await?;
        crate::outbox::release_objects(tx, ctx.tenant, &gone).await?;
    }
    let sha = hex::encode(Sha256::digest(&bytes));
    let stored_type =
        if crate::files::ACTIVE_TYPES.iter().any(|t| content_type.starts_with(t)) { "application/octet-stream".to_string() } else { content_type };
    sqlx::query(
        "insert into file_object (tenant_id, id, entity, record_id, file_name, content_type, size_bytes, storage_key, sha256, created_by, scan_status, document) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'unscanned', $11)",
    )
    .bind(ctx.tenant)
    .bind(id)
    .bind(entity)
    .bind(record)
    .bind(&name)
    .bind(&stored_type)
    .bind(bytes.len() as i64)
    .bind(&key)
    .bind(&sha)
    .bind((!ctx.user.is_nil()).then_some(ctx.user))
    .bind(template)
    .execute(&mut **tx)
    .await?;
    audit::record(tx, ctx, entity, record, "document:generate", None, json!({"template": template, "file": id, "sha256": sha})).await?;
    Ok(Generated {
        file: id,
        url: format!("/files/{id}"),
        name,
        content_type: stored_type,
        size: bytes.len(),
        sha256: sha,
        template: template.to_string(),
        entity: entity.to_string(),
        record,
    })
}

/// A page's bytes: a PDF on its paper (the parts the service repeats with it), or the HTML.
async fn page_bytes(
    k: &Kernel,
    html: String,
    header: Option<String>,
    footer: Option<String>,
    page: &PdfPage,
    pdf: bool,
) -> Result<(Vec<u8>, String, &'static str)> {
    if pdf {
        return Ok((k.page_to_pdf_on(&html, header.as_deref(), footer.as_deref(), page).await?, "application/pdf".into(), "pdf"));
    }
    Ok((html.into_bytes(), "text/html; charset=utf-8".into(), "html"))
}

/// The bytes of what a template's `pages` hold: a page as a PDF (`pdf`, its header and footer repeated) or as HTML;
/// text, CSV and XML in the template's encoding and line ends.
async fn produce(k: &Kernel, def: &DocumentDef, pages: Pages, pdf: bool) -> Result<(Vec<u8>, String, &'static str)> {
    if def.output != DocumentOutput::Page {
        return produce_file(def, pages);
    }
    let page = PdfPage::from_css(pages.header.is_some(), pages.footer.is_some());
    let (h, f) = (pages.header.as_deref().map(margin_page), pages.footer.as_deref().map(margin_page));
    page_bytes(k, pages.body, h, f, &page, pdf).await
}

// ── documents about a query ──

/// A document about a query to render ([`Kernel::render_query_document`], [`Kernel::generate_query_documents`], the
/// `report_documents` effect).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryRun {
    pub template: String,
    /// The report's parameters: `filters`, `from`, `to`.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
    /// What the template reads as `values`.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub values: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// A grouped run's files and where the next run starts (the last record done), when there are more.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GroupFiles {
    pub files: Vec<Generated>,
    pub next: Option<Uuid>,
}

/// The template a run names, if it is about a query, with its query.
async fn query_template(tx: &mut Tx, ctx: &Ctx, key: &str) -> Result<(DocumentDef, DocumentQuery)> {
    let def = effective(tx, ctx, key).await?;
    let q = def.query.clone().ok_or_else(|| Error::invalid(format!("document {key} is about one record, not a query")))?;
    Ok((def, q))
}

/// One file's pages for `rows` (and the record `group`, when grouped): the variables are `rows`, `columns`, `group`,
/// `values`, `settings`, `language`, `currencies` and `now`; totals are the template's to compute (`rows|total('x')`).
#[allow(clippy::too_many_arguments)]
async fn render_rows(
    k: &Kernel,
    tx: &mut Tx,
    ctx: &Ctx,
    def: &DocumentDef,
    columns: &[Value],
    rows: Vec<Map<String, Value>>,
    group: Value,
    run: &QueryRun,
    person: bool,
) -> Result<Pages> {
    let (theirs, workspace) = languages(tx, ctx, person).await?;
    let (tag, body) = bodies(def, &[run.language.as_deref(), theirs.as_deref(), workspace.as_deref()]);
    let text = all_text(def);
    let vars = json!({
        "rows": rows, "columns": columns, "group": group, "values": run.values, "language": tag,
        "settings": crate::settings::load(k, ctx, tx, &named(&text, "settings.")).await?,
        "currencies": currencies(k, tx, ctx, &text).await?, "now": chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string(),
    });
    render_all(def, body, &vars, false, &tag)
}

/// The rows by the record their `group_by` column links to, in that record's id order; rows linking to none are in
/// no file.
fn grouped(rows: Vec<Map<String, Value>>, by: &str) -> BTreeMap<Uuid, Vec<Map<String, Value>>> {
    let mut out: BTreeMap<Uuid, Vec<Map<String, Value>>> = BTreeMap::new();
    for r in rows {
        if let Some(id) = r.get(by).and_then(Value::as_str).and_then(|s| s.parse().ok()) {
            out.entry(id).or_default().push(r);
        }
    }
    out
}

/// Renders and keeps the files of the groups after `after`, at most [`MAX_GROUPS`]: each on its record, the one the
/// template made there before replaced. `ctx` reads the rows and the records (a person, or the workspace); what is
/// kept leaves their secret fields out, as a generated document does. `then` runs for each file kept (an effect's
/// message).
async fn group_files(
    k: &Kernel,
    tx: &mut Tx,
    ctx: &Ctx,
    run: &QueryRun,
    after: Option<Uuid>,
    person: bool,
) -> Result<(GroupFiles, Vec<(String, Uuid, Value)>, usize)> {
    let (def, q) = query_template(tx, ctx, &run.template).await?;
    let (columns, rows) = query_rows(k, ctx, &def, &q, &run.params).await?;
    let target = group_target(ctx, &columns, &q).map_err(Error::invalid)?.ok_or_else(|| Error::invalid(format!("document {} groups by nothing", def.key)))?;
    let tent = k.entity(ctx, &target)?;
    let groups = grouped(rows, q.group_by.as_deref().unwrap_or_default());
    let left: Vec<(Uuid, Vec<Map<String, Value>>)> = groups.into_iter().filter(|(id, _)| after.is_none_or(|a| *id > a)).collect();
    let total = left.len();
    if person && total > MAX_GROUPS {
        return Err(Error::invalid(format!(
            "report {} gives rows for {total} records of {}, more than the {MAX_GROUPS} files one run makes; narrow it with its filters, or let an action or a schedule make them in turns",
            q.report, tent.def.label
        )));
    }
    let (mut files, mut records) = (Vec::new(), Vec::new());
    for (id, rows) in left.into_iter().take(MAX_GROUPS) {
        let mut record = k.get_tx(ctx, tx, &target, id, false).await?;
        k.add_titles(ctx, tx, &tent, std::slice::from_mut(&mut record)).await?;
        crate::messages::without_secrets(&ctx.schema, &tent, &mut record);
        let title = crate::collab::record_title(ctx, &target, &record);
        let pages = render_rows(k, tx, ctx, &def, &columns, rows, Value::Object(record.clone()), run, person).await?;
        let (bytes, content_type, ext) = produce(k, &def, pages, k.pdf_enabled()).await?;
        let name = file_name(&def, &title, None, ext);
        files.push(keep(k, tx, ctx, &def.key, (&target, id), name, bytes, content_type, true).await?);
        records.push((target.clone(), id, tent.cel_view(&Value::Object(record), &ctx.schema)));
    }
    let next = (total > MAX_GROUPS).then(|| files.last().map(|f: &Generated| f.record)).flatten();
    Ok((GroupFiles { files, next }, records, total))
}

/// A query document's first file as the administrator previewing it reads the rows: the first group's, when grouped.
async fn preview_query(k: &Kernel, tx: &mut Tx, ctx: &Ctx, def: &DocumentDef, run: &QueryRun) -> Result<Pages> {
    let q = def.query.clone().unwrap_or_else(|| DocumentQuery { report: String::new(), group_by: None });
    let (columns, rows) = query_rows(k, ctx, def, &q, &json!({})).await?;
    let (rows, group) = match (group_target(ctx, &columns, &q).map_err(Error::invalid)?, &q.group_by) {
        (Some(target), Some(by)) => match grouped(rows, by).into_iter().next() {
            Some((id, rows)) => (rows, Value::Object(k.get_tx(ctx, tx, &target, id, false).await?)),
            None => (vec![], Value::Null),
        },
        _ => (rows, Value::Null),
    };
    render_rows(k, tx, ctx, def, &columns, rows, group, run, true).await
}

/// What the `report_documents` effect asks, kept in its job: the run, the message each record is sent its file with,
/// and the record after which the next job starts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct GroupsJob {
    pub run: QueryRun,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<ReportMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Uuid>,
}

/// The message each record's file goes out with: a template about the grouped entity, to the recipient specs a
/// `message` effect takes (`field:<name>` of that record, `owner`, `role:<name>`, `user:<id>`, `links:…`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportMessage {
    pub template: String,
    #[serde(deserialize_with = "crate::automation::one_or_many")]
    pub to: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,
}

struct Groups;

#[async_trait::async_trait]
impl crate::jobs::JobHandler for Groups {
    fn kind(&self) -> &'static str {
        GROUPS_KIND
    }

    fn title(&self) -> &'static str {
        "Documents from a report"
    }

    /// One batch as the workspace, in one transaction: its files kept and sent, and the next batch's job queued when
    /// there are more, or none of it.
    async fn run(&self, k: &Kernel, job: &crate::jobs::Job) -> Result<Value> {
        let spec: GroupsJob = serde_json::from_value(job.payload.clone()).map_err(Error::internal)?;
        let ctx = Ctx::system(job.tenant_id, k.registry.schema(job.tenant_id).await?);
        let mut tx = k.db.tenant_tx(job.tenant_id).await?;
        let (out, records, total) = Box::pin(group_files(k, &mut tx, &ctx, &spec.run, spec.after, false)).await?;
        if let Some(m) = &spec.message {
            for ((entity, id, doc), file) in records.iter().zip(&out.files) {
                let mut to = Vec::new();
                for t in &m.to {
                    to.extend(crate::messages::recipients(k, &mut tx, &ctx, entity, doc, t).await?);
                }
                let msg = crate::messages::Message {
                    template: m.template.clone(),
                    entity: Some(entity.clone()),
                    record: Some(*id),
                    to,
                    channels: m.channels.clone(),
                    attachments: vec![file.file],
                    ..Default::default()
                };
                crate::messages::send(k, &mut tx, &ctx, &msg).await?;
            }
        }
        if let Some(next) = out.next {
            let more = GroupsJob { after: Some(next), ..spec };
            crate::jobs::enqueue(&mut tx, job.tenant_id, job.created_by, crate::jobs::NewJob::new(GROUPS_KIND, json!(more))).await?;
        }
        tx.commit().await?;
        Ok(json!({"files": out.files.len(), "left": total - out.files.len(), "next": out.next}))
    }

    fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(600)
    }
}

pub(crate) fn handler(kind: &str) -> Option<std::sync::Arc<dyn crate::jobs::JobHandler>> {
    (kind == GROUPS_KIND).then(|| std::sync::Arc::new(Groups) as std::sync::Arc<dyn crate::jobs::JobHandler>)
}

/// Queues the first job of a grouped run for an effect, in the caller's transaction: nothing is done unless it commits.
pub(crate) async fn queue_groups(tx: &mut Tx, ctx: &Ctx, run: QueryRun, message: Option<ReportMessage>) -> Result<Uuid> {
    if query_template(tx, ctx, &run.template).await?.1.group_by.is_none() {
        return Err(Error::invalid(format!("document {} makes one file, not one per record: report_documents makes those", run.template)));
    }
    let by = (!ctx.user.is_nil()).then_some(ctx.user);
    crate::jobs::enqueue(tx, ctx.tenant, by, crate::jobs::NewJob::new(GROUPS_KIND, json!(GroupsJob { run, message, after: None }))).await
}

/// What a preview is asked for (`POST /api/admin/documents/preview`).
#[derive(Debug, Clone, Default)]
pub struct PreviewAsk {
    /// A template's key, or a template not saved yet.
    pub template: Value,
    pub record: Option<Uuid>,
    pub sample: bool,
    pub language: Option<String>,
    pub letterhead: Option<String>,
    /// A letter head not saved yet, worn in place of `letterhead`.
    pub letterhead_draft: Option<Value>,
    pub copies: Vec<String>,
    pub design: bool,
    /// On the canvas, every part at full size (`design: "all"`), not what a condition hides collapsed.
    pub design_all: bool,
    pub pdf: bool,
}

/// A preview: the JSON a page's preview answers, or a PDF's bytes.
pub enum Preview {
    Json(Value),
    Pdf(Vec<u8>),
}

impl Kernel {
    /// [`generate`] in a transaction of its own, for an app outside one.
    pub async fn generate_document(&self, ctx: &Ctx, g: &Generate) -> Result<Generated> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let out = generate(self, &mut tx, ctx, g).await?;
        tx.commit().await?;
        Ok(out)
    }

    // ── administrators ──

    /// Every template: `{key, label, entity, output, kind, default, is_default, enabled, roles, languages, source,
    /// default_changed, based_on, updated_by, updated_at}`, those of `entity` and `output` when given.
    pub async fn document_templates_of(&self, ctx: &Ctx, entity: Option<&str>, output: Option<&str>) -> Result<Vec<Value>> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let all = every(&mut tx, ctx).await?;
        let stamps: Vec<(String, Option<Uuid>, chrono::DateTime<chrono::Utc>, Option<String>)> = sqlx::query_as(
            "select d.key, d.updated_by, d.updated_at, coalesce(nullif(u.full_name, ''), u.email::text) from document_template d \
             left join app_user u on u.tenant_id = d.tenant_id and u.id = d.updated_by where d.tenant_id = $1",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        let stamps: HashMap<String, (Option<Uuid>, chrono::DateTime<chrono::Utc>, Option<String>)> =
            stamps.into_iter().map(|(k, b, a, n)| (k, (b, a, n))).collect();
        let mut defaults: HashMap<String, Option<String>> = HashMap::new();
        for e in all.values().map(|(d, ..)| d.entity.clone()).collect::<BTreeSet<_>>() {
            defaults.insert(e.clone(), default_for(&mut tx, ctx, &all, &e).await?.map(|d| d.key.clone()));
        }
        tx.commit().await?;
        let wanted = |d: &DocumentDef| {
            entity.is_none_or(|e| d.entity == e)
                && output.is_none_or(|o| serde_json::to_value(d.output).ok().and_then(|v| v.as_str().map(|s| s == o)) == Some(true))
        };
        Ok(all
            .into_values()
            .filter(|(d, ..)| wanted(d))
            .map(|(d, source, changed)| {
                let (by, at, name) = match stamps.get(&d.key).cloned() {
                    Some((b, a, n)) => (b, Some(a), n),
                    None => (None, None, None),
                };
                json!({"key": d.key, "label": d.label, "entity": d.entity, "output": d.output, "kind": kind(&d), "default": d.default,
                       "is_default": defaults.get(&d.entity).cloned().flatten().as_deref() == Some(d.key.as_str()), "enabled": d.enabled,
                       "roles": d.roles, "languages": d.languages.keys().collect::<Vec<_>>(), "source": source, "default_changed": changed,
                       "based_on": d.based_on, "updated_by": by, "updated_by_name": name, "updated_at": at})
            })
            .collect())
    }

    /// Every template ([`Kernel::document_templates_of`] unfiltered).
    pub async fn document_templates(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        self.document_templates_of(ctx, None, None).await
    }

    /// `{template, default, edited, default_changed, etag}`: the template as it is used (with its derived `kind`), and
    /// the package's default.
    pub async fn document_template(&self, ctx: &Ctx, key: &str) -> Result<Value> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let (template, source, changed) = every(&mut tx, ctx).await?.remove(key).ok_or_else(|| Error::NotFound(format!("document template {key}")))?;
        tx.commit().await?;
        let default = ctx.schema.documents.iter().find(|d| d.key == key);
        let mut t = serde_json::to_value(&template).map_err(Error::internal)?;
        t["kind"] = json!(kind(&template));
        Ok(json!({"template": t, "default": default, "edited": source != "package", "default_changed": changed, "etag": etag(&template)}))
    }

    /// Saves a workspace's template, its own or its copy of a package's default. Refused when the check finds
    /// anything wrong. Audited.
    pub async fn save_document_template(&self, ctx: &Ctx, def: &DocumentDef) -> Result<()> {
        self.save_document_template_if(ctx, def, None).await.map(drop)
    }

    /// [`Kernel::save_document_template`], refused (`409`) when `if_match` is given and is not the template's entity
    /// tag as it is now (someone else saved it since it was read). Its new tag.
    pub async fn save_document_template_if(&self, ctx: &Ctx, def: &DocumentDef, if_match: Option<&str>) -> Result<String> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        if let Some(tag) = if_match {
            let now = every(&mut tx, ctx).await?.remove(&def.key).map(|t| etag(&t.0));
            if now.as_deref() != Some(tag.trim()) && tag.trim() != "*" {
                return Err(Error::Conflict("changed by someone else since you opened it".into()));
            }
        }
        save_template(self, &mut tx, ctx, def).await?;
        tx.commit().await?;
        Ok(etag(def))
    }

    /// A copy of template `from` as the workspace's own `key` (`based_on` it, not the default, enabled): what
    /// "Customize this format" makes. `409` when `key` is taken.
    pub async fn duplicate_document_template(&self, ctx: &Ctx, from: &str, key: &str, label: &str) -> Result<DocumentDef> {
        admin_only(ctx)?;
        let label = label.trim();
        if label.is_empty() || label.chars().count() > 80 {
            return Err(Error::field("label", "a name of 1 to 80 characters"));
        }
        if ctx.schema.packages.iter().any(|p| key.strip_prefix(p.as_str()).is_some_and(|r| r.starts_with('.'))) {
            return Err(Error::field("key", "a workspace's own format has a key no package's name starts"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let all = every(&mut tx, ctx).await?;
        if all.contains_key(key) {
            return Err(Error::Conflict(format!("there is a format {key} already")));
        }
        let mut def = all.get(from).map(|t| t.0.clone()).ok_or_else(|| Error::NotFound(format!("document template {from}")))?;
        def.key = key.into();
        def.label = Some(label.into());
        def.default = false;
        def.enabled = true;
        def.based_on = Some(from.into());
        def.layout_file = None;
        if let Some(p) = def.problem() {
            return Err(Error::field("key", p));
        }
        save_template(self, &mut tx, ctx, &def).await?;
        tx.commit().await?;
        Ok(def)
    }

    /// Back to the package's default (a workspace's own template is removed, and with it the entity's default when
    /// it was that one; its versions stay). Audited.
    pub async fn reset_document_template(&self, ctx: &Ctx, key: &str) -> Result<()> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("delete from document_template where tenant_id = $1 and key = $2").bind(ctx.tenant).bind(key).execute(&mut *tx).await?;
        if n.rows_affected() == 0 {
            return Err(Error::NotFound(format!("an edited document template {key}")));
        }
        if !ctx.schema.documents.iter().any(|d| d.key == key) {
            sqlx::query("delete from document_default where tenant_id = $1 and key = $2").bind(ctx.tenant).bind(key).execute(&mut *tx).await?;
        }
        audit::record(&mut tx, ctx, "DocumentTemplate", Uuid::nil(), "reset", None, json!({"key": key})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The workspace's default page format for `entity` (`None`: its choice removed, the next in D9's order prints).
    /// Refused for a key that is not an enabled page format of the entity. Audited.
    pub async fn set_document_default(&self, ctx: &Ctx, entity: &str, key: Option<&str>) -> Result<()> {
        admin_only(ctx)?;
        if ctx.schema.entity(entity).is_none() {
            return Err(Error::NotFound(format!("entity {entity}")));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        if let Some(key) = key {
            let all = every(&mut tx, ctx).await?;
            let ok = all.get(key).is_some_and(|(d, ..)| d.entity == entity && d.output == DocumentOutput::Page && d.enabled);
            if !ok {
                return Err(Error::field("key", format!("{key} is not an enabled page format of {entity}")));
            }
        }
        choose_default(&mut tx, ctx, entity, key).await?;
        tx.commit().await?;
        Ok(())
    }

    /// A template's saved versions, newest first: `{n, saved_by, saved_at}`.
    pub async fn document_template_versions(&self, ctx: &Ctx, key: &str) -> Result<Vec<Value>> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<(i32, Option<Uuid>, chrono::DateTime<chrono::Utc>)> =
            sqlx::query_as("select n, saved_by, saved_at from document_template_version where tenant_id = $1 and key = $2 order by n desc limit $3")
                .bind(ctx.tenant)
                .bind(key)
                .bind(MAX_VERSIONS)
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(|(n, by, at)| json!({"n": n, "saved_by": by, "saved_at": at})).collect())
    }

    /// Version `n` of a template as it was saved (restoring it is saving it again).
    pub async fn document_template_version(&self, ctx: &Ctx, key: &str, n: i32) -> Result<Value> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let def: Option<Value> = sqlx::query_scalar("select def from document_template_version where tenant_id = $1 and key = $2 and n = $3")
            .bind(ctx.tenant)
            .bind(key)
            .bind(n)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        def.ok_or_else(|| Error::NotFound(format!("version {n} of {key}")))
    }

    /// What a designer may place for `entity`: its fields, system fields, tables and their columns, linked records'
    /// fields, the company's, computed values, the print's, settings. No secret field.
    pub fn document_fields(&self, ctx: &Ctx, entity: &str) -> Result<Value> {
        admin_only(ctx)?;
        let ent = self.entity(ctx, entity)?;
        Ok(layout::catalogue(&ctx.schema, &ent))
    }

    /// A template rendered against a record as the administrator reads it (the one named, else the latest): a page
    /// as HTML, a file as its text. `template` is a key, or a template not saved yet (checked as a save checks it).
    /// Writes nothing.
    pub async fn preview_document(&self, ctx: &Ctx, template: &Value, language: Option<&str>, record: Option<Uuid>) -> Result<Value> {
        let ask = PreviewAsk { template: template.clone(), record, language: language.map(str::to_string), ..Default::default() };
        match self.preview_print(ctx, &ask).await? {
            Preview::Json(v) => Ok(v),
            Preview::Pdf(_) => Err(Error::internal("a preview asked for HTML made a PDF")),
        }
    }

    /// [`Kernel::preview_document`] with the print's choices: a real record or the sample one, the letter head,
    /// copies, the designer's canvas (`design`, layouts), or a PDF. `{language, output, kind, content, header, footer,
    /// record_id, warnings, ms}`. Writes nothing; draft and cancelled records preview whatever Print Settings say.
    pub async fn preview_print(&self, ctx: &Ctx, ask: &PreviewAsk) -> Result<Preview> {
        admin_only(ctx)?;
        let started = std::time::Instant::now();
        let print = PrintAsk {
            letterhead: ask.letterhead.clone(),
            letterhead_draft: ask.letterhead_draft.clone(),
            language: ask.language.clone(),
            copies: ask.copies.clone(),
            pdf: ask.pdf,
            template: None,
        };
        check_ask(&print)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let def = match &ask.template {
            Value::String(key) => effective(&mut tx, ctx, key).await?,
            v => {
                let def: DocumentDef = serde_json::from_value(v.clone()).map_err(|e| Error::field("template", e.to_string()))?;
                check_template(self, &mut tx, ctx, &def).await?;
                def
            }
        };
        if def.query.is_some() {
            let values: Map<String, Value> = def.values.iter().map(|v| (v.clone(), json!(format!("[{v}]")))).collect();
            let run = QueryRun { template: def.key.clone(), values, language: ask.language.clone(), ..Default::default() };
            let pages = preview_query(self, &mut tx, ctx, &def, &run).await?;
            tx.commit().await?;
            return Ok(Preview::Json(json!({"output": def.output, "kind": kind(&def), "content": pages.body, "header": pages.header, "footer": pages.footer,
                "warnings": [], "ms": started.elapsed().as_millis() as u64})));
        }
        // The latest record the administrator may read, said whatever is previewed, so the canvas may offer it.
        let q = crate::query::ListQuery { sort: Some("updated_at desc".into()), limit: Some(1), ..Default::default() };
        let latest = self.list(ctx, &def.entity, &q).await?.rows.into_iter().next();
        let latest_record = latest.as_ref().map(|d| json!({"id": d.get("id"), "title": crate::collab::record_title(ctx, &def.entity, d)}));
        let id = match (ask.record, ask.sample) {
            (Some(id), _) => Some(id),
            (None, true) if def.output == DocumentOutput::Page => None,
            _ => {
                let q = crate::query::ListQuery { sort: Some("updated_at desc".into()), limit: Some(1), ..Default::default() };
                let first = self.list(ctx, &def.entity, &q).await?.rows.first().map(crate::store::doc_id).transpose()?;
                match first {
                    Some(id) => Some(id),
                    None if def.output == DocumentOutput::Page => None,
                    None => return Err(Error::invalid(format!("there is no {} to preview it with", def.entity))),
                }
            }
        };
        if def.output != DocumentOutput::Page || !def.values.is_empty() {
            let id = id.ok_or_else(|| Error::invalid(format!("there is no {} to preview it with", def.entity)))?;
            let values: Map<String, Value> = def.values.iter().map(|v| (v.clone(), json!(format!("[{v}]")))).collect();
            let (pages, _, tag) = render_record(self, &mut tx, ctx, Reader::Person, &def, id, ask.language.as_deref(), &values).await?;
            tx.commit().await?;
            return Ok(Preview::Json(json!({"language": tag, "output": def.output, "kind": kind(&def), "content": pages.body, "header": pages.header,
                "footer": pages.footer, "record_id": id, "warnings": [], "ms": started.elapsed().as_millis() as u64})));
        }
        let p = render_page(
            self,
            &mut tx,
            ctx,
            &def,
            id,
            &print,
            How { reader: Reader::Person, design: ask.design && def.layout.is_some(), design_all: ask.design_all },
        )
        .await?;
        tx.commit().await?;
        if ask.pdf {
            let (bytes, ..) = page_bytes(self, p.html, p.header, p.footer, &p.pdf, true).await?;
            return Ok(Preview::Pdf(bytes));
        }
        Ok(Preview::Json(json!({"language": p.language, "output": def.output, "kind": kind(&def), "content": p.html, "header": p.header,
            "footer": p.footer, "record_id": id, "latest_record": latest_record, "warnings": p.warnings, "ms": started.elapsed().as_millis() as u64})))
    }

    // ── documents about a query ──

    /// Whether template `key`, about a query, makes one file per record (else one file); refused for a record's.
    pub async fn query_document_groups(&self, ctx: &Ctx, key: &str) -> Result<bool> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let (_, q) = query_template(&mut tx, ctx, key).await?;
        tx.commit().await?;
        Ok(q.group_by.is_some())
    }

    /// Template `run.template`, about a query that does not group, over the report's rows as `ctx` may read them:
    /// one file. A file of data needs export permission and is on the audit trail (as an export of `entity`).
    pub async fn render_query_document(&self, ctx: &Ctx, run: &QueryRun) -> Result<Rendered> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let (def, q) = query_template(&mut tx, ctx, &run.template).await?;
        may_render(ctx, &def)?;
        if q.group_by.is_some() {
            return Err(Error::invalid(format!("document {} makes a file per record: generate them", def.key)));
        }
        let (columns, rows) = query_rows(self, ctx, &def, &q, &run.params).await?;
        let pages = render_rows(self, &mut tx, ctx, &def, &columns, rows, Value::Null, run, true).await?;
        let (bytes, content_type, ext) = produce(self, &def, pages, self.pdf_enabled()).await?;
        if def.output != DocumentOutput::Page {
            let sha = hex::encode(Sha256::digest(&bytes));
            audit::record(&mut tx, ctx, &def.entity, Uuid::nil(), "document:export", None, json!({"template": def.key, "report": q.report, "sha256": sha}))
                .await?;
        }
        tx.commit().await?;
        let name = file_name(&def, &q.report, None, ext);
        Ok(Rendered { bytes, content_type, file_name: name, output: def.output, language: run.language.clone().unwrap_or_default() })
    }

    /// Template `run.template`, about a query grouped by a link, over the report's rows as `ctx` may read them: one
    /// file per record linked, kept on it (the one the template made there before replaced). More than
    /// [`MAX_GROUPS`] records is refused with the numbers.
    pub async fn generate_query_documents(&self, ctx: &Ctx, run: &QueryRun) -> Result<GroupFiles> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let (def, _) = query_template(&mut tx, ctx, &run.template).await?;
        may_render(ctx, &def)?;
        let (out, ..) = Box::pin(group_files(self, &mut tx, ctx, run, None, true)).await?;
        tx.commit().await?;
        Ok(out)
    }

    // ── people printing ──

    /// The templates `ctx` may print `entity` with (`{key, label, output}`), and the one it prints with when none is
    /// picked (`default`, null for the entity's print format).
    pub async fn document_choices(&self, ctx: &Ctx, entity: &str) -> Result<Value> {
        self.print_choices(ctx, entity, None).await
    }

    /// What the print dialog offers for `entity` (§6.1): the enabled formats `ctx` may use (`{key, label, output, kind,
    /// languages, letterhead, copies, page}`), `standard`, D9's `default`, the letter heads, the languages, Print
    /// Settings' copies; with `record`, whether it may be printed now (`may_print`, `refusal`) and the letter head
    /// its company prints on (`default_for_record`).
    pub async fn print_choices(&self, ctx: &Ctx, entity: &str, record: Option<Uuid>) -> Result<Value> {
        ctx.require(entity, Action::Read)?;
        let ent = self.entity(ctx, entity)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let all = every(&mut tx, ctx).await?;
        let default = default_for(&mut tx, ctx, &all, entity).await?.map(|d| d.key.clone());
        let settings = print_settings(self, &mut tx, ctx).await?;
        let sys = Ctx::system(ctx.tenant, ctx.schema.clone());
        let heads = match ctx.schema.entity("LetterHead") {
            Some(_) => self.find_tx(&sys, &mut tx, "LetterHead", &[], Some("letter_head_name asc"), 200).await?,
            None => vec![],
        };
        let mut words: Vec<String> = vec!["en".into()];
        if let Some(w) = ctx.schema.entity("InterfaceWord") {
            let more: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "select distinct language from {} where tenant_id = $1 order by language limit 50",
                crate::ddl::q(&w.table)
            )))
            .bind(ctx.tenant)
            .fetch_all(&mut *tx)
            .await?;
            words.extend(more.into_iter().filter(|l| l != "en"));
        }
        let (mut may_print, mut refusal, mut record_head) = (true, None::<String>, None::<String>);
        if let Some(id) = record {
            let doc = Value::Object(self.get_tx(ctx, &mut tx, entity, id, false).await?);
            if let Err(why) = may_print_status(&ent, &doc, &settings) {
                (may_print, refusal) = (false, Some(why));
            }
            let company = ent.def.fields.iter().find(|f| f.ftype == FieldType::Link && f.to.as_deref() == Some("Company")).and_then(|f| doc.get(&f.name));
            let company_head = match company.and_then(Value::as_str).and_then(|c| c.parse::<Uuid>().ok()) {
                Some(c) => {
                    self.get_tx(&sys, &mut tx, "Company", c, false).await.ok().and_then(|c| c.get("letter_head").and_then(Value::as_str).map(str::to_string))
                }
                None => None,
            };
            record_head = company_head.or_else(|| settings.letter_head.map(|u| u.to_string()));
        }
        tx.commit().await?;
        let templates: Vec<Value> = all
            .values()
            .filter(|(d, ..)| d.entity == entity && d.enabled && d.query.is_none() && may_use(ctx, d))
            .map(|(d, ..)| {
                let page = d.page.as_ref().unwrap_or(&settings.page);
                json!({"key": d.key, "label": d.label, "output": d.output, "kind": kind(d),
                       "languages": if d.layout.is_some() { words.clone() } else { d.languages.keys().cloned().collect() },
                       "letterhead": d.letterhead, "copies": if d.copies.is_empty() { &settings.copies } else { &d.copies },
                       "page": {"size": page.size, "orientation": page.orientation}})
            })
            .collect();
        let letterheads: Vec<Value> = heads
            .iter()
            .filter(|h| h.get("disabled").and_then(Value::as_bool) != Some(true))
            .map(|h| {
                let id = h.get("id").and_then(Value::as_str).unwrap_or_default();
                json!({"id": id, "name": h.get("letter_head_name"), "company": h.get("company"),
                       "default_for_record": record.is_some() && record_head.as_deref() == Some(id)})
            })
            .collect();
        Ok(json!({"templates": templates,
                  "standard": {"label": "Standard", "page": {"size": settings.page.size, "orientation": settings.page.orientation}}, "default": default, "letterheads": letterheads,
                  "languages": words, "copies": settings.copies, "may_print": may_print, "refusal": refusal}))
    }

    /// The entity's default template's key, when it has one `ctx` may print with: what a print without a template
    /// picks. None leaves the print format as it was.
    pub async fn default_document(&self, ctx: &Ctx, entity: &str) -> Result<Option<String>> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let all = every(&mut tx, ctx).await?;
        let out = default_for(&mut tx, ctx, &all, entity).await?.map(|d| d.key.clone());
        tx.commit().await?;
        Ok(out)
    }

    /// Prints record `id` of `entity` as `ctx` may read it (`GET …/print`): the format asked for, else the entity's
    /// default, else its print format (`standard`); with the letter head, language and copies asked for; a PDF on the
    /// format's paper, or HTML. Refused: a format `ctx` may not use (403), a draft or cancelled record Print Settings
    /// do not let print (403), a format of another entity or none (404), a disabled one or a bad choice (422).
    pub async fn print_record(&self, ctx: &Ctx, entity: &str, id: Uuid, ask: &PrintAsk) -> Result<Rendered> {
        ctx.require(entity, Action::Read)?;
        check_ask(ask)?;
        let ent = self.entity(ctx, entity)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let all = every(&mut tx, ctx).await?;
        let def = match ask.template.as_deref() {
            Some("standard") => standard(ctx, entity)?,
            Some(key) => {
                let d = all.get(key).map(|t| t.0.clone()).filter(|d| d.entity == entity).ok_or_else(|| Error::NotFound(format!("a {entity} format {key}")))?;
                if !d.enabled {
                    return Err(Error::field("template", format!("{key} is turned off")));
                }
                d
            }
            None => match default_for(&mut tx, ctx, &all, entity).await? {
                Some(d) => d.clone(),
                None => standard(ctx, entity)?,
            },
        };
        may_render(ctx, &def)?;
        if def.query.is_some() {
            return Err(Error::field("template", format!("{} is about a report's rows, not one record", def.key)));
        }
        if def.output != DocumentOutput::Page {
            tx.commit().await?;
            return self.render_document(ctx, &def.key, id, ask.language.as_deref(), ask.pdf).await;
        }
        let doc = Value::Object(self.get_tx(ctx, &mut tx, entity, id, false).await?);
        let settings = print_settings(self, &mut tx, ctx).await?;
        may_print_status(&ent, &doc, &settings).map_err(Error::Forbidden)?;
        let p = render_page(self, &mut tx, ctx, &def, Some(id), ask, How { reader: Reader::Person, design: false, design_all: false }).await?;
        tx.commit().await?;
        let number = doc.get("number").and_then(Value::as_str).filter(|n| !n.is_empty()).map(str::to_string).unwrap_or(p.title.clone());
        let (bytes, content_type, ext) = page_bytes(self, p.html, p.header, p.footer, &p.pdf, ask.pdf).await?;
        let mut named = def.clone();
        named.label = Some(def.label.clone().unwrap_or_else(|| ent.def.label.clone()));
        Ok(Rendered { bytes, content_type, file_name: file_name(&named, &number, None, ext), output: def.output, language: p.language })
    }

    /// Template `key` for record `id`, as `ctx` may read it: a page as a PDF when `pdf` (HTML otherwise), a file of
    /// data in its encoding. A file of data needs export permission and is on the record's audit trail.
    pub async fn render_document(&self, ctx: &Ctx, key: &str, id: Uuid, language: Option<&str>, pdf: bool) -> Result<Rendered> {
        self.render_documents(ctx, key, &[id], 1, language, pdf).await
    }

    /// Template `key` for each of `ids`, `copies` times each, as one document (labels for items, batches, bins,
    /// parcels): a page's records each start a page, a file's follow one another. At most [`MAX_RECORDS`] records,
    /// [`MAX_COPIES`] copies and [`MAX_LABELS`] in all: more is refused, and the caller asks again for the next ones.
    pub async fn render_documents(&self, ctx: &Ctx, key: &str, ids: &[Uuid], copies: u32, language: Option<&str>, pdf: bool) -> Result<Rendered> {
        if ids.is_empty() || ids.len() > MAX_RECORDS || copies == 0 || copies > MAX_COPIES || ids.len() * copies as usize > MAX_LABELS {
            return Err(Error::invalid(format!(
                "one call renders 1 to {MAX_RECORDS} records, 1 to {MAX_COPIES} copies each, {MAX_LABELS} in all; ask for the rest in another"
            )));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let def = effective(&mut tx, ctx, key).await?;
        may_render(ctx, &def)?;
        if def.query.is_some() {
            return Err(Error::invalid(format!("document {key} is about a query, not a record")));
        }
        if !def.enabled {
            return Err(Error::field("template", format!("{key} is turned off")));
        }
        if def.output == DocumentOutput::Page {
            // Labels and pages of many records wear no letter head unless their format names one.
            let letterhead = (ids.len() > 1 || copies > 1).then(|| "none".to_string()).filter(|_| def.letterhead == "default");
            let ask = PrintAsk { language: language.map(str::to_string), pdf, letterhead, ..Default::default() };
            let mut first: Option<Printed> = None;
            let mut parts = Vec::new();
            for id in ids {
                let p = render_page(self, &mut tx, ctx, &def, Some(*id), &ask, How { reader: Reader::Person, design: false, design_all: false }).await?;
                for _ in 0..copies {
                    parts.extend(p.parts.iter().cloned());
                }
                first.get_or_insert(p);
            }
            tx.commit().await?;
            let p = first.expect("one record at least");
            let html = if parts.len() == 1 {
                p.html
            } else if def.layout.is_some() {
                // The first page's head and CSS, every record's inside.
                let head_end = p.html.find("<div class=\"pf-doc\"").unwrap_or(p.html.len());
                let docs: Vec<String> = parts.iter().map(|x| format!("<div class=\"pf-doc\">{x}</div>")).collect();
                format!("{}{}</body></html>", &p.html[..head_end], docs.join("<div class=\"pf-break\"></div>"))
            } else {
                parts.join(PAGE_BREAK)
            };
            let (bytes, content_type, ext) = page_bytes(self, html, p.header, p.footer, &p.pdf, pdf).await?;
            return Ok(Rendered { bytes, content_type, file_name: file_name(&def, &p.title, None, ext), output: def.output, language: p.language });
        }
        let (mut body, mut first, mut tag) = (Vec::new(), None, String::new());
        for id in ids {
            let (pages, vars, t) = render_record(self, &mut tx, ctx, Reader::Person, &def, *id, language, &Map::new()).await?;
            first.get_or_insert(vars["title"].as_str().unwrap_or_default().to_string());
            tag = t;
            body.extend(std::iter::repeat_n(pages.body, copies as usize));
        }
        let pages = Pages { body: body.join(""), header: None, footer: None };
        let title = first.unwrap_or_default();
        let (bytes, content_type, ext) = produce_file(&def, pages)?;
        // A file of data leaves the workspace as an export does: on each record's trail.
        let sha = hex::encode(Sha256::digest(&bytes));
        for id in ids {
            audit::record(&mut tx, ctx, &def.entity, *id, "document:export", None, json!({"template": def.key, "output": def.output, "sha256": sha})).await?;
        }
        tx.commit().await?;
        Ok(Rendered { bytes, content_type, file_name: file_name(&def, &title, None, ext), output: def.output, language: tag })
    }
}

/// `template` or `layout` (derived; never saved).
fn kind(d: &DocumentDef) -> &'static str {
    if d.layout.is_some() { "layout" } else { "template" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(output: DocumentOutput, body: &str) -> DocumentDef {
        DocumentDef {
            key: "t.x".into(),
            entity: "E".into(),
            output,
            languages: BTreeMap::from([("en".into(), DocumentBody { body: body.into(), ..Default::default() })]),
            ..Default::default()
        }
    }

    fn out(output: DocumentOutput, body: &str, vars: Value) -> String {
        render(output, body, &vars, false).unwrap()
    }

    #[test]
    fn each_output_escapes_for_its_own_syntax() {
        let hostile = json!({"v": "a,\"b\"\n=1+2</x><y a='1'>&\u{1}", "n": null});
        assert_eq!(out(DocumentOutput::Csv, "{{ v }},{{ n }}", hostile.clone()), "\"a,\"\"b\"\"\n=1+2</x><y a='1'>&\u{1}\",");
        assert_eq!(out(DocumentOutput::Xml, "<a>{{ v }}</a>{{ n }}", hostile.clone()), "<a>a,&quot;b&quot;\n=1+2&lt;/x&gt;&lt;y a=&apos;1&apos;&gt;&amp;</a>");
        assert_eq!(out(DocumentOutput::Page, "<p>{{ v }}</p>", json!({"v": "<script>"})), "<p>&lt;script&gt;</p>");
        assert_eq!(out(DocumentOutput::Text, "[{{ v|pad(5) }}][{{ n }}]", hostile), "[a,\"b\"][]");
        // A page writing a CSV field escapes it as HTML all the same.
        assert_eq!(out(DocumentOutput::Page, "{{ v|csv }}", json!({"v": "<b>"})), "&lt;b&gt;");
        // An XML-escaped value is not escaped twice.
        assert_eq!(out(DocumentOutput::Xml, "{{ v|xml }}", json!({"v": "&"})), "&amp;");
    }

    #[test]
    fn a_strict_check_refuses_what_is_not_there() {
        assert!(render(DocumentOutput::Csv, "{{ doc.nope }}", &json!({"doc": {}}), true).is_err());
        assert!(render(DocumentOutput::Page, "{{ doc.nope }}", &json!({"doc": {}}), true).is_err());
        assert!(render(DocumentOutput::Text, "{{ doc.a }}", &json!({"doc": {"a": 1}}), true).is_ok());
    }

    #[test]
    fn the_language_asked_for_sets_how_numbers_read() {
        let mut d = def(DocumentOutput::Text, "x");
        d.languages.insert("hi".into(), DocumentBody { body: "y".into(), ..Default::default() });
        assert_eq!(bodies(&d, &[Some("en-IN")]).0, "en-IN");
        assert_eq!(bodies(&d, &[Some("hi-IN")]).1.body, "y");
        assert_eq!(bodies(&d, &[Some("fr"), None, Some("hi")]).0, "hi");
        assert_eq!(bodies(&d, &[Some("fr")]).0, "en");
    }

    #[test]
    fn file_names_are_safe() {
        let d = def(DocumentOutput::Csv, "x");
        assert_eq!(file_name(&d, "INV/2026/1", None, "csv"), "x INV_2026_1.csv");
        assert_eq!(file_name(&d, "", Some("../../etc/passwd"), "csv"), ".._.._etc_passwd.csv");
        assert_eq!(file_name(&d, "", Some("Offer.PDF"), "pdf"), "Offer.PDF");
    }

    #[test]
    fn totals_and_counts_in_a_file() {
        let body = "{% for l in doc.lines %}{{ l.amount|pad(8, 'right', '0') }}\n{% endfor %}T{{ doc.lines|length|pad(4, 'right', '0') }}{{ doc.lines|total('amount') }}";
        let vars = json!({"doc": {"lines": [{"amount": "10.50"}, {"amount": "0.25"}, {"amount": null}]}});
        assert_eq!(out(DocumentOutput::Text, body, vars), "00010.50\n00000.25\n00000000\nT000310.75");
    }

    #[test]
    fn a_letter_head_and_watermark_go_into_a_templates_page() {
        let page = wear("<html><head></head><BODY class=x><p>Hi</p></body></html>", Some("<b>Acme</b>"), Some("f"), Some("Draft"));
        let at = |s: &str| page.find(s).unwrap();
        let (body, mark, head, hi, foot) =
            (at("<BODY class=x>"), at("class=\"pf-watermark\""), at("class=\"pf-letterhead\""), at("<p>Hi"), at("class=\"pf-letterhead-foot\""));
        assert!(body < mark && mark < head && head < hi && hi < foot && foot < at("</body>"), "{page}");
        assert_eq!(wear("<p>x</p>", None, None, None), "<p>x</p>", "nothing to wear: the page as it was");
    }

    #[test]
    fn an_add_ons_default_beats_what_it_extends() {
        let mut schema = ferrum_meta::Schema { packages: vec!["selling".into(), "zindia".into(), "other".into()], ..Default::default() };
        schema.package_depends = BTreeMap::from([
            ("zindia".to_string(), vec!["accounting".to_string()]),
            ("accounting".to_string(), vec!["selling".to_string()]),
            ("selling".to_string(), vec![]),
            ("other".to_string(), vec![]),
        ]);
        let d = |k: &str| DocumentDef { key: k.into(), ..Default::default() };
        let (other, selling, india) = (d("other.invoice"), d("selling.sales_invoice"), d("zindia.gst_invoice"));
        // In key order, as the defaults come.
        assert_eq!(shipped_default(&schema, &[&selling, &india]).unwrap().key, "zindia.gst_invoice", "through accounting, not by key");
        assert_eq!(shipped_default(&schema, &[&other, &selling, &india]).unwrap().key, "other.invoice", "unrelated: the first by key");
        assert_eq!(shipped_default(&schema, &[&selling]).unwrap().key, "selling.sales_invoice");
    }

    #[test]
    fn a_templates_error_says_where() {
        let e = render_raw(DocumentOutput::Page, "a\nb\n{% if %}", &json!({}), true, &Extras::default()).unwrap_err();
        assert!(where_(&e).starts_with("line 3: "), "{}", where_(&e));
    }
}
