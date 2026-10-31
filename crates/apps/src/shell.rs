//! The shell's contract in `app.yaml` (docs/plan/apps-ui.md §10.1): an app's own interface
//! (`ui`) and what it contributes to the shell's points: launcher entries and the routes they
//! own, which screen shows a record, the bar's Create menu, intents for other apps, side
//! panels, search providers, notification types, "My work" items, record cards and the pages
//! anyone with a link may open.

use crate::AppError;
use crate::bundle::{check_path, is_image_path};
use crate::manifest::AppManifest;
use semver::VersionReq;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The shell's points. Unlike the screen slots (`ui.*`), compiled-in apps contribute to them too.
pub const SHELL_POINTS: &[&str] = &[
    "shell.app",
    "shell.open",
    "shell.create",
    "shell.intent",
    "shell.sidepanel",
    "shell.search",
    "shell.notification",
    "shell.work",
    "shell.card",
    "shell.public",
    "shell.portal",
];

/// First path segments the core keeps for itself. The core only ever adds names starting with
/// `_` or `.`, or nests under these, so a new core page never collides with an installed app
/// (`assistant` joined before any app outside the platform could claim a route).
pub const RESERVED_ROUTES: &[&str] = &[
    "api",
    "assistant",
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

/// Where a person may be told of a notification type by default: in the bar, by email at once, in
/// the daily email digest, by push (in the bar until the platform pushes). None listed: off until
/// they turn it on.
pub const NOTIFY_CHANNELS: &[&str] = &["inapp", "email", "digest", "push"];

/// Kinds of origins an interface may add to its Content-Security-Policy (`form`: where its forms
/// may post, as an online editor's launch does).
pub const CSP_KINDS: &[&str] = &["connect", "frame", "img", "media", "font", "worker", "form"];

/// `FERRUM_…`: a server setting (environment variable) an app compiled into the server may name
/// in its policy, such as the editor server browsers reach.
fn is_env_name(s: &str) -> bool {
    s.strip_prefix("FERRUM_").is_some_and(|r| (1..=60).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'))
}

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
    /// or `wss://…` origins, `setting:{key}` for an origin an operator sets in one of the app's
    /// settings, or (apps compiled into the server) `env:FERRUM_…` for one the operator sets on
    /// the server. Reviewed by the store.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub csp: BTreeMap<String, Vec<String>>,
    /// The name of the family this app heads (`Workspace`): its add-ons with interfaces of their
    /// own share its navigation and its group in the launcher (docs/plan/apps-ui.md §11.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
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
        "shell.open" => (json!({"entity": text(80), "package": text(40), "path": text(200), "list": text(200), "priority": {"type": "integer"}}), vec!["path"]),
        "shell.create" => (json!({"label": text(60), "path": text(200), "entity": text(80)}), vec!["label", "path"]),
        "shell.intent" => (json!({"action": text(31), "type": text(31), "path": text(200), "title": text(60)}), vec!["action", "type", "path"]),
        // `contextual`: opened by apps with something to show (a file's comments), not from the bar.
        "shell.sidepanel" => (json!({"title": text(60), "path": text(200), "icon": text(200), "contextual": {"type": "boolean"}}), vec!["title", "path"]),
        "shell.search" => (json!({"title": text(60), "endpoint": text(200)}), vec!["endpoint"]),
        "shell.notification" => (json!({"label": text(100), "default": list(3, text(10))}), vec!["label"]),
        "shell.work" => (json!({"title": text(60), "endpoint": text(200)}), vec!["endpoint"]),
        "shell.card" => (json!({"entity": text(80), "endpoint": text(200)}), vec!["entity", "endpoint"]),
        "shell.public" => (json!({"path": text(200), "title": text(60)}), vec!["path"]),
        // A page of the workspace's portal at `/portal/{id}/`, drawn by the app's interface;
        // `open`: where under it a record of `entity` opens (`t/{id}`).
        "shell.portal" => (json!({"title": text(60), "description": text(200), "entity": text(80), "open": text(200)}), vec!["title"]),
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

/// An endpoint of the app's own: a plugin's (`/api/ext/{app}/…`) or a native app's
/// (`/api/app/{app}/…`).
fn check_endpoint(at: &str, endpoint: &str, app: &str) -> Result<(), AppError> {
    let rest =
        ["/api/ext/", "/api/app/"].iter().find_map(|base| endpoint.strip_prefix(base)).and_then(|r| r.strip_prefix(app)).and_then(|r| r.strip_prefix('/'));
    let ok = rest.is_some_and(|r| {
        !r.is_empty() && r.split('/').all(|s| !s.is_empty() && s != ".." && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)))
    });
    if !ok {
        return bad(format!("{at}: '{endpoint}' must be one of this app's endpoints, /api/ext/{app}/… or /api/app/{app}/…"));
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
                if let Some(name) = o.strip_prefix("env:") {
                    if m.runtime != crate::Runtime::Native {
                        return bad(format!("ui.csp.{kind}: only apps compiled into the server name server settings ({o})"));
                    }
                    if !is_env_name(name) {
                        return bad(format!("ui.csp.{kind}: '{name}' must be a FERRUM_… setting"));
                    }
                    continue;
                }
                // Images and media from anywhere secure (mail shows remote images on request).
                if o == "https:" && matches!(kind.as_str(), "img" | "media") {
                    continue;
                }
                match o.strip_prefix("setting:") {
                    Some(key) if m.settings.iter().any(|s| s.key == key) => {}
                    Some(key) => return bad(format!("ui.csp.{kind}: '{key}' is not one of this app's settings")),
                    None if is_origin(o) => {}
                    None => return bad(format!("ui.csp.{kind}: '{o}' must be an https:// or wss:// origin, or setting:{{key}}")),
                }
            }
        }
        if let Some(family) = &ui.family {
            if m.parent.is_some() {
                return bad("ui.family: an add-on belongs to its parent's family and names none");
            }
            if family.trim().is_empty() || family.chars().count() > 40 || family.chars().any(char::is_control) {
                return bad("ui.family must be a single line of at most 40 characters");
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
                // A public page lives below a route, never at it: the rest of the app keeps its sign-in.
                if *point == "shell.public" && (path.split('/').count() < 2 || path.ends_with('/')) {
                    return bad(format!("{at}: '{path}' must name a page below one of this app's routes, such as {}/s", path.trim_end_matches('/')));
                }
            }
            if *point == "shell.portal" {
                if m.ui.is_none() {
                    return bad(format!("{at}: a portal page is drawn by the app's own interface (a ui section)"));
                }
                match (field(item, "entity"), field(item, "open")) {
                    (None, None) => {}
                    (Some(_), Some(open)) => {
                        let plain =
                            |seg: &str| !seg.is_empty() && seg != "." && seg != ".." && seg.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b));
                        if !open.contains("{id}") || open.starts_with('/') || !open.split('/').all(|seg| seg == "{id}" || plain(seg)) {
                            return bad(format!("{at}.open: '{open}' must be plain path segments below the page with {{id}} in it, such as t/{{id}}"));
                        }
                    }
                    _ => return bad(format!("{at}: entity and open go together (which records the page shows, and where one opens)")),
                }
            }
            // shell.open's list of the records it opens (`/open/{entity}`).
            if let Some(list) = field(item, "list") {
                let placeholders: &[&str] = if field(item, "package").is_some() { &["{entity}"] } else { &[] };
                check_ui_path(&format!("{at}.list"), list, &routes, placeholders)?;
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
    let public: Vec<&str> = m.contributes.get("shell.public").into_iter().flatten().filter_map(|i| field(i, "path")).collect();
    for (i, a) in public.iter().enumerate() {
        for b in &public[i + 1..] {
            if a == b || b.starts_with(&format!("{a}/")) || a.starts_with(&format!("{b}/")) {
                return bad(format!("contributes.shell.public: '{a}' and '{b}' overlap"));
            }
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
    /// The paths of the pages anyone with a link may open (`drive/s`), longest first.
    pub fn public_paths(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.contributes.get("shell.public").into_iter().flatten().filter_map(|i| field(i, "path")).collect();
        out.sort_by_key(|p| std::cmp::Reverse(p.len()));
        out
    }

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
             \x20 shell.sidepanel:\n    - {{id: chat, title: Chat, path: chat/_panel}}\n    - {{id: thread, title: Thread, path: chat/_panel/thread, contextual: true}}\n\
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
    fn public_pages_and_families() {
        let app = format!("{UI}contributes:\n  shell.app:\n    - {{id: drive, route: drive, title: Drive}}\n");
        let m = parse(&format!("{app}  shell.public:\n    - {{id: link, path: drive/s, title: Shared link}}\n    - {{id: guest, path: drive/guest/files}}\n"))
            .expect("public pages");
        assert_eq!(m.public_paths(), vec!["drive/guest/files", "drive/s"], "longest first");
        refused(&format!("{app}  shell.public:\n    - {{id: all, path: drive}}\n"), "below one of this app's routes");
        refused(&format!("{app}  shell.public:\n    - {{id: all, path: drive/}}\n"), "below one of this app's routes");
        refused(&format!("{app}  shell.public:\n    - {{id: x, path: mail/s}}\n"), "must start with one of this app's routes");
        refused(&format!("{app}  shell.public:\n    - {{id: x, path: \"drive/{{id}}\"}}\n"), "plain path segments");
        refused(&format!("{app}  shell.public:\n    - {{id: a, path: drive/s}}\n    - {{id: b, path: drive/s/x}}\n"), "overlap");
        refused("contributes:\n  shell.public:\n    - {id: a, path: drive/s}\n", "needs the app's own interface");
        assert!(parse(&app).unwrap().public_paths().is_empty());

        let m = parse("ui:\n  dist: ui/dist\n  family: Workspace\n").expect("a family");
        assert_eq!(m.ui.unwrap().family.as_deref(), Some("Workspace"));
        refused("ui:\n  dist: ui/dist\n  family: \"  \"\n", "single line");
        refused(&format!("ui:\n  dist: ui/dist\n  family: {}\n", "x".repeat(41)), "at most 40");
        refused("requires: {drive: \">=1\"}\nparent: drive\nui:\n  dist: ui/dist\n  family: Docs\n", "parent's family");
    }

    #[test]
    fn portal_pages() {
        let app = format!("{UI}contributes:\n  shell.app:\n    - {{id: helpdesk, route: helpdesk, title: Helpdesk}}\n");
        parse(&format!(
            "{app}  shell.portal:\n    - {{id: tickets, title: Your tickets, description: Ask and follow up, entity: Ticket, open: \"t/{{id}}\"}}\n"
        ))
        .expect("a portal page");
        parse(&format!("{app}  shell.portal:\n    - {{id: tickets, title: Your tickets}}\n")).expect("without records to open");
        refused("contributes:\n  shell.portal:\n    - {id: tickets, title: Your tickets}\n", "own interface");
        refused(&format!("{app}  shell.portal:\n    - {{id: tickets, title: T, entity: Ticket}}\n"), "go together");
        refused(&format!("{app}  shell.portal:\n    - {{id: tickets, title: T, entity: Ticket, open: t}}\n"), "{id}");
        refused(&format!("{app}  shell.portal:\n    - {{id: tickets, title: T, entity: Ticket, open: \"../t/{{id}}\"}}\n"), "plain path segments");
        refused(&format!("{app}  shell.portal:\n    - {{id: tickets, title: T, entity: Ticket, open: \"/t/{{id}}\"}}\n"), "plain path segments");
        refused(&format!("{app}  shell.portal:\n    - {{id: tickets}}\n"), "title");
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
        parse(&format!("{app}  shell.open:\n    - {{id: e, package: selling, path: \"mail/e/{{entity}}/{{id}}\", list: \"mail/e/{{entity}}\"}}\n"))
            .expect("a package's lists");
        refused(&format!("{app}  shell.open:\n    - {{id: e, package: selling, path: \"mail/e/{{entity}}/{{id}}\", list: mail/e}}\n"), "{entity}");
        refused(&format!("{app}  shell.open:\n    - {{id: e, entity: Deal, path: \"mail/{{id}}\", list: \"mail/{{id}}\"}}\n"), "plain path segments");
        refused(&format!("{app}  shell.search:\n    - {{id: s, endpoint: /api/ext/drive/search}}\n"), "/api/ext/comms/");
        refused(&format!("{app}  shell.search:\n    - {{id: s, endpoint: /api/ext/comms/../admin}}\n"), "/api/ext/comms/");
        parse(&format!("{app}  shell.search:\n    - {{id: s, endpoint: /api/app/comms/search}}\n")).expect("a native app's endpoint");
        refused(&format!("{app}  shell.search:\n    - {{id: s, endpoint: /api/app/drive/search}}\n"), "/api/app/comms/");
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
        parse("ui:\n  dist: ui/dist\n  csp: {frame: [\"env:FERRUM_OFFICE_ORIGIN\"], form: [\"env:FERRUM_OFFICE_ORIGIN\"]}\n").expect("server settings");
        refused("ui:\n  dist: ui/dist\n  csp: {frame: [\"env:HOME\"]}\n", "FERRUM_");
        parse("ui:\n  dist: ui/dist\n  csp: {img: [\"https:\"]}\n").expect("images from anywhere secure");
        refused("ui:\n  dist: ui/dist\n  csp: {connect: [\"https:\"]}\n", "https:// or wss://");
        refused("ui:\n  dist: ui/dist\n  csp: {frame: [\"env:FERRUM_x\"]}\n", "FERRUM_");
        let store_app = BASE.replace("runtime: native", "runtime: meta");
        let e = AppManifest::parse(&format!("{store_app}ui:\n  dist: ui/dist\n  csp: {{frame: [\"env:FERRUM_OFFICE_ORIGIN\"]}}\n")).unwrap_err().to_string();
        assert!(e.contains("compiled into the server"), "{e}");
        refused("ui:\n  dist: ../dist\n", "ui.dist");
        refused("ui:\n  dist: ui/dist\n  shell: nonsense\n", "ui");
    }
}
