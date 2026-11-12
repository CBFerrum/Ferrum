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

use crate::ctx::Ctx;
use crate::db::Tx;
use crate::error::{Error, Result};
use crate::{Action, Kernel, audit};
use ferrum_meta::{DocumentBody, DocumentDef, DocumentOutput, DocumentQuery, FieldType};
use minijinja::value::Value as JValue;
use minijinja::{AutoEscape, Error as JError, ErrorKind, UndefinedBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// The largest body (or header, or footer) a template keeps.
pub const MAX_BODY: usize = 256 * 1024;
/// Records one call renders together (labels), copies of each, and labels in all: more is refused, never cut; the
/// caller asks for the next records in another call.
pub const MAX_RECORDS: usize = 200;
pub const MAX_COPIES: u32 = 100;
pub const MAX_LABELS: usize = 1000;
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

fn render(output: DocumentOutput, src: &str, vars: &Value, strict: bool) -> Result<String> {
    if src.len() > MAX_BODY {
        return Err(Error::field("template", "a body is larger than 256 KB"));
    }
    environment(output, strict).render_str(src, JValue::from_serialize(vars)).map_err(|e| Error::field("template", e.to_string()))
}

/// A body, its header and its footer, rendered.
struct Pages {
    body: String,
    header: Option<String>,
    footer: Option<String>,
}

fn render_all(def: &DocumentDef, b: &DocumentBody, vars: &Value, strict: bool) -> Result<Pages> {
    let part = |s: &Option<String>| s.as_deref().map(|s| render(def.output, s, vars, strict)).transpose();
    Ok(Pages { body: render(def.output, &b.body, vars, strict)?, header: part(&b.header)?, footer: part(&b.footer)? })
}

/// The names after `prefix` in a template's text (`settings.Company` → `Company`).
fn named(text: &str, prefix: &str) -> BTreeSet<String> {
    text.match_indices(prefix)
        .map(|(i, _)| text[i + prefix.len()..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect::<String>())
        .filter(|n| !n.is_empty())
        .collect()
}

fn all_text(def: &DocumentDef) -> String {
    def.languages.values().flat_map(|b| [Some(&b.body), b.header.as_ref(), b.footer.as_ref()]).flatten().cloned().collect::<Vec<_>>().join("\n")
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
    let currencies = currencies(k, tx, ctx, &text).await?;
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
/// every entity it links to: for checking a template, an unknown field refused.
fn sample(ctx: &Ctx, entity: &str) -> Value {
    fn fields(ent: &ferrum_meta::EffEntity, ctx: &Ctx, lines: bool) -> Map<String, Value> {
        let mut doc: Map<String, Value> = ["id", "number", "created_at", "updated_at", "created_by", "version", "docstatus", "status"]
            .into_iter()
            .map(|f| (f.to_string(), json!("")))
            .collect();
        let mut titles = Map::new();
        for f in ent.def.fields.iter().filter(|f| f.ftype != FieldType::Secret) {
            let v = match f.ftype {
                FieldType::Table if lines => {
                    json!([f.of.as_deref().and_then(|c| ctx.schema.entity(c)).map(|c| Value::Object(fields(c, ctx, false))).unwrap_or(json!({}))])
                }
                FieldType::Table => json!([]),
                FieldType::Check => json!(false),
                t if t.is_numeric() => json!(0),
                _ => json!(""),
            };
            if f.ftype == FieldType::Link {
                titles.insert(f.name.clone(), json!(""));
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
        .filter_map(|f| Some((f.name.clone(), Value::Object(fields(ctx.schema.entity(f.to.as_deref()?)?, ctx, false)))))
        .collect();
    json!({"doc": fields(ent, ctx, true), "links": links, "entity": crate::print::field_meta(ent, &ctx.schema)})
}

/// What is wrong with a template, said as a field error: its shape, its entity, its key (one a package owns must be
/// that package's document), and each body rendered against a record with every field, refusing an unknown one.
async fn check_template(k: &Kernel, tx: &mut Tx, ctx: &Ctx, def: &DocumentDef) -> Result<()> {
    if let Some(p) = def.problem() {
        return Err(Error::field("template", p));
    }
    if ctx.schema.entity(&def.entity).is_none() {
        return Err(Error::field("entity", format!("{} is not an entity here", def.entity)));
    }
    let owner = ctx.schema.packages.iter().find(|p| def.key.strip_prefix(p.as_str()).is_some_and(|r| r.starts_with('.')));
    if let Some(p) = owner
        && !ctx.schema.documents.iter().any(|d| d.key == def.key)
    {
        return Err(Error::field("key", format!("{p} has no document {}; a workspace's own template has a key no package's name starts", def.key)));
    }
    let mut vars = match &def.query {
        Some(q) => query_sample(k, tx, ctx, def, q).await?,
        None => sample(ctx, &def.entity),
    };
    vars["settings"] = crate::settings::load(k, ctx, tx, &named(&all_text(def), "settings.")).await?;
    vars["values"] = Value::Object(def.values.iter().map(|v| (v.clone(), json!(""))).collect());
    for (name, v) in
        [("language", json!("en")), ("currency", json!("USD")), ("currencies", json!({})), ("now", json!("")), ("title", json!("")), ("checking", json!(true))]
    {
        vars[name] = v;
    }
    let ent = if def.query.is_none() { ctx.schema.entity(&def.entity) } else { None };
    let has = |f: &str| match f == "_titles" || ["id", "number", "created_at", "updated_at", "created_by", "version", "docstatus", "status"].contains(&f) {
        true => Some(false),
        false => ent.and_then(|e| e.def.fields.iter().find(|d| d.name == f)).map(|d| d.ftype == FieldType::Secret),
    };
    for (lang, b) in &def.languages {
        vars["language"] = json!(lang);
        render_all(def, b, &vars, true).map_err(|e| {
            let text = [Some(&b.body), b.header.as_ref(), b.footer.as_ref()].into_iter().flatten().cloned().collect::<Vec<_>>().join("\n");
            let said =
                crate::messages::undefined_said(plain(&e), &text, &def.values, ent.map(|e| (e.def.label.as_str(), &has as &dyn Fn(&str) -> Option<bool>)));
            Error::field("template", format!("{lang}: {said}"))
        })?;
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
        Ok(Some(target)) => sample(ctx, &target)["doc"].clone(),
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

fn plain(e: &Error) -> String {
    match e {
        Error::Validation(fs) => fs.iter().map(|f| f.message.clone()).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

// ── templates ──

/// Every template the workspace has: the packages' defaults with its own and edited copies over them, each with
/// where it is from (`package`, `workspace`, `edited`) and, for an edited one, whether a release changed the default
/// since.
async fn every(tx: &mut Tx, ctx: &Ctx) -> Result<BTreeMap<String, (DocumentDef, &'static str, bool)>> {
    let own: Vec<(String, Value, Option<String>)> =
        sqlx::query_as("select key, def, base from document_template where tenant_id = $1").bind(ctx.tenant).fetch_all(&mut **tx).await?;
    let mut out: BTreeMap<String, (DocumentDef, &'static str, bool)> =
        ctx.schema.documents.iter().map(|d| (d.key.clone(), (d.clone(), "package", false))).collect();
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

/// The entity's default: the workspace's (own or edited) over a package's, the first by key among several.
fn default_of<'a>(all: &'a BTreeMap<String, (DocumentDef, &'static str, bool)>, entity: &str) -> Option<&'a DocumentDef> {
    let of = || all.values().filter(|(d, ..)| d.entity == entity && d.default && d.query.is_none());
    of().find(|(_, source, _)| *source != "package").or_else(|| of().next()).map(|t| &t.0)
}

/// [`Kernel::save_document_template`] in the caller's transaction: `created`, `changed` or `unchanged`.
pub(crate) async fn save_template(k: &Kernel, tx: &mut Tx, ctx: &Ctx, def: &DocumentDef) -> Result<&'static str> {
    check_template(k, tx, ctx, def).await?;
    let was: Option<Value> = sqlx::query_scalar("select def from document_template where tenant_id = $1 and key = $2")
        .bind(ctx.tenant)
        .bind(&def.key)
        .fetch_optional(&mut **tx)
        .await?;
    let now = serde_json::to_value(def).map_err(Error::internal)?;
    if was.as_ref() == Some(&now) {
        return Ok("unchanged");
    }
    let base = ctx.schema.documents.iter().find(|m| m.key == def.key).map(digest);
    sqlx::query(
        "insert into document_template (tenant_id, key, def, base, updated_by) values ($1, $2, $3, $4, $5) \
         on conflict (tenant_id, key) do update set def = excluded.def, base = excluded.base, updated_by = excluded.updated_by, updated_at = now()",
    )
    .bind(ctx.tenant)
    .bind(&def.key)
    .bind(now)
    .bind(&base)
    .bind((!ctx.user.is_nil()).then_some(ctx.user))
    .execute(&mut **tx)
    .await?;
    audit::record(tx, ctx, "DocumentTemplate", Uuid::nil(), "save", None, json!({"key": def.key})).await?;
    Ok(if was.is_some() { "changed" } else { "created" })
}

/// The workspace's own and edited templates, for a pack.
pub(crate) async fn for_pack(tx: &mut Tx, tenant: Uuid) -> Result<Vec<DocumentDef>> {
    let defs: Vec<Value> = sqlx::query_scalar("select def from document_template where tenant_id = $1 order by key").bind(tenant).fetch_all(&mut **tx).await?;
    Ok(defs.into_iter().filter_map(|d| serde_json::from_value(d).ok()).collect())
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

/// The bytes of what `pages` hold: a page as a PDF (`pdf`) or as HTML (its header and footer only in a PDF); text,
/// CSV and XML in the template's encoding and line ends.
async fn produce(k: &Kernel, def: &DocumentDef, pages: Pages, pdf: bool) -> Result<(Vec<u8>, String, &'static str)> {
    if def.output == DocumentOutput::Page {
        if pdf {
            let (h, f) = (pages.header.as_deref().map(margin_page), pages.footer.as_deref().map(margin_page));
            return Ok((k.page_to_pdf(&pages.body, h.as_deref(), f.as_deref()).await?, "application/pdf".into(), "pdf"));
        }
        return Ok((pages.body.into_bytes(), "text/html; charset=utf-8".into(), "html"));
    }
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

/// Renders `def` for record `id` as `reader` sees it: the body chosen for `language` (else theirs, else the
/// workspace's), its variables, its pages.
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
    Ok((render_all(def, body, &vars, false)?, vars, tag))
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
    let (pages, vars, _) = render_record(k, tx, ctx, Reader::Outside, &def, g.record, g.language.as_deref(), &g.values).await?;
    // ponytail: the PDF service is called inside the caller's transaction (up to its 60 s); a job if that ever matters.
    let (bytes, content_type, ext) = produce(k, &def, pages, k.pdf_enabled()).await?;
    let name = file_name(&def, vars["title"].as_str().unwrap_or_default(), g.name.as_deref(), ext);
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
    render_all(def, body, &vars, false)
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

impl Kernel {
    /// [`generate`] in a transaction of its own, for an app outside one.
    pub async fn generate_document(&self, ctx: &Ctx, g: &Generate) -> Result<Generated> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let out = generate(self, &mut tx, ctx, g).await?;
        tx.commit().await?;
        Ok(out)
    }

    // ── administrators ──

    /// Every template: `{key, label, entity, output, default, roles, languages, source, default_changed}`.
    pub async fn document_templates(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let all = every(&mut tx, ctx).await?;
        tx.commit().await?;
        Ok(all
            .into_values()
            .map(|(d, source, changed)| {
                json!({"key": d.key, "label": d.label, "entity": d.entity, "output": d.output, "default": d.default, "roles": d.roles,
                       "languages": d.languages.keys().collect::<Vec<_>>(), "source": source, "default_changed": changed})
            })
            .collect())
    }

    /// `{template, default, edited, default_changed}`: the template as it is used, and the package's default.
    pub async fn document_template(&self, ctx: &Ctx, key: &str) -> Result<Value> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let (template, source, changed) = every(&mut tx, ctx).await?.remove(key).ok_or_else(|| Error::NotFound(format!("document template {key}")))?;
        tx.commit().await?;
        let default = ctx.schema.documents.iter().find(|d| d.key == key);
        Ok(json!({"template": template, "default": default, "edited": source != "package", "default_changed": changed}))
    }

    /// Saves a workspace's template, its own or its copy of a package's default. Refused when the check finds
    /// anything wrong. Audited.
    pub async fn save_document_template(&self, ctx: &Ctx, def: &DocumentDef) -> Result<()> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        save_template(self, &mut tx, ctx, def).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Back to the package's default (a workspace's own template is removed). Audited.
    pub async fn reset_document_template(&self, ctx: &Ctx, key: &str) -> Result<()> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("delete from document_template where tenant_id = $1 and key = $2").bind(ctx.tenant).bind(key).execute(&mut *tx).await?;
        if n.rows_affected() == 0 {
            return Err(Error::NotFound(format!("an edited document template {key}")));
        }
        audit::record(&mut tx, ctx, "DocumentTemplate", Uuid::nil(), "reset", None, json!({"key": key})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// A template rendered against a record as the administrator reads it (the one named, else the latest): a page
    /// as HTML, a file as its text. `template` is a key, or a template not saved yet (checked as a save checks it).
    /// Writes nothing.
    pub async fn preview_document(&self, ctx: &Ctx, template: &Value, language: Option<&str>, record: Option<Uuid>) -> Result<Value> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let def = match template {
            Value::String(key) => effective(&mut tx, ctx, key).await?,
            v => {
                let def: DocumentDef = serde_json::from_value(v.clone()).map_err(|e| Error::field("template", e.to_string()))?;
                check_template(self, &mut tx, ctx, &def).await?;
                def
            }
        };
        if def.query.is_some() {
            let values: Map<String, Value> = def.values.iter().map(|v| (v.clone(), json!(format!("[{v}]")))).collect();
            let run = QueryRun { template: def.key.clone(), values, language: language.map(str::to_string), ..Default::default() };
            let pages = preview_query(self, &mut tx, ctx, &def, &run).await?;
            tx.commit().await?;
            return Ok(json!({"output": def.output, "content": pages.body, "header": pages.header, "footer": pages.footer}));
        }
        let id = match record {
            Some(id) => id,
            None => {
                let q = crate::query::ListQuery { sort: Some("updated_at desc".into()), limit: Some(1), ..Default::default() };
                let first = self.list(ctx, &def.entity, &q).await?.rows.first().map(crate::store::doc_id).transpose()?;
                first.ok_or_else(|| Error::invalid(format!("there is no {} to preview it with", def.entity)))?
            }
        };
        let values: Map<String, Value> = def.values.iter().map(|v| (v.clone(), json!(format!("[{v}]")))).collect();
        let (pages, _, tag) = render_record(self, &mut tx, ctx, Reader::Person, &def, id, language, &values).await?;
        tx.commit().await?;
        Ok(json!({"language": tag, "output": def.output, "content": pages.body, "header": pages.header, "footer": pages.footer, "record_id": id}))
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
        ctx.require(entity, Action::Read)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let all = every(&mut tx, ctx).await?;
        tx.commit().await?;
        let default = default_of(&all, entity).filter(|d| may_use(ctx, d)).map(|d| d.key.clone());
        let templates: Vec<Value> = all
            .values()
            .filter(|(d, ..)| d.entity == entity && d.query.is_none() && may_use(ctx, d))
            .map(|(d, ..)| json!({"key": d.key, "label": d.label, "output": d.output}))
            .collect();
        Ok(json!({"templates": templates, "default": default}))
    }

    /// The entity's default template's key, when it has one `ctx` may print with: what a print without a template
    /// picks. None leaves the print format as it was.
    pub async fn default_document(&self, ctx: &Ctx, entity: &str) -> Result<Option<String>> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let all = every(&mut tx, ctx).await?;
        tx.commit().await?;
        Ok(default_of(&all, entity).filter(|d| may_use(ctx, d)).map(|d| d.key.clone()))
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
        let (mut body, mut first, mut tag) = (Vec::new(), None, String::new());
        let mut head = (None, None);
        for id in ids {
            let (pages, vars, t) = render_record(self, &mut tx, ctx, Reader::Person, &def, *id, language, &Map::new()).await?;
            first.get_or_insert(vars["title"].as_str().unwrap_or_default().to_string());
            (tag, head) = (t, (pages.header, pages.footer));
            body.extend(std::iter::repeat_n(pages.body, copies as usize));
        }
        let pages = Pages { body: body.join(if def.output == DocumentOutput::Page { PAGE_BREAK } else { "" }), header: head.0, footer: head.1 };
        let title = first.unwrap_or_default();
        if def.output == DocumentOutput::Page {
            tx.commit().await?;
            let (bytes, content_type, ext) = produce(self, &def, pages, pdf).await?;
            return Ok(Rendered { bytes, content_type, file_name: file_name(&def, &title, None, ext), output: def.output, language: tag });
        }
        let (bytes, content_type, ext) = produce(self, &def, pages, pdf).await?;
        // A file of data leaves the workspace as an export does: on each record's trail.
        let sha = hex::encode(Sha256::digest(&bytes));
        for id in ids {
            audit::record(&mut tx, ctx, &def.entity, *id, "document:export", None, json!({"template": def.key, "output": def.output, "sha256": sha})).await?;
        }
        tx.commit().await?;
        Ok(Rendered { bytes, content_type, file_name: file_name(&def, &title, None, ext), output: def.output, language: tag })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(output: DocumentOutput, body: &str) -> DocumentDef {
        DocumentDef {
            key: "t.x".into(),
            label: None,
            entity: "E".into(),
            output,
            default: false,
            roles: vec![],
            encoding: None,
            crlf: false,
            values: vec![],
            languages: BTreeMap::from([("en".into(), DocumentBody { body: body.into(), ..Default::default() })]),
            query: None,
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
}
