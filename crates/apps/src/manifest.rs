//! `app.yaml`: what an app is, what it needs and what it brings.

use crate::AppError;
use crate::bundle::{PLUGIN_JSON, check_path, is_image_path};
use crate::contract;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Most extension points an app declares, and most it contributes to.
const MAX_POINTS: usize = 20;
/// Most items an app contributes to one point.
const MAX_ITEMS: usize = 50;

/// An extension point an app offers other apps (docs/plugins.md, "Extension points").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPoint {
    pub description: String,
    /// What each contribution must look like, in the subset of JSON Schema [`contract`] checks
    /// (the contribution's `id` is not part of it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
}

/// A contribution without its `id`, as contracts describe it.
pub fn contribution_body(item: &Value) -> Value {
    let mut body = item.clone();
    if let Some(o) = body.as_object_mut() {
        o.remove("id");
    }
    body
}

/// Store categories an app may list (one to three).
pub const CATEGORIES: &[&str] = &[
    "sales",
    "crm",
    "support",
    "hr",
    "finance",
    "inventory",
    "manufacturing",
    "projects",
    "commerce",
    "pos",
    "communication",
    "productivity",
    "marketing",
    "analytics",
    "integrations",
    "localization",
    "energy",
    "iot",
    "education",
    "healthcare",
    "industry",
    "developer",
    // Apps that bundle others (a suite requires its members) and apps that join two others (a
    // connector requires both sides): docs/plan/apps-ui.md §13.
    "suite",
    "connector",
];

/// Kinds of app settings; `secret` values are write-only.
pub const SETTING_TYPES: &[&str] = &["text", "secret", "number", "check", "select"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    /// Rust compiled into the Ferrum distribution (reviewed platform code).
    Native,
    /// A sandboxed WebAssembly plugin plus metadata.
    Wasm,
    /// Metadata only, no code.
    Meta,
    /// Code on its publisher's servers, reached over HTTPS ([`ServiceDef`]): it acts as an account
    /// of its own with the records its `access` names, and hears of the events it names.
    Service,
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Runtime::Native => "native",
            Runtime::Wasm => "wasm",
            Runtime::Meta => "meta",
            Runtime::Service => "service",
        })
    }
}

/// Where a service app runs and what it hears of (`runtime: service`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceDef {
    /// Its address: the server calls `{url}/install`, `{url}/events` and `{url}/uninstall`.
    pub url: String,
    /// Topics it hears of, as a plugin subscribes (`SalesInvoice.submitted`, `Customer.*`), each
    /// about records its `access` reads. A delivery names the record, without its contents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<String>,
}

impl ServiceDef {
    /// The host its URL names (without a port), once the URL is valid.
    pub fn host(&self) -> &str {
        let rest = self.url.split_once("://").map_or("", |(_, r)| r);
        let authority = rest.split('/').next().unwrap_or_default();
        authority.rsplit_once(':').filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit())).map_or(authority, |(h, _)| h)
    }
}

/// Whether removing the app offers to delete its data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Uninstall {
    #[default]
    Keep,
    Ask,
}

/// One field on the app's settings page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingDef {
    pub key: String,
    pub label: String,
    /// One of [`SETTING_TYPES`].
    #[serde(rename = "type")]
    pub kind: String,
    /// Choices of a `select` setting.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Types of an operator setting, as the server's registry has them (server/src/settings.yaml).
pub const OPERATOR_SETTING_TYPES: &[&str] = &["text", "bool", "int", "json", "choice"];

/// A setting an operator chooses for the whole installation (`operator_settings`), which the app's code in the server
/// reads with `ferrum_kernel::env::var(env)`: the environment's value when it sets the variable, else the server's
/// `setting` table's (kept there as `<app id>.<key>`), else nothing (docs/app-contract.md, *Operator settings*). Never a
/// secret: an app keeps its secrets in its own vault. Not the settings page (`settings`), which each workspace fills.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorSetting {
    pub key: String,
    /// The variable the app read before this block: it still works, and wins, through the release after.
    pub env: String,
    /// One of [`OPERATOR_SETTING_TYPES`]; `text` when left out.
    #[serde(rename = "type", default = "text_type")]
    pub kind: String,
    /// A `choice` setting's values ("" among them when it may be left empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// As the variable would say it; "" for nothing (the app's own default).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default: String,
    /// `start` (the default): at a replica's next start; `live`: every replica takes a change within seconds.
    #[serde(default = "start_applies")]
    pub applies: String,
    /// The variable of the secret this address is given, while that secret is in the environment: the table's value is
    /// then not used, so that a row in the database never sends the environment's secret elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sends: Option<String>,
    /// What it does, for the operations guide.
    pub doc: String,
}

fn text_type() -> String {
    "text".into()
}

fn start_applies() -> String {
    "start".into()
}

/// An environment variable's name: an upper-case letter, then upper-case letters, digits and `_`.
fn is_variable(s: &str) -> bool {
    let b = s.as_bytes();
    (3..=100).contains(&b.len()) && b[0].is_ascii_uppercase() && b.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

/// The optional services an installation may have, by id (server/src/services.rs says which this build has and whether
/// they are set up). An app `wants` or `needs` them by these ids; a service is there or not, so no versions.
pub const SERVICES: &[&str] = &["keys.external", "search", "events.stream", "analytics", "notify"];

/// Classes of resource an app can ask for. A class is a contract about behaviour, not a product.
pub const RESOURCE_CLASSES: &[&str] = &["relational", "object", "time_series", "search", "cache", "stream"];
const RESOURCE_ISOLATIONS: &[&str] = &["database", "prefix"];
const RESOURCE_AUTHORITIES: &[&str] = &["primary", "derived", "archival", "disposable"];
const RESOURCE_SEMANTICS: &[&str] = &["transactions", "row_security", "read_after_write", "conditional_writes", "immutable_versions", "point_in_time_recovery"];
const RESOURCE_CLASSIFICATIONS: &[&str] = &["public", "internal", "confidential", "restricted"];

fn resource_version() -> u32 {
    1
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// What it is expected to carry, as whole numbers: what a consent screen shows of its cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceCapacity {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub storage_gb: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub connections: u32,
}

/// Something the app needs that is not the platform's own records: an entry of `resources`. It says
/// what is needed. Who provides it is for the workspace to choose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceDef {
    /// `[a-z][a-z0-9_]{0,39}`: how the app's code names it.
    pub name: String,
    /// One of [`RESOURCE_CLASSES`].
    pub class: String,
    /// The version of the class's contract this was written against.
    #[serde(default = "resource_version")]
    pub version: u32,
    /// The app cannot run without it, so it is not activated until it is there.
    #[serde(default)]
    pub required: bool,
    /// What it depends on the provider guaranteeing.
    #[serde(default)]
    pub semantics: BTreeSet<String>,
    pub isolation: String,
    /// Whether it is the only copy of something, which decides what losing it means.
    pub authority: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<String>,
    #[serde(default)]
    pub retention_days: u32,
    #[serde(default)]
    pub backup: bool,
    /// `retain`, or `offer_deletion`. Destroying data always takes someone's decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_uninstall: Option<String>,
    #[serde(default)]
    pub capacity: ResourceCapacity,
}

impl ResourceDef {
    /// One line for a consent screen. Anything here changing is a change someone has to agree to.
    pub fn consent_line(&self) -> String {
        let mut line = format!("{}: {} {}", self.name, self.class, self.authority);
        if self.required {
            line.push_str(", required");
        }
        if self.capacity.storage_gb > 0 {
            line.push_str(&format!(", {} GB", self.capacity.storage_gb));
        }
        if self.capacity.connections > 0 {
            line.push_str(&format!(", {} connections", self.capacity.connections));
        }
        line
    }
}

/// That its publisher is retiring it, and what to do about that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deprecation {
    /// `YYYY-MM-DD`: when support ends. Nothing here promises anything beyond that date.
    pub support_ends: String,
    /// The app that takes over, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
    /// How to get data out.
    pub export: String,
}

/// The manifest, `app.yaml` at the root of a bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppManifest {
    /// `[a-z][a-z0-9-]{1,39}`, unique in the store.
    pub id: String,
    pub name: String,
    pub version: Version,
    /// Store publisher id (same syntax as app ids).
    pub publisher: String,
    /// One line for listings.
    pub summary: String,
    /// A markdown file in the bundle (usually `README.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    /// An image under `assets/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Images under `assets/`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub screenshots: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// An http(s) URL or an e-mail address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support: Option<String>,
    pub runtime: Runtime,
    /// App API versions it works with (checked against [`crate::PLATFORM_VERSION`]).
    pub platform: VersionReq,
    /// Other apps it needs: app id → version requirement.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub requires: BTreeMap<String, VersionReq>,
    /// Optional services of the installation ([`SERVICES`]) it works better with: the install dialog says whether they are there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wants: Vec<String>,
    /// Optional services it cannot work without: installing it is refused while the installation lacks one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    /// The app this one is an add-on of, shown under it; it must also be in `requires`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Package directories under `packages/`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<String>,
    /// wasm apps only: `plugin.json` (the module is `plugin.wasm`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    /// service apps only: where the service is and what it hears of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceDef>,
    /// PascalCase namespace of a third-party app's entities (reserved per publisher in the store).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<SettingDef>,
    /// Native apps only: the settings an operator chooses for the installation, which its code reads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operator_settings: Vec<OperatorSetting>,
    /// Points other apps may contribute to, named `<this app's id>.<name>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extension_points: BTreeMap<String, ExtensionPoint>,
    /// What it adds to extension points: the shell's, its own or those of apps it requires. Point → items, each with an `id` unique within the point.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub contributes: BTreeMap<String, Vec<Value>>,
    /// The app's own interface (docs/plan/apps-ui.md §10.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<crate::shell::UiSection>,
    /// Records the app's pages may reach for the person (its interface and screens), within the
    /// person's own rights: what an app without a plugin declares (a plugin declares its own in
    /// plugin.json). Consented to at install, like the rest of its permissions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access: Vec<AccessDef>,
    /// What it needs beyond the platform's records. Consented to at install, like its permissions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<ResourceDef>,
    /// Set by its publisher when it is being retired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecation: Option<Deprecation>,
    /// An industrial solution: the site runtime's connectors it needs, the device profiles and
    /// acceptance procedures it ships (FERRUM-IND-018, [`crate::industrial`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub industrial: Option<crate::industrial::IndustrialSection>,
    #[serde(default)]
    pub uninstall: Uninstall,
}

/// One entry of a manifest's `access`: what a plugin's permission says, without conditions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessDef {
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
    /// Only records the person created.
    #[serde(default)]
    pub if_owner: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_groups: Vec<String>,
}

impl AccessDef {
    pub fn permission(&self) -> ferrum_meta::PermissionDef {
        ferrum_meta::PermissionDef {
            entity: self.entity.clone(),
            read: self.read,
            write: self.write,
            create: self.create,
            delete: self.delete,
            submit: self.submit,
            cancel: self.cancel,
            export: self.export,
            import: self.import,
            if_owner: self.if_owner,
            read_groups: self.read_groups.clone(),
            write_groups: self.write_groups.clone(),
            ..Default::default()
        }
    }
}

fn bad<T>(msg: impl Into<String>) -> Result<T, AppError> {
    Err(AppError::Manifest(msg.into()))
}

/// `[a-z][a-z0-9-]{1,39}`: app ids, publisher ids and plugin names.
pub(crate) fn is_app_id(s: &str) -> bool {
    let b = s.as_bytes();
    (2..=40).contains(&b.len()) && b[0].is_ascii_lowercase() && b[1..].iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// A required single line of text of at most `max` characters.
fn line(field: &str, value: &str, max: usize) -> Result<(), AppError> {
    if value.trim().is_empty() {
        return bad(format!("{field} is required"));
    }
    if value.chars().count() > max {
        return bad(format!("{field} is longer than {max} characters"));
    }
    if value.chars().any(char::is_control) {
        return bad(format!("{field} must be a single line of text"));
    }
    Ok(())
}

fn is_web_url(s: &str) -> bool {
    let rest = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://"));
    rest.is_some_and(|r| !r.is_empty() && !r.starts_with('/'))
        && s.len() <= 300
        && !s.chars().any(|c| c.is_whitespace() || c.is_control() || "\"'<>\\`".contains(c))
}

fn is_email(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else { return false };
    s.len() <= 254
        && !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !s.chars().any(|c| c.is_whitespace() || c.is_control() || "\"'<>()[]\\,;:`".contains(c))
        && !domain.contains('@')
}

fn is_prefix(p: &str) -> bool {
    let b = p.as_bytes();
    (2..=8).contains(&b.len()) && b[0].is_ascii_uppercase() && b.iter().all(u8::is_ascii_alphanumeric)
}

impl AppManifest {
    /// Parses and validates `app.yaml`.
    pub fn parse(yaml: &str) -> Result<AppManifest, AppError> {
        let m: AppManifest = serde_yaml::from_str(yaml).map_err(|e| {
            let text = e.to_string();
            // What this server does not know may be a requirement, so it is never skipped.
            if text.contains("unknown field") {
                AppError::Manifest(format!(
                    "{text}. This server does not know that field, and will not ignore what may be a requirement: this app needs a newer Ferrum"
                ))
            } else {
                AppError::Manifest(text)
            }
        })?;
        m.validate()?;
        Ok(m)
    }

    /// Checks every rule that needs only the manifest itself (the bundle checks the rest: files
    /// exist, packages match, entity names carry the prefix...).
    pub fn validate(&self) -> Result<(), AppError> {
        if !is_app_id(&self.id) {
            return bad(format!("id '{}' must be 2-40 characters: a lower-case letter, then lower-case letters, digits or '-'", self.id));
        }
        line("name", &self.name, 60)?;
        if !self.version.build.is_empty() {
            return bad(format!("version {} must not carry build metadata (+{})", self.version, self.version.build));
        }
        if !is_app_id(&self.publisher) {
            return bad(format!("publisher '{}' must be a store publisher id: 2-40 lower-case letters, digits or '-'", self.publisher));
        }
        line("summary", &self.summary, 200)?;
        if let Some(d) = &self.description {
            check_path(d).map_err(|e| AppError::Manifest(format!("description: {e}")))?;
            if !d.ends_with(".md") {
                return bad(format!("description must name a markdown file in the bundle (such as README.md), not {d}"));
            }
        }

        if self.categories.is_empty() || self.categories.len() > 3 {
            return bad(format!("categories: list 1 to 3 of: {}", CATEGORIES.join(", ")));
        }
        let mut seen = BTreeSet::new();
        for c in &self.categories {
            if !CATEGORIES.contains(&c.as_str()) {
                return bad(format!("unknown category '{c}' (use: {})", CATEGORIES.join(", ")));
            }
            if !seen.insert(c) {
                return bad(format!("category '{c}' is listed twice"));
            }
        }

        let images = self.icon.iter().map(|p| ("icon", p)).chain(self.screenshots.iter().map(|p| ("screenshots", p)));
        for (field, p) in images {
            check_path(p).map_err(|e| AppError::Manifest(format!("{field}: {e}")))?;
            if !is_image_path(p) {
                return bad(format!("{field}: {p} must be an image under assets/ (png, jpg, jpeg, gif, webp or svg)"));
            }
        }
        for p in &self.access {
            if p.entity != "*" && !ferrum_meta::is_entity_name(&p.entity) {
                return bad(format!("access: '{}' is not an entity name (or *)", p.entity));
            }
        }
        if self.access.len() > 100 {
            return bad("access: at most 100 entries");
        }
        if self.screenshots.len() > 10 {
            return bad("at most 10 screenshots");
        }
        if self.screenshots.iter().collect::<BTreeSet<_>>().len() != self.screenshots.len() {
            return bad("a screenshot is listed twice");
        }

        if let Some(l) = &self.license {
            line("license", l, 100)?;
        }
        if let Some(h) = &self.homepage
            && !is_web_url(h)
        {
            return bad(format!("homepage must be an http(s) URL, not '{h}'"));
        }
        if let Some(s) = &self.support
            && !is_web_url(s)
            && !is_email(s)
        {
            return bad(format!("support must be an http(s) URL or an e-mail address, not '{s}'"));
        }

        self.validate_resources()?;
        if let Some(d) = &self.deprecation {
            let date = d.support_ends.as_bytes();
            if date.len() != 10 || date[4] != b'-' || date[7] != b'-' || !date.iter().enumerate().all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit()) {
                return bad(format!("deprecation.support_ends must be YYYY-MM-DD, not {}", d.support_ends));
            }
            line("deprecation.export", &d.export, 300)?;
            if let Some(r) = &d.replacement
                && !is_app_id(r)
            {
                return bad(format!("deprecation.replacement: '{r}' is not an app id"));
            }
        }

        for dep in self.requires.keys() {
            if !is_app_id(dep) {
                return bad(format!("requires: '{dep}' is not an app id"));
            }
            if dep == &self.id {
                return bad("an app cannot require itself");
            }
        }

        for (key, list) in [("wants", &self.wants), ("needs", &self.needs)] {
            if let Some(id) = list.iter().find(|s| !SERVICES.contains(&s.as_str())) {
                return bad(format!("{key}: '{id}' is not a service ({})", SERVICES.join(", ")));
            }
        }
        if let Some(id) = self.wants.iter().find(|s| self.needs.contains(s)) {
            return bad(format!("service '{id}' is in both wants and needs"));
        }

        let mut seen = BTreeSet::new();
        for p in &self.packages {
            if !ferrum_meta::is_package_name(p) {
                return bad(format!("packages: '{p}' is not a package name (a lower-case letter, then lower-case letters, digits, '_' or '-')"));
            }
            if !seen.insert(p) {
                return bad(format!("packages: '{p}' is listed twice"));
            }
        }
        // An app of metadata brings tables, or at least an interface of its own (over other
        // apps' records, through the API); or it is a suite, the apps it requires.
        if self.runtime == Runtime::Meta && self.packages.is_empty() && self.ui.is_none() && self.requires.is_empty() {
            return bad("a meta app needs at least one package, an interface of its own (ui), or the apps of its suite (requires)");
        }
        if self.categories.iter().any(|c| c == "suite") != self.is_suite() {
            return bad("a suite (category suite) is a meta app with only the apps it requires: no packages, interface or contributions");
        }

        match (self.runtime, self.plugin.as_deref()) {
            (Runtime::Wasm, None) => return bad("wasm apps must set plugin: plugin.json"),
            (Runtime::Wasm, Some(p)) if p != PLUGIN_JSON => return bad(format!("plugin must be plugin.json (the module itself is plugin.wasm), not {p}")),
            (Runtime::Wasm, Some(_)) => {}
            (rt, Some(_)) => return bad(format!("only wasm apps have a plugin (this app's runtime is {rt})")),
            (_, None) => {}
        }
        match (self.runtime, &self.service) {
            (Runtime::Service, None) => return bad("service apps must say where the service is (service.url)"),
            (Runtime::Service, Some(s)) => self.validate_service(s)?,
            (rt, Some(_)) => return bad(format!("only service apps have a service section (this app's runtime is {rt})")),
            (_, None) => {}
        }

        if let Some(p) = &self.prefix {
            if !is_prefix(p) {
                return bad(format!("prefix '{p}' must be PascalCase: 2-8 letters or digits starting with a capital (such as Ev)"));
            }
            if p.starts_with('X') {
                return bad(format!("prefix '{p}' cannot start with X (reserved for custom and plugin entities)"));
            }
        }

        let mut keys = BTreeSet::new();
        for s in &self.settings {
            if !ferrum_meta::is_ident(&s.key) {
                return bad(format!("settings: key '{}' must be snake_case (a lower-case letter, then lower-case letters, digits or '_')", s.key));
            }
            if !keys.insert(&s.key) {
                return bad(format!("settings: key '{}' is defined twice", s.key));
            }
            line(&format!("settings.{}.label", s.key), &s.label, 100)?;
            if !SETTING_TYPES.contains(&s.kind.as_str()) {
                return bad(format!("settings.{}: type must be one of {}, not '{}'", s.key, SETTING_TYPES.join(", "), s.kind));
            }
            if s.kind == "select" {
                if s.options.is_empty() {
                    return bad(format!("settings.{}: a select setting needs options", s.key));
                }
                for o in &s.options {
                    line(&format!("settings.{} option", s.key), o, 100)?;
                }
                if s.options.iter().collect::<BTreeSet<_>>().len() != s.options.len() {
                    return bad(format!("settings.{}: an option is listed twice", s.key));
                }
            } else if !s.options.is_empty() {
                return bad(format!("settings.{}: options are only for select settings", s.key));
            }
            if let Some(d) = &s.description {
                line(&format!("settings.{}.description", s.key), d, 500)?;
            }
        }
        self.validate_operator_settings()?;
        self.validate_extensions()
    }

    /// `operator_settings`: each a setting of the server's registry, kept as `<app id>.<key>`, read by the app's own code
    /// in the server (so only a native app has them), and never a secret's place.
    fn validate_operator_settings(&self) -> Result<(), AppError> {
        if self.operator_settings.is_empty() {
            return Ok(());
        }
        if self.runtime != Runtime::Native {
            return bad(format!("operator_settings: only an app compiled into the server reads them (this app's runtime is {})", self.runtime));
        }
        let (mut keys, mut vars) = (BTreeSet::new(), BTreeSet::new());
        for s in &self.operator_settings {
            let at = format!("operator_settings.{}", s.key);
            if !ferrum_meta::is_ident(&s.key) || self.id.len() + 1 + s.key.len() > 63 {
                return bad(format!("operator_settings: key '{}' must be snake_case, and '{}.{}' at most 63 characters", s.key, self.id, s.key));
            }
            if !keys.insert(&s.key) {
                return bad(format!("operator_settings: key '{}' is declared twice", s.key));
            }
            if !is_variable(&s.env) || s.env == "DATABASE_URL" {
                return bad(format!("{at}.env must be an environment variable's name (FERRUM_…), and not DATABASE_URL, not '{}'", s.env));
            }
            if !vars.insert(&s.env) {
                return bad(format!("operator_settings: {} is declared twice", s.env));
            }
            if !OPERATOR_SETTING_TYPES.contains(&s.kind.as_str()) {
                return bad(format!("{at}: type must be one of {}, not '{}'", OPERATOR_SETTING_TYPES.join(", "), s.kind));
            }
            if (s.kind == "choice") == s.values.is_empty() || s.values.iter().collect::<BTreeSet<_>>().len() != s.values.len() {
                return bad(format!("{at}: a choice has its values, each once, and only a choice has values"));
            }
            let t = s.default.trim();
            let fits = t.is_empty()
                || match s.kind.as_str() {
                    "bool" => t == "true" || t == "false",
                    "int" => t.parse::<u64>().is_ok(),
                    "json" => serde_json::from_str::<Value>(t).is_ok(),
                    "choice" => s.values.iter().any(|v| v == t),
                    _ => true,
                };
            if !fits {
                return bad(format!("{at}: its default '{}' is not a {} value", s.default, s.kind));
            }
            if !matches!(s.applies.as_str(), "start" | "live") {
                return bad(format!("{at}: applies is start or live, not '{}'", s.applies));
            }
            if let Some(v) = &s.sends
                && (!is_variable(v) || self.operator_settings.iter().any(|o| &o.env == v))
            {
                return bad(format!("{at}.sends names the variable of the secret this address is given, not '{v}'"));
            }
            if s.doc.trim().is_empty() || s.doc.chars().count() > 1000 {
                return bad(format!("{at}.doc says what it does, in at most 1000 characters"));
            }
        }
        Ok(())
    }

    /// A service's address is a plain http(s) URL (the server takes plain http only where it
    /// allows private addresses, for development), and it hears only of records it reads.
    fn validate_service(&self, s: &ServiceDef) -> Result<(), AppError> {
        let host = s.host();
        if !is_web_url(&s.url)
            || s.url.len() > 200
            || s.url.contains(['?', '#', '@'])
            || host.is_empty()
            || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        {
            return bad(format!("service.url must be an http(s) URL of at most 200 characters, without a query, fragment or user, not '{}'", s.url));
        }
        if s.events.len() > 50 {
            return bad("service.events: at most 50 topics");
        }
        let reads = |entity: &str| self.access.iter().any(|a| a.read && (a.entity == "*" || a.entity == entity));
        let mut seen = BTreeSet::new();
        for t in &s.events {
            if t.is_empty() || t.len() > 120 || !t.chars().all(|c| c.is_ascii_alphanumeric() || "._-*".contains(c)) {
                return bad(format!("service.events: '{t}': use topics like SalesInvoice.submitted, SalesInvoice.* or *"));
            }
            if !seen.insert(t) {
                return bad(format!("service.events: '{t}' is listed twice"));
            }
            let entity = t.split('.').next().unwrap_or_default();
            if !reads(entity) {
                return bad(format!("service.events: '{t}' is about {entity}, which its access does not read"));
            }
        }
        Ok(())
    }

    /// `parent`, `extension_points` and `contributes`. Contributions to another app's point are
    /// checked against its contract when a plan brings the two together.
    fn validate_resources(&self) -> Result<(), AppError> {
        if self.runtime == Runtime::Meta && !self.resources.is_empty() {
            return bad("resources: an app with no code has nothing to use them with");
        }
        if self.runtime == Runtime::Service && !self.resources.is_empty() {
            return bad("resources: a service's code runs on its own servers, where the platform's resources do not reach");
        }
        let mut seen = BTreeSet::new();
        for r in &self.resources {
            let at = format!("resources.{}", r.name);
            let name = r.name.as_bytes();
            if name.is_empty()
                || name.len() > 40
                || !name[0].is_ascii_lowercase()
                || !name.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
            {
                return bad(format!("resources: '{}' must be a lower-case letter, then lower-case letters, digits or '_', up to 40", r.name));
            }
            if !seen.insert(&r.name) {
                return bad(format!("{at} is listed twice"));
            }
            if r.version == 0 {
                return bad(format!("{at}.version starts at 1"));
            }
            for (field, value, allowed) in
                [("class", &r.class, RESOURCE_CLASSES), ("isolation", &r.isolation, RESOURCE_ISOLATIONS), ("authority", &r.authority, RESOURCE_AUTHORITIES)]
            {
                if !allowed.contains(&value.as_str()) {
                    return bad(format!("{at}.{field}: this server does not know '{value}' (use: {})", allowed.join(", ")));
                }
            }
            if let Some(s) = r.semantics.iter().find(|s| !RESOURCE_SEMANTICS.contains(&s.as_str())) {
                return bad(format!("{at}.semantics: this server does not know '{s}', and will not ignore what may be a requirement"));
            }
            if let Some(c) = r.classification.as_ref().filter(|c| !RESOURCE_CLASSIFICATIONS.contains(&c.as_str())) {
                return bad(format!("{at}.classification: '{c}' is not one of {}", RESOURCE_CLASSIFICATIONS.join(", ")));
            }
            if let Some(u) = r.on_uninstall.as_ref().filter(|u| !["retain", "offer_deletion"].contains(&u.as_str())) {
                return bad(format!("{at}.on_uninstall: '{u}' is not retain or offer_deletion"));
            }
        }
        Ok(())
    }

    fn validate_extensions(&self) -> Result<(), AppError> {
        if self.id == "ui" || self.id == "shell" {
            return bad(format!("the id '{}' is reserved for the platform's points", self.id));
        }
        if let Some(p) = &self.parent
            && !self.requires.contains_key(p)
        {
            return bad(format!("parent '{p}' must also be listed in requires"));
        }
        if self.extension_points.len() > MAX_POINTS || self.contributes.len() > MAX_POINTS {
            return bad(format!("at most {MAX_POINTS} extension points, and contributions to at most {MAX_POINTS}"));
        }
        for (name, point) in &self.extension_points {
            if !name.strip_prefix(self.id.as_str()).and_then(|r| r.strip_prefix('.')).is_some_and(ferrum_meta::is_ident) {
                return bad(format!("extension point '{name}' must be named {}.<name> (lower-case letters, digits, '_')", self.id));
            }
            line(&format!("extension_points.{name}.description"), &point.description, 300)?;
            if let Some(schema) = &point.schema {
                contract::check_schema(schema).map_err(|e| AppError::Manifest(format!("extension_points.{name}.schema: {e}")))?;
            }
        }
        for (point, items) in &self.contributes {
            let shell = crate::shell::SHELL_POINTS.contains(&point.as_str());
            let foreign = point.split_once('.').is_some_and(|(owner, rest)| self.requires.contains_key(owner) && ferrum_meta::is_ident(rest));
            if !shell && !foreign && !self.extension_points.contains_key(point) {
                return bad(format!(
                    "contributes: '{point}' is not a shell point ({}), an extension point of this app, or one of an app in requires",
                    crate::shell::SHELL_POINTS.join(", ")
                ));
            }
            if items.is_empty() || items.len() > MAX_ITEMS {
                return bad(format!("contributes.{point}: list 1 to {MAX_ITEMS} items"));
            }
            let schema = if shell { Some(crate::shell::contract(point)) } else { self.extension_points.get(point).and_then(|p| p.schema.clone()) };
            let mut ids = BTreeSet::new();
            for item in items {
                let Some(id) = item.get("id").and_then(Value::as_str).filter(|i| ferrum_meta::is_ident(i)) else {
                    return bad(format!("contributes.{point}: every item needs an id (lower-case letters, digits, '_')"));
                };
                if !ids.insert(id) {
                    return bad(format!("contributes.{point}: id '{id}' is used twice"));
                }
                if let Some(s) = &schema {
                    contract::conforms(&contribution_body(item), s).map_err(|e| AppError::Manifest(format!("contributes.{point}.{id}: {e}")))?;
                }
            }
        }
        crate::shell::check(self)
    }

    /// Whether this app works with platform (app API) version `platform`.
    /// A suite: installs the apps it requires, and is nothing itself.
    pub fn is_suite(&self) -> bool {
        self.runtime == Runtime::Meta && self.packages.is_empty() && self.ui.is_none() && self.contributes.is_empty() && !self.requires.is_empty()
    }

    pub fn compatible_with(&self, platform: &Version) -> bool {
        self.platform.matches(platform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
id: ev-charging
name: EV Charging
version: 1.4.0
publisher: voltworks
summary: Run charge points over OCPP 1.6J and roam over OCPI 2.2
description: README.md
categories: [energy, iot]
icon: assets/icon.svg
screenshots: [assets/stations.png]
license: Apache-2.0
homepage: https://voltworks.example
support: help@voltworks.example
runtime: wasm
platform: ">=0.4, <0.5"
requires:
  selling: ">=0.3"
packages: [ev_charging]
plugin: plugin.json
prefix: Ev
settings:
  - {key: ocpi_token, label: OCPI token, type: secret}
  - {key: mode, label: Mode, type: select, options: [Live, Test], required: true}
uninstall: keep
"#;

    /// The valid manifest with `from` replaced by `to` (which must occur).
    fn with(from: &str, to: &str) -> Result<AppManifest, AppError> {
        assert!(VALID.contains(from), "{from}");
        AppManifest::parse(&VALID.replacen(from, to, 1))
    }

    const RESOURCE: &str = "resources:\n  - {name: ledger, class: relational, isolation: database, authority: primary, required: true, semantics: [transactions], capacity: {storage_gb: 10}}\nuninstall: keep\n";

    fn with_resources(yaml: &str) -> Result<AppManifest, AppError> {
        with("uninstall: keep\n", yaml)
    }

    fn refused_resource(from: &str, to: &str, needle: &str) {
        let err = with_resources(&RESOURCE.replacen(from, to, 1)).expect_err(needle).to_string();
        assert!(err.contains(needle), "expected '{needle}' in: {err}");
    }

    #[test]
    fn resources_are_declared_strictly() {
        let m = with_resources(RESOURCE).unwrap();
        assert_eq!(m.resources[0].consent_line(), "ledger: relational primary, required, 10 GB");
        assert_eq!(m.resources[0].version, 1);
        refused_resource("name: ledger", "name: Ledger", "lower-case");
        refused_resource("class: relational", "class: quantum", "does not know 'quantum'");
        refused_resource("isolation: database", "isolation: hope", "does not know 'hope'");
        refused_resource("authority: primary", "authority: vibes", "does not know 'vibes'");
        refused_resource("semantics: [transactions]", "semantics: [teleportation]", "will not ignore what may be a requirement");
        refused_resource("required: true", "required: true, tenant: someone-else", "unknown field");
        refused_resource("capacity: {storage_gb: 10}", "capacity: {storage_gb: 10, cost_ceiling: 5}", "unknown field");
        let twice = "resources:\n  - {name: a, class: cache, isolation: prefix, authority: disposable}\n  - {name: a, class: cache, isolation: prefix, authority: disposable}\nuninstall: keep\n";
        assert!(with_resources(twice).unwrap_err().to_string().contains("listed twice"));
        let meta = VALID.replace("runtime: wasm", "runtime: meta").replace("plugin: plugin.json\n", "").replace("uninstall: keep\n", RESOURCE);
        assert!(AppManifest::parse(&meta).unwrap_err().to_string().contains("no code"), "metadata cannot use a database");
    }

    #[test]
    fn operator_settings_are_declared_strictly() {
        let native = |block: &str| {
            let yaml = VALID
                .replace("runtime: wasm", "runtime: native")
                .replace("plugin: plugin.json\n", "")
                .replace("uninstall: keep\n", &format!("{block}uninstall: keep\n"));
            AppManifest::parse(&yaml)
        };
        let good = "operator_settings:\n  - {key: ocpi_url, env: FERRUM_EV_OCPI_URL, doc: The OCPI hub this server reaches., sends: FERRUM_EV_OCPI_TOKEN}\n  - {key: retries, env: FERRUM_EV_RETRIES, type: int, default: \"3\", applies: live, doc: How often a call is tried.}\n";
        let m = native(good).unwrap();
        assert_eq!((m.operator_settings[0].kind.as_str(), m.operator_settings[0].applies.as_str()), ("text", "start"), "text, at start, unless said");
        assert_eq!(m.operator_settings[1].default, "3");
        assert!(
            AppManifest::parse(&VALID.replace("uninstall: keep\n", &format!("{good}uninstall: keep\n")))
                .unwrap_err()
                .to_string()
                .contains("only an app compiled into the server")
        );
        for (from, to, said) in [
            ("key: ocpi_url", "key: OcpiUrl", "snake_case"),
            ("key: retries", "key: ocpi_url", "declared twice"),
            ("env: FERRUM_EV_RETRIES", "env: FERRUM_EV_OCPI_URL", "declared twice"),
            ("env: FERRUM_EV_OCPI_URL", "env: DATABASE_URL", "not DATABASE_URL"),
            ("env: FERRUM_EV_OCPI_URL", "env: ferrum_ev", "environment variable"),
            ("type: int", "type: number", "type must be one of"),
            ("default: \"3\"", "default: three", "not a int value"),
            ("applies: live", "applies: soon", "start or live"),
            ("sends: FERRUM_EV_OCPI_TOKEN", "sends: FERRUM_EV_RETRIES", "sends names"),
            ("doc: How often a call is tried.", "doc: \"\"", "says what it does"),
            ("type: int, default: \"3\"", "type: choice", "a choice has its values"),
            ("{key: retries", "{secret: true, key: retries", "unknown field"),
        ] {
            let err = native(&good.replacen(from, to, 1)).expect_err(said).to_string();
            assert!(err.contains(said), "{from} → {to}: expected '{said}' in: {err}");
        }
        let choice = "operator_settings:\n  - {key: mode, env: FERRUM_EV_MODE, type: choice, values: [\"\", live, test], doc: Which hub.}\n";
        assert!(native(choice).is_ok());
        assert!(native(&choice.replace("values: [\"\", live, test]", "values: [live, live]")).is_err(), "each once");
        let long = format!("operator_settings:\n  - {{key: {}, env: FERRUM_EV_X, doc: Long.}}\n", "k".repeat(60));
        assert!(native(&long).unwrap_err().to_string().contains("at most 63"), "app id, a dot and the key fit the table's key");
    }

    #[test]
    fn what_this_server_does_not_know_is_refused_not_skipped() {
        let err = with("uninstall: keep\n", "services:\n  - {image: sha256:abc, egress: everywhere}\nuninstall: keep\n").unwrap_err().to_string();
        assert!(err.contains("services") && err.contains("newer Ferrum"), "{err}");
    }

    #[test]
    fn retiring_says_when_and_how_to_leave() {
        let good = "deprecation: {support_ends: 2027-06-30, replacement: other-app, export: Download everything from its settings page}\nuninstall: keep\n";
        assert_eq!(with("uninstall: keep\n", good).unwrap().deprecation.unwrap().support_ends, "2027-06-30");
        assert!(with("uninstall: keep\n", &good.replace("2027-06-30", "soon")).is_err());
        assert!(with("uninstall: keep\n", &good.replace("other-app", "Not An App")).is_err());
        assert!(with("uninstall: keep\n", "deprecation: {support_ends: 2027-06-30}\nuninstall: keep\n").is_err(), "with a way out");
    }

    fn rejected(from: &str, to: &str, needle: &str) {
        let err = with(from, to).expect_err(&format!("{from} -> {to} should be refused")).to_string();
        assert!(err.contains(needle), "{from} -> {to}: {err}");
    }

    #[test]
    fn parses_the_specification_example() {
        let m = AppManifest::parse(VALID).unwrap();
        assert_eq!(m.id, "ev-charging");
        assert_eq!(m.version, Version::new(1, 4, 0));
        assert_eq!(m.runtime, Runtime::Wasm);
        assert_eq!(m.requires["selling"], VersionReq::parse(">=0.3").unwrap());
        assert_eq!(m.settings[0].kind, "secret");
        assert_eq!(m.uninstall, Uninstall::Keep);
        assert!(m.compatible_with(&Version::parse(crate::PLATFORM_VERSION).unwrap()));
        assert!(!m.compatible_with(&Version::new(0, 5, 0)));
        // Round trip through JSON (how the server stores it).
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(serde_json::from_value::<AppManifest>(json).unwrap(), m);
    }

    #[test]
    fn minimal_manifest_and_defaults() {
        let m = AppManifest::parse(
            "id: kit\nname: Kit\nversion: 0.1.0\npublisher: acme\nsummary: A kit\ncategories: [industry]\nruntime: meta\nplatform: '^0.4'\npackages: [kit]\n",
        )
        .unwrap();
        assert_eq!(m.uninstall, Uninstall::Keep);
        assert!(m.requires.is_empty() && m.settings.is_empty() && m.prefix.is_none());
    }

    #[test]
    fn wants_and_needs_name_known_services_and_never_the_same_one() {
        let m = with("uninstall: keep", "uninstall: keep\nwants: [search]\nneeds: [keys.external]").unwrap();
        assert_eq!((m.wants, m.needs), (vec!["search".to_string()], vec!["keys.external".to_string()]));
        rejected("uninstall: keep", "uninstall: keep\nwants: [pager]", "wants: 'pager' is not a service");
        rejected("uninstall: keep", "uninstall: keep\nneeds: [Search]", "needs: 'Search' is not a service");
        rejected("uninstall: keep", "uninstall: keep\nwants: [search]\nneeds: [search]", "service 'search' is in both wants and needs");
        assert!(AppManifest::parse(VALID).unwrap().wants.is_empty(), "an app that names none is as before");
    }

    const EXTENDS: &str = r#"uninstall: keep
parent: selling
extension_points:
  ev-charging.tariff:
    description: Prices a charging session
    schema: {type: object, required: [unit], additionalProperties: false, properties: {unit: {enum: [kwh, minute]}, label: {type: string, maxLength: 40}}}
contributes:
  ev-charging.tariff:
    - {id: flat, unit: kwh}
  selling.quote_source:
    - {id: sessions, anything: goes}"#;

    #[test]
    fn parent_extension_points_and_contributions() {
        let m = with("uninstall: keep", EXTENDS).unwrap();
        assert_eq!(m.parent.as_deref(), Some("selling"));
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(serde_json::from_value::<AppManifest>(json).unwrap(), m);

        let swap = |from: &str, to: &str, needle: &str| {
            assert!(EXTENDS.contains(from), "{from}");
            rejected("uninstall: keep", &EXTENDS.replacen(from, to, 1), needle);
        };
        swap("parent: selling", "parent: crm", "parent 'crm' must also be listed in requires");
        swap("  ev-charging.tariff:\n    description", "  tariff:\n    description", "must be named ev-charging.<name>");
        swap("maxLength: 40", "pattern: x", "label: 'pattern' is not supported");
        swap("    - {id: flat, unit: kwh}", "    - {id: flat, unit: day}", "contributes.ev-charging.tariff.flat: unit must be one of");
        swap("  selling.quote_source:", "  crm.quote_source:", "'crm.quote_source' is not a shell point");
        swap("{id: flat, unit", "{unit", "contributes.ev-charging.tariff: every item needs an id");
        swap(
            "    - {id: flat, unit: kwh}",
            "    - {id: flat, unit: kwh}\n    - {id: flat, unit: minute}",
            "contributes.ev-charging.tariff: id 'flat' is used twice",
        );
    }

    #[test]
    fn required_fields_and_unknown_fields() {
        for field in ["id: ev-charging\n", "version: 1.4.0\n", "runtime: wasm\n", "platform: \">=0.4, <0.5\"\n", "publisher: voltworks\n"] {
            assert!(with(field, "").is_err(), "{field} is required");
        }
        rejected("uninstall: keep", "uninstall: keep\nfavourite: yes", "unknown field");
        rejected("uninstall: keep", "uninstall: never", "unknown variant");
        rejected("runtime: wasm", "runtime: python", "unknown variant");
        rejected("version: 1.4.0", "version: 1.4", "");
        rejected("platform: \">=0.4, <0.5\"", "platform: \"not a requirement\"", "");
    }

    #[test]
    fn identity_rules() {
        rejected("id: ev-charging", "id: EvCharging", "id 'EvCharging'");
        rejected("id: ev-charging", "id: e", "id 'e'");
        rejected("id: ev-charging", "id: 1ev", "id '1ev'");
        rejected("id: ev-charging", "id: ev_charging", "id 'ev_charging'");
        rejected("id: ev-charging", &format!("id: a{}", "b".repeat(40)), "must be 2-40");
        assert!(with("id: ev-charging", &format!("id: a{}", "b".repeat(39))).is_ok());
        rejected("name: EV Charging", "name: \"  \"", "name is required");
        rejected("name: EV Charging", &format!("name: {}", "n".repeat(61)), "longer than 60");
        rejected("publisher: voltworks", "publisher: Volt Works", "publisher");
        rejected("version: 1.4.0", "version: 1.4.0+build.7", "build metadata");
        assert!(with("version: 1.4.0", "version: 1.4.0-beta.2").is_ok());
        rejected("summary: Run charge points over OCPP 1.6J and roam over OCPI 2.2", "summary: \"two\\nlines\"", "single line");
        rejected("summary: Run charge points over OCPP 1.6J and roam over OCPI 2.2", &format!("summary: {}", "s".repeat(201)), "longer than 200");
    }

    #[test]
    fn category_rules() {
        rejected("categories: [energy, iot]", "categories: []", "1 to 3");
        rejected("categories: [energy, iot]", "categories: [energy, iot, crm, sales]", "1 to 3");
        rejected("categories: [energy, iot]", "categories: [energy, games]", "unknown category 'games'");
        rejected("categories: [energy, iot]", "categories: [energy, energy]", "twice");
        assert!(with("categories: [energy, iot]", "categories: [energy, iot, developer]").is_ok());
    }

    #[test]
    fn path_and_link_rules() {
        rejected("description: README.md", "description: notes.txt", "markdown");
        rejected("description: README.md", "description: ../README.md", "description");
        rejected("icon: assets/icon.svg", "icon: icon.svg", "under assets/");
        rejected("icon: assets/icon.svg", "icon: assets/icon.exe", "under assets/");
        rejected("icon: assets/icon.svg", "icon: /etc/icon.svg", "absolute");
        rejected("screenshots: [assets/stations.png]", "screenshots: [assets/a.png, assets/a.png]", "twice");
        rejected("homepage: https://voltworks.example", "homepage: javascript:alert(1)", "homepage");
        rejected("homepage: https://voltworks.example", "homepage: https://", "homepage");
        rejected("support: help@voltworks.example", "support: \"<script>@x.y\"", "support");
        rejected("support: help@voltworks.example", "support: nobody", "support");
        assert!(with("support: help@voltworks.example", "support: https://voltworks.example/help").is_ok());
        rejected("license: Apache-2.0", "license: \"\"", "license is required");
    }

    #[test]
    fn dependency_and_package_rules() {
        rejected("  selling: \">=0.3\"", "  ev-charging: \">=1\"", "cannot require itself");
        rejected("  selling: \">=0.3\"", "  Selling: \">=0.3\"", "not an app id");
        rejected("packages: [ev_charging]", "packages: [Ev]", "not a package name");
        rejected("packages: [ev_charging]", "packages: [a, a]", "twice");
        let meta = VALID.replace("runtime: wasm", "runtime: meta").replace("plugin: plugin.json\n", "").replace("packages: [ev_charging]", "packages: []");
        // Without packages or an interface, it is a suite: the apps it requires, in category suite.
        assert!(AppManifest::parse(&meta).unwrap_err().to_string().contains("a suite (category suite)"));
        let suite = AppManifest::parse(&meta.replace("categories: [energy, iot]", "categories: [suite]")).expect("a suite");
        assert!(suite.is_suite());
        let lone = meta.replace("requires:\n  selling: \">=0.3\"\n", "");
        assert!(AppManifest::parse(&lone).unwrap_err().to_string().contains("at least one package"));
    }

    #[test]
    fn plugin_rules() {
        rejected("plugin: plugin.json\n", "", "must set plugin");
        rejected("plugin: plugin.json", "plugin: other.json", "must be plugin.json");
        rejected("runtime: wasm", "runtime: meta", "only wasm apps have a plugin");
        rejected("runtime: wasm", "runtime: native", "only wasm apps have a plugin");
        assert!(AppManifest::parse(&VALID.replace("runtime: wasm", "runtime: native").replace("plugin: plugin.json\n", "")).is_ok());
    }

    #[test]
    fn service_rules() {
        let service = |section: &str| {
            VALID.replace("runtime: wasm", "runtime: service").replace("plugin: plugin.json\n", section).replace(
                "uninstall: keep\n",
                "access:\n  - {entity: Customer, read: true}\n  - {entity: SalesInvoice, read: true, write: true}\nuninstall: keep\n",
            )
        };
        let ok = service("service:\n  url: https://svc.example.com:8443/ferrum\n  events: [Customer.created, SalesInvoice.*]\n");
        let m = AppManifest::parse(&ok).unwrap();
        let s = m.service.as_ref().unwrap();
        assert_eq!((m.runtime, s.host(), s.events.len()), (Runtime::Service, "svc.example.com", 2));
        let refused = |section: &str, needle: &str| {
            let err = AppManifest::parse(&service(section)).expect_err(section).to_string();
            assert!(err.contains(needle), "{section}: {err}");
        };
        refused("", "must say where the service is");
        refused("service:\n  url: ftp://svc.example.com\n", "service.url");
        refused("service:\n  url: https://svc.example.com/?key=1\n", "without a query");
        refused("service:\n  url: https://me@svc.example.com/\n", "without a query");
        refused("service:\n  url: https://svc.example.com\n  events: [Employee.created]\n", "which its access does not read");
        refused("service:\n  url: https://svc.example.com\n  events: ['*']\n", "which its access does not read");
        refused("service:\n  url: https://svc.example.com\n  events: [Customer.created, Customer.created]\n", "listed twice");
        refused("service:\n  url: https://svc.example.com\n  events: [\"Customer created\"]\n", "use topics like");
        rejected("uninstall: keep\n", "service:\n  url: https://svc.example.com\nuninstall: keep\n", "only service apps have a service section");
        let meta = ok.replace("runtime: service", "runtime: meta");
        assert!(AppManifest::parse(&meta).unwrap_err().to_string().contains("only service apps have a service section"));
        let resources = ok.replace("uninstall: keep\n", RESOURCE);
        assert!(AppManifest::parse(&resources).unwrap_err().to_string().contains("where the platform's resources do not reach"));
    }

    #[test]
    fn prefix_rules() {
        for ok in ["Ev", "VoltWork", "B2b", "EV"] {
            assert!(with("prefix: Ev", &format!("prefix: {ok}")).is_ok(), "{ok}");
        }
        rejected("prefix: Ev", "prefix: ev", "PascalCase");
        rejected("prefix: Ev", "prefix: E", "PascalCase");
        rejected("prefix: Ev", "prefix: VoltWorks", "PascalCase");
        rejected("prefix: Ev", "prefix: Ev_", "PascalCase");
        rejected("prefix: Ev", "prefix: Xv", "cannot start with X");
    }

    #[test]
    fn setting_rules() {
        let first = "{key: ocpi_token, label: OCPI token, type: secret}";
        rejected(first, "{key: OcpiToken, label: OCPI token, type: secret}", "snake_case");
        rejected(first, "{key: mode, label: OCPI token, type: secret}", "defined twice");
        rejected(first, "{key: ocpi_token, label: OCPI token, type: password}", "type must be one of");
        rejected(first, "{key: ocpi_token, label: \"\", type: secret}", "label is required");
        rejected(first, "{key: ocpi_token, label: OCPI token, type: select}", "needs options");
        rejected(first, "{key: ocpi_token, label: OCPI token, type: text, options: [a]}", "only for select");
        rejected(first, "{key: ocpi_token, label: OCPI token, type: select, options: [a, a]}", "twice");
        rejected(first, "{key: ocpi_token, label: OCPI token, type: secret, colour: red}", "unknown field");
        rejected(first, "{key: ocpi_token, label: OCPI token}", "missing field `type`");
        assert!(with(first, "{key: ocpi_token, label: OCPI token, type: number, description: Seconds}").is_ok());
    }
}
