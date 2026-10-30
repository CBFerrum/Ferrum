//! The shell's contract in `app.yaml` (docs/plan/apps-ui.md §10.1): an app's own interface
//! (`ui`) and what it contributes to the shell's points: launcher entries and the routes they
//! own, which screen shows a record, the bar's Create menu, intents for other apps, side
//! panels, search providers, notification types, "My work" items and record cards.

use crate::AppError;
use crate::bundle::{check_path, is_image_path};
use crate::manifest::AppManifest;
use semver::VersionReq;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The shell's points. Unlike the screen slots (`ui.*`), compiled-in apps contribute to them too.
pub const SHELL_POINTS: &[&str] =
    &["shell.app", "shell.open", "shell.create", "shell.intent", "shell.sidepanel", "shell.search", "shell.notification", "shell.work", "shell.card"];

/// First path segments the core keeps for itself. The core only ever adds names starting with
/// `_` or `.`, or nests under these, so a new core page never collides with an installed app.
pub const RESERVED_ROUTES: &[&str] = &[
    "api",
    "ws",
    "files",
    "mcp",
    "metrics",
    "oauth",
    "open",
    "data",
    "admin",
    "store",
    "account",
    "portal",
    "accounts",
    "login",
    "logout",
    "s",
    "app",
    "home",
    "sw.js",
    "robots.txt",
    "favicon.ico",
    "manifest.webmanifest",
];

/// Intents the platform defines payloads for; apps may define their own actions and types.
pub const COMMON_INTENTS: &[&str] = &["pick", "create", "compose", "share", "preview", "schedule"];

/// Where a person may be told of a notification type.
pub const NOTIFY_CHANNELS: &[&str] = &["inapp", "email", "push"];

/// Kinds of origins an interface may add to its Content-Security-Policy.
pub const CSP_KINDS: &[&str] = &["connect", "frame", "img", "media", "font", "worker"];

/// An app's own interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSection {
    /// The built files: a directory in the bundle with an `index.html` (usually `ui/dist`).
    pub dist: String,
    /// The shell API versions it works with.
    #[serde(default = "any_shell")]
    pub shell: VersionReq,
    /// Origins its pages need beyond the workspace's host, by kind ([`CSP_KINDS`]): `https://…`
    /// or `wss://…` origins, or `setting:{key}` for an origin an operator sets in one of the app's
    /// settings. Reviewed by the store.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub csp: BTreeMap<String, Vec<String>>,
}

fn any_shell() -> VersionReq {
    VersionReq::parse(">=1.0").expect("valid")
}

/// `[a-z][a-z0-9-]{1,30}`: the first path segment an app's interface owns.
pub fn is_route(s: &str) -> bool {
    let b = s.as_bytes();
    (2..=31).contains(&b.len()) && b[0].is_ascii_lowercase() && b[1..].iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Whether the core keeps this first path segment.
pub fn is_reserved_route(s: &str) -> bool {
    RESERVED_ROUTES.contains(&s) || s.starts_with('_') || s.starts_with('.')
}

/// What each point's items hold besides `id`, in the subset of JSON Schema `contract` checks.
pub(crate) fn contract(point: &str) -> Value {
    let text = |max: u64| json!({"type": "string", "maxLength": max});
    let list = |max: u64, item: Value| json!({"type": "array", "items": item, "maxItems": max});
    let (props, required): (Value, Vec<&str>) = match point {
        "shell.app" => (
            json!({
                "route": text(31), "title": text(60), "description": text(200), "icon": text(200), "color": text(7),
                "roles": list(30, text(60)), "aliases": list(5, text(31)), "compact": {"type": "boolean"},
                "priority": {"type": "integer"}, "entity": text(80),
            }),
            vec!["title"],
        ),
        "shell.open" => (json!({"entity": text(80), "package": text(40), "path": text(200), "priority": {"type": "integer"}}), vec!["path"]),
        "shell.create" => (json!({"label": text(60), "path": text(200), "entity": text(80)}), vec!["label", "path"]),
        "shell.intent" => (json!({"action": text(31), "type": text(31), "path": text(200), "title": text(60)}), vec!["action", "type", "path"]),
        "shell.sidepanel" => (json!({"title": text(60), "path": text(200), "icon": text(200)}), vec!["title", "path"]),
        "shell.search" => (json!({"title": text(60), "endpoint": text(200)}), vec!["endpoint"]),
        "shell.notification" => (json!({"label": text(100), "default": list(3, text(10))}), vec!["label"]),
        "shell.work" => (json!({"title": text(60), "endpoint": text(200)}), vec!["endpoint"]),
        "shell.card" => (json!({"entity": text(80), "endpoint": text(200)}), vec!["entity", "endpoint"]),
        _ => (json!({}), vec![]),
    };
    json!({"type": "object", "additionalProperties": false, "required": required, "properties": props})
}

fn bad<T>(msg: impl Into<String>) -> Result<T, AppError> {
    Err(AppError::Manifest(msg.into()))
}

fn field<'a>(item: &'a Value, k: &str) -> Option<&'a str> {
    item.get(k).and_then(Value::as_str)
}

/// A path into the app's own interface: its first segment is one of the app's routes; then
/// plain segments, and the placeholders the point fills in (`{id}`, `{entity}`).
fn check_ui_path(at: &str, path: &str, routes: &BTreeSet<&str>, placeholders: &[&str]) -> Result<(), AppError> {
    let mut segments = path.split('/');
    let first = segments.next().unwrap_or_default();
    if !routes.contains(first) {
        return bad(format!("{at}: '{path}' must start with one of this app's routes ({})", routes.iter().copied().collect::<Vec<_>>().join(", ")));
    }
    for (i, s) in segments.enumerate() {
        let last = i + 2 == path.split('/').count();
        let plain = !s.is_empty() && s != "." && s != ".." && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
        if !(plain || placeholders.contains(&s) || (last && s.is_empty())) {
            let also = if placeholders.is_empty() { String::new() } else { format!(" and {}", placeholders.join(", ")) };
            return bad(format!("{at}: '{path}' may hold only plain path segments{also}"));
        }
    }
    for ph in placeholders {
        if !path.contains(ph) {
            return bad(format!("{at}: '{path}' must say where the record's {} goes, as {ph}", ph.trim_matches(['{', '}'])));
        }
    }
    Ok(())
}

/// An endpoint of the app's own (`/api/ext/{app}/…`).
fn check_endpoint(at: &str, endpoint: &str, app: &str) -> Result<(), AppError> {
    let rest = endpoint.strip_prefix("/api/ext/").and_then(|r| r.strip_prefix(app)).and_then(|r| r.strip_prefix('/'));
    let ok = rest.is_some_and(|r| {
        !r.is_empty() && r.split('/').all(|s| !s.is_empty() && s != ".." && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)))
    });
    if !ok {
        return bad(format!("{at}: '{endpoint}' must be one of this app's endpoints, /api/ext/{app}/…"));
    }
    Ok(())
}

fn check_entity(at: &str, entity: &str) -> Result<(), AppError> {
    if !ferrum_meta::is_entity_name(entity) {
        return bad(format!("{at}: '{entity}' is not an entity name"));
    }
    Ok(())
}

fn ident(s: &str) -> bool {
    let b = s.as_bytes();
    (2..=31).contains(&b.len()) && b[0].is_ascii_lowercase() && b[1..].iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

fn is_origin(s: &str) -> bool {
    let rest = s.strip_prefix("https://").or_else(|| s.strip_prefix("wss://"));
    rest.is_some_and(|r| {
        let host = r.split(':').next().unwrap_or_default();
        let port_ok = r.split_once(':').is_none_or(|(_, p)| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()));
        !host.is_empty()
            && host.len() <= 253
            && port_ok
            && host.split('.').all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'*'))
            && host.matches('*').count() <= 1
            && (!host.contains('*') || host.starts_with("*."))
    })
}

/// The `ui` section and the contributions to the shell's points.
pub(crate) fn check(m: &AppManifest) -> Result<(), AppError> {
    if let Some(ui) = &m.ui {
        check_path(&ui.dist).map_err(|e| AppError::Manifest(format!("ui.dist: {e}")))?;
        if ui.dist.ends_with('/') || ui.dist.starts_with("packages/") || ui.dist.starts_with("assets/") {
            return bad(format!("ui.dist: '{}' must be a directory of built files of its own, such as ui/dist", ui.dist));
        }
        for (kind, origins) in &ui.csp {
            if !CSP_KINDS.contains(&kind.as_str()) {
                return bad(format!("ui.csp: '{kind}' is not one of {}", CSP_KINDS.join(", ")));
            }
            if origins.len() > 10 {
                return bad(format!("ui.csp.{kind}: at most 10 origins"));
            }
            for o in origins {
                match o.strip_prefix("setting:") {
                    Some(key) if m.settings.iter().any(|s| s.key == key) => {}
                    Some(key) => return bad(format!("ui.csp.{kind}: '{key}' is not one of this app's settings")),
                    None if is_origin(o) => {}
                    None => return bad(format!("ui.csp.{kind}: '{o}' must be an https:// or wss:// origin, or setting:{{key}}")),
                }
            }
        }
    }

    // Routes first: other points' paths start with them.
    let mut routes = BTreeSet::new();
    for item in m.contributes.get("shell.app").into_iter().flatten() {
        let id = field(item, "id").unwrap_or_default();
        let at = format!("contributes.shell.app.{id}");
        match (field(item, "route"), field(item, "entity")) {
            (Some(route), None) => {
                if m.ui.is_none() {
                    return bad(format!("{at}: a route needs the app's own interface (a ui section); records-only apps name an entity instead"));
                }
                for (k, r) in std::iter::once(("route", route))
                    .chain(item.get("aliases").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(|a| ("aliases", a)))
                {
                    if !is_route(r) {
                        return bad(format!("{at}.{k}: '{r}' must be 2-31 characters: a lower-case letter, then lower-case letters, digits or '-'"));
                    }
                    if is_reserved_route(r) {
                        return bad(format!("{at}.{k}: '{r}' is kept by the core"));
                    }
                    if !routes.insert(r) {
                        return bad(format!("{at}.{k}: '{r}' is used twice"));
                    }
                }
            }
            (None, Some(entity)) => check_entity(&at, entity)?,
            _ => return bad(format!("{at}: give a route (the app's own interface) or an entity (a records-only entry), not both")),
        }
        if let Some(color) = field(item, "color")
            && !(color.len() == 7 && color.starts_with('#') && color[1..].bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return bad(format!("{at}.color must be #rrggbb"));
        }
        if let Some(icon) = field(item, "icon") {
            let name = !icon.is_empty() && icon.len() <= 40 && icon.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            let image = check_path(icon).is_ok() && is_image_path(icon);
            if !(name || image) {
                return bad(format!("{at}.icon must be an icon name such as mail, or an image under assets/"));
            }
        }
        for role in item.get("roles").and_then(Value::as_array).into_iter().flatten() {
            let r = role.as_str().unwrap_or_default();
            if r.trim().is_empty() || r.chars().any(char::is_control) {
                return bad(format!("{at}.roles: role names are single lines of text"));
            }
        }
        if item.get("aliases").is_some() && field(item, "route").is_none() {
            return bad(format!("{at}.aliases: only an entry with a route has aliases"));
        }
    }

    for point in SHELL_POINTS.iter().filter(|p| **p != "shell.app") {
        for item in m.contributes.get(*point).into_iter().flatten() {
            let id = field(item, "id").unwrap_or_default();
            let at = format!("contributes.{point}.{id}");
            if *point == "shell.open" {
                match (field(item, "entity"), field(item, "package")) {
                    (Some(_), None) | (None, Some(_)) => {}
                    _ => return bad(format!("{at}: name an entity, or a package whose records the app shows")),
                }
                if let Some(pkg) = field(item, "package")
                    && !ferrum_meta::is_ident(pkg)
                {
                    return bad(format!("{at}.package: '{pkg}' is not a package name"));
                }
            }
            if let Some(path) = field(item, "path") {
                if m.ui.is_none() {
                    return bad(format!("{at}: a path needs the app's own interface (a ui section)"));
                }
                let placeholders: &[&str] = match (*point, field(item, "package")) {
                    ("shell.open", Some(_)) => &["{entity}", "{id}"],
                    ("shell.open", None) => &["{id}"],
                    _ => &[],
                };
                check_ui_path(&at, path, &routes, placeholders)?;
            }
            if let Some(endpoint) = field(item, "endpoint") {
                check_endpoint(&at, endpoint, &m.id)?;
            }
            if let Some(entity) = field(item, "entity") {
                check_entity(&at, entity)?;
            }
            for k in ["action", "type"] {
                if let Some(v) = field(item, k)
                    && !ident(v)
                {
                    return bad(format!("{at}.{k}: '{v}' must be 2-31 lower-case letters, digits, '-' or '_'"));
                }
            }
            for k in ["title", "label"] {
                if let Some(v) = field(item, k)
                    && (v.trim().is_empty() || v.chars().any(char::is_control))
                {
                    return bad(format!("{at}.{k} must be a single line of text"));
                }
            }
            for c in item.get("default").and_then(Value::as_array).into_iter().flatten() {
                if !c.as_str().is_some_and(|c| NOTIFY_CHANNELS.contains(&c)) {
                    return bad(format!("{at}.default: channels are {}", NOTIFY_CHANNELS.join(", ")));
                }
            }
        }
    }

    // One handler per action and type within an app; one page per entity.
    let mut intents = BTreeSet::new();
    for item in m.contributes.get("shell.intent").into_iter().flatten() {
        let key = (field(item, "action").unwrap_or_default(), field(item, "type").unwrap_or_default());
        if !intents.insert(key) {
            return bad(format!("contributes.shell.intent: {} {} is handled twice", key.0, key.1));
        }
    }
    let mut opened = BTreeMap::new();
    for item in m.contributes.get("shell.open").into_iter().flatten() {
        let what = field(item, "entity").or_else(|| field(item, "package")).unwrap_or_default();
        if opened.insert(what, ()).is_some() {
            return bad(format!("contributes.shell.open: {what} is opened twice"));
        }
    }
    Ok(())
}

impl AppManifest {
    /// The routes this app's interface owns, with their aliases.
    pub fn routes(&self) -> Vec<(String, Vec<String>)> {
        self.contributes
            .get("shell.app")
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let route = field(item, "route")?;
                let aliases = item.get("aliases").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
                Some((route.to_string(), aliases))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str =
        "id: comms\nname: Comms\nversion: 1.0.0\npublisher: cbferrum\nsummary: Mail\ncategories: [communication]\nruntime: native\nplatform: \">=0.3.1\"\n";

    fn parse(extra: &str) -> Result<AppManifest, AppError> {
        AppManifest::parse(&format!("{BASE}{extra}"))
    }

    fn refused(extra: &str, says: &str) {
        let e = parse(extra).expect_err(extra).to_string();
        assert!(e.contains(says), "{e}\n-- expected: {says}");
    }

    const UI: &str = "ui:\n  dist: ui/dist\n";

    #[test]
    fn an_interface_with_routes_intents_and_panels() {
        let m = parse(&format!(
            "{UI}settings:\n  - {{key: meet_url, label: Meet, type: text}}\n\
             contributes:\n  shell.app:\n    - {{id: mail, route: mail, title: Mail, icon: mail, color: \"#2563eb\", roles: [Comms User], aliases: [inbox]}}\n    - {{id: chat, route: chat, title: Chat, icon: assets/chat.svg}}\n\
             \x20 shell.open:\n    - {{id: event, entity: CommsEvent, path: \"chat/events/{{id}}\"}}\n\
             \x20 shell.create:\n    - {{id: email, label: Email, path: mail/compose}}\n\
             \x20 shell.intent:\n    - {{id: compose, action: compose, type: email, path: mail/_intent/compose}}\n\
             \x20 shell.sidepanel:\n    - {{id: chat, title: Chat, path: chat/_panel}}\n\
             \x20 shell.search:\n    - {{id: mail, endpoint: /api/ext/comms/search}}\n\
             \x20 shell.notification:\n    - {{id: mention, label: Someone mentions you, default: [inapp, push]}}\n\
             \x20 shell.card:\n    - {{id: event, entity: CommsEvent, endpoint: /api/ext/comms/cards/event}}\n"
        ))
        .expect("valid");
        assert_eq!(m.routes(), vec![("mail".to_string(), vec!["inbox".to_string()]), ("chat".to_string(), vec![])]);
        assert_eq!(m.ui.as_ref().unwrap().shell, VersionReq::parse(">=1.0").unwrap());
        // csp from a setting, and origins.
        parse(
            "ui:\n  dist: ui/dist\n  csp: {connect: [\"setting:meet_url\", \"wss://*.livekit.cloud\"], img: [\"https://maps.example.com:8443\"]}\n\
             settings:\n  - {key: meet_url, label: Meet, type: text}\n",
        )
        .expect("csp");
        // A records-only entry needs no interface.
        parse("contributes:\n  shell.app:\n    - {id: assets, title: Assets, entity: Asset}\n").expect("records-only");
    }

    #[test]
    fn routes_paths_and_endpoints_are_checked() {
        refused("contributes:\n  shell.app:\n    - {id: mail, route: mail, title: Mail}\n", "needs the app's own interface");
        refused(&format!("{UI}contributes:\n  shell.app:\n    - {{id: x, route: admin, title: X}}\n"), "kept by the core");
        refused(&format!("{UI}contributes:\n  shell.app:\n    - {{id: x, route: _x, title: X}}\n"), "must be 2-31 characters");
        refused(&format!("{UI}contributes:\n  shell.app:\n    - {{id: x, route: Mail, title: X}}\n"), "must be 2-31 characters");
        refused(&format!("{UI}contributes:\n  shell.app:\n    - {{id: x, route: mail, title: X, aliases: [mail]}}\n"), "used twice");
        refused(&format!("{UI}contributes:\n  shell.app:\n    - {{id: x, route: mail, entity: Mail, title: X}}\n"), "not both");
        refused(&format!("{UI}contributes:\n  shell.app:\n    - {{id: x, route: mail, title: X, color: blue}}\n"), "#rrggbb");
        refused(&format!("{UI}contributes:\n  shell.app:\n    - {{id: x, route: mail, title: X, icon: \"../x.png\"}}\n"), "icon must be");
        refused(&format!("{UI}contributes:\n  shell.app:\n    - {{id: x, route: mail, title: X, extra: 1}}\n"), "extra");
        let app = format!("{UI}contributes:\n  shell.app:\n    - {{id: mail, route: mail, title: Mail}}\n");
        refused(&format!("{app}  shell.create:\n    - {{id: e, label: E, path: chat/compose}}\n"), "must start with one of this app's routes");
        refused(&format!("{app}  shell.create:\n    - {{id: e, label: E, path: mail/../admin}}\n"), "plain path segments");
        refused(&format!("{app}  shell.open:\n    - {{id: e, entity: CommsEvent, path: mail/events}}\n"), "{id}");
        refused(&format!("{app}  shell.create:\n    - {{id: e, label: E, path: \"mail/{{id}}\"}}\n"), "plain path segments");
        refused(&format!("{app}  shell.open:\n    - {{id: e, package: selling, path: \"mail/{{id}}\"}}\n"), "{entity}");
        refused(&format!("{app}  shell.open:\n    - {{id: e, entity: Deal, package: selling, path: \"mail/{{id}}\"}}\n"), "name an entity, or a package");
        parse(&format!("{app}  shell.open:\n    - {{id: e, package: selling, path: \"mail/e/{{entity}}/{{id}}\"}}\n")).expect("a package's records");
        refused(&format!("{app}  shell.search:\n    - {{id: s, endpoint: /api/ext/drive/search}}\n"), "/api/ext/comms/");
        refused(&format!("{app}  shell.search:\n    - {{id: s, endpoint: /api/ext/comms/../admin}}\n"), "/api/ext/comms/");
        refused(&format!("{app}  shell.intent:\n    - {{id: a, action: Pick, type: file, path: mail/pick}}\n"), "lower-case");
        refused(
            &format!(
                "{app}  shell.intent:\n    - {{id: a, action: pick, type: file, path: mail/a}}\n    - {{id: b, action: pick, type: file, path: mail/b}}\n"
            ),
            "handled twice",
        );
        refused(&format!("{app}  shell.notification:\n    - {{id: n, label: N, default: [sms]}}\n"), "channels are");
        refused("ui:\n  dist: ui/dist\n  csp: {script: [\"https://cdn.example.com\"]}\n", "is not one of");
        refused("ui:\n  dist: ui/dist\n  csp: {connect: [\"http://insecure.example.com\"]}\n", "https:// or wss://");
        refused("ui:\n  dist: ui/dist\n  csp: {connect: [\"setting:nope\"]}\n", "not one of this app's settings");
        refused("ui:\n  dist: ../dist\n", "ui.dist");
        refused("ui:\n  dist: ui/dist\n  shell: nonsense\n", "ui");
    }
}
