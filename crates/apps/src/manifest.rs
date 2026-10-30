//! `app.yaml`: what an app is, what it needs and what it brings.

use crate::AppError;
use crate::bundle::{PLUGIN_JSON, check_path, is_image_path};
use crate::contract;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// The platform's extension points for screens (docs/plugins.md, "Screens"): a page with a
/// sidebar entry, a dashboard widget, a panel on the forms of an entity, the app's settings page.
pub const UI_POINTS: &[&str] = &["ui.page", "ui.widget", "ui.panel", "ui.settings"];
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

/// What a screen slot's items hold besides `id`.
fn screen_contract(point: &str) -> Value {
    let text = |max: u64| json!({"type": "string", "maxLength": max});
    let mut props = serde_json::Map::new();
    props.insert("entry".into(), text(200));
    props.insert("title".into(), text(60));
    let mut required = vec!["entry"];
    match point {
        "ui.page" => {
            props.insert("icon".into(), text(40));
            required.push("title");
        }
        "ui.widget" => {
            props.insert("size".into(), json!({"type": "array", "items": {"type": "integer"}, "maxItems": 2}));
            required.push("title");
        }
        "ui.panel" => {
            props.insert("entity".into(), text(80));
            required.extend(["title", "entity"]);
        }
        _ => {}
    }
    json!({"type": "object", "additionalProperties": false, "required": required, "properties": props})
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
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Runtime::Native => "native",
            Runtime::Wasm => "wasm",
            Runtime::Meta => "meta",
        })
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
    /// The app this one is an add-on of, shown under it; it must also be in `requires`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Package directories under `packages/`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<String>,
    /// wasm apps only: `plugin.json` (the module is `plugin.wasm`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    /// PascalCase namespace of a third-party app's entities (reserved per publisher in the store).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<SettingDef>,
    /// Points other apps may contribute to, named `<this app's id>.<name>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extension_points: BTreeMap<String, ExtensionPoint>,
    /// What it adds to extension points: the screen slots ([`UI_POINTS`]), its own points or
    /// those of apps it requires. Point → items, each with an `id` unique within the point.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub contributes: BTreeMap<String, Vec<Value>>,
    /// The app's own interface (docs/plan/apps-ui.md §10.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<crate::shell::UiSection>,
    #[serde(default)]
    pub uninstall: Uninstall,
}

fn bad<T>(msg: impl Into<String>) -> Result<T, AppError> {
    Err(AppError::Manifest(msg.into()))
}

/// What the contract of a screen slot cannot say: the entry is an HTML page under `ui/`, titles
/// are single lines, icons are icon names, widget sizes fit the 12-column grid.
fn check_screen(point: &str, id: &str, item: &Value) -> Result<(), AppError> {
    let field = |k: &str| item.get(k).and_then(Value::as_str);
    let at = format!("contributes.{point}.{id}");
    let entry = field("entry").unwrap_or_default();
    if let Err(e) = check_path(entry) {
        return bad(format!("{at}.entry: {e}"));
    }
    if !entry.starts_with("ui/") || !entry.ends_with(".html") {
        return bad(format!("{at}.entry must be an HTML page under ui/, not {entry}"));
    }
    if let Some(title) = field("title") {
        line(&format!("{at}.title"), title, 60)?;
    }
    if let Some(icon) = field("icon")
        && (icon.is_empty() || !icon.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
    {
        return bad(format!("{at}.icon must be an icon name such as truck or file-text"));
    }
    if let Some(entity) = field("entity")
        && !ferrum_meta::is_entity_name(entity)
    {
        return bad(format!("{at}.entity '{entity}' is not an entity name"));
    }
    if let Some(size) = item.get("size").and_then(Value::as_array) {
        let ok = size.len() == 2 && size.iter().all(|n| n.as_u64().is_some_and(|n| (1..=12).contains(&n)));
        if !ok {
            return bad(format!("{at}.size must be [width, height], each 1 to 12 grid cells"));
        }
    }
    Ok(())
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
        let m: AppManifest = serde_yaml::from_str(yaml).map_err(|e| AppError::Manifest(e.to_string()))?;
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

        for dep in self.requires.keys() {
            if !is_app_id(dep) {
                return bad(format!("requires: '{dep}' is not an app id"));
            }
            if dep == &self.id {
                return bad("an app cannot require itself");
            }
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
        self.validate_extensions()
    }

    /// `parent`, `extension_points` and `contributes`. Contributions to another app's point are
    /// checked against its contract when a plan brings the two together.
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
            let screen = UI_POINTS.contains(&point.as_str());
            let shell = crate::shell::SHELL_POINTS.contains(&point.as_str());
            let foreign = point.split_once('.').is_some_and(|(owner, rest)| self.requires.contains_key(owner) && ferrum_meta::is_ident(rest));
            if !screen && !shell && !foreign && !self.extension_points.contains_key(point) {
                return bad(format!(
                    "contributes: '{point}' is not a screen slot ({}), a shell point ({}), an extension point of this app, or one of an app in requires",
                    UI_POINTS.join(", "),
                    crate::shell::SHELL_POINTS.join(", ")
                ));
            }
            if screen && self.runtime == Runtime::Native {
                return bad(format!("contributes.{point}: native apps put their screens in the web app"));
            }
            if items.is_empty() || items.len() > MAX_ITEMS {
                return bad(format!("contributes.{point}: list 1 to {MAX_ITEMS} items"));
            }
            if point == "ui.settings" && items.len() > 1 {
                return bad("contributes.ui.settings: an app has one settings page");
            }
            let schema = if screen {
                Some(screen_contract(point))
            } else if shell {
                Some(crate::shell::contract(point))
            } else {
                self.extension_points.get(point).and_then(|p| p.schema.clone())
            };
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
                if screen {
                    check_screen(point, id, item)?;
                }
            }
        }
        crate::shell::check(self)
    }

    /// The pages (`ui/….html`) its screen contributions open.
    pub fn screen_entries(&self) -> Vec<&str> {
        UI_POINTS.iter().filter_map(|p| self.contributes.get(*p)).flatten().filter_map(|item| item.get("entry").and_then(Value::as_str)).collect()
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
platform: ">=0.3, <0.4"
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
        assert!(!m.compatible_with(&Version::new(0, 4, 0)));
        // Round trip through JSON (how the server stores it).
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(serde_json::from_value::<AppManifest>(json).unwrap(), m);
    }

    #[test]
    fn minimal_manifest_and_defaults() {
        let m = AppManifest::parse(
            "id: kit\nname: Kit\nversion: 0.1.0\npublisher: acme\nsummary: A kit\ncategories: [industry]\nruntime: meta\nplatform: '^0.3'\npackages: [kit]\n",
        )
        .unwrap();
        assert_eq!(m.uninstall, Uninstall::Keep);
        assert!(m.requires.is_empty() && m.settings.is_empty() && m.prefix.is_none());
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
    - {id: sessions, anything: goes}
  ui.page:
    - {id: board, title: Charge points, entry: ui/board.html, icon: zap}
  ui.widget:
    - {id: load, title: Live load, entry: ui/load.html, size: [4, 3]}
  ui.panel:
    - {id: sessions, title: Sessions, entity: Customer, entry: ui/sessions.html}
  ui.settings:
    - {id: settings, entry: ui/settings.html}"#;

    #[test]
    fn parent_extension_points_and_contributions() {
        let m = with("uninstall: keep", EXTENDS).unwrap();
        assert_eq!(m.parent.as_deref(), Some("selling"));
        assert_eq!(m.screen_entries(), ["ui/board.html", "ui/load.html", "ui/sessions.html", "ui/settings.html"]);
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
        swap("  selling.quote_source:", "  crm.quote_source:", "'crm.quote_source' is not a screen slot");
        swap("{id: board, title", "{title", "contributes.ui.page: every item needs an id");
        swap(
            "    - {id: board, title: Charge points, entry: ui/board.html, icon: zap}",
            "    - {id: board, title: Charge points, entry: ui/board.html, icon: zap}\n    - {id: board, title: Again, entry: ui/again.html}",
            "contributes.ui.page: id 'board' is used twice",
        );
        swap("{id: sessions, title", "{id: sessions, colour: red, title", "unknown field colour");
        swap("title: Charge points, ", "", "contributes.ui.page.board: (top) needs title");
        swap("entry: ui/board.html", "entry: board.html", "must be an HTML page under ui/");
        swap("entry: ui/board.html", "entry: ui/../board.html", "'..' is not allowed");
        swap("entry: ui/board.html", "entry: ui/board.js", "must be an HTML page under ui/");
        swap("size: [4, 3]", "size: [13, 3]", "each 1 to 12 grid cells");
        swap("icon: zap", "icon: Zap!", "must be an icon name");
        swap("entity: Customer", "entity: customer", "'customer' is not an entity name");
        swap("    - {id: settings, entry: ui/settings.html}", "    - {id: a, entry: ui/a.html}\n    - {id: b, entry: ui/b.html}", "one settings page");

        let native = "id: kit\nname: Kit\nversion: 0.1.0\npublisher: acme\nsummary: A kit\ncategories: [industry]\nruntime: native\nplatform: '^0.3'\n\
                      contributes:\n  ui.page:\n    - {id: home, title: Kit, entry: ui/home.html}\n";
        let err = AppManifest::parse(native).unwrap_err().to_string();
        assert!(err.contains("native apps put their screens in the web app"), "{err}");
    }

    #[test]
    fn required_fields_and_unknown_fields() {
        for field in ["id: ev-charging\n", "version: 1.4.0\n", "runtime: wasm\n", "platform: \">=0.3, <0.4\"\n", "publisher: voltworks\n"] {
            assert!(with(field, "").is_err(), "{field} is required");
        }
        rejected("uninstall: keep", "uninstall: keep\nfavourite: yes", "unknown field");
        rejected("uninstall: keep", "uninstall: never", "unknown variant");
        rejected("runtime: wasm", "runtime: python", "unknown variant");
        rejected("version: 1.4.0", "version: 1.4", "");
        rejected("platform: \">=0.3, <0.4\"", "platform: \"not a requirement\"", "");
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
