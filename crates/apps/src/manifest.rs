//! `app.yaml`: what an app is, what it needs and what it brings.

use crate::AppError;
use crate::bundle::{PLUGIN_JSON, check_path, is_image_path};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

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
    #[serde(default)]
    pub uninstall: Uninstall,
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
        if self.runtime == Runtime::Meta && self.packages.is_empty() {
            return bad("a meta app needs at least one package");
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
        Ok(())
    }

    /// Whether this app works with platform (app API) version `platform`.
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
        assert!(AppManifest::parse(&meta).unwrap_err().to_string().contains("at least one package"));
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
