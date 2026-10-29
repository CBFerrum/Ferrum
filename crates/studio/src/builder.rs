//! Builder records: saved reports, dashboard charts, number cards, dashboards and spaces (module home pages).
//!
//! They are ordinary records (generic CRUD API, audit trail, comments, customization) whose JSON
//! settings are compiled and checked on every save, so an invalid one is refused with field-level
//! errors instead of failing later on a dashboard. Viewing goes through the studio endpoints:
//! sharing decides who may open a record (its creator, a role, everyone; managers and
//! administrators always), a dashboard or space lends that to the widgets placed on it, and
//! the data inside is always computed with the viewer's own permissions.

use crate::query::{Resolver, compile};
use crate::run::{run, title_map};
use crate::spec::{Bucket, DateRange, Sort, Spec, TimeSeries, filter_list, resolve_date};
use async_trait::async_trait;
use chrono::{NaiveDate, SecondsFormat, TimeZone};
use ferrum_kernel::ddl::q;
use ferrum_kernel::store::field_expr;
use ferrum_kernel::{Access, Action, Ctx, Doc, DocHook, Error, FieldError, HookCtx, Kernel, ListQuery, Result};
use ferrum_meta::EntityKind;
use futures::StreamExt;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::AssertSqlSafe;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use uuid::Uuid;

pub const REPORT: &str = "Report";
pub const CHART: &str = "DashboardChart";
pub const CARD: &str = "NumberCard";
pub const DASHBOARD: &str = "Dashboard";
pub const SPACE: &str = "Space";
pub const ENTITIES: [&str; 5] = [REPORT, CHART, CARD, DASHBOARD, SPACE];

/// Widgets resolved at once per dashboard or space (each holds a pooled connection).
const PARALLEL: usize = 4;
const GRID_COLUMNS: i64 = 12;

fn s<'a>(doc: &'a Doc, k: &str) -> Option<&'a str> {
    doc.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty())
}

fn id_of(doc: &Doc) -> Option<Uuid> {
    doc.get("id").and_then(Value::as_str).and_then(|v| v.parse().ok())
}

/// Errors from compiling a stored setting, reported against the record field holding it.
fn field_errors(field: &str, e: Error) -> Vec<FieldError> {
    match e {
        Error::Validation(list) => list
            .into_iter()
            .map(|f| FieldError::new(field, if f.field.is_empty() || f.field == field { f.message } else { format!("{}: {}", f.field, f.message) }))
            .collect(),
        other => vec![FieldError::new(field, other.to_string())],
    }
}

fn parse<T: serde::de::DeserializeOwned>(v: &Value, at: &str) -> std::result::Result<T, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("{at}: {e}"))
}

// ───────────────────────────── settings shapes ─────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChartType {
    Bar,
    StackedBar,
    Line,
    Area,
    Pie,
    Donut,
    Percentage,
    Heatmap,
    Scatter,
    Funnel,
    Number,
}

/// A report's chart settings (`Report.chart`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChartSettings {
    #[serde(rename = "type")]
    pub kind: ChartType,
    #[serde(default)]
    pub x: Option<String>,
    #[serde(default)]
    pub y: Vec<String>,
    #[serde(default)]
    pub colors: Vec<String>,
    #[serde(default)]
    pub stacked: bool,
    #[serde(default)]
    pub show_legend: Option<bool>,
    #[serde(default)]
    pub show_values: bool,
    #[serde(default)]
    pub height: Option<u32>,
}

/// One visible column of a saved report, in display order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnSetting {
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub format: Option<Format>,
}

/// Display formatting; the data stays exact, the UI applies it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Format {
    #[serde(default)]
    pub style: Option<FormatStyle>,
    #[serde(default)]
    pub precision: Option<u32>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub suffix: Option<String>,
    /// Conditional colors: the first matching rule wins.
    #[serde(default)]
    pub rules: Vec<FormatRule>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormatStyle {
    Number,
    Int,
    Currency,
    Percent,
    Bar,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormatRule {
    pub op: String,
    pub value: Value,
    pub color: String,
}

/// A dashboard-level filter, applied to every widget that accepts it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterDef {
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(rename = "type")]
    pub kind: FilterType,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub default: Option<Value>,
    /// The field it filters on in widgets that do not map it themselves (defaults to `name`).
    #[serde(default)]
    pub field: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FilterType {
    Link,
    Select,
    Data,
    Check,
    Date,
    DateRange,
}

/// A space block or dashboard widget.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Block {
    /// A tile opening a list (optionally filtered, with a live count), a report, a dashboard or a URL.
    Shortcut {
        label: String,
        #[serde(default)]
        entity: Option<String>,
        #[serde(default)]
        report: Option<Uuid>,
        #[serde(default)]
        dashboard: Option<Uuid>,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        filters: Value,
        #[serde(default)]
        show_count: bool,
        #[serde(default)]
        color: Option<String>,
    },
    /// A card of links.
    Links {
        label: String,
        items: Vec<LinkItem>,
    },
    Chart {
        chart: Uuid,
    },
    Card {
        card: Uuid,
    },
    /// The latest records of an entity.
    List {
        #[serde(default)]
        label: Option<String>,
        entity: String,
        #[serde(default)]
        filters: Value,
        #[serde(default)]
        fields: Vec<String>,
        #[serde(default)]
        sort: Option<String>,
        #[serde(default)]
        limit: Option<u32>,
    },
    Markdown {
        content: String,
    },
    /// A section heading.
    Heading {
        text: String,
        #[serde(default)]
        level: Option<u8>,
    },
    /// Getting-started steps, each done once a matching record exists.
    Onboarding {
        #[serde(default)]
        title: Option<String>,
        steps: Vec<Step>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkItem {
    pub label: String,
    #[serde(default)]
    pub entity: Option<String>,
    #[serde(default)]
    pub report: Option<Uuid>,
    #[serde(default)]
    pub dashboard: Option<Uuid>,
    /// A built-in report (`/api/reports` catalog).
    #[serde(default)]
    pub builtin: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub entity: Option<String>,
    #[serde(default)]
    pub filters: Value,
    #[serde(default)]
    pub url: Option<String>,
}

/// A dashboard grid item: position (12 columns) plus a widget.
#[derive(Debug, Clone)]
pub struct GridItem {
    pub i: String,
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
    pub block: Block,
}

fn take_int(m: &mut Map<String, Value>, k: &str, at: &str, lo: i64, hi: i64) -> std::result::Result<i64, String> {
    match m.remove(k) {
        Some(Value::Number(n)) if n.as_i64().is_some_and(|v| (lo..=hi).contains(&v)) => Ok(n.as_i64().unwrap_or_default()),
        Some(_) => Err(format!("{at}.{k} must be a whole number from {lo} to {hi}")),
        None => Err(format!("{at}.{k} is required")),
    }
}

/// Parses a dashboard layout: positions inside the 12-column grid, unique ids, no overlaps.
pub fn parse_layout(v: &Value) -> std::result::Result<Vec<GridItem>, String> {
    let items = match v {
        Value::Null => return Ok(vec![]),
        Value::Array(a) => a,
        _ => return Err("must be a list of grid items".into()),
    };
    if items.len() > 100 {
        return Err("at most 100 items".into());
    }
    let mut out: Vec<GridItem> = Vec::new();
    for (n, item) in items.iter().enumerate() {
        let at = format!("[{n}]");
        let mut m = item.as_object().cloned().ok_or_else(|| format!("{at} must be an object"))?;
        let i = match m.remove("i") {
            Some(Value::String(s)) if !s.trim().is_empty() && s.len() <= 64 => s,
            None => format!("w{n}"),
            Some(_) => return Err(format!("{at}.i must be a short text id")),
        };
        let (x, y) = (take_int(&mut m, "x", &at, 0, GRID_COLUMNS - 1)?, take_int(&mut m, "y", &at, 0, 10_000)?);
        let (w, h) = (take_int(&mut m, "w", &at, 1, GRID_COLUMNS)?, take_int(&mut m, "h", &at, 1, 100)?);
        if x + w > GRID_COLUMNS {
            return Err(format!("{at} overflows the {GRID_COLUMNS}-column grid (x + w = {})", x + w));
        }
        let block: Block = parse(&Value::Object(m), &at)?;
        if matches!(block, Block::Links { .. } | Block::Onboarding { .. }) {
            return Err(format!("{at}: dashboards hold charts, cards, shortcuts, markdown and lists"));
        }
        if out.iter().any(|o| o.i == i) {
            return Err(format!("{at}.i '{i}' is used twice"));
        }
        if let Some(o) = out.iter().find(|o| x < o.x + o.w && o.x < x + w && y < o.y + o.h && o.y < y + h) {
            return Err(format!("{at} overlaps item '{}'", o.i));
        }
        out.push(GridItem { i, x, y, w, h, block });
    }
    Ok(out)
}

/// Parses space blocks: a widget plus an optional `width` (1-12 columns).
pub fn parse_blocks(v: &Value) -> std::result::Result<Vec<(Option<i64>, Block)>, String> {
    let items = match v {
        Value::Null => return Ok(vec![]),
        Value::Array(a) => a,
        _ => return Err("must be a list of blocks".into()),
    };
    if items.len() > 100 {
        return Err("at most 100 blocks".into());
    }
    items
        .iter()
        .enumerate()
        .map(|(n, item)| {
            let at = format!("[{n}]");
            let mut m = item.as_object().cloned().ok_or_else(|| format!("{at} must be an object"))?;
            let width = if m.contains_key("width") { Some(take_int(&mut m, "width", &at, 1, GRID_COLUMNS)?) } else { None };
            Ok((width, parse(&Value::Object(m), &at)?))
        })
        .collect()
}

pub fn parse_filters_def(v: &Value) -> std::result::Result<Vec<FilterDef>, String> {
    let defs: Vec<FilterDef> = match v {
        Value::Null => vec![],
        other => parse(other, "filters")?,
    };
    if defs.len() > 20 {
        return Err("at most 20 dashboard filters".into());
    }
    let mut seen = HashSet::new();
    for (n, d) in defs.iter().enumerate() {
        if !ferrum_meta::is_ident(&d.name) {
            return Err(format!("[{n}].name must be a lower-case identifier like 'company'"));
        }
        if !seen.insert(d.name.as_str()) {
            return Err(format!("[{n}].name '{}' is used twice", d.name));
        }
        match d.kind {
            FilterType::Link if d.to.is_none() => return Err(format!("[{n}]: a link filter names the entity it links to in 'to'")),
            FilterType::Select if d.options.is_empty() => return Err(format!("[{n}]: a select filter lists its options")),
            _ => {}
        }
    }
    Ok(defs)
}

fn color_ok(c: &str) -> bool {
    let hex = c.strip_prefix('#').is_some_and(|h| matches!(h.len(), 3 | 6 | 8) && h.chars().all(|x| x.is_ascii_hexdigit()));
    hex || (c.len() <= 30 && c.starts_with(|x: char| x.is_ascii_lowercase()) && c.chars().all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '-'))
}

fn url_ok(u: &str) -> bool {
    (u.starts_with('/') && !u.starts_with("//")) || u.starts_with("https://") || u.starts_with("http://")
}

/// Chart rules over the query's columns (`None` when they are only known at run time).
fn check_chart(kind: ChartType, x: Option<&str>, y: &[String], colors: &[String], cols: Option<&Value>) -> Vec<String> {
    let mut errs = Vec::new();
    if colors.len() > 20 || colors.iter().any(|c| !color_ok(c)) {
        errs.push("colors: at most 20 colors, each #rgb, #rrggbb, #rrggbbaa or a lower-case color name".into());
    }
    let Some(cols) = cols.and_then(Value::as_array) else { return errs };
    let role = |c: &Value| c["role"].as_str().unwrap_or_default().to_string();
    let dims: Vec<&str> = cols.iter().filter(|c| role(c) == "dimension").filter_map(|c| c["name"].as_str()).collect();
    let values: Vec<&str> = cols.iter().filter(|c| matches!(role(c).as_str(), "measure" | "previous" | "pivot")).filter_map(|c| c["name"].as_str()).collect();
    if let Some(x) = x
        && !dims.contains(&x)
    {
        errs.push(format!("x: '{x}' is not a dimension of the query (one of: {})", dims.join(", ")));
    }
    for v in y {
        if !values.contains(&v.as_str()) {
            errs.push(format!("y: '{v}' is not a measure of the query (one of: {})", values.join(", ")));
        }
    }
    let measures = if y.is_empty() { cols.iter().filter(|c| role(c) == "measure").count() } else { y.len() };
    let date_dim = cols.iter().any(|c| role(c) == "dimension" && c["type"] == "date");
    match kind {
        ChartType::Number if !dims.is_empty() || measures != 1 => errs.push("a number chart shows one measure without dimensions".into()),
        ChartType::Pie | ChartType::Donut | ChartType::Percentage | ChartType::Funnel if dims.len() != 1 || measures != 1 => {
            errs.push("pie, donut, percentage and funnel charts need exactly one dimension and one measure".into())
        }
        ChartType::Heatmap if !date_dim => errs.push("a heatmap needs a date dimension (a time series by day works best)".into()),
        ChartType::Scatter if measures < 2 => errs.push("a scatter chart needs two measures (x and y values)".into()),
        ChartType::Bar | ChartType::StackedBar | ChartType::Line | ChartType::Area if dims.is_empty() => {
            errs.push("bar, line and area charts need at least one dimension (group_by or time_series)".into())
        }
        _ => {}
    }
    errs
}

// ───────────────────────────── validation ─────────────────────────────

/// What the database knows about referenced records, for validation inside the saving transaction.
struct Refs<'a, 'b> {
    hc: &'a mut HookCtx<'b>,
}

impl Refs<'_, '_> {
    /// Referenced records that do not exist or that the saver may not open: placing a widget
    /// shares it with the container's audience, so only what one may open can be placed.
    async fn unavailable(&mut self, entity: &str, ids: &[Uuid]) -> Result<Vec<Uuid>> {
        let sys = system(self.hc.ctx);
        let mut out = Vec::new();
        for id in ids.iter().copied().collect::<HashSet<Uuid>>() {
            match self.hc.kernel.get_tx(&sys, self.hc.tx, entity, id, false).await {
                Ok(doc) if visible(self.hc.ctx, entity, &doc) => {}
                Ok(_) | Err(Error::NotFound(_)) => out.push(id),
                Err(e) => return Err(e),
            }
        }
        out.sort();
        Ok(out)
    }

    async fn roles_missing(&mut self, roles: &[String]) -> Result<Vec<String>> {
        if roles.is_empty() {
            return Ok(vec![]);
        }
        let found: Vec<String> = sqlx::query_scalar("select name from role where tenant_id = $1 and name = any($2)")
            .bind(self.hc.ctx.tenant)
            .bind(roles)
            .fetch_all(&mut **self.hc.tx)
            .await?;
        Ok(roles.iter().filter(|r| !found.contains(r)).cloned().collect())
    }
}

fn entity_ok(ctx: &Ctx, entity: &str) -> std::result::Result<(), String> {
    match ctx.schema.entity(entity) {
        None => Err(format!("unknown entity '{entity}'")),
        Some(e) if matches!(e.def.kind, EntityKind::Child | EntityKind::Ledger) => Err(format!("{entity} has no list of its own")),
        Some(_) => Ok(()),
    }
}

/// Filters for an entity, checked by compiling a count with them (with the saver's permissions:
/// nobody stores filters on fields they cannot read).
fn filters_ok(ctx: &Ctx, entity: &str, filters: &Value) -> std::result::Result<(), String> {
    if filters.is_null() || filters.as_array().is_some_and(Vec::is_empty) || filters.as_object().is_some_and(Map::is_empty) {
        return Ok(());
    }
    let spec = Spec { entity: entity.into(), filters: filters.clone(), measures: vec![count()], ..Default::default() };
    compile(ctx, &spec).map(|_| ()).map_err(|e| field_errors("", e).into_iter().map(|f| f.message).collect::<Vec<_>>().join("; "))
}

fn count() -> crate::spec::Measure {
    crate::spec::Measure { func: crate::spec::Agg::Count, field: None, label: None }
}

/// Checks one block; collects referenced records for an existence check.
fn check_block(ctx: &Ctx, b: &Block, at: &str, ids: &mut HashMap<&'static str, Vec<Uuid>>, errs: &mut Vec<String>) {
    let mut err = |m: String| errs.push(format!("{at}: {m}"));
    let label_ok = |l: &str| !l.trim().is_empty() && l.chars().count() <= 140;
    match b {
        Block::Shortcut { label, entity, report, dashboard, url, filters, show_count, color } => {
            if !label_ok(label) {
                err("a shortcut needs a label (up to 140 characters)".into());
            }
            let targets = [entity.is_some(), report.is_some(), dashboard.is_some(), url.is_some()].iter().filter(|x| **x).count();
            if targets != 1 {
                err("a shortcut opens exactly one of: entity, report, dashboard, url".into());
            }
            if (!filters.is_null() || *show_count) && entity.is_none() {
                err("filters and show_count apply to entity shortcuts".into());
            }
            if let Some(e) = entity {
                match entity_ok(ctx, e).and_then(|_| filters_ok(ctx, e, filters)) {
                    Ok(()) => {}
                    Err(m) => err(m),
                }
            }
            ids.entry(REPORT).or_default().extend(report.iter());
            ids.entry(DASHBOARD).or_default().extend(dashboard.iter());
            if url.as_deref().is_some_and(|u| !url_ok(u)) {
                err("url must be an app path (/app/...) or an http(s) address".into());
            }
            if color.as_deref().is_some_and(|c| !color_ok(c)) {
                err("color must be #rgb, #rrggbb or a lower-case color name".into());
            }
        }
        Block::Links { label, items } => {
            if !label_ok(label) {
                err("a link card needs a label".into());
            }
            if items.is_empty() || items.len() > 50 {
                err("a link card holds 1 to 50 links".into());
            }
            let catalog = ferrum_domain::reports::catalog();
            for (n, l) in items.iter().enumerate() {
                let targets =
                    [l.entity.is_some(), l.report.is_some(), l.dashboard.is_some(), l.builtin.is_some(), l.url.is_some()].iter().filter(|x| **x).count();
                if targets != 1 || !label_ok(&l.label) {
                    err(format!("items[{n}] needs a label and exactly one of: entity, report, dashboard, builtin, url"));
                }
                if let Some(e) = &l.entity
                    && let Err(m) = entity_ok(ctx, e)
                {
                    err(format!("items[{n}]: {m}"));
                }
                if let Some(name) = &l.builtin
                    && !catalog.as_array().is_some_and(|c| c.iter().any(|r| r["name"] == json!(name)))
                {
                    err(format!("items[{n}]: no built-in report '{name}'"));
                }
                if l.url.as_deref().is_some_and(|u| !url_ok(u)) {
                    err(format!("items[{n}]: url must be an app path or an http(s) address"));
                }
                ids.entry(REPORT).or_default().extend(l.report.iter());
                ids.entry(DASHBOARD).or_default().extend(l.dashboard.iter());
            }
        }
        Block::Chart { chart } => ids.entry(CHART).or_default().push(*chart),
        Block::Card { card } => ids.entry(CARD).or_default().push(*card),
        Block::List { entity, filters, fields, sort, limit, .. } => {
            if let Err(m) = entity_ok(ctx, entity).and_then(|_| filters_ok(ctx, entity, filters)) {
                err(m);
                return;
            }
            if let Some(ent) = ctx.schema.entity(entity) {
                for f in fields {
                    if !ferrum_kernel::store::SYSTEM_KEYS.contains(&f.as_str()) && ent.field(f).is_none_or(|d| !d.ftype.is_stored()) {
                        err(format!("'{f}' is not a field of {}", ent.def.label));
                    }
                }
                if fields.len() > 12 {
                    err("a list shows at most 12 fields".into());
                }
                if let Some(s) = sort
                    && let Err(e) = ferrum_kernel::query::parse_sort(ent, Some(s))
                {
                    err(e.to_string());
                }
            }
            if limit.is_some_and(|l| !(1..=50).contains(&l)) {
                err("limit must be 1 to 50".into());
            }
        }
        Block::Markdown { content } => {
            if content.chars().count() > 20_000 {
                err("markdown is limited to 20000 characters".into());
            }
        }
        Block::Heading { text, level } => {
            if text.trim().is_empty() || text.chars().count() > 200 || level.is_some_and(|l| !(1..=3).contains(&l)) {
                err("a heading has text (up to 200 characters) and a level from 1 to 3".into());
            }
        }
        Block::Onboarding { steps, .. } => {
            if steps.is_empty() || steps.len() > 20 {
                err("onboarding has 1 to 20 steps".into());
            }
            for (n, st) in steps.iter().enumerate() {
                if !label_ok(&st.label) {
                    err(format!("steps[{n}] needs a label"));
                }
                if let Some(e) = &st.entity
                    && let Err(m) = entity_ok(ctx, e).and_then(|_| filters_ok(ctx, e, &st.filters))
                {
                    err(format!("steps[{n}]: {m}"));
                }
                if st.url.as_deref().is_some_and(|u| !url_ok(u)) {
                    err(format!("steps[{n}]: url must be an app path or an http(s) address"));
                }
            }
        }
    }
}

/// Validates `accepts` (dashboard filter name → field path of the widget, or `false`).
fn check_accepts(ctx: &Ctx, entity: Option<&str>, builtin_params: Option<&[String]>, v: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    let m = match v {
        Value::Null => return errs,
        Value::Object(m) => m,
        _ => return vec!["must be an object mapping dashboard filter names to fields, e.g. {\"period\": \"posting_date\"}".into()],
    };
    for (k, target) in m {
        match target {
            Value::Bool(false) => {}
            Value::String(path) => {
                if let Some(params) = builtin_params {
                    if !params.iter().any(|p| p == path || (path == "period" && p == "from_date")) {
                        errs.push(format!("{k}: the report has no parameter '{path}'"));
                    }
                } else if let Some(e) = entity
                    && let Err(e) = Resolver::new(ctx, e, "entity").and_then(|r| r.resolve(path, k))
                {
                    errs.extend(field_errors("", e).into_iter().map(|f| f.message));
                }
            }
            _ => errs.push(format!("{k}: map a filter to a field path, or false to ignore it")),
        }
    }
    errs
}

fn add(errs: &mut Vec<FieldError>, field: &str, list: Vec<String>) {
    errs.extend(list.into_iter().map(|m| FieldError::new(field, m)));
}

pub struct StudioHook(pub &'static str);

#[async_trait]
impl DocHook for StudioHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        // Standard records come with packages: only the package (the system) changes them.
        if doc.get("is_standard") == Some(&json!(true)) && doc.contains_key("created_at") && !hc.ctx.is_system {
            return Err(Error::Forbidden("standard records come with their package and cannot be changed; duplicate it to customize".into()));
        }
        let spec_entity = doc.get("spec").and_then(|v| v.get("entity")).and_then(Value::as_str).map(str::to_string);
        match self.0 {
            REPORT | CARD => {
                doc.insert("base_entity".into(), json!(spec_entity));
            }
            CHART => {
                let entity = match s(doc, "source").unwrap_or("query") {
                    "report" => match doc.get("report").and_then(Value::as_str).and_then(|v| v.parse::<Uuid>().ok()) {
                        Some(rid) => sqlx::query_scalar::<_, Option<String>>("select base_entity from e_report where tenant_id = $1 and id = $2")
                            .bind(hc.ctx.tenant)
                            .bind(rid)
                            .fetch_optional(&mut **hc.tx)
                            .await?
                            .flatten(),
                        None => None,
                    },
                    "builtin" => None,
                    _ => spec_entity,
                };
                doc.insert("base_entity".into(), json!(entity));
            }
            DASHBOARD => {
                // Grid items get stable ids the data endpoint answers by.
                if let Some(Value::Array(items)) = doc.get_mut("layout") {
                    let mut used: HashSet<String> = items.iter().filter_map(|i| i.get("i").and_then(Value::as_str).map(str::to_string)).collect();
                    for (n, item) in items.iter_mut().enumerate() {
                        if let Value::Object(m) = item
                            && !m.contains_key("i")
                        {
                            let mut id = format!("w{n}");
                            while used.contains(&id) {
                                id.push('_');
                            }
                            used.insert(id.clone());
                            m.insert("i".into(), json!(id));
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let ctx = hc.ctx.clone();
        let mut errs: Vec<FieldError> = Vec::new();
        let roles: Vec<String> = doc
            .get("roles")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|r| r.get("role").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        if s(doc, "sharing") == Some("roles") && roles.is_empty() {
            add(&mut errs, "roles", vec!["choose the roles to share with".into()]);
        }
        let unknown = Refs { hc }.roles_missing(&roles).await?;
        if !unknown.is_empty() {
            add(&mut errs, "roles", vec![format!("unknown roles: {}", unknown.join(", "))]);
        }
        let mut ids: HashMap<&'static str, Vec<Uuid>> = HashMap::new();
        match self.0 {
            REPORT => {
                let compiled = match doc.get("spec") {
                    None | Some(Value::Null) => {
                        add(&mut errs, "spec", vec!["is required".into()]);
                        None
                    }
                    Some(v) => match Spec::parse(v).and_then(|sp| compile(&ctx, &sp).map(|p| (sp, p))) {
                        Ok(x) => Some(x),
                        Err(e) => {
                            errs.extend(field_errors("spec", e));
                            None
                        }
                    },
                };
                if let Some((spec, plan)) = &compiled {
                    let cols = columns_of(plan, spec);
                    let kind = s(doc, "kind").unwrap_or("table");
                    let shape = match kind {
                        "table" if !spec.measures.is_empty() || !spec.group_by.is_empty() => Some("a table report lists fields; use kind group_by to measure"),
                        "group_by" if spec.measures.is_empty() => Some("a group_by report needs measures"),
                        "group_by" if spec.pivot.is_some() => Some("use kind pivot for a pivot"),
                        "pivot" if spec.pivot.is_none() => Some("a pivot report needs spec.pivot"),
                        "chart" if spec.measures.is_empty() => Some("a chart report needs measures"),
                        _ => None,
                    };
                    if let Some(m) = shape {
                        add(&mut errs, "kind", vec![m.into()]);
                    }
                    match doc.get("columns") {
                        None | Some(Value::Null) => {}
                        Some(v) => match parse::<Vec<ColumnSetting>>(v, "columns") {
                            Ok(list) => {
                                let names: Vec<&str> = cols.iter().filter_map(|c| c["name"].as_str()).collect();
                                for (n, c) in list.iter().enumerate() {
                                    if !names.contains(&c.name.as_str()) {
                                        add(
                                            &mut errs,
                                            "columns",
                                            vec![format!("[{n}].name: '{}' is not a column of this report (one of: {})", c.name, names.join(", "))],
                                        );
                                    }
                                    if c.width.is_some_and(|w| !(40..=2000).contains(&w))
                                        || c.format.as_ref().is_some_and(|f| f.precision.is_some_and(|p| p > 9))
                                    {
                                        add(&mut errs, "columns", vec![format!("[{n}]: width is 40 to 2000 pixels, precision 0 to 9")]);
                                    }
                                    if c.format.as_ref().is_some_and(|f| {
                                        f.rules
                                            .iter()
                                            .any(|r| !color_ok(&r.color) || !matches!(r.op.as_str(), "=" | "!=" | ">" | ">=" | "<" | "<=" | "between"))
                                    }) {
                                        add(
                                            &mut errs,
                                            "columns",
                                            vec![format!("[{n}].format.rules: op is one of = != > >= < <= between, color #hex or a name")],
                                        );
                                    }
                                }
                            }
                            Err(m) => add(&mut errs, "columns", vec![m]),
                        },
                    }
                    match doc.get("chart") {
                        None | Some(Value::Null) if kind == "chart" => {
                            add(&mut errs, "chart", vec!["a chart report needs chart settings, e.g. {\"type\": \"bar\"}".into()])
                        }
                        None | Some(Value::Null) => {}
                        Some(v) => match parse::<ChartSettings>(v, "chart") {
                            Ok(c) => add(&mut errs, "chart", check_chart(c.kind, c.x.as_deref(), &c.y, &c.colors, Some(&Value::Array(cols.clone())))),
                            Err(m) => add(&mut errs, "chart", vec![m]),
                        },
                    }
                }
            }
            CHART => {
                let kind: ChartType = match doc.get("chart_type").cloned().map(serde_json::from_value) {
                    Some(Ok(k)) => k,
                    _ => ChartType::Bar,
                };
                let y: Vec<String> = match doc.get("y_fields") {
                    None | Some(Value::Null) => vec![],
                    Some(v) => parse(v, "y_fields").unwrap_or_else(|m| {
                        add(&mut errs, "y_fields", vec![m]);
                        vec![]
                    }),
                };
                let colors: Vec<String> = match doc.get("colors") {
                    None | Some(Value::Null) => vec![],
                    Some(v) => parse(v, "colors").unwrap_or_else(|m| {
                        add(&mut errs, "colors", vec![m]);
                        vec![]
                    }),
                };
                let mut columns: Option<Value> = None;
                match s(doc, "source").unwrap_or("query") {
                    "query" => match doc.get("spec").filter(|v| !v.is_null()) {
                        None => add(&mut errs, "spec", vec!["a query chart needs a spec".into()]),
                        Some(v) => match Spec::parse(v).and_then(|sp| compile(&ctx, &sp).map(|p| (sp, p))) {
                            Ok((sp, p)) => {
                                columns = Some(Value::Array(columns_of(&p, &sp)));
                                add(&mut errs, "accepts", check_accepts(&ctx, Some(&sp.entity), None, doc.get("accepts").unwrap_or(&Value::Null)));
                            }
                            Err(e) => errs.extend(field_errors("spec", e)),
                        },
                    },
                    "report" => match doc.get("report").and_then(Value::as_str).and_then(|v| v.parse::<Uuid>().ok()) {
                        None => add(&mut errs, "report", vec!["a report chart names the saved report".into()]),
                        Some(rid) => match s(doc, "base_entity") {
                            Some(e) => {
                                ids.entry(REPORT).or_default().push(rid);
                                add(&mut errs, "accepts", check_accepts(&ctx, Some(e), None, doc.get("accepts").unwrap_or(&Value::Null)))
                            }
                            None => add(&mut errs, "report", vec!["the report does not exist".into()]),
                        },
                    },
                    "builtin" => {
                        let catalog = ferrum_domain::reports::catalog();
                        let name = s(doc, "builtin_report").unwrap_or_default();
                        match catalog.as_array().and_then(|c| c.iter().find(|r| r["name"] == json!(name))) {
                            None => add(&mut errs, "builtin_report", vec![format!("no built-in report '{name}' (see /api/reports)")]),
                            Some(def) => {
                                let params: Vec<String> =
                                    def["params"].as_array().into_iter().flatten().filter_map(|p| p["name"].as_str().map(str::to_string)).collect();
                                match doc.get("builtin_params") {
                                    None | Some(Value::Null) => {}
                                    Some(Value::Object(m)) => {
                                        for (k, v) in m {
                                            if !params.contains(k) || !v.is_string() {
                                                add(
                                                    &mut errs,
                                                    "builtin_params",
                                                    vec![format!("{k}: not a text parameter of {name} (parameters: {})", params.join(", "))],
                                                );
                                            }
                                        }
                                    }
                                    Some(_) => add(&mut errs, "builtin_params", vec!["must be an object of text parameters".into()]),
                                }
                                add(&mut errs, "accepts", check_accepts(&ctx, None, Some(&params), doc.get("accepts").unwrap_or(&Value::Null)));
                            }
                        }
                    }
                    other => add(&mut errs, "source", vec![format!("unknown source '{other}'")]),
                }
                add(&mut errs, "chart_type", check_chart(kind, s(doc, "x_field"), &y, &colors, columns.as_ref()));
            }
            CARD => match doc.get("spec").filter(|v| !v.is_null()) {
                None => add(&mut errs, "spec", vec!["is required".into()]),
                Some(v) => match Spec::parse(v).and_then(|sp| compile(&ctx, &sp).map(|_| sp)) {
                    Ok(sp) => {
                        if sp.measures.len() != 1 || !sp.group_by.is_empty() || sp.pivot.is_some() || sp.time_series.is_some() {
                            add(
                                &mut errs,
                                "spec",
                                vec!["a number card reduces to one value: exactly one measure, without group_by, pivot or time_series".into()],
                            );
                        }
                        if s(doc, "sparkline").is_some() && !sp.date_range.as_ref().is_some_and(|r| r.from.is_some() && r.to.is_some()) {
                            add(&mut errs, "sparkline", vec!["a sparkline needs the card's spec.date_range with from and to".into()]);
                        }
                        add(&mut errs, "accepts", check_accepts(&ctx, Some(&sp.entity), None, doc.get("accepts").unwrap_or(&Value::Null)));
                    }
                    Err(e) => errs.extend(field_errors("spec", e)),
                },
            },
            DASHBOARD => {
                match parse_layout(doc.get("layout").unwrap_or(&Value::Null)) {
                    Ok(items) => {
                        let mut list = Vec::new();
                        for (n, it) in items.iter().enumerate() {
                            check_block(&ctx, &it.block, &format!("[{n}]"), &mut ids, &mut list);
                        }
                        add(&mut errs, "layout", list);
                    }
                    Err(m) => add(&mut errs, "layout", vec![m]),
                }
                match parse_filters_def(doc.get("filters").unwrap_or(&Value::Null)) {
                    Ok(defs) => {
                        for d in &defs {
                            if let Some(to) = &d.to
                                && ctx.schema.entity(to).is_none()
                            {
                                add(&mut errs, "filters", vec![format!("{}: unknown entity '{to}'", d.name)]);
                            }
                        }
                    }
                    Err(m) => add(&mut errs, "filters", vec![m]),
                }
                if doc.get("refresh_interval").and_then(Value::as_i64).is_some_and(|r| r != 0 && !(30..=86_400).contains(&r)) {
                    add(&mut errs, "refresh_interval", vec!["0 (off) or 30 to 86400 seconds".into()]);
                }
            }
            SPACE => match parse_blocks(doc.get("blocks").unwrap_or(&Value::Null)) {
                Ok(blocks) => {
                    let mut list = Vec::new();
                    for (n, (_, b)) in blocks.iter().enumerate() {
                        check_block(&ctx, b, &format!("[{n}]"), &mut ids, &mut list);
                    }
                    add(&mut errs, "blocks", list);
                }
                Err(m) => add(&mut errs, "blocks", vec![m]),
            },
            _ => {}
        }
        let field = match self.0 {
            DASHBOARD => "layout",
            CHART => "report",
            _ => "blocks",
        };
        for (entity, list) in ids {
            let missing = Refs { hc }.unavailable(entity, &list).await?;
            if !missing.is_empty() {
                let label = ctx.schema.entity(entity).map(|e| e.def.label.clone()).unwrap_or_default();
                errs.push(FieldError::new(field, format!("{label} not found: {}", missing.iter().map(Uuid::to_string).collect::<Vec<_>>().join(", "))));
            }
        }
        if errs.is_empty() { Ok(()) } else { Err(Error::Validation(errs)) }
    }

    /// `run` on a Report (generic action API, MCP `run_action`): its data with runtime settings
    /// (`filters`, `search`, `sort`, `limit`, `from`, `to`), with the caller's permissions.
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if self.0 != REPORT || name != "run" {
            return Ok(None);
        }
        let data = report_data(hc.kernel, hc.ctx, doc, &Runtime::parse(params)?).await?;
        Ok(Some(json!({"report": summary(doc, &["base_entity", "kind", "columns", "chart", "space"]), "data": data})))
    }

    async fn before_delete(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if doc.get("is_standard") == Some(&json!(true)) && !hc.ctx.is_system {
            return Err(Error::Forbidden("standard records come with their package and cannot be deleted".into()));
        }
        let key = match self.0 {
            CHART => "chart",
            CARD => "card",
            REPORT => "report",
            DASHBOARD => "dashboard",
            _ => return Ok(()),
        };
        let Some(id) = id_of(doc) else { return Ok(()) };
        let direct = json!([{key: id.to_string()}]);
        let nested = json!([{"items": [{key: id.to_string()}]}]);
        let user: Option<String> = sqlx::query_scalar(
            "select label from (select d.label, d.layout as j from e_dashboard d where d.tenant_id = $1 \
             union all select w.label, w.blocks from e_space w where w.tenant_id = $1) x where x.j @> $2 or x.j @> $3 limit 1",
        )
        .bind(hc.ctx.tenant)
        .bind(&direct)
        .bind(&nested)
        .fetch_optional(&mut **hc.tx)
        .await?;
        match user {
            Some(label) => Err(Error::Conflict(format!("'{label}' still shows this {}; remove it there first", key.replace('_', " ")))),
            None => Ok(()),
        }
    }
}

/// The columns a compiled query produces (pivot cells are dynamic and not listed).
fn columns_of(plan: &crate::query::Plan, spec: &Spec) -> Vec<Value> {
    let mut cols: Vec<Value> = plan.dims.iter().map(|d| d.column("dimension")).collect();
    cols.extend(plan.raw.iter().map(|d| d.column("field")));
    cols.extend(plan.measures.iter().map(|m| m.column()));
    if spec.compare.is_some() {
        cols.extend(plan.measures.iter().map(|m| json!({"name": format!("previous({})", m.name), "role": "previous"})));
    }
    cols
}

// ───────────────────────────── sharing ─────────────────────────────

/// Whether `ctx` may open a builder record: its creator, the roles or everyone it is shared with,
/// managers (read access to all records of the kind) and administrators.
pub fn visible(ctx: &Ctx, entity: &str, doc: &Doc) -> bool {
    if ctx.is_admin || ctx.is_system || ctx.perms.access(entity, Action::Read) == Access::All {
        return true;
    }
    if doc.get("created_by").and_then(Value::as_str).and_then(|v| v.parse::<Uuid>().ok()) == Some(ctx.user) {
        return true;
    }
    match s(doc, "sharing") {
        Some("everyone") => true,
        Some("roles") => doc
            .get("roles")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|r| r.get("role").and_then(Value::as_str).is_some_and(|role| ctx.roles.iter().any(|x| x == role))),
        _ => false,
    }
}

fn system(ctx: &Ctx) -> Ctx {
    Ctx::system(ctx.tenant, ctx.schema.clone())
}

/// Loads a builder record the viewer may open (anything else is "not found").
pub async fn load(k: &Kernel, ctx: &Ctx, entity: &str, id: Uuid) -> Result<Doc> {
    let doc = load_any(k, ctx, entity, id).await?;
    if visible(ctx, entity, &doc) { Ok(doc) } else { Err(Error::NotFound(entity_label(ctx, entity))) }
}

/// Loads a builder record without the sharing check (widgets of a container the viewer opened).
async fn load_any(k: &Kernel, ctx: &Ctx, entity: &str, id: Uuid) -> Result<Doc> {
    if ctx.schema.entity(entity).is_none() {
        return Err(Error::NotFound(entity.into()));
    }
    k.get(&system(ctx), entity, id, false).await
}

fn entity_label(ctx: &Ctx, entity: &str) -> String {
    ctx.schema.entity(entity).map(|e| e.def.label.clone()).unwrap_or_else(|| entity.into())
}

/// Builder records of one kind the viewer may open, optionally for a module or base entity.
pub async fn list(k: &Kernel, ctx: &Ctx, entity: &str, module: Option<&str>, base: Option<&str>) -> Result<Value> {
    let ent = ctx.schema.entity(entity).cloned().ok_or_else(|| Error::NotFound(entity.into()))?;
    let roles_table = ent.children.get("roles").map(|c| c.table.clone()).ok_or_else(|| Error::internal("roles table missing"))?;
    let extra: &[&str] = match entity {
        REPORT => &["base_entity", "kind", "space"],
        CHART => &["base_entity", "chart_type", "source"],
        CARD => &["base_entity", "format"],
        DASHBOARD => &["refresh_interval"],
        _ => &["icon", "sequence"],
    };
    let mut cols = vec!["a.id".to_string(), "a.updated_at".to_string(), "a.created_by = $2 as mine".to_string()];
    for f in ["label", "module", "sharing", "is_standard", "description"].iter().chain(extra) {
        if let Some(e) = field_expr(&ent, f, "a") {
            cols.push(format!("{e} as {}", q(f)));
        }
    }
    let manager = ctx.is_admin || ctx.is_system || ctx.perms.access(entity, Action::Read) == Access::All;
    let order = if entity == SPACE { "a.\"sequence\" nulls last, a.\"label\"" } else { "a.\"label\"" };
    let sql = format!(
        "select to_jsonb(x) from (select {} from {} a where a.tenant_id = $1 and ($3 or a.created_by = $2 or a.\"sharing\" = 'everyone' \
         or (a.\"sharing\" = 'roles' and exists (select 1 from {} r where r.tenant_id = a.tenant_id and r.parent_id = a.id and r.\"role\" = any($4)))) \
         and ($5::text is null or a.\"module\" = $5) and ($6::text is null or {} = $6) order by {order}, a.id limit 1000) x",
        cols.join(", "),
        q(&ent.table),
        q(&roles_table),
        if ent.field("base_entity").is_some() { "a.\"base_entity\"" } else { "null::text" },
    );
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let rows: Vec<Value> = sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(ctx.tenant)
        .bind(ctx.user)
        .bind(manager)
        .bind(&ctx.roles)
        .bind(module)
        .bind(base)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(json!({"rows": rows}))
}

fn summary(doc: &Doc, keys: &[&str]) -> Value {
    let mut m = Map::new();
    for k in ["id", "label", "description", "module", "sharing", "is_standard"].iter().chain(keys) {
        if let Some(v) = doc.get(*k) {
            m.insert(k.to_string(), v.clone());
        }
    }
    Value::Object(m)
}

// ───────────────────────────── reports ─────────────────────────────

/// Runtime settings for running a saved report: extra filters (AND), search, sort, limit and the
/// period (`from` / `to`, which replace those of its date range or time series).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    #[serde(default)]
    pub filters: Value,
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub sort: Option<Vec<Sort>>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
}

impl Runtime {
    pub fn parse(v: &Value) -> Result<Runtime> {
        match v {
            Value::Null => Ok(Runtime::default()),
            other => serde_json::from_value(other.clone()).map_err(|e| Error::field("body", e.to_string())),
        }
    }

    fn apply(&self, spec: &mut Spec) -> Result<()> {
        spec.and_filters(&self.filters)?;
        if let Some(s) = &self.search {
            spec.search = Some(s.clone());
        }
        if let Some(s) = &self.sort {
            spec.sort = s.clone();
        }
        if let Some(l) = self.limit {
            spec.limit = Some(l);
        }
        if self.from.is_some() || self.to.is_some() {
            match (&mut spec.time_series, &mut spec.date_range) {
                (Some(ts), _) => (ts.from, ts.to) = (self.from.clone().or(ts.from.take()), self.to.clone().or(ts.to.take())),
                (None, Some(dr)) => (dr.from, dr.to) = (self.from.clone().or(dr.from.take()), self.to.clone().or(dr.to.take())),
                (None, None) => return Err(Error::field("from", "this report has no date range or time series to set")),
            }
        }
        Ok(())
    }
}

/// Orders, hides, relabels and annotates (width, format) result columns per the saved settings;
/// columns the settings do not mention keep their place after the listed ones.
fn arrange(data: &mut Value, settings: &Value) {
    let Ok(list) = serde_json::from_value::<Vec<ColumnSetting>>(settings.clone()) else { return };
    if list.is_empty() {
        return;
    }
    let Some(cols) = data.get("columns").and_then(Value::as_array).cloned() else { return };
    let mut out = Vec::new();
    for c in &list {
        if c.hidden {
            continue;
        }
        if let Some(mut col) = cols.iter().find(|x| x["name"] == json!(c.name)).cloned() {
            if let Some(l) = c.label.as_deref().filter(|l| !l.trim().is_empty()) {
                col["label"] = json!(l);
            }
            if let Some(w) = c.width {
                col["width"] = json!(w);
            }
            if let Some(f) = &c.format {
                col["format"] = json!(f);
            }
            out.push(col);
        }
    }
    for col in cols {
        if !list.iter().any(|c| col["name"] == json!(c.name)) {
            out.push(col);
        }
    }
    data["columns"] = Value::Array(out);
}

pub async fn run_report(k: &Kernel, ctx: &Ctx, id: Uuid, rt: &Runtime) -> Result<Value> {
    let doc = load(k, ctx, REPORT, id).await?;
    let data = report_data(k, ctx, &doc, rt).await?;
    Ok(json!({"report": summary(&doc, &["base_entity", "kind", "columns", "chart", "space"]), "data": data}))
}

async fn report_data(k: &Kernel, ctx: &Ctx, doc: &Doc, rt: &Runtime) -> Result<Value> {
    let mut spec = Spec::parse(doc.get("spec").unwrap_or(&Value::Null))?;
    rt.apply(&mut spec)?;
    let mut data = run(k, ctx, &spec).await?;
    arrange(&mut data, doc.get("columns").unwrap_or(&Value::Null));
    Ok(data)
}

/// A saved report as CSV (export permission on its entity, like list exports).
/// Every row of a saved report (up to the export limit) for a download → its label and the result.
pub async fn export_report(k: &Kernel, ctx: &Ctx, id: Uuid, rt: &Runtime) -> Result<(String, Value)> {
    let doc = load(k, ctx, REPORT, id).await?;
    if let Some(e) = s(&doc, "base_entity") {
        ctx.require(e, Action::Export)?;
    }
    let mut rt = rt.clone();
    rt.limit = rt.limit.or(Some(crate::spec::MAX_LIMIT));
    let data = report_data(k, ctx, &doc, &rt).await?;
    Ok((s(&doc, "label").unwrap_or("report").to_string(), data))
}

// ───────────────────────────── dashboard filters ─────────────────────────────

fn present(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::String(s) => !s.trim().is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => o.values().any(present),
        _ => true,
    }
}

/// The effective filter values: defaults, overridden by what the viewer chose.
pub fn filter_values(defs: &[FilterDef], given: &Map<String, Value>) -> Result<Map<String, Value>> {
    for k in given.keys() {
        if !defs.iter().any(|d| &d.name == k) {
            return Err(Error::field(
                "filters",
                format!("unknown dashboard filter '{k}' (filters: {})", defs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ")),
            ));
        }
    }
    let mut out = Map::new();
    for d in defs {
        let v = given.get(&d.name).cloned().filter(present).or_else(|| d.default.clone().filter(present));
        if let Some(v) = v {
            if d.kind == FilterType::DateRange && !v.is_object() {
                return Err(Error::field("filters", format!("{} is a date range: send {{\"from\": ..., \"to\": ...}}", d.name)));
            }
            out.insert(d.name.clone(), v);
        }
    }
    Ok(out)
}

fn range_of(v: &Value) -> (Option<String>, Option<String>) {
    let get = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_string);
    (get("from"), get("to"))
}

/// Applies dashboard filters to a query widget: through its `accepts` map, else onto its own
/// date field (date ranges) or a field of the same name (anything else) when it has one.
fn apply_to_spec(ctx: &Ctx, spec: &mut Spec, accepts: &Map<String, Value>, defs: &[FilterDef], values: &Map<String, Value>) -> Result<()> {
    let resolver = Resolver::new(ctx, &spec.entity, "entity")?;
    let mut extra = Vec::new();
    for d in defs {
        let Some(v) = values.get(&d.name) else { continue };
        let target = match accepts.get(&d.name) {
            Some(Value::Bool(false)) => continue,
            Some(Value::String(p)) => Some(p.clone()),
            _ if d.kind == FilterType::DateRange => {
                spec.time_series.as_ref().map(|t| t.field.clone()).or_else(|| spec.date_range.as_ref().map(|r| r.field.clone()))
            }
            _ => {
                let p = d.field.clone().unwrap_or_else(|| d.name.clone());
                resolver.resolve(&p, "").is_ok().then_some(p)
            }
        };
        let Some(target) = target else { continue };
        if d.kind == FilterType::DateRange {
            let (from, to) = range_of(v);
            match (&mut spec.time_series, &mut spec.date_range) {
                (Some(ts), _) if ts.field == target => (ts.from, ts.to) = (from, to),
                (_, Some(dr)) if dr.field == target => (dr.from, dr.to) = (from, to),
                (None, None) => spec.date_range = Some(DateRange { field: target, from, to }),
                _ => {
                    let (today, fy) = (ctx.today(), ctx.fy_start_month);
                    for (op, d) in [(">=", from), ("<=", to)] {
                        if let Some(d) = d {
                            let day = resolve_date(&d, today, fy).map_err(|m| Error::field("filters", format!("{}: {m}", d)))?;
                            extra.push(json!([target, op, day.to_string()]));
                        }
                    }
                }
            }
        } else {
            extra.push(json!([target, if v.is_array() { "in" } else { "=" }, v]));
        }
    }
    spec.and_filters(&Value::Array(extra))
}

/// Dashboard filters as built-in report parameters.
fn apply_to_params(
    params: &mut std::collections::HashMap<String, String>,
    declared: &[String],
    accepts: &Map<String, Value>,
    defs: &[FilterDef],
    values: &Map<String, Value>,
) {
    for d in defs {
        let Some(v) = values.get(&d.name) else { continue };
        let target = match accepts.get(&d.name) {
            Some(Value::Bool(false)) => continue,
            Some(Value::String(p)) => p.clone(),
            _ => d.name.clone(),
        };
        if d.kind == FilterType::DateRange {
            let (from, to) = range_of(v);
            if declared.iter().any(|p| p == "from_date") {
                params.extend(from.map(|f| ("from_date".to_string(), f)));
                params.extend(to.map(|t| ("to_date".to_string(), t)));
            } else if declared.iter().any(|p| p == "as_of") {
                params.extend(to.map(|t| ("as_of".to_string(), t)));
            }
        } else if declared.contains(&target)
            && let Some(text) = v.as_str().map(str::to_string).or_else(|| v.as_bool().map(|b| b.to_string()))
        {
            params.insert(target, text);
        }
    }
}

// ───────────────────────────── charts and cards ─────────────────────────────

fn accepts_of(doc: &Doc) -> Map<String, Value> {
    doc.get("accepts").and_then(Value::as_object).cloned().unwrap_or_default()
}

/// A chart's data. `saved`: a stored chart, which lends its source report to its viewers (its
/// creator could open it when saving); an unsaved preview may only use reports the caller may open.
pub async fn chart_data(k: &Kernel, ctx: &Ctx, doc: &Doc, saved: bool, rt: &Runtime, defs: &[FilterDef], values: &Map<String, Value>) -> Result<Value> {
    let accepts = accepts_of(doc);
    let mut drill = Value::Null;
    let data = match s(doc, "source").unwrap_or("query") {
        "builtin" => {
            let name = s(doc, "builtin_report").unwrap_or_default().to_string();
            let mut params: std::collections::HashMap<String, String> = doc
                .get("builtin_params")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect();
            let catalog = ferrum_domain::reports::catalog();
            let declared: Vec<String> = catalog
                .as_array()
                .and_then(|c| c.iter().find(|r| r["name"] == json!(name)))
                .map(|r| r["params"].as_array().into_iter().flatten().filter_map(|p| p["name"].as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            apply_to_params(&mut params, &declared, &accepts, defs, values);
            if declared.iter().any(|p| p == "from_date") {
                params.extend(rt.from.clone().map(|f| ("from_date".to_string(), f)));
                params.extend(rt.to.clone().map(|t| ("to_date".to_string(), t)));
            }
            ferrum_domain::reports::run(k, ctx, &name, &params).await?
        }
        source => {
            let (mut spec, columns) = if source == "report" {
                let rid = doc.get("report").and_then(Value::as_str).and_then(|v| v.parse().ok()).ok_or_else(|| Error::NotFound("Report".into()))?;
                let report = if saved { load_any(k, ctx, REPORT, rid).await? } else { load(k, ctx, REPORT, rid).await? };
                (Spec::parse(report.get("spec").unwrap_or(&Value::Null))?, report.get("columns").cloned().unwrap_or(Value::Null))
            } else {
                (Spec::parse(doc.get("spec").unwrap_or(&Value::Null))?, Value::Null)
            };
            rt.apply(&mut spec)?;
            apply_to_spec(ctx, &mut spec, &accepts, defs, values)?;
            let mut data = run(k, ctx, &spec).await?;
            arrange(&mut data, &columns);
            drill = drill_down(ctx, &spec, &data);
            data
        }
    };
    let cols: Vec<&Value> = data["columns"].as_array().map(|c| c.iter().collect()).unwrap_or_default();
    let x = s(doc, "x_field")
        .map(str::to_string)
        .or_else(|| cols.iter().find(|c| c["role"] == "dimension").or(cols.first()).and_then(|c| c["name"].as_str().map(str::to_string)));
    let y: Vec<String> = match doc.get("y_fields").and_then(Value::as_array) {
        Some(list) if !list.is_empty() => list.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        _ => cols
            .iter()
            .filter(|c| matches!(c["role"].as_str(), Some("measure" | "pivot" | "previous")) || (c.get("role").is_none() && c["name"].as_str() != x.as_deref()))
            .filter_map(|c| c["name"].as_str().map(str::to_string))
            .collect(),
    };
    let mut chart = summary(doc, &["chart_type", "source", "base_entity", "colors", "builtin_report"]);
    chart["x"] = json!(x);
    chart["y"] = json!(y);
    // A point drills into the list: these filters plus the point's dimension values.
    Ok(json!({"chart": chart, "data": data, "drill_down": drill}))
}

/// `value` with `previous`, `delta`, `delta_percent` and `direction` when compared.
pub async fn card_data(k: &Kernel, ctx: &Ctx, doc: &Doc, rt: &Runtime, defs: &[FilterDef], values: &Map<String, Value>) -> Result<Value> {
    let mut spec = Spec::parse(doc.get("spec").unwrap_or(&Value::Null))?;
    rt.apply(&mut spec)?;
    apply_to_spec(ctx, &mut spec, &accepts_of(doc), defs, values)?;
    let data = run(k, ctx, &spec).await?;
    let measure =
        data["columns"].as_array().and_then(|c| c.iter().find(|x| x["role"] == "measure")).and_then(|c| c["name"].as_str()).unwrap_or("count").to_string();
    let value = data["totals"][&measure].clone();
    let mut card = summary(doc, &["base_entity", "format", "precision", "color"]);
    if let Some(cur) = doc.get("currency").and_then(Value::as_str).and_then(|v| v.parse::<Uuid>().ok()) {
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let codes = title_map(&mut tx, ctx, "Currency", &[cur]).await?;
        tx.commit().await?;
        card["currency"] = json!(codes.get(&cur));
    }
    let mut out = json!({"card": card, "value": value, "drill_down": drill_down(ctx, &spec, &data), "meta": data["meta"]});
    // The same measure over the card's period, bucketed: a trend line under the number.
    if let (Some(bucket), Some(range)) = (doc.get("sparkline").and_then(|v| serde_json::from_value::<Bucket>(v.clone()).ok()), spec.date_range.clone()) {
        let series = Spec {
            date_range: None,
            compare: None,
            time_series: Some(TimeSeries { field: range.field, bucket, from: range.from, to: range.to, fill_zero: true }),
            ..spec.clone()
        };
        // A trend that cannot be drawn (too many buckets) does not cost the card its number.
        match run(k, ctx, &series).await {
            Ok(trend) => {
                let x = trend["columns"][0]["name"].as_str().unwrap_or_default().to_string();
                out["sparkline"] = Value::Array(trend["rows"].as_array().into_iter().flatten().map(|r| json!([r[&x], r[&measure]])).collect());
            }
            Err(e) => out["sparkline_error"] = error_json(&e),
        }
    }
    let prev_key = format!("previous({measure})");
    if let Some(prev) = data["totals"].get(&prev_key) {
        let num = |v: &Value| v.as_str().and_then(|s| Decimal::from_str(s).ok()).or_else(|| v.as_i64().map(Decimal::from));
        out["previous"] = prev.clone();
        if let (Some(cur), Some(p)) = (num(&value).or(Some(Decimal::ZERO)), num(prev).or(Some(Decimal::ZERO))) {
            let delta = cur - p;
            out["delta"] = json!(delta.normalize().to_string());
            out["delta_percent"] = if p.is_zero() { Value::Null } else { json!((delta * Decimal::ONE_HUNDRED / p.abs()).round_dp(2).normalize().to_string()) };
            out["direction"] = json!(match delta.cmp(&Decimal::ZERO) {
                std::cmp::Ordering::Greater => "up",
                std::cmp::Ordering::Less => "down",
                std::cmp::Ordering::Equal => "flat",
            });
        }
    }
    Ok(out)
}

/// Filters that open the list view on exactly what a card counts (paths the list API supports;
/// date ranges as local-day bounds).
fn drill_down(ctx: &Ctx, spec: &Spec, data: &Value) -> Value {
    let mut filters: Vec<Value> =
        filter_list(&spec.filters, "filters").unwrap_or_default().into_iter().filter(|f| f[0].as_str().is_some_and(|p| p.split('.').count() <= 2)).collect();
    let range = &data["meta"]["date_range"];
    if let Some(field) = range["field"].as_str().filter(|f| f.split('.').count() <= 2) {
        let day = |k: &str| range[k].as_str().and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
        let datetime = Resolver::new(ctx, &spec.entity, "entity").and_then(|r| r.resolve(field, "")).is_ok_and(|r| r.ty.is_datetime());
        let tz: chrono_tz::Tz = ctx.schema.timezone.parse().unwrap_or(chrono_tz::UTC);
        let local = |d: NaiveDate| {
            d.and_hms_opt(0, 0, 0).and_then(|t| tz.from_local_datetime(&t).earliest()).map(|t| t.to_utc().to_rfc3339_opts(SecondsFormat::Millis, true))
        };
        match (day("from"), day("to"), datetime) {
            (from, to, true) => {
                filters.extend(from.and_then(local).map(|t| json!([field, ">=", t])));
                filters.extend(to.and_then(|t| t.succ_opt()).and_then(local).map(|t| json!([field, "<", t])));
            }
            (Some(f), Some(t), false) => filters.push(json!([field, "between", [f.to_string(), t.to_string()]])),
            (Some(f), None, false) => filters.push(json!([field, ">=", f.to_string()])),
            (None, Some(t), false) => filters.push(json!([field, "<=", t.to_string()])),
            _ => {}
        }
    }
    json!({"entity": spec.entity, "filters": filters})
}

// ───────────────────────────── dashboards and spaces ─────────────────────────────

/// Resolves one widget for the viewer: `None` when they cannot read it (its entity, a field it
/// uses, a report or dashboard they may not open), an `error` entry when it is broken.
async fn resolve_block(k: &Kernel, ctx: &Ctx, block: &Block, defs: &[FilterDef], values: &Map<String, Value>) -> Result<Option<Value>> {
    let readable = |e: &str| ctx.schema.entity(e).is_some() && ctx.perms.access(e, Action::Read) != Access::None;
    let hidden = |e: &Error| matches!(e, Error::Forbidden(_) | Error::NotFound(_));
    let opened = |entity: &'static str, id: Uuid| async move { load(k, ctx, entity, id).await.ok().and_then(|d| s(&d, "label").map(str::to_string)) };
    let count_of = |entity: String, filters: Value| async move {
        let spec = Spec { entity, filters, measures: vec![count()], ..Default::default() };
        run(k, ctx, &spec).await.map(|d| d["totals"]["count"].clone())
    };
    Ok(match block {
        Block::Shortcut { label, entity, report, dashboard, url, filters, show_count, color } => {
            let mut out = json!({"type": "shortcut", "label": label, "color": color});
            if let Some(e) = entity {
                if !readable(e) {
                    return Ok(None);
                }
                out["entity"] = json!(e);
                out["filters"] = filters.clone();
                if *show_count {
                    match count_of(e.clone(), filters.clone()).await {
                        Ok(n) => out["count"] = n,
                        Err(e) if hidden(&e) => return Ok(None),
                        Err(e) => out["error"] = error_json(&e),
                    }
                }
            } else if let Some(id) = report {
                let Some(title) = opened(REPORT, *id).await else { return Ok(None) };
                out["report"] = json!(id);
                out["title"] = json!(title);
            } else if let Some(id) = dashboard {
                let Some(title) = opened(DASHBOARD, *id).await else { return Ok(None) };
                out["dashboard"] = json!(id);
                out["title"] = json!(title);
            } else {
                out["url"] = json!(url);
            }
            Some(out)
        }
        Block::Links { label, items } => {
            let mut kept = Vec::new();
            for it in items {
                let ok = match (&it.entity, it.report, it.dashboard) {
                    (Some(e), _, _) => readable(e),
                    (_, Some(id), _) => opened(REPORT, id).await.is_some(),
                    (_, _, Some(id)) => opened(DASHBOARD, id).await.is_some(),
                    _ => true,
                };
                if ok {
                    kept.push(json!(it));
                }
            }
            (!kept.is_empty()).then(|| json!({"type": "links", "label": label, "items": kept}))
        }
        Block::Chart { chart } => {
            let doc = match load_any(k, ctx, CHART, *chart).await {
                Ok(d) => d,
                Err(e) if hidden(&e) => return Ok(None),
                Err(e) => return Err(e),
            };
            match chart_data(k, ctx, &doc, true, &Runtime::default(), defs, values).await {
                Ok(d) => Some(json!({"type": "chart", "chart": d["chart"], "data": d["data"]})),
                Err(e) if hidden(&e) => None,
                Err(e) => Some(json!({"type": "chart", "chart": summary(&doc, &["chart_type"]), "error": error_json(&e)})),
            }
        }
        Block::Card { card } => {
            let doc = match load_any(k, ctx, CARD, *card).await {
                Ok(d) => d,
                Err(e) if hidden(&e) => return Ok(None),
                Err(e) => return Err(e),
            };
            match card_data(k, ctx, &doc, &Runtime::default(), defs, values).await {
                Ok(mut d) => {
                    d["type"] = json!("card");
                    Some(d)
                }
                Err(e) if hidden(&e) => None,
                Err(e) => Some(json!({"type": "card", "card": summary(&doc, &["format"]), "error": error_json(&e)})),
            }
        }
        Block::List { label, entity, filters, fields, sort, limit } => {
            if !readable(entity) {
                return Ok(None);
            }
            // The list API does not check filter fields against field groups; the compiler does.
            let check = Spec { entity: entity.clone(), filters: filters.clone(), measures: vec![count()], ..Default::default() };
            if let Err(e) = compile(ctx, &check) {
                return Ok(if hidden(&e) { None } else { Some(json!({"type": "list", "label": label, "entity": entity, "error": error_json(&e)})) });
            }
            let q = ListQuery {
                filters: ferrum_kernel::query::parse_filters(filters)?,
                fields: (!fields.is_empty()).then(|| fields.clone()),
                sort: sort.clone(),
                limit: Some(limit.unwrap_or(5)),
                with_titles: true,
                ..Default::default()
            };
            match k.list(ctx, entity, &q).await {
                Ok(page) => Some(json!({"type": "list", "label": label, "entity": entity, "fields": fields, "rows": page.rows})),
                Err(e) if hidden(&e) => None,
                Err(e) => Some(json!({"type": "list", "label": label, "entity": entity, "error": error_json(&e)})),
            }
        }
        Block::Markdown { content } => Some(json!({"type": "markdown", "content": content})),
        Block::Heading { text, level } => Some(json!({"type": "heading", "text": text, "level": level.unwrap_or(2)})),
        Block::Onboarding { title, steps } => {
            let mut out = Vec::new();
            for st in steps {
                let mut step = json!({"label": st.label, "description": st.description, "url": st.url});
                if let Some(e) = &st.entity {
                    if !readable(e) {
                        continue;
                    }
                    step["entity"] = json!(e);
                    match count_of(e.clone(), st.filters.clone()).await {
                        Ok(n) => step["done"] = json!(n.as_i64().unwrap_or(0) > 0),
                        Err(e) if hidden(&e) => continue,
                        Err(e) => step["error"] = error_json(&e),
                    }
                }
                out.push(step);
            }
            (!out.is_empty()).then(|| json!({"type": "onboarding", "title": title, "steps": out}))
        }
    })
}

fn error_json(e: &Error) -> Value {
    let mut v = json!({"error": e.code(), "message": e.to_string()});
    if let Error::Validation(fields) = e {
        v["fields"] = json!(fields);
    }
    v
}

pub async fn dashboard_data(k: &Kernel, ctx: &Ctx, id: Uuid, given: &Map<String, Value>) -> Result<Value> {
    let doc = load(k, ctx, DASHBOARD, id).await?;
    let defs = parse_filters_def(doc.get("filters").unwrap_or(&Value::Null)).map_err(|m| Error::field("filters", m))?;
    let values = filter_values(&defs, given)?;
    let items = parse_layout(doc.get("layout").unwrap_or(&Value::Null)).map_err(|m| Error::field("layout", m))?;
    let pending: Vec<_> = items.iter().map(|it| resolve_block(k, ctx, &it.block, &defs, &values)).collect();
    let resolved: Vec<Result<Option<Value>>> = futures::stream::iter(pending).buffered(PARALLEL).collect().await;
    let mut out = Vec::new();
    for (it, r) in items.iter().zip(resolved) {
        let data = match r {
            Ok(Some(d)) => d,
            Ok(None) => continue,
            Err(e) => json!({"error": error_json(&e)}),
        };
        out.push(json!({"i": it.i, "x": it.x, "y": it.y, "w": it.w, "h": it.h, "widget": data}));
    }
    let mut dashboard = summary(&doc, &["refresh_interval"]);
    dashboard["filters"] = json!(defs);
    Ok(json!({"dashboard": dashboard, "filters": values, "items": out}))
}

pub async fn space_data(k: &Kernel, ctx: &Ctx, id: Uuid) -> Result<Value> {
    let doc = load(k, ctx, SPACE, id).await?;
    let blocks = parse_blocks(doc.get("blocks").unwrap_or(&Value::Null)).map_err(|m| Error::field("blocks", m))?;
    let none = Map::new();
    let pending: Vec<_> = blocks.iter().map(|(_, b)| resolve_block(k, ctx, b, &[], &none)).collect();
    let resolved: Vec<Result<Option<Value>>> = futures::stream::iter(pending).buffered(PARALLEL).collect().await;
    let mut out = Vec::new();
    for ((width, _), r) in blocks.iter().zip(resolved) {
        let mut data = match r {
            Ok(Some(d)) => d,
            Ok(None) => continue,
            Err(e) => json!({"error": error_json(&e)}),
        };
        if let Some(w) = width {
            data["width"] = json!(w);
        }
        out.push(data);
    }
    Ok(json!({"space": summary(&doc, &["icon", "sequence"]), "blocks": out}))
}

/// A request for one chart's or card's data: runtime settings (`filters`, `search`, `sort`,
/// `limit`, `from`, `to`), optionally the `dashboard` it sits on with that dashboard's filter
/// `values`, or — for the builder's live preview — an unsaved `record` with `filter_defs`.
pub struct WidgetRequest {
    pub rt: Runtime,
    pub dashboard: Option<Uuid>,
    pub values: Map<String, Value>,
    pub record: Option<Doc>,
    pub filter_defs: Value,
}

impl WidgetRequest {
    pub fn parse(v: &Value) -> Result<WidgetRequest> {
        let mut m = match v {
            Value::Null => Map::new(),
            Value::Object(m) => m.clone(),
            _ => return Err(Error::field("body", "must be an object")),
        };
        let dashboard = match m.remove("dashboard") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_str().and_then(|s| s.parse().ok()).ok_or_else(|| Error::field("dashboard", "must be a dashboard id"))?),
        };
        let values = match m.remove("values") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(o)) => o,
            Some(_) => return Err(Error::field("values", "must be an object of dashboard filter values")),
        };
        let record = match m.remove("record") {
            None | Some(Value::Null) => None,
            Some(Value::Object(o)) => Some(o),
            Some(_) => return Err(Error::field("record", "must be the unsaved record")),
        };
        let filter_defs = m.remove("filter_defs").unwrap_or(Value::Null);
        Ok(WidgetRequest { rt: Runtime::parse(&Value::Object(m))?, dashboard, values, record, filter_defs })
    }
}

/// Data of a chart or card (`entity`): a saved one by `id`, or the request's unsaved `record`.
pub async fn widget_data(k: &Kernel, ctx: &Ctx, entity: &'static str, id: Option<Uuid>, req: &WidgetRequest) -> Result<Value> {
    let (doc, defs) = match (id, req.dashboard) {
        (Some(id), Some(did)) => {
            // Shown on a dashboard the viewer opened: the dashboard lends its sharing.
            let dash = load(k, ctx, DASHBOARD, did).await?;
            let key = if entity == CHART { "chart" } else { "card" };
            let placed = dash
                .get("layout")
                .and_then(Value::as_array)
                .is_some_and(|l| l.iter().any(|it| it.get(key).and_then(Value::as_str) == Some(id.to_string().as_str())));
            if !placed {
                return Err(Error::NotFound(entity_label(ctx, entity)));
            }
            let defs = parse_filters_def(dash.get("filters").unwrap_or(&Value::Null)).map_err(|m| Error::field("filters", m))?;
            (load_any(k, ctx, entity, id).await?, defs)
        }
        (Some(id), None) => (load(k, ctx, entity, id).await?, parse_filters_def(&req.filter_defs).map_err(|m| Error::field("filter_defs", m))?),
        (None, _) => (
            req.record.clone().ok_or_else(|| Error::field("record", "send the unsaved record as {\"record\": {...}}"))?,
            parse_filters_def(&req.filter_defs).map_err(|m| Error::field("filter_defs", m))?,
        ),
    };
    let values = filter_values(&defs, &req.values)?;
    if entity == CHART { chart_data(k, ctx, &doc, id.is_some(), &req.rt, &defs, &values).await } else { card_data(k, ctx, &doc, &req.rt, &defs, &values).await }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_fit_the_grid_without_overlaps() {
        let id = Uuid::now_v7().to_string();
        let ok = parse_layout(&json!([
            {"i": "a", "type": "chart", "chart": id, "x": 0, "y": 0, "w": 6, "h": 4},
            {"type": "markdown", "content": "hi", "x": 6, "y": 0, "w": 6, "h": 2},
        ]))
        .unwrap();
        assert_eq!((ok[1].i.as_str(), ok[1].w), ("w1", 6));
        let err = |v: Value| parse_layout(&v).unwrap_err();
        assert!(err(json!([{"type": "markdown", "content": "x", "x": 8, "y": 0, "w": 6, "h": 2}])).contains("overflows"));
        assert!(err(json!([{"type": "markdown", "content": "x", "x": 0, "y": 0, "w": 6, "h": 2}, {"type": "markdown", "content": "y", "x": 3, "y": 1, "w": 6, "h": 2}])).contains("overlaps"));
        assert!(err(json!([{"type": "markdown", "content": "x", "x": 0, "y": 0, "w": 0, "h": 2}])).contains("w must be"));
        assert!(err(json!([{"type": "markdown", "content": "x", "x": 0, "y": 0, "w": 2}])).contains("h is required"));
        assert!(err(json!([{"type": "pie", "x": 0, "y": 0, "w": 2, "h": 2}])).contains("unknown variant `pie`"));
        assert!(err(json!([{"type": "chart", "chart": id, "colour": "red", "x": 0, "y": 0, "w": 2, "h": 2}])).contains("unknown field `colour`"));
        assert!(err(json!([{"type": "links", "label": "x", "items": [], "x": 0, "y": 0, "w": 2, "h": 2}])).contains("dashboards hold"));
        assert!(err(json!([{"i": "a", "type": "markdown", "content": "x", "x": 0, "y": 0, "w": 2, "h": 2}, {"i": "a", "type": "markdown", "content": "x", "x": 4, "y": 0, "w": 2, "h": 2}])).contains("used twice"));
    }

    #[test]
    fn blocks_and_filters_are_strict() {
        let b = parse_blocks(&json!([{"type": "markdown", "content": "x", "width": 6}, {"type": "shortcut", "label": "Invoices", "entity": "SalesInvoice"}]))
            .unwrap();
        assert_eq!(b[0].0, Some(6));
        assert!(parse_blocks(&json!([{"type": "markdown", "content": "x", "width": 13}])).is_err());
        assert!(parse_filters_def(&json!([{"name": "company", "type": "link"}])).unwrap_err().contains("'to'"));
        assert!(parse_filters_def(&json!([{"name": "Company", "type": "data"}])).unwrap_err().contains("identifier"));
        let defs = parse_filters_def(&json!([{"name": "company", "type": "link", "to": "Company"}, {"name": "period", "type": "date_range", "default": {"from": "start_of_year", "to": "today"}}])).unwrap();
        let v = filter_values(&defs, &json!({"company": "c1"}).as_object().cloned().unwrap()).unwrap();
        assert_eq!(v["period"]["from"], json!("start_of_year"));
        assert!(filter_values(&defs, &json!({"nope": 1}).as_object().cloned().unwrap()).is_err());
        assert!(filter_values(&defs, &json!({"period": "2026"}).as_object().cloned().unwrap()).is_err());
    }

    #[test]
    fn dashboard_filters_reach_widgets() {
        let ctx = crate::query::tests::sales(crate::query::tests::schema());
        let defs = parse_filters_def(&json!([{"name": "company", "type": "link", "to": "Company"}, {"name": "period", "type": "date_range"},
            {"name": "customer_group", "type": "link", "to": "CustomerGroup", "field": "customer.customer_group"}, {"name": "warehouse", "type": "link", "to": "Warehouse"}]))
        .unwrap();
        let values = json!({"company": "c1", "period": {"from": "2026-09-01", "to": "2026-09-30"}, "customer_group": ["g1", "g2"], "warehouse": "w1"})
            .as_object()
            .cloned()
            .unwrap();
        let mut spec =
            Spec::parse(&json!({"entity": "SalesInvoice", "measures": [{"fn": "count"}], "date_range": {"field": "posting_date", "from": "start_of_year"}}))
                .unwrap();
        apply_to_spec(&ctx, &mut spec, &Map::new(), &defs, &values).unwrap();
        assert_eq!(spec.date_range.as_ref().unwrap().from.as_deref(), Some("2026-09-01"), "date ranges land on the widget's own date field");
        assert_eq!(
            spec.filters,
            json!([["company", "=", "c1"], ["customer.customer_group", "in", ["g1", "g2"]]]),
            "SalesInvoice has no warehouse: not applied"
        );
        let mut mapped = Spec::parse(&json!({"entity": "SalesInvoice", "measures": [{"fn": "count"}]})).unwrap();
        let accepts = json!({"company": false, "period": "due_date"}).as_object().cloned().unwrap();
        apply_to_spec(&ctx, &mut mapped, &accepts, &defs, &values).unwrap();
        assert_eq!(mapped.date_range.as_ref().unwrap().field, "due_date");
        assert!(!mapped.filters.to_string().contains("c1"), "opted out of company");
        let mut params = std::collections::HashMap::new();
        apply_to_params(&mut params, &["company".into(), "from_date".into(), "to_date".into()], &Map::new(), &defs, &values);
        assert_eq!((params["company"].as_str(), params["from_date"].as_str(), params["to_date"].as_str()), ("c1", "2026-09-01", "2026-09-30"));
    }

    #[test]
    fn charts_fit_their_query() {
        let cols = json!([{"name": "customer", "role": "dimension", "type": "link"}, {"name": "count", "role": "measure"}, {"name": "sum(grand_total)", "role": "measure"}]);
        assert!(check_chart(ChartType::Bar, Some("customer"), &["count".into()], &["#336699".into(), "teal".into()], Some(&cols)).is_empty());
        assert!(!check_chart(ChartType::Pie, None, &[], &[], Some(&cols)).is_empty(), "pie with two measures");
        assert!(check_chart(ChartType::Pie, None, &["count".into()], &[], Some(&cols)).is_empty());
        assert!(!check_chart(ChartType::Number, None, &["count".into()], &[], Some(&cols)).is_empty(), "number with a dimension");
        assert!(!check_chart(ChartType::Heatmap, None, &[], &[], Some(&cols)).is_empty());
        assert_eq!(check_chart(ChartType::Bar, Some("nope"), &["also_nope".into()], &["javascript:x".into()], Some(&cols)).len(), 3, "x, y and colors");
        assert!(check_chart(ChartType::Line, None, &[], &[], None).is_empty(), "built-in sources are checked when run");
    }

    #[test]
    fn visibility_follows_sharing() {
        let s = crate::query::tests::schema();
        let mut ctx = crate::query::tests::user(s, &["SalesInvoice"], &[], &[]);
        ctx.roles = vec!["Sales User".into()];
        let doc = |v: Value| v.as_object().cloned().unwrap();
        let other = Uuid::now_v7().to_string();
        assert!(visible(&ctx, REPORT, &doc(json!({"created_by": ctx.user.to_string(), "sharing": "private"}))));
        assert!(!visible(&ctx, REPORT, &doc(json!({"created_by": other, "sharing": "private"}))));
        assert!(visible(&ctx, REPORT, &doc(json!({"created_by": other, "sharing": "everyone"}))));
        assert!(visible(&ctx, REPORT, &doc(json!({"created_by": other, "sharing": "roles", "roles": [{"role": "Sales User"}]}))));
        assert!(!visible(&ctx, REPORT, &doc(json!({"created_by": other, "sharing": "roles", "roles": [{"role": "Accounts Manager"}]}))));
    }
}
