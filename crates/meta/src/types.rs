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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handover: Vec<HandoverDef>,
    /// Engine tables a workspace export carries (journals, stock moves): whole rows, so none of
    /// these may hold a secret. Tables not named here are listed in the export as left out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub export_tables: Vec<String>,
    /// Engine tables holding links to a `mergeable` entity, moved when two records merge. Not
    /// append-only history: that keeps the id it was posted under (`ferrum_kernel::merge::merged_ids`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merge_links: Vec<MergeLinkDef>,
    /// Data a release moves once its schema has grown: in batches, per workspace, resumed where it
    /// stopped (`ferrum_kernel::backfill`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backfills: Vec<BackfillDef>,
    /// Business events this package publishes besides its records' own (created, submitted and the
    /// rest), for processes to start on and wait for (docs/logic). Topic `{package}.{name}`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<EventDef>,
    /// The messages this package sends to people, each a template a workspace may edit (BD-20). Key
    /// `{package}.{name}`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<MessageDef>,
    /// The documents and files this package renders from its records, each a template a workspace may edit (BD-21).
    /// Key `{package}.{name}`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub documents: Vec<DocumentDef>,
    /// Approval policies on submitting or cancelling this package's own documents (BD-19), as defaults: a workspace
    /// switches one off, replaces or edits it with `set_approval`, and a release changing it flags the workspace's
    /// (a transition's default is the `approval` of the package's workflow, changed with `set_transition_approval`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<PackageApproval>,
    /// Automations on this package's own entities, installed in each workspace as defaults it may switch off or edit:
    /// a release changes one the workspace has not edited, and flags one it has.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub automations: Vec<PackageAutomation>,
}

/// A package's approval policy on submitting or cancelling its documents ([`PackageManifest::approvals`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageApproval {
    pub entity: String,
    pub on: DocumentAction,
    pub policy: ApprovalPolicy,
}

/// A package's automation ([`PackageManifest::automations`]): what an administrator saves as one, `name` unique in the
/// package and the workspace's copy found by it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageAutomation {
    pub name: String,
    pub entity: String,
    /// `created`, `updated`, `submitted`, `cancelled`, `deleted`, `transitioned`, or `scheduled` with `schedule`.
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<Value>,
    pub actions: Vec<Value>,
}

impl PackageAutomation {
    /// As a workspace's automation row holds it (what is compared to tell an edited copy).
    pub fn definition(&self) -> Value {
        serde_json::json!({"name": self.name, "entity": self.entity, "event": self.event, "condition": self.condition,
            "schedule": self.schedule, "actions": self.actions})
    }
}

/// What a document template makes: a page (HTML, a PDF where the server makes PDFs), text (fixed-width, a label
/// printer's language), CSV, or XML. Each escapes what it prints for its own syntax.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentOutput {
    #[default]
    Page,
    Text,
    Csv,
    Xml,
}

impl DocumentOutput {
    /// The file extension and content type of what it makes, as text.
    pub fn file_type(self) -> (&'static str, &'static str) {
        match self {
            DocumentOutput::Page => ("html", "text/html"),
            DocumentOutput::Text => ("txt", "text/plain"),
            DocumentOutput::Csv => ("csv", "text/csv"),
            DocumentOutput::Xml => ("xml", "application/xml"),
        }
    }
}

/// The encodings a text, CSV or XML file is written in. A character the encoding has no place for is refused, never
/// replaced.
pub const DOCUMENT_ENCODINGS: [&str; 3] = ["utf-8", "iso-8859-1", "ascii"];

/// A document template (docs/builder/decisions.md BD-21): a package's default, or a workspace's own or edited copy.
/// Bodies are MiniJinja over `doc` (the record with its lines), `links`, `settings`, `values`, `entity`, `language`,
/// `currency` and `now`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentDef {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The entity whose record it renders.
    pub entity: String,
    #[serde(default)]
    pub output: DocumentOutput,
    /// The one its entity prints with when no template is picked. A workspace's default wins over a package's.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
    /// The roles that may pick it; none: whoever may print the record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// For text, CSV and XML: one of [`DOCUMENT_ENCODINGS`] (UTF-8 when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// For text, CSV and XML: every line ends in CR LF (a bank's or a filing's format), else LF.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub crlf: bool,
    /// The names of the values its caller passes (`period`): a body reading another is refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// Language tag (`en`, `hi`, `pt-BR`) → its bodies.
    pub languages: std::collections::BTreeMap<String, DocumentBody>,
    /// A query's rows in place of one record: `entity` is then the entity the query reads, and bodies read `rows`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<DocumentQuery>,
}

/// What a document about a query renders: a saved report's rows, one file for the whole result or, with `group_by`
/// (a column of its rows that links to a record: a customer, an employee), one file per record linked, attached to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentQuery {
    /// The saved report's label.
    pub report: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<String>,
}

/// A document's bodies in one language: the body, and for a page the header and footer each PDF page repeats (where
/// they may say `page_number()` and `page_count()`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentBody {
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footer: Option<String>,
}

impl DocumentDef {
    /// What is wrong with its shape (its bodies are the kernel's to check): a key as a message's, a language with a
    /// body, a header or footer only on a page, an encoding there is, values that are names, roles that have one.
    pub fn problem(&self) -> Option<String> {
        let k = &self.key;
        let well_formed = k.len() <= 100
            && k.starts_with(|c: char| c.is_ascii_lowercase())
            && k.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
            && !k.ends_with('.')
            && !k.contains("..");
        if !well_formed {
            return Some(format!("document key '{k}' must be written [a-z][a-z0-9_.]*, at most 100 characters"));
        }
        if self.languages.is_empty() || self.languages.values().any(|b| b.body.trim().is_empty()) {
            return Some(format!("document {k} has a language without a body, or none"));
        }
        if let Some(l) = self.languages.keys().find(|l| l.is_empty() || l.len() > 20 || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')) {
            return Some(format!("document {k}: '{l}' is not a language tag"));
        }
        if self.output != DocumentOutput::Page && self.languages.values().any(|b| b.header.is_some() || b.footer.is_some()) {
            return Some(format!("document {k}: only a page has a header and a footer"));
        }
        if self.output == DocumentOutput::Page && (self.encoding.is_some() || self.crlf) {
            return Some(format!("document {k}: a page is UTF-8 with its own line ends; encoding and crlf are for files"));
        }
        if let Some(e) = self.encoding.as_deref().filter(|e| !DOCUMENT_ENCODINGS.contains(e)) {
            return Some(format!("document {k}: encoding '{e}' is not one of {}", DOCUMENT_ENCODINGS.join(", ")));
        }
        if self.roles.iter().any(|r| r.trim().is_empty()) {
            return Some(format!("document {k}: a role has a name"));
        }
        if let Some(v) = self.values.iter().find(|v| !crate::is_ident(v)) {
            return Some(format!("document {k}: '{v}' is not a name"));
        }
        if let Some(q) = &self.query
            && (q.report.trim().is_empty() || q.group_by.as_deref().is_some_and(|g| g.trim().is_empty()) || self.default)
        {
            return Some(format!("document {k}: a query's document names its report (and a column to group by, when it groups), and is no record's default"));
        }
        None
    }
}

/// The channels a message can go on, in the order tried when a template names none.
pub const MESSAGE_CHANNELS: [&str; 4] = ["email", "text", "chat", "inapp"];

/// A message template (docs/builder/decisions.md BD-20): a package's default, or a workspace's own or edited copy.
/// Bodies are MiniJinja over `doc` (the record), `settings`, `values` (what the sender passes) and `opt_out`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageDef {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The entity whose record it is about, when it is about one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
    #[serde(default)]
    pub category: MessageCategory,
    /// The channels tried, in order; none: every channel it has a body for, in [`MESSAGE_CHANNELS`] order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,
    /// The names of the values its sender passes (`code`, `link`): a body reading another is refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// Language tag (`en`, `pt-BR`) → its bodies.
    pub languages: std::collections::BTreeMap<String, MessageBodies>,
}

/// Operational messages (a confirmation, a code, a receipt) always go; promotional ones respect opt-outs and quiet hours.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageCategory {
    #[default]
    Operational,
    Promotional,
}

/// A message's bodies in one language, one per channel it has.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageBodies {
    /// `subject` (text) and `body` (HTML: every value it prints is escaped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<MailBody>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<TextBody>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<ChatBody>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inapp: Option<NoticeBody>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MailBody {
    pub subject: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextBody {
    pub text: String,
}

/// A chat-app message: the provider's pre-approved template by name with its ordered values, or free text where the
/// provider allows it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoticeBody {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl MessageBodies {
    /// Whether it has a body for `channel`.
    pub fn has(&self, channel: &str) -> bool {
        match channel {
            "email" => self.email.is_some(),
            "text" => self.text.is_some(),
            "chat" => self.chat.is_some(),
            "inapp" => self.inapp.is_some(),
            _ => false,
        }
    }
}

impl MessageDef {
    /// The channels tried, in order.
    pub fn order(&self) -> Vec<String> {
        if !self.channels.is_empty() {
            return self.channels.clone();
        }
        MESSAGE_CHANNELS.iter().filter(|c| self.languages.values().any(|b| b.has(c))).map(|c| c.to_string()).collect()
    }

    /// What is wrong with its shape (its bodies' templates are the kernel's to check): a key `[a-z][a-z0-9_.]*` of at
    /// most 100 characters, a language with a body, channels it has bodies for, a chat body with a template or text.
    pub fn problem(&self) -> Option<String> {
        let k = &self.key;
        let well_formed = k.len() <= 100
            && k.starts_with(|c: char| c.is_ascii_lowercase())
            && k.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
            && !k.ends_with('.')
            && !k.contains("..");
        if !well_formed {
            return Some(format!("message key '{k}' must be written [a-z][a-z0-9_.]*, at most 100 characters"));
        }
        if self.languages.is_empty() || self.languages.values().all(|b| MESSAGE_CHANNELS.iter().all(|c| !b.has(c))) {
            return Some(format!("message {k} has no body in any language"));
        }
        if let Some(l) = self.languages.keys().find(|l| l.is_empty() || l.len() > 20 || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')) {
            return Some(format!("message {k}: '{l}' is not a language tag"));
        }
        if let Some(c) = self.channels.iter().find(|c| !MESSAGE_CHANNELS.contains(&c.as_str()) || !self.languages.values().any(|b| b.has(c))) {
            return Some(format!("message {k}: channel '{c}' is not one of {} it has a body for", MESSAGE_CHANNELS.join(", ")));
        }
        if self.languages.values().filter_map(|b| b.chat.as_ref()).any(|c| c.template.is_none() == c.text.is_none()) {
            return Some(format!("message {k}: a chat body has either a provider's template or text"));
        }
        if let Some(v) = self.values.iter().find(|v| !crate::is_ident(v)) {
            return Some(format!("message {k}: value '{v}' is not a name"));
        }
        None
    }
}

/// A business event a package publishes through the outbox.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EventDef {
    /// `[a-z][a-z0-9_.]*`, unique in the package (`order_acknowledged`).
    pub name: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The entity it is about, when it is about a record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
    /// JSON Schema of its payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

/// What an action promises beyond its parameters (docs/logic, FLC-CAP-001). Everything is optional and
/// derived when left out: a read-only action is a query, any other a transactional action with a
/// receipt committed beside it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ActionContract {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<ferrum_logic::capability::Effect>,
    /// JSON Schema of what the action returns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returns: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency: Option<ferrum_logic::capability::Idempotency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown_outcome: Option<ferrum_logic::capability::UnknownOutcome>,
    /// What it means when it reports done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<String>,
    /// Another action of the same entity that undoes this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensate_with: Option<String>,
    /// That the action is on its way out: what replaces it and until when (FLC-CAP-008). Releases bound to it keep
    /// it; one checked against it is told.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<String>,
    /// Fields of what it returns that are given to the caller once and never kept with its receipt (a new card's
    /// code): a repeat under the same key gets the rest, and is told so and what to do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_once: Option<AnswerOnce>,
}

/// Result fields given once ([`ActionContract::answer_once`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AnswerOnce {
    pub fields: Vec<String>,
    /// What a caller whose first answer was lost should do ("Make a new code with New Code").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub then: Option<String>,
}

impl AnswerOnce {
    /// What a repeat under the same key is told, for an action labelled `label`.
    pub fn told(&self, label: &str) -> String {
        let then = self.then.as_deref().unwrap_or("What was done stands, and was not done again.");
        format!("{label} was done under this key, and its {} were given in that first answer only: they are not kept. {then}", self.fields.join(", "))
    }
}

/// Rows of an engine table (`table`) or of an entity's (`entity`) that a release changes in
/// batches: those that still `need` it (an SQL condition over the row as `t`) get what it `set`s
/// (SQL assignments, their values over `t`). A set should leave a row no longer needing it; the
/// rows that still do when it is done are counted in its report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BackfillDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
    pub needs: String,
    pub set: String,
    /// Rows a batch changes, each batch in its own transaction (1000 when unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<u32>,
}

/// A column of an engine table that holds a record's id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MergeLinkDef {
    pub table: String,
    pub column: String,
    pub to: String,
}

/// Records of one entity that belong to a person: those whose `match` field (an email field, or a
/// data field holding an address) holds the person's email (and whose fields hold the `where` values); `erase` lists what erasure blanks
/// (a child table listed there loses its rows).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PersonalDataDef {
    pub entity: String,
    /// The field holding the person's address, their phone field (`phone` as well) where the records
    /// hold no address, a link to a record another declaration finds by address or number, or
    /// `table.field` for people held in a child table's rows (only those rows are theirs).
    #[serde(rename = "match")]
    pub match_field: String,
    /// The fields an erasure blanks. Empty with `retain`.
    #[serde(default)]
    pub erase: Vec<String>,
    /// Why these records stay when someone asks for erasure: exported, never blanked, and named in
    /// the answer so that nobody takes them for erased.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain: Option<String>,
    #[serde(default, rename = "where", skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub condition: std::collections::BTreeMap<String, Value>,
    /// A field holding the person's phone number: asked with a number, records whose number has the
    /// same digits (however it was written) are theirs as well.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    /// A field holding their name: asked with a name, records holding it are listed as possibly
    /// theirs, for a person to judge, and never erased for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Work a person is named on, which moves to whoever takes over when they leave: an email field of
/// a record, or `table.field` of its child rows, on the records `where` picks (open ones, usually;
/// what is finished keeps the name of who did it).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HandoverDef {
    pub entity: String,
    pub field: String,
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
    /// The package that defines it: set in the effective schema, never written in package.yaml
    /// (`tenant` for a workspace's own menu).
    #[serde(default, skip_deserializing, skip_serializing_if = "String::is_empty")]
    pub package: String,
    /// A workspace's own menu only (`define_module`): an app gives these through its manifest.
    /// The launcher tile's colour, `#rrggbb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Entity items offered in the bar's Create menu.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub create: Vec<String>,
    /// What the assistant is told on the menu's pages, and suggests asking there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<ModuleAssistant>,
    /// Shown in the workspace's portal too (docs/builder/portal.md): portal people whose portal roles
    /// reach some of its entities get it as an entry of the portal's home, on Studio's generic page.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub portal: bool,
}

impl ModuleDef {
    /// Whether it says what only a workspace's own menu may.
    pub fn has_menu_properties(&self) -> bool {
        self.color.is_some() || self.description.is_some() || !self.create.is_empty() || self.assistant.is_some() || self.portal
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleAssistant {
    pub instructions: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<String>,
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
    /// Who may see this entity's titles where a record they can read links to it.
    #[serde(default)]
    pub reference: Reference,
    #[serde(default)]
    pub numbering: Option<NumberingDef>,
    #[serde(default)]
    pub submittable: bool,
    /// Not to leave for an AI provider, whatever the person reading it may see, unless a workspace
    /// says otherwise (pay, health, personnel files).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub internal: bool,
    /// Duplicates can be merged into one record (crates/kernel/src/merge.rs).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mergeable: bool,
    /// Fields that, equal apart from case and spaces, suggest two records are one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub duplicate_fields: Vec<String>,
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
    /// Service targets a workspace set with `add_target` (docs/api.md, "Service targets").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<TargetDef>,
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

impl EntityDef {
    /// `label_plural`, or the label with an "s"; a label ending in "s" ("Booking Settings", "Lesson
    /// Progress") stays as it is.
    pub fn plural_label(&self) -> String {
        match &self.label_plural {
            Some(plural) => plural.clone(),
            None if self.label.ends_with('s') => self.label.clone(),
            None => format!("{}s", self.label),
        }
    }
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
    /// Only when a workflow transition is taken (never on a save or a submit of its own), on the record as it would
    /// be in its new state; the check also reads `transition`: `{action, from, to, params}` (BD-19).
    Transition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleDef {
    pub name: String,
    /// CEL expression that must evaluate to `true`.
    pub check: String,
    /// What a person is told; it may show the record's values (`{{ doc.title }}`), as that person may read them.
    pub message: String,
    #[serde(default)]
    pub on: RuleOn,
    /// The field the refusal is shown on (default: the rule's name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
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
    /// What the action asks for before it runs (the UI shows a dialog; the API takes them as the
    /// action's parameters).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<ActionParam>,
    /// Actions of one group show under one menu (e.g. "e-Invoice").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Called by plugins and other code, never offered to a person as a button.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// What it promises to processes and agents (docs/logic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<ActionContract>,
    /// A configured action (one with `effects`): the roles one of which its runner holds (an administrator
    /// always may), besides the permission it names. Empty: the permission alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// A configured action: the first of these that holds refuses it, as forbidden, with its message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refuse_if: Vec<ActionRefusal>,
    /// What a configured action does, in order, in its one transaction, as the system: automation actions
    /// (`set_fields`, `update_linked`, `create_document`, `notify`, `email`), checked by the kernel when the
    /// customization is saved. An action with effects needs no handler.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<Value>,
}

/// A service target: "this must happen within so much working time" (docs/api.md, "Service targets"). The
/// expressions see `doc`, `user`, `today` and `now`; the kernel keeps `due_field` and `state_field`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetDef {
    pub name: String,
    pub label: String,
    /// A business calendar's name; none counts plain elapsed time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar: Option<String>,
    /// The clock starts at the first save on which it holds; without it, when the record is created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_when: Option<String>,
    /// Seconds or an ISO 8601 duration (`PT8H`); null or empty: the record has no such target.
    pub within: String,
    /// While it holds the clock is stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_when: Option<String>,
    pub met_when: String,
    /// A datetime field: the due moment.
    pub due_field: String,
    /// A data or select field: `running`, `paused`, `met`, `met_late` or `breached`.
    pub state_field: String,
}

/// A published list (`publish_list`, BD-15): the records of an entity that `filter` picks, in the order
/// `sort` says, showing only `fields`, to anyone at `/api/public/{workspace}/lists/{name}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedListDef {
    /// Lowercase letters, digits and hyphens: part of a public address, one of a kind in the workspace.
    pub name: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The only fields sent, in this order: the entity's own stored fields (or `number`).
    pub fields: Vec<String>,
    /// `[[field, op, value], …]` on any stored field of the entity, as list queries take them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filter: Vec<(String, String, Value)>,
    /// `[field, "asc" | "desc"]`; newest first without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<(String, SortDirection)>,
    /// Also offered as CSV.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub csv: bool,
    /// Rows at most (200 without it, 1000 at most).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDirection {
    Asc,
    Desc,
}

/// A published list and the entity it shows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PublishedList {
    pub entity: String,
    pub list: PublishedListDef,
}

/// A reason a configured action refuses to run: `when` (CEL over `doc`, `user` and `params`) holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRefusal {
    pub when: String,
    pub message: String,
}

/// One input an action asks for.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionParam {
    pub name: String,
    pub label: String,
    /// `data` (one line), `text` (several lines), `select` (one of `options`), `int`, `date`, `check` (yes or no,
    /// shown as a tick), or `link` (a record of `to`).
    #[serde(rename = "type", default = "default_param_type")]
    pub ptype: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A secret (a card's PIN): the handler reads it, and the audit log, the receipts and error messages show it as
    /// [`REDACTED`]; the digest a receipt is claimed with holds it only keyed with the server's secret key. An action's
    /// only: a transition keeps what it is given.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub secret: bool,
}

/// What a secret parameter is shown as wherever it would be kept.
pub const REDACTED: &str = "[redacted]";

impl ActionDef {
    /// The text of each secret parameter given in `params` (not empty), for keeping them out of what is written.
    pub fn secret_values(&self, params: &Value) -> Vec<String> {
        self.params
            .iter()
            .filter(|p| p.secret)
            .filter_map(|p| match params.get(&p.name)? {
                Value::Null => None,
                Value::String(s) => Some(s.clone()),
                other => Some(other.to_string()),
            })
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// `params` with each secret parameter given shown as [`REDACTED`].
    pub fn redacted(&self, params: &Value) -> Value {
        let mut out = params.clone();
        if let Some(o) = out.as_object_mut() {
            for p in self.params.iter().filter(|p| p.secret) {
                if let Some(v) = o.get_mut(&p.name).filter(|v| !v.is_null()) {
                    *v = Value::String(REDACTED.into());
                }
            }
        }
        out
    }

    /// The fields of its result given once (`contract.answer_once`).
    pub fn answer_once(&self) -> Option<&AnswerOnce> {
        self.contract.as_ref().and_then(|c| c.answer_once.as_ref()).filter(|a| !a.fields.is_empty())
    }

    /// `result` as a receipt keeps it: without the fields given once.
    pub fn kept_result(&self, result: &Value) -> Value {
        let mut out = result.clone();
        if let (Some(a), Some(o)) = (self.answer_once(), out.as_object_mut()) {
            for f in &a.fields {
                o.remove(f);
            }
        }
        out
    }
}

fn default_param_type() -> String {
    "data".into()
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
    /// Whether a value stored under `self` still reads as `to` (a custom entity's redefinition may
    /// change a field this way while records hold values in it): free text among its kinds, a
    /// choice to free text, a whole number to a decimal one, decimal kinds among themselves.
    pub fn converts_to(self, to: FieldType) -> bool {
        use FieldType::*;
        let free = |t: FieldType| matches!(t, Data | Text | Markdown | Code | Email | Phone | Url);
        self == to || ((free(self) || self == Select) && free(to)) || ((self == Int || self.is_decimal()) && to.is_decimal())
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
    /// Never in a list's export (CSV, spreadsheet), whoever exports: read on the record, not taken
    /// away in bulk (a bank account or identity number).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_export: bool,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    pub default_expr: Option<String>,
    /// CEL computed value; the field becomes read-only. Not for money (CEL has no decimal type).
    #[serde(default)]
    pub computed: Option<String>,
    /// An exact total over the record's own lines, computed by the server at every save (and in
    /// the draft preview) before validation; a value sent by a client is overwritten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollup: Option<RollupDef>,
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
    /// What a person is told when a change they send is refused because `read_only_if` holds; without it the
    /// change is ignored, as for a `read_only` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_message: Option<String>,
    /// `link_field.target_field`: copied from the linked record when the link changes.
    #[serde(default)]
    pub fetch_from: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// How forms edit the field where its type alone does not say: `studio_query` for a json field
    /// holding a Studio query, which forms build with Studio's report builder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widget: Option<String>,
    /// Field-level permission group; roles grant read/write per group.
    #[serde(default)]
    pub perm_group: Option<String>,
    /// For a field in a `perm_group`: who is outside the group sees this much of it, not nothing.
    /// It stays closed to them in every other way: they cannot filter or sort on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask: Option<FieldMask>,
    #[serde(default)]
    pub on_delete: OnDelete,
    /// Static equality filters applied to link candidates and validated on write, e.g. `{is_group: false}`.
    #[serde(default)]
    pub link_filters: Option<serde_json::Map<String, Value>>,
}

/// `sum` of a numeric field of a table's rows, or `count` of its rows, in exact decimals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollupDef {
    pub table: String,
    pub op: RollupOp,
    /// The row field summed; not for `count`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollupOp {
    Sum,
    Count,
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
    /// For the portal (docs/builder/decisions.md BD-02): portal people hold only such roles, and
    /// such a role holds only own-record rules (see [`PermissionDef::portal_problem`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub portal: bool,
}

/// What a record shows of the records it links to.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reference {
    /// The target's title, to anyone who can read the record linking to it (a sales invoice names
    /// its customer, whether or not the reader may open the customer).
    #[default]
    Open,
    /// Only to those who may read the target itself; others see that there is a link, not to what.
    Restricted,
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
    /// Only records whose fields hold these values, all of them: `{field: value}`,
    /// `{field: [values]}`, or a range on a number or date, `{field: {lte: 10000}}` (see
    /// [`ConditionRange`]). Combines with `if_owner`; a role may hold several rules for one
    /// entity with different conditions, and a record qualifies through any of them.
    #[serde(default, rename = "where", skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub condition: std::collections::BTreeMap<String, Value>,
    /// Own records reached through a path rather than by who made them (BD-02): at most two links,
    /// then optionally a child table, ending at a field holding the person's email (`patient.email`,
    /// `student.guardians.email`; see [`crate::via`]). Makes the rule an own-record rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// The entity's actions its holders may run on the records this rule reaches, whatever the
    /// action's own `permission` asks (BD-02: a patient cancels their visit without editing it). The
    /// action's condition and its app's checks still apply.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<String>,
    /// What a portal role's holders see of the conversation on the records this rule lets them read: `read`, the
    /// comments and files staff marked shared with outside people; `add`, those and their own beside them. None:
    /// nothing of it (the default). Members read every comment and file of a record they read, whatever this says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared: Option<String>,
}

/// What a rule's `shared` may say.
pub const SHARED_REACH: &[&str] = &["read", "add"];

/// Whether records of `e` hold personal data: a package declares them its people's (`declared`), or
/// they keep an email or a phone number. A portal role reads such records only as its holders' own.
pub fn holds_personal_data(e: &EntityDef, declared: impl Fn(&str) -> bool) -> bool {
    declared(&e.name) || e.fields.iter().any(|f| matches!(f.ftype, FieldType::Email | FieldType::Phone))
}

/// Field types a permission condition may test (compared as text, so they must be exact).
const CONDITION_TYPES: &[FieldType] = &[FieldType::Data, FieldType::Select, FieldType::Check, FieldType::Link, FieldType::Email, FieldType::Int];

/// A range a field's value must fall in, ends included: `{lte: 10000}` on a number, `{gte:
/// "2026-01-01"}` on a date, or on a date days from today, `{from_today: 0, to_today: 7}` ("due in
/// the next week"), read in the workspace's time zone each time the rule is used. A record with no
/// value in the field is outside every range.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionRange {
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "number_or_text")]
    pub gte: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "number_or_text")]
    pub lte: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_today: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_today: Option<i32>,
}

fn number_or_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(_) => Err(serde::de::Error::custom("give a number or a date")),
    }
}

impl ConditionRange {
    fn check(&self, name: &str, ftype: FieldType) -> Result<(), String> {
        let relative = self.from_today.is_some() || self.to_today.is_some();
        if self.gte.is_none() && self.lte.is_none() && !relative {
            return Err(format!("condition field {name}: give gte, lte, from_today or to_today"));
        }
        if relative && (self.gte.is_some() || self.lte.is_some()) {
            return Err(format!("condition field {name}: use dates or days from today, not both"));
        }
        let date = matches!(ftype, FieldType::Date | FieldType::Datetime);
        if relative && !date {
            return Err(format!("condition field {name}: days from today work on dates"));
        }
        if !date && !ftype.is_numeric() {
            return Err(format!("condition field {name}: ranges work on numbers and dates"));
        }
        for end in [&self.gte, &self.lte].into_iter().flatten() {
            let ok = if date { chrono::NaiveDate::parse_from_str(end, "%Y-%m-%d").is_ok() } else { end.parse::<rust_decimal::Decimal>().is_ok() };
            if !ok {
                return Err(format!("condition field {name}: {end} is not a {}", if date { "date (YYYY-MM-DD)" } else { "number" }));
            }
        }
        Ok(())
    }
}

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
            if v.is_object() {
                let range: ConditionRange = serde_json::from_value(v.clone()).map_err(|e| format!("condition field {name}: {e}"))?;
                range.check(name, f.ftype)?;
                continue;
            }
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
            .filter(|(_, v)| !v.is_object())
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

    /// The condition's ranges; `None` when one no longer reads as a range (the rule then grants
    /// nothing).
    pub fn condition_ranges(&self) -> Option<Vec<(String, ConditionRange)>> {
        self.condition.iter().filter(|(_, v)| v.is_object()).map(|(k, v)| Some((k.clone(), serde_json::from_value(v.clone()).ok()?))).collect()
    }

    /// The condition in one canonical form, telling apart rules of one role and entity ('' = none).
    pub fn condition_key(&self) -> String {
        // A rule reaching records through a path is another rule than one reaching them otherwise.
        let via = self.via.as_deref().map(|v| format!("via:{}", v.trim())).unwrap_or_default();
        if self.condition.is_empty() {
            return via;
        }
        let key = match self.condition_ranges().filter(|r| !r.is_empty()) {
            Some(ranges) => serde_json::to_string(&(self.condition_values(), ranges)).unwrap_or_default(),
            None => serde_json::to_string(&self.condition_values()).unwrap_or_default(),
        };
        if via.is_empty() { key } else { format!("{via}|{key}") }
    }

    /// Whether the actions it names are the entity's: `None` when they are.
    pub fn unknown_action(&self, entity: Option<&EntityDef>) -> Option<String> {
        if self.actions.is_empty() {
            return None;
        }
        let Some(e) = entity else { return Some("a rule for every entity names no actions".into()) };
        self.actions.iter().find(|a| !e.actions.iter().any(|d| &d.name == *a)).map(|a| format!("{} has no action {a}", e.name))
    }

    /// Why this rule cannot belong to a portal role, if it cannot (BD-02): a portal role reaches its
    /// holders' own records, and reads reference data (`personal`: whether the entity holds personal
    /// data, see [`holds_personal_data`]) whole; it never deletes, cancels, exports or imports.
    pub fn portal_problem(&self, personal: bool) -> Option<String> {
        if self.entity == "*" {
            return Some("a portal role names each entity it reaches, never every entity".into());
        }
        if !self.if_owner && self.via.is_none() {
            // Reference data (consent forms, appointment types) may be read whole, to pick from.
            let reads_only = !(self.write || self.create || self.submit || !self.actions.is_empty() || !self.write_groups.is_empty() || self.shared.is_some());
            if personal || !reads_only {
                return Some(format!(
                    "a portal role reaches only its holders' own {}: make the rule own records only, or give the path they are reached by \
                     (every record only to read, and only of an entity holding no personal data)",
                    self.entity
                ));
            }
        }
        let refused: Vec<&str> = [(self.delete, "delete"), (self.cancel, "cancel"), (self.export, "export"), (self.import, "import")]
            .into_iter()
            .filter_map(|(on, verb)| on.then_some(verb))
            .collect();
        (!refused.is_empty()).then(|| format!("a portal role may not {} {}", refused.join(", "), self.entity))
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
    /// The only fields a person may change while the document is in this state (the server's own
    /// updates are not held to it). Empty: whatever the entity allows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub editable: Vec<String>,
    /// Fields a document needs to enter this state, and to be saved while in it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
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
    /// Links whose records the acting user may not have raised: `field`, or `table.field` for a child
    /// table's links. Whoever raised an order does not approve paying it (FEO-013).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_creator_of: Vec<String>,
    /// What whoever takes it is asked for, declared as a button's are; a required one missing refuses it. Its
    /// effects read them as `params`, rules `on: transition` as `transition.params`, and the history keeps them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<ActionParam>,
    /// What it does once taken, in its transaction, as the system on the taker's behalf: a configured action's
    /// effects (BD-11, BD-19), checked by the kernel when the customization is saved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<Value>,
    /// Who must agree before it completes (BD-19): taking it then asks for approval, and the record stays where it is
    /// until the policy is satisfied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalPolicy>,
}

/// An approval policy on a transition (docs/api.md, "Approvals"): steps in order, a step marked `with_next` opening
/// with the one after it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalPolicy {
    pub steps: Vec<ApprovalStep>,
    /// Whoever took the transition may approve it too (never by default).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_requester: bool,
    /// Fields whose change while it is pending cancels the decisions so far (an amount); other edits leave them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reset_on: Vec<String>,
    /// The state a rejection moves the record to (one of the same document status); without it the record stays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_reject: Option<String>,
    /// Fields an approver reading the record while their decision is pending sees though a field group hides them
    /// from them (none by default).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub show: Vec<String>,
    /// While it is pending the record is not edited, but for a change to a `reset_on` field (which voids the
    /// decisions so far); without it edits are allowed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lock: bool,
    /// On a document's submit or cancel: the system (an automation, another app's engine) goes ahead without asking.
    /// Never by default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub system_bypass: bool,
    /// A message template (BD-20) its notices are sent with instead of the fixed wording: it reads `values.notice`
    /// (`waiting`, `reminder`, `escalated`, `approved`, `rejected`, `not_done`) and `values.title`, the fixed wording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// What a document approval policy is on (`set_approval`, BD-19): submitting a submittable entity's documents, or
/// cancelling them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentAction {
    Submit,
    Cancel,
}

impl DocumentAction {
    pub fn as_str(self) -> &'static str {
        match self {
            DocumentAction::Submit => "submit",
            DocumentAction::Cancel => "cancel",
        }
    }
}

/// One step of an approval policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalStep {
    pub label: String,
    pub approvers: Approvers,
    /// How many different people must approve: a number, `all` or `any` (the default).
    #[serde(default, skip_serializing_if = "ApprovalNeed::is_any")]
    pub need: ApprovalNeed,
    /// Over `doc` and `settings`: the step is skipped when it does not hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// Opens together with the next step; both must be satisfied before the steps after them open.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub with_next: bool,
    /// No one who decided an earlier step decides this one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not_earlier_deciders: bool,
    /// Transitions (by action) whose takers on this record do not decide this step.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_who_took: Vec<String>,
    /// A business calendar: the times below count its working time; elapsed time without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar: Option<String>,
    /// Seconds or an ISO 8601 duration after the step opens: those who have not decided are reminded, once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remind_after: Option<String>,
    /// After this, `escalate_to` may decide the step too, and is told, once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalate_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalate_to: Option<Approvers>,
}

/// Who decides: exactly one of a role's holders, the people a field of the record names (a user's id or e-mail, or a
/// list of them), or those an expression over `doc` and `settings` gives the same way.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approvers {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ApprovalNeed {
    Count(u32),
    Word(NeedWord),
}

impl Default for ApprovalNeed {
    fn default() -> Self {
        ApprovalNeed::Word(NeedWord::Any)
    }
}

impl ApprovalNeed {
    fn is_any(&self) -> bool {
        *self == ApprovalNeed::Word(NeedWord::Any)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NeedWord {
    All,
    Any,
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

/// How a field looks to someone who may not read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldMask {
    /// `•••• 1234`.
    Last4,
    /// `p•••@example.com`.
    Email,
    /// Says there is a value, and shows none of it.
    Hidden,
}

impl FieldMask {
    pub fn apply(self, value: &str) -> String {
        const DOT: char = '•';
        match self {
            FieldMask::Hidden => DOT.to_string().repeat(8),
            FieldMask::Last4 => {
                let chars: Vec<char> = value.chars().filter(|c| !c.is_whitespace()).collect();
                let shown: String = chars.iter().skip(chars.len().saturating_sub(4)).collect();
                // A value of four or fewer characters is all "last four": none of it shows.
                if chars.len() <= 4 { DOT.to_string().repeat(8) } else { format!("{} {shown}", DOT.to_string().repeat(4)) }
            }
            FieldMask::Email => match value.split_once('@') {
                Some((local, domain)) => format!("{}{}@{domain}", local.chars().next().map(String::from).unwrap_or_default(), DOT.to_string().repeat(3)),
                None => DOT.to_string().repeat(8),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum OverlayOp {
    AddField {
        field: FieldDef,
        #[serde(default)]
        after: Option<String>,
        /// Package-only: the field is a column of the entity's own table, placed in the form right
        /// after `after`, and needs no field prefix. How a package takes over a field that moves
        /// out of the entity's definition (docs/plan/apps-ui.md §13): its name, column, data and
        /// place stay as they were.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        column: bool,
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
    /// Tenant-only: replaces the definition of an existing custom entity, as an earlier
    /// `define_entity` (or `redefine_entity`) gave it. Records keep every value: a field that goes
    /// keeps its values out of sight, and a field of the same name coming back sees them again.
    RedefineEntity {
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
    /// A package's: a document action on another package's entity, implemented by the overlaying
    /// package's hooks (e.g. Projects adds "Make Sales Invoice" to core's Project). A workspace's: a
    /// configured action (one with effects); one named as an action an earlier customization added
    /// replaces it where it stands.
    AddAction {
        action: ActionDef,
    },
    /// Tenant-only: removes an action a customization added (never a package's).
    RemoveAction {
        name: String,
    },
    /// Tenant-only: a workspace's own menu (named `x_…`): a launcher tile, a sidebar group, quick
    /// create and the assistant's notes for its entities. One of the same name replaces it.
    DefineModule {
        module: ModuleDef,
    },
    /// Tenant-only: removes a workspace's own menu.
    RemoveModule {
        name: String,
    },
    /// Tenant-only: a service target; one named as an earlier one replaces it where it stands.
    AddTarget {
        target: TargetDef,
    },
    /// Tenant-only: removes a service target.
    RemoveTarget {
        name: String,
    },
    /// Tenant-only: a list of some records' chosen fields that anyone may read without signing in
    /// (docs/builder/decisions.md BD-15); one of the same name replaces it.
    PublishList {
        list: PublishedListDef,
    },
    /// Tenant-only: takes a published list down.
    UnpublishList {
        name: String,
    },
    /// Tenant-only: an approval policy on submitting (`on: submit`) or cancelling (`on: cancel`) a submittable entity's
    /// documents, outside any workflow (BD-19); `approval: null` takes it away.
    SetApproval {
        on: DocumentAction,
        approval: Option<ApprovalPolicy>,
    },
    /// Tenant-only: the approval policy on the workflow's transition `action` from `from` in place of its package's
    /// (BD-19); `approval: null` switches it off.
    SetTransitionApproval {
        action: String,
        from: String,
        approval: Option<ApprovalPolicy>,
    },
    /// Package-only: a form section for fields the package adds, after the section holding the
    /// field `after` (or last).
    AddSection {
        section: SectionDef,
        #[serde(default)]
        after: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::EntityDef;

    #[test]
    fn plural_labels() {
        let plural = |yaml: &str| serde_yaml::from_str::<EntityDef>(&format!("{{name: X, kind: document, fields: [], {yaml}}}")).unwrap().plural_label();
        assert_eq!(plural("label: Customer"), "Customers");
        assert_eq!(plural("label: Booking Settings"), "Booking Settings");
        assert_eq!(plural("label: Lesson Progress"), "Lesson Progress");
        assert_eq!(plural("label: Company, label_plural: Companies"), "Companies");
    }
}
