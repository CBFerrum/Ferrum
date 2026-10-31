//! What an app may do, for consent screens: the summary, its digest and what an upgrade adds.

use crate::bundle::{PLUGIN_JSON, namespaced};
use crate::manifest::{AppManifest, Runtime};
use crate::{AppError, Bundle, MAX_PLUGIN_FUEL, MAX_PLUGIN_MEMORY_MB};
use ferrum_meta::{EntityDef, OverlayOp, PermissionDef};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// Document stages a plugin hook may bind (the plugin host's list).
const STAGES: &[&str] = &["before_validate", "validate", "before_submit", "on_submit", "before_cancel", "on_cancel", "before_delete"];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookBinding {
    entity: String,
    stage: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Route {
    path: String,
    #[serde(default)]
    public: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Schedule {
    name: String,
    every_seconds: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SocketDef {
    path: String,
    #[serde(default)]
    public: bool,
    #[serde(default, rename = "subprotocols")]
    _subprotocols: Vec<String>,
}

/// `plugin.json`: the plugin manifest the server installs (`ferrum_plugins::NewPlugin` without
/// the module), with the same field types and the checks that need no schema. Unknown fields are
/// refused so that nothing a plugin asks for can escape the permission summary. The server still
/// runs its own validation (entities named in hooks and permissions must exist) at install.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PluginManifest {
    name: String,
    #[serde(default, rename = "version")]
    _version: String,
    #[serde(default, rename = "description")]
    _description: Option<String>,
    #[serde(default)]
    fuel_limit: Option<i64>,
    #[serde(default)]
    memory_limit_mb: Option<i32>,
    #[serde(default, rename = "config")]
    _config: Option<Map<String, Value>>,
    #[serde(default)]
    hooks: Vec<HookBinding>,
    #[serde(default)]
    routes: Vec<Route>,
    #[serde(default)]
    subscriptions: Vec<String>,
    #[serde(default)]
    schedules: Vec<Schedule>,
    #[serde(default)]
    sockets: Vec<SocketDef>,
    #[serde(default)]
    permissions: Vec<PermissionDef>,
    #[serde(default)]
    http_hosts: Vec<String>,
    #[serde(default)]
    entities: Vec<EntityDef>,
}

impl PluginManifest {
    pub(crate) fn parse(json: &[u8]) -> Result<PluginManifest, AppError> {
        serde_json::from_slice(json).map_err(|e| AppError::Plugin(format!("plugin.json: {e}")))
    }

    /// The plugin host's checks that need no schema, plus the app rules (name, entity prefix,
    /// resource caps).
    pub(crate) fn validate(&self, app: &AppManifest) -> Result<(), AppError> {
        let bad = |m: String| Err(AppError::Plugin(m));
        if self.name != app.id {
            return bad(format!("plugin.json name must be the app id '{}', not '{}'", app.id, self.name));
        }
        if self.hooks.is_empty() && self.routes.is_empty() && self.subscriptions.is_empty() && self.schedules.is_empty() && self.sockets.is_empty() {
            return bad("a plugin needs at least one hook, route, subscription, schedule or socket".into());
        }
        let entity_ok = |e: &str| e == "*" || ferrum_meta::is_entity_name(e);
        for h in &self.hooks {
            if !STAGES.contains(&h.stage.as_str()) {
                return bad(format!("hook on {}: stage must be one of {}", h.entity, STAGES.join(", ")));
            }
            if !entity_ok(&h.entity) {
                return bad(format!("hook entity '{}' is not an entity name or *", h.entity));
            }
        }
        let path_ok = |p: &str| p.starts_with('/') && p.len() <= 100 && !p.contains("..") && !p.contains("//");
        for p in self.routes.iter().map(|r| &r.path).chain(self.sockets.iter().map(|s| &s.path)) {
            if !path_ok(p) {
                return bad(format!("path '{p}' must start with / (at most 100 characters, no .. or //)"));
            }
        }
        let mut names = BTreeSet::new();
        for s in &self.schedules {
            if s.every_seconds < 60 {
                return bad(format!("schedule '{}': every_seconds must be at least 60", s.name));
            }
            if s.name.is_empty() || s.name.len() > 60 || !names.insert(&s.name) {
                return bad(format!("schedule '{}' needs a unique name of at most 60 characters", s.name));
            }
        }
        for t in &self.subscriptions {
            if t.is_empty() || t.len() > 120 || !t.chars().all(|c| c.is_ascii_alphanumeric() || "._-*".contains(c)) {
                return bad(format!("subscription '{t}': use topics like SalesInvoice.submitted, SalesInvoice.* or *"));
            }
        }
        for p in &self.permissions {
            if !entity_ok(&p.entity) {
                return bad(format!("permission entity '{}' is not an entity name or *", p.entity));
            }
        }
        for h in &self.http_hosts {
            let bare = h.strip_prefix("*.").unwrap_or(h);
            if bare.is_empty() || bare.contains('*') || !bare.chars().all(|c| c.is_ascii_alphanumeric() || ".-".contains(c)) || !bare.contains('.') {
                return bad(format!("http_hosts: '{h}' must be a host name such as api.example.com or *.example.com"));
            }
        }
        for e in &self.entities {
            let ok =
                ferrum_meta::is_entity_name(&e.name) && app.prefix.as_deref().is_some_and(|p| e.name.strip_prefix('X').is_some_and(|rest| namespaced(rest, p)));
            if !ok {
                let p = app.prefix.as_deref().unwrap_or("<prefix>");
                return bad(format!(
                    "plugin entity {} must be named X{p}… (X, the app prefix, then a capital letter or digit; set prefix in app.yaml)",
                    e.name
                ));
            }
        }
        if let Some(f) = self.fuel_limit
            && !(1..=MAX_PLUGIN_FUEL).contains(&f)
        {
            return bad(format!("fuel_limit must be between 1 and {MAX_PLUGIN_FUEL}"));
        }
        if let Some(m) = self.memory_limit_mb
            && !(1..=MAX_PLUGIN_MEMORY_MB).contains(&m)
        {
            return bad(format!("memory_limit_mb must be between 1 and {MAX_PLUGIN_MEMORY_MB}"));
        }
        Ok(())
    }
}

/// Everything an app asks for, as a consent screen shows it. Every collection is sorted, so equal
/// permissions have equal [`Permissions::digest`]s. `increases(&Permissions::default())` lists
/// all of it in readable lines.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Permissions {
    /// Runs native code inside the server: everything the platform can do (reviewed code only).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub native: bool,
    /// Records its code may access (its plugin's service role, and the most it may do for a
    /// user): entity, or `*` for every entity → `read`, `write`, `create`, `delete`, `submit`,
    /// `cancel`, `export`, `import` (suffixed ` (own records)` when limited to them) and
    /// `read field group <g>` / `write field group <g>`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub entities: BTreeMap<String, BTreeSet<String>>,
    /// Document stages its code runs in (and may veto or change): entity → stages.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: BTreeMap<String, BTreeSet<String>>,
    /// Hosts it calls over HTTPS.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub hosts: BTreeSet<String>,
    /// HTTP endpoints anyone can call without signing in.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub public_routes: BTreeSet<String>,
    /// WebSocket endpoints (`/path`, or `/path (public)` when no sign-in is needed).
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub sockets: BTreeSet<String>,
    /// Periodic jobs (`name every N s`).
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub schedules: BTreeSet<String>,
    /// Events it receives.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub subscriptions: BTreeSet<String>,
    /// Entities it adds: its packages' (tables) and its plugin's (custom records).
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub new_entities: BTreeSet<String>,
    /// Changes to entities it does not define: entity → changes (fields, rules, properties,
    /// actions, workflows, seed records, print formats).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub overlays: BTreeMap<String, BTreeSet<String>>,
    /// Roles its packages define or extend with grants: role → entity → access, as for `entities`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
    /// SQL scripts its packages run on the cluster (`package/file.sql`).
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub sql: BTreeSet<String>,
}

fn access(p: &PermissionDef) -> BTreeSet<String> {
    let own = if p.if_owner { " (own records)" } else { "" };
    let verbs = [
        ("read", p.read),
        ("write", p.write),
        ("create", p.create),
        ("delete", p.delete),
        ("submit", p.submit),
        ("cancel", p.cancel),
        ("export", p.export),
        ("import", p.import),
    ];
    let mut out: BTreeSet<String> = verbs.iter().filter(|(_, on)| *on).map(|(v, _)| format!("{v}{own}")).collect();
    out.extend(p.read_groups.iter().map(|g| format!("read field group {g}")));
    out.extend(p.write_groups.iter().map(|g| format!("write field group {g}")));
    out
}

fn describe(op: &OverlayOp) -> String {
    match op {
        OverlayOp::AddField { field, .. } => format!("adds field {}", field.name),
        OverlayOp::SetProperty { field, property, value } => format!("sets {field}.{property} = {value}"),
        OverlayOp::AddRule { rule } => format!("adds rule {}", rule.name),
        OverlayOp::AddOptions { field, options } => format!("adds options to {field}: {}", options.join(", ")),
        OverlayOp::SetEntityProperty { property, value } => format!("sets {property} = {value}"),
        OverlayOp::DefineEntity { entity } => format!("defines entity {}", entity.name),
        OverlayOp::SetWorkflow { workflow } => format!("sets workflow {}", workflow.name),
        OverlayOp::SetPrintTemplate { .. } => "replaces the print template".into(),
        OverlayOp::AddAction { action } => format!("adds action {}", action.name),
        OverlayOp::AddSection { section, .. } => format!("adds form section {}", section.label.as_deref().unwrap_or("(untitled)")),
    }
}

/// Lines for items of `now` missing from `before`.
fn added_items(out: &mut Vec<String>, label: &str, now: &BTreeSet<String>, before: &BTreeSet<String>) {
    out.extend(now.difference(before).map(|x| format!("{label}: {x}")));
}

/// Lines for (key, item) pairs of `now` missing from `before`.
fn added_pairs(
    out: &mut Vec<String>,
    now: &BTreeMap<String, BTreeSet<String>>,
    before: &BTreeMap<String, BTreeSet<String>>,
    line: impl Fn(&str, &str) -> String,
) {
    let empty = BTreeSet::new();
    for (key, items) in now {
        let was = before.get(key).unwrap_or(&empty);
        out.extend(items.difference(was).map(|x| line(key, x)));
    }
}

fn records(entity: &str) -> String {
    if entity == "*" { "every entity".into() } else { entity.to_string() }
}

impl Permissions {
    /// Stable lower-case hex SHA-256 of the canonical form, recorded with an administrator's
    /// consent. Empty categories are left out of the canonical form, so adding a category to
    /// this type later does not change the digest of apps that do not use it.
    pub fn digest(&self) -> String {
        let mut h = Sha256::new();
        h.update(b"ferrum-app-permissions-v1\n");
        h.update(serde_json::to_vec(self).unwrap_or_default());
        hex::encode(h.finalize())
    }

    /// What this set asks for beyond `previous`, one readable line per new item (empty when an
    /// upgrade needs no new consent). Conservative: any item that was not there before counts,
    /// even one a wildcard already covered.
    pub fn increases(&self, previous: &Permissions) -> Vec<String> {
        let mut out = Vec::new();
        if self.native && !previous.native {
            out.push("runs native code inside the server, with full access".to_string());
        }
        added_pairs(&mut out, &self.entities, &previous.entities, |e, a| format!("records: {a} {}", records(e)));
        added_pairs(&mut out, &self.hooks, &previous.hooks, |e, s| format!("runs during: {} {s}", records(e)));
        added_items(&mut out, "calls host", &self.hosts, &previous.hosts);
        added_items(&mut out, "public endpoint", &self.public_routes, &previous.public_routes);
        added_items(&mut out, "socket", &self.sockets, &previous.sockets);
        added_items(&mut out, "schedule", &self.schedules, &previous.schedules);
        added_items(&mut out, "receives events", &self.subscriptions, &previous.subscriptions);
        added_items(&mut out, "new entity", &self.new_entities, &previous.new_entities);
        added_pairs(&mut out, &self.overlays, &previous.overlays, |e, c| format!("changes {e}: {c}"));
        let none = BTreeMap::new();
        for (role, grants) in &self.roles {
            let was = previous.roles.get(role);
            if was.is_none() {
                out.push(format!("role: {role}"));
            }
            added_pairs(&mut out, grants, was.unwrap_or(&none), |e, a| format!("role {role}: {a} {}", records(e)));
        }
        added_items(&mut out, "runs SQL", &self.sql, &previous.sql);
        out
    }
}

/// Records `what` against `entity` unless the app defines that entity itself.
fn note(overlays: &mut BTreeMap<String, BTreeSet<String>>, own: &BTreeSet<String>, entity: &str, what: String) {
    if !own.contains(entity) {
        overlays.entry(entity.to_string()).or_default().insert(what);
    }
}

impl Bundle {
    /// Everything this app asks for: its plugin's permissions, hooks, hosts, public routes,
    /// sockets, schedules and subscriptions; its packages' new entities, changes to other
    /// entities, roles and SQL.
    pub fn permissions(&self) -> Result<Permissions, AppError> {
        let mut out = Permissions { native: self.manifest.runtime == Runtime::Native, ..Permissions::default() };
        let packages = self.packages()?;
        let own: BTreeSet<String> = packages.iter().flat_map(|p| p.entities.iter().map(|e| e.name.clone())).collect();
        out.new_entities.extend(own.iter().cloned());
        for pkg in &packages {
            for ov in &pkg.manifest.overlays {
                for op in &ov.ops {
                    note(&mut out.overlays, &own, &ov.entity, describe(op));
                }
            }
            for wf in &pkg.manifest.workflows {
                note(&mut out.overlays, &own, &wf.entity, format!("workflow {}", wf.name));
            }
            for seed in &pkg.manifest.seed {
                note(&mut out.overlays, &own, &seed.entity, format!("adds records ({} from package {})", seed.records.len(), pkg.name()));
            }
            for (entity, _) in &pkg.print_formats {
                note(&mut out.overlays, &own, entity, "print format".into());
            }
            for role in &pkg.manifest.roles {
                let grants = out.roles.entry(role.name.clone()).or_default();
                for p in &role.permissions {
                    let a = access(p);
                    if !a.is_empty() {
                        grants.entry(p.entity.clone()).or_default().extend(a);
                    }
                }
            }
            out.sql.extend(pkg.sql.iter().map(|(file, _)| format!("{}/{file}", pkg.name())));
        }
        for p in &self.manifest.access {
            let a = access(&p.permission());
            if !a.is_empty() {
                out.entities.entry(p.entity.clone()).or_default().extend(a);
            }
        }
        if self.manifest.runtime == Runtime::Wasm {
            let plugin = PluginManifest::parse(self.file(PLUGIN_JSON).unwrap_or_default())?;
            for p in &plugin.permissions {
                let a = access(p);
                if !a.is_empty() {
                    out.entities.entry(p.entity.clone()).or_default().extend(a);
                }
            }
            for h in &plugin.hooks {
                out.hooks.entry(h.entity.clone()).or_default().insert(h.stage.clone());
            }
            out.hosts.extend(plugin.http_hosts.iter().cloned());
            out.public_routes.extend(plugin.routes.iter().filter(|r| r.public).map(|r| r.path.clone()));
            out.sockets.extend(plugin.sockets.iter().map(|s| if s.public { format!("{} (public)", s.path) } else { s.path.clone() }));
            out.schedules.extend(plugin.schedules.iter().map(|s| format!("{} every {} s", s.name, s.every_seconds)));
            out.subscriptions.extend(plugin.subscriptions.iter().cloned());
            out.new_entities.extend(plugin.entities.iter().map(|e| e.name.clone()));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn with_plugin(edit: impl Fn(&mut Value)) -> Result<Bundle, AppError> {
        let mut files = wasm_app();
        let mut json: Value = serde_json::from_str(GW_PLUGIN).unwrap();
        edit(&mut json);
        files.insert("plugin.json".into(), serde_json::to_vec(&json).unwrap());
        Bundle::from_files(files)
    }

    fn plugin_refused(edit: impl Fn(&mut Value), needle: &str) {
        let err = with_plugin(edit).expect_err(needle).to_string();
        assert!(err.contains(needle), "expected '{needle}' in: {err}");
    }

    #[test]
    fn summary_of_a_wasm_app() {
        let p = Bundle::from_files(wasm_app()).unwrap().permissions().unwrap();
        assert_eq!(p.entities["GwDevice"], set(&["read", "write"]));
        assert_eq!(p.entities["Customer"], set(&["read"]));
        assert_eq!(p.hooks["GwDevice"], set(&["validate"]));
        assert_eq!(p.hosts, set(&["api.example.com"]));
        assert_eq!(p.public_routes, set(&["/status"]), "signed-in routes are not public endpoints");
        assert_eq!(p.sockets, set(&["/devices/* (public)"]));
        assert_eq!(p.schedules, set(&["sweep every 300 s"]));
        assert_eq!(p.subscriptions, set(&["SalesInvoice.*"]));
        assert_eq!(p.new_entities, set(&["GwDevice", "XGwReading"]));
        assert_eq!(p.sql, set(&["gw/001_init.sql"]));
        assert!(p.overlays.is_empty() && p.roles.is_empty());
    }

    #[test]
    fn summary_of_a_meta_app() {
        let p = Bundle::from_files(meta_app()).unwrap().permissions().unwrap();
        assert!(p.entities.is_empty() && p.hosts.is_empty() && p.hooks.is_empty(), "meta apps run no code");
        assert_eq!(p.new_entities, set(&["KtThing"]));
        assert_eq!(p.overlays["Customer"], set(&["adds field kt_level"]));
        assert_eq!(p.roles["Kit User"]["KtThing"], set(&["create", "read", "write"]));
        let all = p.increases(&Permissions::default());
        assert_eq!(
            all,
            vec![
                "new entity: KtThing",
                "changes Customer: adds field kt_level",
                "role: Kit User",
                "role Kit User: create KtThing",
                "role Kit User: read KtThing",
                "role Kit User: write KtThing"
            ]
        );
    }

    #[test]
    fn what_an_app_without_a_plugin_declares() {
        let mut f = meta_app();
        let yaml = String::from_utf8(f["app.yaml"].clone()).unwrap()
            + "access:\n  - {entity: Customer, read: true}\n  - {entity: Contact, read: true, write: true, if_owner: true}\n";
        f.insert("app.yaml".into(), yaml.into_bytes());
        let p = Bundle::from_files(f).unwrap().permissions().unwrap();
        assert_eq!(p.entities["Customer"], set(&["read"]));
        assert_eq!(p.entities["Contact"], set(&["read (own records)", "write (own records)"]));
        let mut bad = meta_app();
        let yaml = String::from_utf8(bad["app.yaml"].clone()).unwrap() + "access:\n  - {entity: \"no such\", read: true}\n";
        bad.insert("app.yaml".into(), yaml.into_bytes());
        assert!(Bundle::from_files(bad).is_err(), "an entity name, or *");
    }

    #[test]
    fn switching_to_native_code_is_an_increase() {
        let mut files = meta_app();
        let yaml = String::from_utf8(files["app.yaml"].clone()).unwrap().replace("runtime: meta", "runtime: native");
        files.insert("app.yaml".into(), yaml.into_bytes());
        let native = Bundle::from_files(files).unwrap().permissions().unwrap();
        let meta = Bundle::from_files(meta_app()).unwrap().permissions().unwrap();
        assert!(native.native && !meta.native);
        assert_eq!(native.increases(&meta), vec!["runs native code inside the server, with full access"]);
        assert!(meta.increases(&native).is_empty());
        assert!(!serde_json::to_string(&meta).unwrap().contains("native"), "absent unless true");
    }

    #[test]
    fn digest_is_stable_and_sensitive() {
        let a = Bundle::from_files(wasm_app()).unwrap().permissions().unwrap();
        let b = Bundle::from_files(wasm_app()).unwrap().permissions().unwrap();
        assert_eq!(a.digest(), b.digest());
        assert_eq!(a.digest().len(), 64);
        // A JSON round trip (as stored by the server) keeps the digest.
        let back: Permissions = serde_json::from_value(serde_json::to_value(&a).unwrap()).unwrap();
        assert_eq!(back.digest(), a.digest());
        let mut c = a.clone();
        c.hosts.insert("evil.example.com".into());
        assert_ne!(c.digest(), a.digest());
        assert_ne!(Permissions::default().digest(), a.digest());
        // Empty categories are not part of the canonical form.
        assert_eq!(serde_json::to_string(&Permissions::default()).unwrap(), "{}");
    }

    #[test]
    fn increases_lists_only_what_is_new() {
        let v1 = Bundle::from_files(wasm_app()).unwrap().permissions().unwrap();
        assert!(v1.increases(&v1).is_empty());
        let v2 = with_plugin(|j| {
            j["http_hosts"] = serde_json::json!(["api.example.com", "*.payments.example"]);
            j["permissions"][1]["write"] = true.into();
            j["permissions"][1]["if_owner"] = false.into();
            j["routes"][1]["public"] = true.into();
            j["permissions"].as_array_mut().unwrap().push(serde_json::json!({"entity": "*", "read": true}));
        })
        .unwrap()
        .permissions()
        .unwrap();
        assert_eq!(
            v2.increases(&v1),
            vec!["records: read every entity", "records: write Customer", "calls host: *.payments.example", "public endpoint: /admin"]
        );
        // Fewer permissions are no increase.
        assert!(v1.increases(&v2).is_empty());
        // Limiting access to own records and then lifting the limit is an increase.
        let own = with_plugin(|j| j["permissions"][1]["if_owner"] = true.into()).unwrap().permissions().unwrap();
        assert_eq!(v1.increases(&own), vec!["records: read Customer"]);
    }

    #[test]
    fn plugin_manifest_rules() {
        plugin_refused(|j| j["name"] = "other".into(), "must be the app id 'gw'");
        plugin_refused(|j| j["wasm_base64"] = "AAAA".into(), "unknown field `wasm_base64`");
        plugin_refused(|j| j["routes"][0]["method"] = "GET".into(), "unknown field `method`");
        plugin_refused(|j| j["hooks"][0]["stage"] = "after_save".into(), "stage must be one of");
        plugin_refused(|j| j["hooks"][0]["entity"] = "not an entity".into(), "hook entity");
        plugin_refused(|j| j["routes"][0]["path"] = "status".into(), "must start with /");
        plugin_refused(|j| j["sockets"][0]["path"] = "/a/../b".into(), "must start with /");
        plugin_refused(|j| j["schedules"][0]["every_seconds"] = 59.into(), "at least 60");
        plugin_refused(|j| j["schedules"] = serde_json::json!([{"name": "a", "every_seconds": 60}, {"name": "a", "every_seconds": 90}]), "unique name");
        plugin_refused(|j| j["subscriptions"][0] = "Sales Invoice".into(), "subscription");
        plugin_refused(|j| j["permissions"][0]["entity"] = "lower".into(), "permission entity");
        plugin_refused(|j| j["http_hosts"][0] = "*.com".into(), "http_hosts");
        plugin_refused(|j| j["http_hosts"][0] = "https://api.example.com".into(), "http_hosts");
        plugin_refused(|j| j["entities"][0]["name"] = "XReading".into(), "must be named XGw");
        plugin_refused(|j| j["entities"][0]["name"] = "GwReading".into(), "must be named XGw");
        plugin_refused(|j| j["fuel_limit"] = (MAX_PLUGIN_FUEL + 1).into(), "fuel_limit");
        plugin_refused(|j| j["memory_limit_mb"] = 0.into(), "memory_limit_mb");
        plugin_refused(
            |j| {
                for k in ["hooks", "routes", "sockets", "schedules", "subscriptions"] {
                    j[k] = serde_json::json!([]);
                }
            },
            "at least one hook",
        );
        plugin_refused(|j| *j = serde_json::json!(["not", "an", "object"]), "plugin.json");
        assert!(with_plugin(|j| j["fuel_limit"] = MAX_PLUGIN_FUEL.into()).is_ok());
    }
}
