//! Serializable metadata definitions (what package YAML files contain).

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageManifest {
    pub package: String,
    pub version: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub depends: Vec<String>,
    /// Prefix every field this package adds to *other* packages' entities must carry (e.g. `in_`).
    #[serde(default)]
    pub field_prefix: Option<String>,
    #[serde(default)]
    pub modules: Vec<ModuleDef>,
    #[serde(default)]
    pub roles: Vec<RoleDef>,
    #[serde(default)]
    pub workflows: Vec<WorkflowDef>,
    #[serde(default)]
    pub overlays: Vec<Overlay>,
    #[serde(default)]
    pub seed: Vec<SeedDef>,
    /// Where this package keeps people's personal data: what a personal-data request exports and
    /// erases for a person (an email).
    #[serde(default)]
    pub personal_data: Vec<PersonalDataDef>,
}

/// Records of one entity that belong to a person: those whose `match` email field holds the
/// person's email (and whose fields hold the `where` values); `erase` lists what erasure blanks
/// (a child table listed there loses its rows).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PersonalDataDef {
    pub entity: String,
    #[serde(rename = "match")]
    pub match_field: String,
    pub erase: Vec<String>,
    #[serde(default, rename = "where", skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub condition: std::collections::BTreeMap<String, Value>,
}

/// Field types erasure can blank (text, contact details, dates and JSON such as form answers); a
/// child table loses its rows.
pub const ERASABLE_TYPES: &[FieldType] = &[
    FieldType::Data,
    FieldType::Text,
    FieldType::Markdown,
    FieldType::Email,
    FieldType::Phone,
    FieldType::Url,
    FieldType::Date,
    FieldType::Json,
    FieldType::Table,
];

/// A fully loaded package: manifest + entity files + SQL + print templates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    #[serde(flatten)]
    pub manifest: PackageManifest,
    #[serde(default)]
    pub entities: Vec<EntityDef>,
    /// `(file name, sql)` run once per cluster in file-name order after entity tables exist.
    #[serde(default, skip_serializing)]
    pub sql: Vec<(String, String)>,
    /// `(entity, template source)` default HTML print formats.
    #[serde(default, skip_serializing)]
    pub print_formats: Vec<(String, String)>,
}

impl Package {
    pub fn name(&self) -> &str {
        &self.manifest.package
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleDef {
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub order: i32,
    /// Entities and reports shown in this module's sidebar, in order.
    #[serde(default)]
    pub items: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    /// Reference data; editable; no submit lifecycle.
    Master,
    /// Transaction; may be submittable (draft → submitted → cancelled).
    Document,
    /// Line items owned by a parent through a `table` field.
    Child,
    /// One row per tenant (and company when company-scoped).
    Settings,
    /// Engine-owned append-only table (journal, stock moves): read-only through the API.
    Ledger,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityDef {
    pub name: String,
    pub label: String,
    #[serde(default)]
    pub label_plural: Option<String>,
    pub kind: EntityKind,
    #[serde(default)]
    pub module: Option<String>,
    /// Physical table override (ledger entities point at engine-owned tables).
    #[serde(default)]
    pub table: Option<String>,
    #[serde(default)]
    pub title_field: Option<String>,
    #[serde(default)]
    pub numbering: Option<NumberingDef>,
    #[serde(default)]
    pub submittable: bool,
    /// Requires a `company` link field; records are scoped by the user's allowed companies.
    #[serde(default)]
    pub company_scoped: bool,
    #[serde(default)]
    pub fields: Vec<FieldDef>,
    #[serde(default)]
    pub unique: Vec<Vec<String>>,
    #[serde(default)]
    pub indexes: Vec<Vec<String>>,
    /// Postgres exclusion constraints: no two rows with equal `fields` may have overlapping validity.
    #[serde(default)]
    pub exclusion: Vec<ExclusionDef>,
    #[serde(default)]
    pub rules: Vec<RuleDef>,
    #[serde(default)]
    pub list: ListDef,
    #[serde(default)]
    pub form: Vec<SectionDef>,
    #[serde(default)]
    pub search_fields: Vec<String>,
    #[serde(default)]
    pub actions: Vec<ActionDef>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    /// Who else owns a record besides its creator, for own-records permissions (`if_owner`):
    /// fields holding a user's email, or `table.field` for a child table of emails. Assignees of
    /// leads, tasks or tickets then see and edit them like their creators.
    #[serde(default)]
    pub owner_fields: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NumberingOn {
    #[default]
    Insert,
    /// Assigned inside the submit transaction; required for legally gapless series.
    Submit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NumberingDef {
    /// Tokens: `{YYYY}`, `{YY}`, `{MM}`, `{DD}`, `{FY}` (fiscal year start year), `{field:<name>}`, `{seq:<width>}`.
    pub template: String,
    #[serde(default)]
    pub gapless: bool,
    #[serde(default)]
    pub on: NumberingOn,
    /// Field whose value is the date for date tokens (defaults to today).
    #[serde(default)]
    pub date_field: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RuleOn {
    #[default]
    Save,
    Submit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleDef {
    pub name: String,
    /// CEL expression that must evaluate to `true`.
    pub check: String,
    pub message: String,
    #[serde(default)]
    pub on: RuleOn,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExclusionDef {
    pub fields: Vec<String>,
    pub from: String,
    /// Open-ended when the value is null.
    pub to: String,
    /// What users read when a record would overlap another (default: "this overlaps an existing record").
    #[serde(default)]
    pub message: Option<String>,
    /// Records whose field holds one of these values never count as overlapping, e.g.
    /// `{status: [Cancelled, No Show]}` (select or data fields; plain words only).
    #[serde(default)]
    pub when_not: std::collections::BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ListDef {
    #[serde(default)]
    pub columns: Vec<String>,
    /// e.g. `posting_date desc`
    #[serde(default)]
    pub sort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SectionDef {
    #[serde(default)]
    pub label: Option<String>,
    pub fields: Vec<String>,
    #[serde(default)]
    pub columns: Option<u8>,
    #[serde(default)]
    pub collapsible: bool,
    /// Sections with the same tab label are shown together under that tab (in order).
    #[serde(default)]
    pub tab: Option<String>,
    /// CEL condition on `doc`: the section shows only when it holds.
    #[serde(default)]
    pub visible_if: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionDef {
    pub name: String,
    pub label: String,
    /// Allowed docstatus values (0 draft, 1 submitted, 2 cancelled). Empty = any.
    #[serde(default)]
    pub docstatus: Vec<i16>,
    /// Permission required: read | write | create | submit | cancel.
    #[serde(default = "default_action_perm")]
    pub permission: String,
    #[serde(default)]
    pub confirm: Option<String>,
    /// CEL condition on `doc` for showing/allowing the action.
    #[serde(default)]
    pub condition: Option<String>,
    /// The action only reads (a lookup, a preview): it runs without locking the record or
    /// writing an audit entry, in a read-only transaction when run on its own.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
}

fn default_action_perm() -> String {
    "write".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    Data,
    Text,
    Markdown,
    Code,
    Int,
    Decimal,
    Currency,
    Percent,
    Date,
    Datetime,
    Check,
    Select,
    Link,
    Table,
    Json,
    Email,
    Phone,
    Url,
    Attach,
    Color,
    /// Write-only text (passwords, API keys): encrypted at rest outside the record; reads only
    /// show whether one is set (`true`). Server code reads it with `Kernel::secret`.
    Secret,
}

impl FieldType {
    /// Whether values live in the record's own row (child tables and secrets live elsewhere).
    pub fn is_stored(self) -> bool {
        !matches!(self, FieldType::Table | FieldType::Secret)
    }
    pub fn is_numeric(self) -> bool {
        matches!(self, FieldType::Int | FieldType::Decimal | FieldType::Currency | FieldType::Percent)
    }
    pub fn is_decimal(self) -> bool {
        matches!(self, FieldType::Decimal | FieldType::Currency | FieldType::Percent)
    }
    pub fn is_text(self) -> bool {
        matches!(
            self,
            FieldType::Data
                | FieldType::Text
                | FieldType::Markdown
                | FieldType::Code
                | FieldType::Select
                | FieldType::Email
                | FieldType::Phone
                | FieldType::Url
                | FieldType::Attach
                | FieldType::Color
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnDelete {
    #[default]
    Restrict,
    SetNull,
    Cascade,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldDef {
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(rename = "type")]
    pub ftype: FieldType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub unique: bool,
    #[serde(default)]
    pub index: bool,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub in_list: bool,
    #[serde(default)]
    pub in_filter: bool,
    /// Overlays may tighten but never loosen this field (e.g. un-require it).
    #[serde(default)]
    pub locked: bool,
    /// Not copied on amend/duplicate.
    #[serde(default)]
    pub no_copy: bool,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    pub default_expr: Option<String>,
    /// CEL computed value; the field becomes read-only. Not for money (CEL has no decimal type).
    #[serde(default)]
    pub computed: Option<String>,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub of: Option<String>,
    #[serde(default)]
    pub precision: Option<u32>,
    /// For `currency`: name of the field holding the currency code (`parent.<field>` from child rows).
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub min: Option<Decimal>,
    #[serde(default)]
    pub max: Option<Decimal>,
    #[serde(default)]
    pub max_length: Option<u32>,
    #[serde(default)]
    pub visible_if: Option<String>,
    #[serde(default)]
    pub required_if: Option<String>,
    #[serde(default)]
    pub read_only_if: Option<String>,
    /// `link_field.target_field`: copied from the linked record when the link changes.
    #[serde(default)]
    pub fetch_from: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Field-level permission group; roles grant read/write per group.
    #[serde(default)]
    pub perm_group: Option<String>,
    #[serde(default)]
    pub on_delete: OnDelete,
    /// Static equality filters applied to link candidates and validated on write, e.g. `{is_group: false}`.
    #[serde(default)]
    pub link_filters: Option<serde_json::Map<String, Value>>,
}

impl FieldDef {
    pub fn label(&self) -> String {
        self.label.clone().unwrap_or_else(|| humanize(&self.name))
    }
}

pub fn humanize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, part) in name.trim_start_matches("x_").split('_').filter(|p| !p.is_empty()).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut c = part.chars();
        if let Some(f) = c.next() {
            out.extend(f.to_uppercase());
            out.push_str(c.as_str());
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub permissions: Vec<PermissionDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PermissionDef {
    /// Entity name or `*`.
    pub entity: String,
    #[serde(default)]
    pub read: bool,
    #[serde(default)]
    pub write: bool,
    #[serde(default)]
    pub create: bool,
    #[serde(default)]
    pub delete: bool,
    #[serde(default)]
    pub submit: bool,
    #[serde(default)]
    pub cancel: bool,
    #[serde(default)]
    pub export: bool,
    #[serde(default)]
    pub import: bool,
    /// Only records the user created.
    #[serde(default)]
    pub if_owner: bool,
    /// Field groups readable/writable beyond the default group.
    #[serde(default)]
    pub read_groups: Vec<String>,
    #[serde(default)]
    pub write_groups: Vec<String>,
    /// Only records whose fields hold these values, all of them: `{field: value}` or
    /// `{field: [values]}`. Combines with `if_owner`; a role may hold several rules for one
    /// entity with different conditions, and a record qualifies through any of them.
    #[serde(default, rename = "where", skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub condition: std::collections::BTreeMap<String, Value>,
}

/// Field types a permission condition may test (compared as text, so they must be exact).
const CONDITION_TYPES: &[FieldType] = &[FieldType::Data, FieldType::Select, FieldType::Check, FieldType::Link, FieldType::Email, FieldType::Int];

impl PermissionDef {
    /// Checks `where` against the entity's fields (`field` looks one up by name).
    pub fn check_condition<'a>(&self, field: impl Fn(&str) -> Option<&'a FieldDef>) -> Result<(), String> {
        if self.condition.is_empty() {
            return Ok(());
        }
        if self.entity == "*" {
            return Err("a condition needs one entity, not *".into());
        }
        if self.condition.len() > 10 {
            return Err("at most 10 fields in a condition".into());
        }
        for (name, v) in &self.condition {
            let f = field(name).ok_or_else(|| format!("condition field {name} is not a field of {}", self.entity))?;
            if !CONDITION_TYPES.contains(&f.ftype) {
                return Err(format!("condition field {name} must be data, select, check, link, email or int"));
            }
            let values = match v {
                Value::Array(a) if !a.is_empty() && a.len() <= 50 => a.as_slice(),
                Value::Array(_) => return Err(format!("condition field {name}: give 1 to 50 values")),
                one => std::slice::from_ref(one),
            };
            for x in values {
                let ok = match f.ftype {
                    FieldType::Check => x.is_boolean(),
                    FieldType::Int => x.as_i64().is_some(),
                    // A record id, as links store it: lowercase and hyphenated.
                    FieldType::Link => x.as_str().is_some_and(|s| {
                        s.len() == 36
                            && s.char_indices()
                                .all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_digit() || ('a'..='f').contains(&c) })
                    }),
                    _ => x.as_str().is_some_and(|s| !s.trim().is_empty() && s.len() <= 200),
                };
                if !ok {
                    return Err(format!("condition field {name}: {x} is not a value of a {:?} field", f.ftype).to_lowercase());
                }
            }
        }
        Ok(())
    }

    /// The condition as the values each field may hold, as text (how the database compares them).
    pub fn condition_values(&self) -> Vec<(String, Vec<String>)> {
        // Stored text values are trimmed, so conditions compare trimmed values too.
        let text = |v: &Value| match v {
            Value::String(s) => s.trim().to_string(),
            other => other.to_string(),
        };
        self.condition
            .iter()
            .map(|(k, v)| {
                let mut vals: Vec<String> = match v {
                    Value::Array(a) => a.iter().map(text).collect(),
                    one => vec![text(one)],
                };
                vals.sort();
                vals.dedup();
                (k.clone(), vals)
            })
            .collect()
    }

    /// The condition in one canonical form, telling apart rules of one role and entity ('' = none).
    pub fn condition_key(&self) -> String {
        if self.condition.is_empty() { String::new() } else { serde_json::to_string(&self.condition_values()).unwrap_or_default() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDef {
    pub name: String,
    pub entity: String,
    pub initial_state: String,
    pub states: Vec<WorkflowState>,
    pub transitions: Vec<WorkflowTransition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowState {
    pub name: String,
    /// Document status this state corresponds to (0 draft, 1 submitted, 2 cancelled).
    #[serde(default)]
    pub docstatus: i16,
    /// Roles allowed to edit the document while in this state.
    #[serde(default)]
    pub edit_roles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowTransition {
    pub action: String,
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub roles: Vec<String>,
    /// CEL guard on `doc`/`user`.
    #[serde(default)]
    pub condition: Option<String>,
    /// The acting user may not be the document's creator (four-eyes principle).
    #[serde(default)]
    pub not_creator: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedDef {
    pub entity: String,
    /// Stable key fields used to make seeding idempotent.
    pub key: Vec<String>,
    pub records: Vec<serde_json::Map<String, Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Overlay {
    pub entity: String,
    pub ops: Vec<OverlayOp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum OverlayOp {
    AddField {
        field: FieldDef,
        #[serde(default)]
        after: Option<String>,
    },
    SetProperty {
        field: String,
        property: String,
        value: Value,
    },
    AddRule {
        rule: RuleDef,
    },
    AddOptions {
        field: String,
        options: Vec<String>,
    },
    SetEntityProperty {
        property: String,
        value: Value,
    },
    /// Tenant-only: a new entity stored in the generic `custom_record` table (name must start with `X`).
    DefineEntity {
        entity: Box<EntityDef>,
    },
    /// Tenant-only: attach (or replace) the approval workflow of an entity.
    SetWorkflow {
        workflow: WorkflowDef,
    },
    /// Tenant-only: replace the HTML print template of an entity.
    SetPrintTemplate {
        template: String,
    },
    /// Package-only: a document action on another package's entity, implemented by the overlaying
    /// package's hooks (e.g. Projects adds "Make Sales Invoice" to core's Project).
    AddAction {
        action: ActionDef,
    },
}
