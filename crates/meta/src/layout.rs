//! Print layouts (docs/builder/decisions.md BD-22; the contract is `tools/PRINT-FORMATS-SPEC.md` §4): a format built
//! from sections, columns and blocks that the kernel's one renderer (`ferrum_kernel::layout`) turns into HTML. Nothing
//! in a layout is markup, CSS or a URL: every string is text, every number is checked to a range, every colour is
//! `#rrggbb`. Here: the types (unknown keys refused) and the checks that need no schema; the kernel checks the paths
//! against the entity's fields.

use crate::expr::Expr;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

/// The largest layout (its JSON), as a template body's.
pub const MAX_LAYOUT_BYTES: usize = 256 * 1024;
pub const MAX_SECTIONS: usize = 100;
pub const MAX_BLOCKS: usize = 500;
pub const MAX_COLUMN_BLOCKS: usize = 50;
pub const MAX_CONDITIONS: usize = 100;
pub const MAX_CEL: usize = 500;
pub const MAX_IMAGES: usize = 20;
pub const MAX_TEXT: usize = 10_000;

static ID: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^[a-z][a-z0-9_-]{0,39}$").unwrap());
static HEX: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^#[0-9a-fA-F]{6}$").unwrap());
static FILE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"^/files/[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$").unwrap());
/// `{doc.number}` or `{doc.grand_total|words}` in a heading, a total's label, a signature's label, a watermark.
pub static TOKEN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\{((?:doc|links|company|settings|print)\.[A-Za-z0-9_.]+)(?:\|([a-z]+))?\}").unwrap());
static PATH: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^(doc|links|company|settings|print|row)(\.[A-Za-z0-9_]+){1,3}$").unwrap());

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Font {
    Sans,
    Serif,
    Mono,
}

impl Font {
    pub fn name(self) -> &'static str {
        match self {
            Font::Sans => "sans",
            Font::Serif => "serif",
            Font::Mono => "mono",
        }
    }
    pub fn parse(s: &str) -> Option<Font> {
        serde_json::from_value(Value::String(s.into())).ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Size {
    #[serde(rename = "xs")]
    Xs,
    #[serde(rename = "sm")]
    Sm,
    #[serde(rename = "md")]
    Md,
    #[serde(rename = "lg")]
    Lg,
    #[serde(rename = "xl")]
    Xl,
    #[serde(rename = "2xl")]
    Xxl,
}

impl Size {
    /// Its class suffix (`pf-s-md`).
    pub fn name(self) -> &'static str {
        match self {
            Size::Xs => "xs",
            Size::Sm => "sm",
            Size::Md => "md",
            Size::Lg => "lg",
            Size::Xl => "xl",
            Size::Xxl => "2xl",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    Start,
    Center,
    End,
}

impl Align {
    pub fn name(self) -> &'static str {
        match self {
            Align::Start => "start",
            Align::Center => "center",
            Align::End => "end",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Weight {
    Normal,
    Bold,
}

/// One path, or up to four of which the first that is not empty prints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Paths {
    One(String),
    Many(Vec<String>),
}

impl Paths {
    pub fn list(&self) -> &[String] {
        match self {
            Paths::One(p) => std::slice::from_ref(p),
            Paths::Many(v) => v,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum As {
    Auto,
    Text,
    Multiline,
    Number,
    Amount,
    Percent,
    Date,
    Datetime,
    Words,
    Check,
}

impl As {
    pub fn parse(s: &str) -> Option<As> {
        serde_json::from_value(Value::String(s.into())).ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShowCurrency {
    Symbol,
    Code,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DateStyle {
    Short,
    /// Day, a month's name and year (`29 Oct 2026`): the default.
    Medium,
    Long,
}

/// How a value prints (`as: auto`: by the field's type).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Format {
    #[serde(rename = "as", default, skip_serializing_if = "Option::is_none")]
    pub as_: Option<As>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show: Option<ShowCurrency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<DateStyle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// `abs`: a number, amount or amount in words without its sign (a credit note's totals print positive);
    /// `{abs_when: <condition>}`: without it when the condition holds (`doc.is_return`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign: Option<Sign>,
}

/// How a number's sign prints: always left off (`"abs"`), or left off when a condition holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Sign {
    Always(SignWord),
    When(SignWhen),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignWord {
    Abs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignWhen {
    pub abs_when: String,
}

impl Format {
    /// The condition under which the sign is left off, when it is a condition.
    pub fn abs_when(&self) -> Option<&str> {
        match &self.sign {
            Some(Sign::When(w)) => Some(&w.abs_when),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Style {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font: Option<Font>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<Size>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_height: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Watermark {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

/// A layout format (§4.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    pub version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<Style>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark: Option<Watermark>,
    #[serde(default)]
    pub sections: Vec<Section>,
    /// Translations of its labels and texts (slice 3): kept and bounded, not yet applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub texts: Option<BTreeMap<String, BTreeMap<String, String>>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub columns: Vec<Column>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widths: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap_mm: Option<u8>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub border: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub padding_mm: Option<u8>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep_together: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub break_before: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin_top_mm: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    pub id: String,
    #[serde(default)]
    pub blocks: Vec<Block>,
}

/// A block: what every block has (`id`, `when`, `align`, `margin_top_mm`) and its kind's own fields, one JSON object.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub id: String,
    pub when: Option<String>,
    pub align: Option<Align>,
    pub margin_top_mm: Option<u8>,
    pub kind: BlockKind,
}

impl<'de> Deserialize<'de> for Block {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut m = Map::<String, Value>::deserialize(d)?;
        let id = match m.remove("id") {
            Some(Value::String(s)) => s,
            _ => return Err(D::Error::custom("a block has an id")),
        };
        let take = |m: &mut Map<String, Value>, k: &str| m.remove(k).filter(|v| !v.is_null());
        let at = |e: serde_json::Error| D::Error::custom(format!("block {id}: {e}"));
        let when = take(&mut m, "when").map(serde_json::from_value).transpose().map_err(at)?;
        let align = take(&mut m, "align").map(serde_json::from_value).transpose().map_err(at)?;
        let margin_top_mm = take(&mut m, "margin_top_mm").map(serde_json::from_value).transpose().map_err(at)?;
        let kind = serde_json::from_value(Value::Object(m)).map_err(at)?;
        Ok(Block { id, when, align, margin_top_mm, kind })
    }
}

impl Serialize for Block {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = match serde_json::to_value(&self.kind).map_err(serde::ser::Error::custom)? {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        m.insert("id".into(), Value::String(self.id.clone()));
        if let Some(w) = &self.when {
            m.insert("when".into(), Value::String(w.clone()));
        }
        if let Some(a) = self.align {
            m.insert("align".into(), Value::String(a.name().into()));
        }
        if let Some(t) = self.margin_top_mm {
            m.insert("margin_top_mm".into(), Value::from(t));
        }
        m.serialize(s)
    }
}

/// `null` given is `Some(None)`; absent is `None` (a field block's `label`: absent = the field's, null = none).
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ImageSrc {
    File { file: String },
    Value { value: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelPosition {
    Inline,
    Above,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    Contain,
    Cover,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Borders {
    Rows,
    All,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BarKind {
    Code128,
    Ean13,
    Qr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineStyle {
    Solid,
    Dashed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BlockKind {
    Field {
        value: Paths,
        #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
        label: Option<Option<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label_position: Option<LabelPosition>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        weight: Option<Weight>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<Size>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<Format>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hide_empty: Option<bool>,
    },
    Heading {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        level: Option<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<Size>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<String>,
    },
    Text {
        content: Vec<Paragraph>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<Size>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<String>,
    },
    Image {
        src: ImageSrc,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width_mm: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height_mm: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fit: Option<Fit>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alt: Option<String>,
    },
    Table {
        value: String,
        columns: Vec<TableColumn>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        header: Option<bool>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        row_numbers: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        borders: Option<Borders>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        striped: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<Size>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hide_zero_rows: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        totals: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        totals_label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        empty_text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hide_empty: Option<bool>,
    },
    Totals {
        rows: Vec<TotalRow>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width_pct: Option<f64>,
    },
    Signature {
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<ImageSrc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width_mm: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<bool>,
    },
    Barcode {
        value: Paths,
        kind: BarKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height_mm: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        show_text: Option<bool>,
    },
    CopyLabel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<Size>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        weight: Option<Weight>,
    },
    Divider {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thickness: Option<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        style: Option<LineStyle>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        color: Option<String>,
    },
    Spacer {
        height_mm: f64,
    },
    PageBreak {},
}

impl BlockKind {
    pub fn name(&self) -> &'static str {
        match self {
            BlockKind::Field { .. } => "field",
            BlockKind::Heading { .. } => "heading",
            BlockKind::Text { .. } => "text",
            BlockKind::Image { .. } => "image",
            BlockKind::Table { .. } => "table",
            BlockKind::Totals { .. } => "totals",
            BlockKind::Signature { .. } => "signature",
            BlockKind::Barcode { .. } => "barcode",
            BlockKind::CopyLabel { .. } => "copy_label",
            BlockKind::Divider { .. } => "divider",
            BlockKind::Spacer { .. } => "spacer",
            BlockKind::PageBreak {} => "page_break",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableColumn {
    pub id: String,
    pub value: Paths,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<Align>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Format>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary: Option<Paths>,
    /// A cell whose value is zero is left blank.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hide_zero: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TotalRow {
    pub label: String,
    pub value: Paths,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Format>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub emphasis: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hide_zero: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paragraph {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<Align>,
    pub spans: Vec<Span>,
}

/// Text (`t`) or a value (`v`), bold, italic or underlined.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Span {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v: Option<Paths>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Format>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub b: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub i: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub u: bool,
}

/// Where a path is read: the document's body, a table's columns (`row.*` too), or a letter head (`print.page` too).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAt {
    Body,
    Row,
    LetterHead,
}

/// What a layout names, for the kernel to check against the entity and to load: each `(id, path, where)`.
#[derive(Debug, Default)]
pub struct Named<'a> {
    pub paths: Vec<(&'a str, String, PathAt)>,
    pub conditions: Vec<(&'a str, &'a str)>,
    pub files: Vec<(&'a str, &'a str)>,
}

/// The paths in a string's tokens.
pub fn tokens(s: &str) -> impl Iterator<Item = (String, Option<String>)> + '_ {
    TOKEN.captures_iter(s).map(|c| (c[1].to_string(), c.get(2).map(|m| m.as_str().to_string())))
}

/// The blocks a letter head may hold.
pub const LETTERHEAD_BLOCKS: [&str; 6] = ["field", "text", "heading", "image", "divider", "spacer"];

impl Layout {
    /// A letter head's `header` or `footer` value (`{"sections": [...]}`) as a layout; null or `{}`: none.
    pub fn from_letterhead(v: &Value) -> Result<Option<Layout>, String> {
        let sections = match v {
            Value::Null => return Ok(None),
            Value::Object(m) if m.is_empty() => return Ok(None),
            Value::Object(m) => {
                if let Some(k) = m.keys().find(|k| *k != "sections") {
                    return Err(format!("a letter head's part holds sections only, not {k}"));
                }
                m.get("sections").cloned().unwrap_or(Value::Array(vec![]))
            }
            _ => return Err("a letter head's part is {\"sections\": [...]}".into()),
        };
        let sections: Vec<Section> = serde_json::from_value(sections).map_err(|e| e.to_string())?;
        Ok(Some(Layout { version: 1, style: None, watermark: None, sections, texts: None }))
    }

    /// Every block, with its section and column.
    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.sections.iter().flat_map(|s| s.columns.iter().flat_map(|c| c.blocks.iter()))
    }

    /// Every value format it holds, with the id of what holds it.
    pub fn formats(&self) -> Vec<(&str, &Format)> {
        let mut out = Vec::new();
        for b in self.blocks() {
            let id = b.id.as_str();
            match &b.kind {
                BlockKind::Field { format: Some(f), .. } => out.push((id, f)),
                BlockKind::Text { content, .. } => out.extend(content.iter().flat_map(|p| p.spans.iter()).filter_map(|s| s.format.as_ref()).map(|f| (id, f))),
                BlockKind::Table { columns, .. } => out.extend(columns.iter().filter_map(|c| c.format.as_ref().map(|f| (c.id.as_str(), f)))),
                BlockKind::Totals { rows, .. } => out.extend(rows.iter().filter_map(|r| r.format.as_ref()).map(|f| (id, f))),
                _ => {}
            }
        }
        out
    }

    /// The paths, conditions and files it names.
    pub fn named(&self) -> Named<'_> {
        let mut n = Named::default();
        if let Some(w) = &self.watermark {
            for (p, _) in tokens(&w.text) {
                n.paths.push(("watermark", p, PathAt::Body));
            }
            if let Some(c) = &w.when {
                n.conditions.push(("watermark", c));
            }
        }
        for s in &self.sections {
            if let Some(c) = &s.when {
                n.conditions.push((&s.id, c));
            }
            for b in s.columns.iter().flat_map(|c| c.blocks.iter()) {
                let id = b.id.as_str();
                if let Some(c) = &b.when {
                    n.conditions.push((id, c));
                }
                let mut push = |p: &str, at: PathAt| n.paths.push((id, p.to_string(), at));
                match &b.kind {
                    BlockKind::Field { value, .. } | BlockKind::Barcode { value, .. } => value.list().iter().for_each(|p| push(p, PathAt::Body)),
                    BlockKind::Heading { text, .. } => tokens(text).for_each(|(p, _)| push(&p, PathAt::Body)),
                    BlockKind::Text { content, .. } => {
                        for sp in content.iter().flat_map(|p| p.spans.iter()) {
                            sp.v.iter().flat_map(|v| v.list()).for_each(|p| push(p, PathAt::Body));
                        }
                    }
                    BlockKind::Image { src, .. } => match src {
                        ImageSrc::File { file } => n.files.push((id, file)),
                        ImageSrc::Value { value } => push(value, PathAt::Body),
                    },
                    BlockKind::Signature { label, image, .. } => {
                        tokens(label).for_each(|(p, _)| push(&p, PathAt::Body));
                        match image {
                            Some(ImageSrc::File { file }) => n.files.push((id, file)),
                            Some(ImageSrc::Value { value }) => push(value, PathAt::Body),
                            None => {}
                        }
                    }
                    BlockKind::Totals { rows, .. } => {
                        for r in rows {
                            tokens(&r.label).for_each(|(p, _)| push(&p, PathAt::Body));
                            r.value.list().iter().for_each(|p| push(p, PathAt::Body));
                            if let Some(c) = &r.when {
                                n.conditions.push((id, c));
                            }
                        }
                    }
                    BlockKind::Table { value, columns, hide_zero_rows, .. } => {
                        push(value, PathAt::Body);
                        if let Some(h) = hide_zero_rows {
                            n.paths.push((id, h.clone(), PathAt::Row));
                        }
                        for c in columns {
                            for p in c.value.list().iter().chain(c.secondary.iter().flat_map(|s| s.list())) {
                                n.paths.push((&c.id, p.clone(), PathAt::Row));
                            }
                            if let Some(w) = &c.when {
                                n.conditions.push((&c.id, w));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        n
    }

    /// What is wrong with it, each as `(id, message)` (`id` empty: the layout as a whole): its shape and every limit
    /// of §4.2 that needs no schema. `letterhead`: a letter head's part (its own blocks only, no sections needed).
    pub fn problems(&self, letterhead: bool) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let mut bad = |id: &str, msg: String| out.push((id.to_string(), msg));
        if self.version != 1 {
            bad("", format!("version {} is not one this server reads (1)", self.version));
        }
        let (min, max) = if letterhead { (0, 20) } else { (1, MAX_SECTIONS) };
        if self.sections.len() < min || self.sections.len() > max {
            bad("", format!("a layout has {min} to {max} sections, not {}", self.sections.len()));
        }
        let colour = |bad: &mut dyn FnMut(&str, String), id: &str, c: &Option<String>| {
            if let Some(c) = c.as_deref().filter(|c| !HEX.is_match(c)) {
                bad(id, format!("'{}' is not a colour written #rrggbb", cut(c)));
            }
        };
        let text = |bad: &mut dyn FnMut(&str, String), id: &str, what: &str, s: &str, max: usize| {
            if s.chars().count() > max {
                bad(id, format!("{what} is longer than {max} characters"));
            }
        };
        let space = |bad: &mut dyn FnMut(&str, String), id: &str, what: &str, v: Option<u8>| {
            if let Some(v) = v.filter(|v| ![0, 1, 2, 3, 4, 6, 8].contains(v)) {
                bad(id, format!("{what} is one of 0, 1, 2, 3, 4, 6 or 8 mm, not {v}"));
            }
        };
        let range = |bad: &mut dyn FnMut(&str, String), id: &str, what: &str, v: Option<f64>, lo: f64, hi: f64| {
            if let Some(v) = v.filter(|v| !v.is_finite() || *v < lo || *v > hi) {
                bad(id, format!("{what} is {lo} to {hi}, not {v}"));
            }
        };
        let format = |bad: &mut dyn FnMut(&str, String), id: &str, f: &Option<Format>| {
            let Some(f) = f else { return };
            if f.decimals.is_some_and(|d| d > 9) {
                bad(id, "decimals are 0 to 9".into());
            }
            if let Some(c) = f.currency.as_deref().filter(|c| c.len() != 3 || !c.chars().all(|c| c.is_ascii_uppercase())) {
                bad(id, format!("'{}' is not a currency code", cut(c)));
            }
        };
        let paths = |bad: &mut dyn FnMut(&str, String), id: &str, p: &Paths| {
            let l = p.list();
            if l.is_empty() || l.len() > 4 {
                bad(id, "a value is 1 to 4 paths".into());
            }
            for p in l.iter().filter(|p| !PATH.is_match(p)) {
                bad(id, format!("'{}' is not a path (doc.<field>, links.<link field>.<field>, company.<field>, row.<field>…)", cut(p)));
            }
        };
        if let Some(s) = &self.style {
            colour(&mut bad, "", &s.color);
            colour(&mut bad, "", &s.accent);
            if let Some(l) = s.line_height.filter(|l| ![1.2, 1.3, 1.45, 1.6].contains(l)) {
                bad("", format!("line height is 1.2, 1.3, 1.45 or 1.6, not {l}"));
            }
        }
        if let Some(w) = &self.watermark {
            text(&mut bad, "", "a watermark", &w.text, 40);
        }
        if let Some(t) = &self.texts {
            for (lang, words) in t {
                if words.len() > 500 || lang.is_empty() || lang.len() > 20 {
                    bad("", format!("texts of {}: a language tag and at most 500 texts", cut(lang)));
                }
            }
        }
        let mut ids: HashSet<String> = HashSet::new();
        let mut seen = |bad: &mut dyn FnMut(&str, String), id: &str| {
            if !ID.is_match(id) {
                bad(id, format!("'{}' is not an id: a lower-case letter, then letters, digits, _ or -, at most 40", cut(id)));
            } else if !ids.insert(id.to_string()) {
                bad(id, format!("{id} is the id of two things: each is unique"));
            }
        };
        let (mut blocks, mut images, mut conditions) = (0usize, 0usize, 0usize);
        let mut cel = |bad: &mut dyn FnMut(&str, String), id: &str, c: &Option<String>| {
            let Some(c) = c else { return };
            conditions += 1;
            if c.len() > MAX_CEL {
                bad(id, format!("a condition is at most {MAX_CEL} characters"));
            } else if let Err(e) = Expr::compile(c) {
                bad(id, format!("the condition does not compile: {e}"));
            }
        };
        if let Some(w) = &self.watermark {
            cel(&mut bad, "", &w.when);
        }
        for s in &self.sections {
            seen(&mut bad, &s.id);
            cel(&mut bad, &s.id, &s.when);
            if let Some(t) = &s.title {
                text(&mut bad, &s.id, "a section's title", t, 120);
            }
            if s.columns.is_empty() || s.columns.len() > 4 {
                bad(&s.id, format!("a section has 1 to 4 columns, not {}", s.columns.len()));
            }
            if let Some(w) = &s.widths
                && (w.len() != s.columns.len() || w.iter().any(|w| !(1..=12).contains(w)))
            {
                bad(&s.id, "widths are one per column, each 1 to 12".into());
            }
            space(&mut bad, &s.id, "the gap", s.gap_mm);
            space(&mut bad, &s.id, "the padding", s.padding_mm);
            space(&mut bad, &s.id, "the space above", s.margin_top_mm);
            colour(&mut bad, &s.id, &s.background);
            for c in &s.columns {
                seen(&mut bad, &c.id);
                if c.blocks.len() > MAX_COLUMN_BLOCKS {
                    bad(&c.id, format!("a column holds at most {MAX_COLUMN_BLOCKS} blocks"));
                }
                for b in &c.blocks {
                    blocks += 1;
                    let id = b.id.as_str();
                    seen(&mut bad, id);
                    cel(&mut bad, id, &b.when);
                    space(&mut bad, id, "the space above", b.margin_top_mm);
                    if letterhead && !LETTERHEAD_BLOCKS.contains(&b.kind.name()) {
                        bad(id, format!("a letter head holds {} blocks, not a {}", LETTERHEAD_BLOCKS.join(", "), b.kind.name()));
                    }
                    let mut image = |bad: &mut dyn FnMut(&str, String), src: &ImageSrc| {
                        images += 1;
                        match src {
                            ImageSrc::File { file } if !FILE.is_match(file) => {
                                bad(id, format!("'{}' is not one of this workspace's files (/files/<id>)", cut(file)))
                            }
                            ImageSrc::Value { value } => paths(bad, id, &Paths::One(value.clone())),
                            _ => {}
                        }
                    };
                    match &b.kind {
                        BlockKind::Field { value, label, color, format: f, .. } => {
                            paths(&mut bad, id, value);
                            if let Some(Some(l)) = label {
                                text(&mut bad, id, "a label", l, 80);
                            }
                            colour(&mut bad, id, color);
                            format(&mut bad, id, f);
                        }
                        BlockKind::Heading { text: t, level, color, .. } => {
                            text(&mut bad, id, "a heading", t, 200);
                            if level.is_some_and(|l| !(1..=3).contains(&l)) {
                                bad(id, "a heading's level is 1, 2 or 3".into());
                            }
                            colour(&mut bad, id, color);
                        }
                        BlockKind::Text { content, color, .. } => {
                            colour(&mut bad, id, color);
                            let mut chars = 0;
                            for sp in content.iter().flat_map(|p| p.spans.iter()) {
                                match (&sp.t, &sp.v) {
                                    (Some(t), None) => chars += t.chars().count(),
                                    (None, Some(v)) => paths(&mut bad, id, v),
                                    _ => bad(id, "a span is text (t) or a value (v)".into()),
                                }
                                format(&mut bad, id, &sp.format);
                            }
                            if chars > MAX_TEXT {
                                bad(id, format!("a text is at most {MAX_TEXT} characters"));
                            }
                        }
                        BlockKind::Image { src, width_mm, height_mm, alt, .. } => {
                            image(&mut bad, src);
                            range(&mut bad, id, "the width", *width_mm, 5.0, 200.0);
                            range(&mut bad, id, "the height", *height_mm, 5.0, 200.0);
                            if let Some(a) = alt {
                                text(&mut bad, id, "a description", a, 120);
                            }
                        }
                        BlockKind::Table { value, columns, totals, totals_label, empty_text, hide_zero_rows, .. } => {
                            let dots = value.matches('.').count();
                            let ok = (value.starts_with("doc.") || value.starts_with("company.")) && dots == 1 || value.starts_with("links.") && dots == 2;
                            if !ok || !PATH.is_match(value) {
                                bad(
                                    id,
                                    format!(
                                        "a table prints a table of the record (doc.<table>), of a record it links to (links.<link>.<table>) or of the company (company.<table>), not {}",
                                        cut(value)
                                    ),
                                );
                            }
                            if columns.is_empty() || columns.len() > 20 {
                                bad(id, format!("a table has 1 to 20 columns, not {}", columns.len()));
                            }
                            if let Some(h) = hide_zero_rows.as_deref().filter(|h| !h.starts_with("row.")) {
                                bad(id, format!("rows are left out by a row.<field>, not {}", cut(h)));
                            }
                            for t in [totals_label, empty_text].into_iter().flatten() {
                                text(&mut bad, id, "a label", t, 80);
                            }
                            for c in columns {
                                seen(&mut bad, &c.id);
                                cel(&mut bad, &c.id, &c.when);
                                paths(&mut bad, &c.id, &c.value);
                                if let Some(s) = &c.secondary {
                                    paths(&mut bad, &c.id, s);
                                }
                                if let Some(l) = &c.label {
                                    text(&mut bad, &c.id, "a label", l, 80);
                                }
                                range(&mut bad, &c.id, "the width", c.width_pct, 3.0, 100.0);
                                format(&mut bad, &c.id, &c.format);
                            }
                            if let Some(t) = totals.iter().find(|t| !columns.iter().any(|c| &c.id == *t)) {
                                bad(id, format!("{} is not one of the table's columns", cut(t)));
                            }
                        }
                        BlockKind::Totals { rows, width_pct } => {
                            if rows.is_empty() || rows.len() > 12 {
                                bad(id, "totals have 1 to 12 rows".into());
                            }
                            range(&mut bad, id, "the width", *width_pct, 30.0, 100.0);
                            for r in rows {
                                text(&mut bad, id, "a label", &r.label, 80);
                                paths(&mut bad, id, &r.value);
                                format(&mut bad, id, &r.format);
                                cel(&mut bad, id, &r.when);
                            }
                        }
                        BlockKind::Signature { label, image: img, width_mm, .. } => {
                            text(&mut bad, id, "a label", label, 80);
                            if let Some(src) = img {
                                image(&mut bad, src);
                            }
                            range(&mut bad, id, "the width", *width_mm, 30.0, 120.0);
                        }
                        BlockKind::Barcode { value, height_mm, .. } => {
                            paths(&mut bad, id, value);
                            range(&mut bad, id, "the height", *height_mm, 5.0, 60.0);
                        }
                        BlockKind::Divider { thickness, color, .. } => {
                            if thickness.is_some_and(|t| !(1..=3).contains(&t)) {
                                bad(id, "a divider is 1, 2 or 3 thick".into());
                            }
                            colour(&mut bad, id, color);
                        }
                        BlockKind::Spacer { height_mm } => range(&mut bad, id, "the height", Some(*height_mm), 1.0, 100.0),
                        BlockKind::CopyLabel { .. } | BlockKind::PageBreak {} => {}
                    }
                }
            }
        }
        // A sign left off on a condition is a condition like `when`.
        for (id, f) in self.formats() {
            if let Some(c) = f.abs_when() {
                cel(&mut bad, id, &Some(c.to_string()));
            }
        }
        if blocks > MAX_BLOCKS {
            bad("", format!("a layout holds at most {MAX_BLOCKS} blocks"));
        }
        if images > MAX_IMAGES {
            bad("", format!("a layout shows at most {MAX_IMAGES} images"));
        }
        if conditions > MAX_CONDITIONS {
            bad("", format!("a layout has at most {MAX_CONDITIONS} conditions"));
        }
        out
    }
}

/// Whether `s` is a UUID as ids are written.
pub fn is_uuid(s: &str) -> bool {
    s.len() == 36 && FILE.is_match(&format!("/files/{s}"))
}

/// A value quoted back in a message, cut short.
fn cut(s: &str) -> String {
    s.chars().take(60).collect()
}

/// Paper sizes in millimetres (width × height, portrait).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaperSize {
    A3,
    A4,
    A5,
    Letter,
    Legal,
    Custom,
}

impl PaperSize {
    pub fn mm(self) -> Option<(f64, f64)> {
        Some(match self {
            PaperSize::A3 => (297.0, 420.0),
            PaperSize::A4 => (210.0, 297.0),
            PaperSize::A5 => (148.0, 210.0),
            PaperSize::Letter => (215.9, 279.4),
            PaperSize::Legal => (215.9, 355.6),
            PaperSize::Custom => return None,
        })
    }
    pub fn parse(s: &str) -> Option<PaperSize> {
        serde_json::from_value(Value::String(s.into())).ok()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    #[default]
    Portrait,
    Landscape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Margins {
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
    pub left: u32,
}

impl Default for Margins {
    fn default() -> Self {
        Margins { top: 15, right: 12, bottom: 15, left: 12 }
    }
}

/// A format's paper and margins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageSetup {
    pub size: PaperSize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width_mm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height_mm: Option<u32>,
    #[serde(default)]
    pub orientation: Orientation,
    #[serde(default)]
    pub margin_mm: Margins,
}

impl PageSetup {
    /// Width and height in millimetres as printed (landscape turned).
    pub fn sheet_mm(&self) -> (f64, f64) {
        let (w, h) = self.size.mm().unwrap_or((self.width_mm.unwrap_or(210) as f64, self.height_mm.unwrap_or(297) as f64));
        if self.orientation == Orientation::Landscape { (h, w) } else { (w, h) }
    }

    pub fn problem(&self) -> Option<String> {
        let custom = self.size == PaperSize::Custom;
        let ok = |v: Option<u32>| v.is_some_and(|v| (40..=600).contains(&v));
        if custom && !(ok(self.width_mm) && ok(self.height_mm)) {
            return Some("a custom page has a width and a height, each 40 to 600 mm".into());
        }
        if !custom && (self.width_mm.is_some() || self.height_mm.is_some()) {
            return Some("only a custom page has a width and a height".into());
        }
        let m = self.margin_mm;
        if [m.top, m.right, m.bottom, m.left].iter().any(|v| *v > 50) {
            return Some("margins are 0 to 50 mm".into());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn layout(v: Value) -> Layout {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_block_keeps_its_shape_and_refuses_what_it_does_not_know() {
        let b: Block = serde_json::from_value(json!({"id": "a", "type": "field", "value": "doc.x", "label": null, "align": "end"})).unwrap();
        assert_eq!(b.align, Some(Align::End));
        assert!(matches!(&b.kind, BlockKind::Field { label: Some(None), .. }), "null is no label: {b:?}");
        assert_eq!(serde_json::to_value(&b).unwrap(), json!({"id": "a", "type": "field", "value": "doc.x", "label": null, "align": "end"}));
        let b: Block = serde_json::from_value(json!({"id": "a", "type": "field", "value": "doc.x"})).unwrap();
        assert!(matches!(&b.kind, BlockKind::Field { label: None, .. }), "absent is the field's label");
        for wrong in [
            json!({"id": "a", "type": "field", "value": "doc.x", "html": "<b>"}),
            json!({"id": "a", "type": "html", "value": "<b>"}),
            json!({"type": "spacer", "height_mm": 2}),
            json!({"id": "a", "type": "page_break", "x": 1}),
        ] {
            assert!(serde_json::from_value::<Block>(wrong.clone()).is_err(), "{wrong}");
        }
        assert!(serde_json::from_value::<Layout>(json!({"version": 1, "sections": [], "css": "x"})).is_err());
    }

    #[test]
    fn limits_are_checked() {
        let ok = layout(json!({"version": 1, "sections": [{"id": "s", "columns": [{"id": "c", "blocks": [
            {"id": "t", "type": "table", "value": "doc.items", "totals": ["q"], "columns": [{"id": "q", "value": "row.qty"}]}
        ]}]}]}));
        assert!(ok.problems(false).is_empty(), "{:?}", ok.problems(false));
        let cases = [
            (json!({"id": "s", "columns": []}), "1 to 4 columns"),
            (json!({"id": "S", "columns": [{"id": "c"}]}), "is not an id"),
            (json!({"id": "s", "columns": [{"id": "s"}]}), "the id of two things"),
            (json!({"id": "s", "gap_mm": 5, "columns": [{"id": "c"}]}), "0, 1, 2, 3, 4, 6 or 8"),
            (json!({"id": "s", "background": "red;x:url(a)", "columns": [{"id": "c"}]}), "#rrggbb"),
            (json!({"id": "s", "when": "doc.(", "columns": [{"id": "c"}]}), "does not compile"),
            (
                json!({"id": "s", "columns": [{"id": "c", "blocks": [{"id": "i", "type": "image", "src": {"file": "https://x.test/a.png"}}]}]}),
                "this workspace's files",
            ),
            (json!({"id": "s", "columns": [{"id": "c", "blocks": [{"id": "h", "type": "heading", "text": "x".repeat(201)}]}]}), "longer than 200"),
            (
                json!({"id": "s", "columns": [{"id": "c", "blocks": [{"id": "t", "type": "table", "value": "doc.items", "totals": ["z"], "columns": [{"id": "q", "value": "row.qty"}]}]}]}),
                "not one of the table's columns",
            ),
            (json!({"id": "s", "columns": [{"id": "c", "blocks": [{"id": "f", "type": "field", "value": "javascript:alert(1)"}]}]}), "is not a path"),
            (json!({"id": "s", "columns": [{"id": "c", "blocks": [{"id": "p", "type": "spacer", "height_mm": 1000}]}]}), "1 to 100"),
        ];
        for (section, says) in cases {
            let l = layout(json!({"version": 1, "sections": [section]}));
            let p = l.problems(false);
            assert!(p.iter().any(|(_, m)| m.contains(says)), "{says}: {p:?}");
        }
        let blocks: Vec<Value> = (0..51).map(|i| json!({"id": format!("b{i}"), "type": "spacer", "height_mm": 1})).collect();
        let l = layout(json!({"version": 1, "sections": [{"id": "s", "columns": [{"id": "c", "blocks": blocks}]}]}));
        assert_eq!(l.problems(false)[0], ("c".into(), "a column holds at most 50 blocks".into()));
        let sections: Vec<Value> = (0..101).map(|i| json!({"id": format!("s{i}"), "columns": [{"id": format!("c{i}")}]})).collect();
        assert!(layout(json!({"version": 1, "sections": sections})).problems(false)[0].1.contains("1 to 100 sections"));
        let lh = Layout::from_letterhead(&json!({"sections": [{"id": "s", "columns": [{"id": "c", "blocks": [{"id": "t", "type": "table", "value": "doc.items", "columns": [{"id": "q", "value": "row.qty"}]}]}]}]}))
            .unwrap()
            .unwrap();
        assert!(lh.problems(true).iter().any(|(id, m)| id == "t" && m.contains("a letter head holds")));
    }

    #[test]
    fn page_setups() {
        let p: PageSetup = serde_json::from_value(json!({"size": "A5", "orientation": "landscape"})).unwrap();
        assert_eq!((p.sheet_mm(), p.margin_mm), ((210.0, 148.0), Margins::default()));
        assert!(serde_json::from_value::<PageSetup>(json!({"size": "Custom", "width_mm": 30, "height_mm": 50})).unwrap().problem().is_some());
        assert!(
            serde_json::from_value::<PageSetup>(json!({"size": "A4", "margin_mm": {"top": 60, "right": 0, "bottom": 0, "left": 0}}))
                .unwrap()
                .problem()
                .is_some()
        );
    }
}
