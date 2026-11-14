//! Packs (docs/api.md, "Packs"; docs/builder/decisions.md BD-07): one JSON document carrying what an
//! administrator configured in a workspace, installed in another.
//!
//! * **Export** reads the workspace's active customizations, the roles it made itself and the rules it
//!   added to packaged ones, its automations, Studio's items, the running release of each solution and the
//!   reference records the exporter names. Records are written by key, never by id, and links between them
//!   by the key of the record they reach. No user, secret, file or other record travels.
//! * **Plan** does the whole install in a transaction it rolls back, and says what each item would do.
//! * **Install** takes the plan's digest, refuses another, and applies everything through the functions the
//!   admin API uses, in one transaction: customizations, roles, automations, Studio's items, records. Each
//!   solution then arrives as a draft, after the commit, and is reported one by one.
//!
//! Installing a pack again changes nothing; nothing is ever deleted. Uninstalling, signing and packs inside
//! app bundles are not here yet.

use crate::Doc;
use crate::Kernel;
use crate::ctx::Ctx;
use crate::customize::NewCustomization;
use crate::db::Tx;
use crate::error::{Error, Result};
use crate::query::Filter;
use chrono::{DateTime, Utc};
use ferrum_meta::{EffEntity, FieldDef, FieldType, OverlayOp, PermissionDef, RoleDef};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use uuid::Uuid;

/// What a pack says it is.
pub const FORMAT: &str = "ferrum.pack/1";
/// The largest pack read or written.
pub const MAX_PACK: usize = 5 * 1024 * 1024;
/// The most records of one entity a pack carries.
pub const MAX_ROWS: usize = 10_000;
/// First key of the advisory lock an install holds (the two-key form, apart from the kernel's 64-bit locks);
/// the second is made from the workspace.
const INSTALL_LOCK: i32 = 0x7061_636b;
/// Studio's items, in the order an install writes them: a report names its space, a chart its report, a
/// dashboard its charts and cards; a space's blocks, which name any of them, are written last.
// ponytail: Studio's entities by name, since the kernel does not depend on Studio; a new kind of Studio item
// is added here.
const STUDIO: [&str; 5] = ["Space", "Report", "DashboardChart", "NumberCard", "Dashboard"];
/// Where a dashboard's layout or a space's blocks name another Studio item by id, and its entity.
const STUDIO_REFS: [(&str, &str); 4] = [("chart", "DashboardChart"), ("card", "NumberCard"), ("report", "Report"), ("dashboard", "Dashboard")];
/// Roles the platform makes for accounts that are not people (support, solutions, plugins, services): they
/// belong to what made them, never to a pack.
const MACHINE_ROLES: [&str; 4] = ["support:", "solution:", "plugin:", "service:"];

fn one() -> i64 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pack {
    pub format: String,
    pub name: String,
    pub label: String,
    #[serde(default = "one")]
    pub version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exported_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub requires: Requires,
    /// The business calendars its service targets name, installed where the workspace has none of the name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calendars: Vec<PackCalendar>,
    #[serde(default)]
    pub customizations: Vec<Customization>,
    #[serde(default)]
    pub roles: Vec<RoleDef>,
    #[serde(default)]
    pub automations: Vec<Automation>,
    #[serde(default)]
    pub studio: Vec<StudioItem>,
    /// `ferrum.solution-pack/1` of each solution's running release.
    #[serde(default)]
    pub solutions: Vec<Value>,
    #[serde(default)]
    pub records: Vec<Records>,
    /// The workspace's own message templates and its edited copies of packages' defaults (BD-20).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<ferrum_meta::MessageDef>,
    /// Its text and chat-app channel connections, described, never with their secrets; installed turned off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<crate::messages::PackChannel>,
    /// Its connections to outside services (BD-33), described, never with a secret or a token; installed turned off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connections: Vec<crate::connections::PackConnection>,
    /// The workspace's own document templates and its edited copies of packages' defaults (BD-21).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub documents: Vec<ferrum_meta::DocumentDef>,
    /// The workspace's default print format per entity (BD-28): entity → template key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub document_defaults: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requires {
    /// Package → the version the exporter had, the least the installer needs.
    #[serde(default)]
    pub packages: BTreeMap<String, String>,
}

/// A business calendar as a pack carries it: what `PUT /api/admin/calendars/{name}` takes, with its name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackCalendar {
    pub name: String,
    pub zone: String,
    pub weekends: Vec<String>,
    #[serde(default)]
    pub holidays: BTreeSet<chrono::NaiveDate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours: Option<BTreeMap<String, Vec<(String, String)>>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exceptions: BTreeMap<chrono::NaiveDate, ferrum_logic::calendar::Exception>,
}

impl PackCalendar {
    fn calendar(&self) -> ferrum_logic::calendar::Calendar {
        let PackCalendar { name, zone, weekends, holidays, hours, exceptions } = self.clone();
        ferrum_logic::calendar::Calendar { name, zone, weekends, holidays, revision: 0, hours, exceptions }
    }
}

/// The calendars the service targets of a pack's customizations name.
fn target_calendars(pack: &Pack) -> BTreeSet<String> {
    let named = |op: &OverlayOp| match op {
        OverlayOp::AddTarget { target } => target.calendar.iter().cloned().collect::<Vec<_>>(),
        OverlayOp::DefineEntity { entity } | OverlayOp::RedefineEntity { entity } => entity.targets.iter().filter_map(|t| t.calendar.clone()).collect(),
        _ => vec![],
    };
    pack.customizations.iter().flat_map(|c| named(&c.op)).collect()
}

/// What a pack's published lists show to anyone, said plainly for whoever installs it: "This pack
/// publishes a list of Permits to anyone: Holder, Type, Issued on."
fn publishes(ctx: &Ctx, pack: &Pack) -> Vec<String> {
    let def_of = |entity: &str| {
        let defined = pack.customizations.iter().rev().find_map(|c| match &c.op {
            OverlayOp::DefineEntity { entity: e } | OverlayOp::RedefineEntity { entity: e } if e.name == entity => Some((**e).clone()),
            _ => None,
        });
        defined.or_else(|| ctx.schema.entity(entity).map(|e| e.def.clone()))
    };
    let mut out = Vec::new();
    for c in &pack.customizations {
        let OverlayOp::PublishList { list } = &c.op else { continue };
        let def = def_of(&c.entity);
        let label = |f: &str| match def.as_ref().and_then(|d| d.fields.iter().find(|x| x.name == f)) {
            Some(x) => x.label(),
            None if f == "number" => "Number".into(),
            None => ferrum_meta::humanize(f),
        };
        let what = def.as_ref().map_or_else(|| c.entity.clone(), |d| d.plural_label());
        let fields: Vec<String> = list.fields.iter().map(|f| label(f)).collect();
        out.push(format!("This pack publishes a list of {what} to anyone, without signing in ({}): {}.", list.name, fields.join(", ")));
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Customization {
    pub entity: String,
    pub op: OverlayOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
pub struct Automation {
    pub name: String,
    pub entity: String,
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    pub actions: Value,
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StudioItem {
    pub entity: String,
    /// Its label: Studio's items are matched by entity and label.
    pub name: String,
    pub record: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Records {
    pub entity: String,
    /// The fields a row is matched by.
    pub key: Vec<String>,
    pub rows: Vec<Map<String, Value>>,
}

/// What an install does to one section: the names it creates, changes and leaves alone.
#[derive(Debug, Default, Serialize)]
struct Section {
    created: Vec<String>,
    changed: Vec<String>,
    unchanged: Vec<String>,
}

impl Section {
    fn add(&mut self, name: String, status: &str) {
        match status {
            "created" => self.created.push(name),
            "changed" => self.changed.push(name),
            _ => self.unchanged.push(name),
        }
    }
}

type Summary = BTreeMap<&'static str, Section>;

/// What an exporter asks for (`GET /api/admin/pack`).
#[derive(Debug, Clone, Default)]
pub struct ExportOptions {
    pub name: String,
    pub label: String,
    pub version: i64,
    /// Entities whose records travel as reference data, in the order they are installed.
    pub records: Vec<String>,
}

fn require_admin(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("only administrators export and install packs".into())) }
}

fn check_header(name: &str, label: &str, version: i64) -> Result<()> {
    let slug = |s: &str| (1..=64).contains(&s.len()) && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !slug(name) {
        return Err(Error::field("name", "a pack's name is 1 to 64 lower-case letters, digits and dashes"));
    }
    if label.trim().is_empty() || label.chars().count() > 200 {
        return Err(Error::field("label", "a pack's label is 1 to 200 characters"));
    }
    if version < 1 {
        return Err(Error::field("version", "a pack's version is a whole number from 1"));
    }
    Ok(())
}

/// Reads a pack strictly: at most [`MAX_PACK`], no key it does not know.
pub fn parse(raw: &Value) -> Result<Pack> {
    let size = serde_json::to_vec(raw).map_err(Error::internal)?.len();
    if size > MAX_PACK {
        return Err(Error::field("pack", format!("is {size} bytes: a pack is at most {MAX_PACK} bytes (5 MiB)")));
    }
    let pack: Pack = serde_json::from_value(raw.clone()).map_err(|e| Error::field("pack", format!("is not a {FORMAT} pack: {e}")))?;
    if pack.format != FORMAT {
        return Err(Error::field("pack", format!("is a {} document, not a {FORMAT} pack", pack.format)));
    }
    check_header(&pack.name, &pack.label, pack.version)?;
    Ok(pack)
}

/// The digest of a pack in the platform's canonical form, without `exported_at`: two exports of an
/// unchanged workspace have the same digest.
pub fn digest(pack: &Pack) -> Result<String> {
    let mut body = serde_json::to_value(pack).map_err(Error::internal)?;
    if let Some(m) = body.as_object_mut() {
        m.remove("exported_at");
    }
    ferrum_logic::canonical::digest(FORMAT, &ferrum_logic::capability::digestible(&body)).map_err(|e| Error::field("pack", e.to_string()))
}

/// Whether a field's value travels: what a person enters, not what the platform derives; no secret, no file.
fn carried(f: &FieldDef) -> bool {
    !f.read_only && f.computed.is_none() && f.rollup.is_none() && f.fetch_from.is_none() && !matches!(f.ftype, FieldType::Secret | FieldType::Attach)
}

/// The fields a record is known by in a pack: its first unique key, else its title field; none for the one record of
/// a workspace's settings (BD-17).
fn key_of(ent: &EffEntity) -> Option<Vec<String>> {
    if one_record(ent) {
        return Some(vec![]);
    }
    ent.unique_keys().into_iter().next().or_else(|| ent.def.title_field.clone().map(|t| vec![t]))
}

/// Settings with one record for the workspace: a pack carries that record, keyed by nothing.
fn one_record(ent: &EffEntity) -> bool {
    ent.def.kind == ferrum_meta::EntityKind::Settings && !ent.def.company_scoped
}

/// Why records of `ent` (and its lines) may not travel as reference data, if they may not.
fn not_reference(ctx: &Ctx, ent: &EffEntity) -> Option<&'static str> {
    // Its lines' entities, and their own lines' (BD-06).
    let mut children: Vec<&EffEntity> = Vec::new();
    let mut next: Vec<&EffEntity> = vec![ent];
    while let Some(e) = next.pop() {
        for c in e.children.values().filter_map(|c| ctx.schema.entity(&c.entity).map(|e| &**e)) {
            children.push(c);
            next.push(c);
        }
    }
    let fields = || ent.def.fields.iter().chain(children.iter().flat_map(|c| c.def.fields.iter()));
    if ctx.schema.personal_data.iter().any(|d| d.entity == ent.def.name) {
        Some("a package declares that it holds people's data")
    } else if fields().any(|f| matches!(f.ftype, FieldType::Email | FieldType::Phone)) {
        Some("it keeps an email or a phone number")
    } else if fields().any(|f| f.ftype == FieldType::Secret) {
        Some("it keeps a secret")
    } else if ent.is_submittable() {
        Some("its records are submitted documents, not reference data")
    } else if ent.def.kind == ferrum_meta::EntityKind::Child || ent.table.is_empty() {
        Some("its rows are the lines of another record")
    } else {
        None
    }
}

fn reference_entity(ctx: &Ctx, name: &str) -> Result<Arc<EffEntity>> {
    let ent = ctx.schema.entity(name).cloned().ok_or_else(|| Error::field("records", format!("{name} is not an entity here")))?;
    if let Some(why) = not_reference(ctx, &ent) {
        return Err(Error::field("records", format!("records of {name} cannot travel in a pack: {why}")));
    }
    Ok(ent)
}

/// An error about one item of a pack, saying which.
pub(crate) fn about(what: &str, e: Error) -> Error {
    match e {
        Error::Validation(fs) => Error::Validation(
            fs.into_iter()
                .map(|mut f| {
                    f.message = format!("{what}: {}", f.message);
                    f
                })
                .collect(),
        ),
        Error::Invalid(m) => Error::Invalid(format!("{what}: {m}")),
        Error::Conflict(m) => Error::Conflict(format!("{what}: {m}")),
        Error::Forbidden(m) => Error::Forbidden(format!("{what}: {m}")),
        Error::NotFound(m) => Error::Invalid(format!("{what}: {m} not found")),
        other => other,
    }
}

/// A key's values as a pack writes them: one value alone, several as a list.
fn key_value(values: Vec<Value>) -> Value {
    if values.len() == 1 { values.into_iter().next().unwrap_or_default() } else { Value::Array(values) }
}

fn key_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(key_text).collect::<Vec<_>>().join(" / "),
        other => other.to_string(),
    }
}

/// Each reference a dashboard's layout or a space's blocks hold to another Studio item, rewritten by `to`
/// (an id to a label on export, a label to an id on install).
fn rewrite_refs(items: &mut Value, mut to: impl FnMut(&str, &str) -> Result<String>) -> Result<()> {
    let Value::Array(items) = items else { return Ok(()) };
    let mut one = |m: &mut Map<String, Value>| -> Result<()> {
        for (key, entity) in STUDIO_REFS {
            if let Some(Value::String(s)) = m.get_mut(key) {
                *s = to(entity, s)?;
            }
        }
        Ok(())
    };
    for item in items.iter_mut().filter_map(Value::as_object_mut) {
        one(item)?;
        if let Some(Value::Array(links)) = item.get_mut("items") {
            for link in links.iter_mut().filter_map(Value::as_object_mut) {
                one(link)?;
            }
        }
    }
    Ok(())
}

/// The field of a Studio item that names others.
fn refs_field(entity: &str) -> Option<&'static str> {
    match entity {
        "Dashboard" => Some("layout"),
        "Space" => Some("blocks"),
        _ => None,
    }
}

/// Studio's items that others name: (entity, id, label, standard).
type StudioIndex = Vec<(String, String, String, bool)>;

fn label_of_id(index: &StudioIndex, entity: &str, id: &str, owner: &str) -> Result<String> {
    index
        .iter()
        .find(|(e, i, _, _)| e == entity && i == id)
        .map(|(_, _, l, _)| l.clone())
        .ok_or_else(|| Error::invalid(format!("{owner} shows a {entity} that is gone: remove it there before exporting")))
}

fn id_of_label(index: &StudioIndex, entity: &str, label: &str, owner: &str) -> Result<String> {
    let all: Vec<&(String, String, String, bool)> = index.iter().filter(|(e, _, l, _)| e == entity && l == label).collect();
    let own: Vec<&(String, String, String, bool)> = all.iter().copied().filter(|x| !x.3).collect();
    match (all.as_slice(), own.as_slice()) {
        ([one], _) | (_, [one]) => Ok(one.1.clone()),
        ([], _) => Err(Error::invalid(format!("{owner} shows the {entity} '{label}', which this workspace does not have"))),
        _ => Err(Error::invalid(format!("{owner} shows the {entity} '{label}', and this workspace has more than one of that name"))),
    }
}

#[derive(sqlx::FromRow)]
struct Rule {
    role: String,
    entity: String,
    can_read: bool,
    can_write: bool,
    can_create: bool,
    can_delete: bool,
    can_submit: bool,
    can_cancel: bool,
    can_export: bool,
    can_import: bool,
    if_owner: bool,
    read_groups: Vec<String>,
    write_groups: Vec<String>,
    match_rule: Option<Value>,
    via: Option<String>,
    actions: Vec<String>,
    shared: Option<String>,
    hide: Vec<String>,
}

impl Rule {
    fn def(self) -> PermissionDef {
        PermissionDef {
            entity: self.entity,
            read: self.can_read,
            write: self.can_write,
            create: self.can_create,
            delete: self.can_delete,
            submit: self.can_submit,
            cancel: self.can_cancel,
            export: self.can_export,
            import: self.can_import,
            if_owner: self.if_owner,
            read_groups: self.read_groups,
            write_groups: self.write_groups,
            condition: self.match_rule.and_then(|m| serde_json::from_value(m).ok()).unwrap_or_default(),
            via: self.via,
            actions: self.actions,
            shared: self.shared,
            hide: self.hide,
        }
    }
}

/// A rule as it is kept: a path makes it an own-record rule.
fn as_kept(p: &PermissionDef) -> PermissionDef {
    let via = p.via.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    PermissionDef { if_owner: p.if_owner || via.is_some(), via, ..p.clone() }
}

async fn rules_of(tx: &mut Tx, tenant: Uuid, role: Option<&str>) -> Result<Vec<Rule>> {
    Ok(sqlx::query_as(
        "select role, entity, can_read, can_write, can_create, can_delete, can_submit, can_cancel, can_export, can_import, if_owner, \
         read_groups, write_groups, match_rule, via, actions, shared, hide from role_permission where tenant_id = $1 and ($2::text is null or role = $2) \
         order by role, entity, if_owner, rule_key",
    )
    .bind(tenant)
    .bind(role)
    .fetch_all(&mut **tx)
    .await?)
}

/// The workspace's own customizations that installing `pack` leaves replaced (`superseded_by`) when they were
/// not before, and that the pack does not carry itself, in plain words.
fn stops_applying(pack: &Pack, before: &[crate::customize::OverlayRow], after: &[crate::customize::OverlayRow]) -> Vec<String> {
    let carried = |r: &crate::customize::OverlayRow| pack.customizations.iter().any(|c| serde_json::to_value(&c.op).is_ok_and(|op| op == r.op));
    let mut out = Vec::new();
    for b in before.iter().filter(|b| b.is_active && b.superseded_by.is_none() && !carried(b)) {
        let Some(by) = after.iter().find(|a| a.id == b.id).and_then(|a| a.superseded_by).and_then(|id| after.iter().find(|x| x.id == id)) else { continue };
        let how = if by.op["op"] == "redefine_entity" { format!("the pack redefines {}", by.entity) } else { "the pack replaces it".into() };
        out.push(format!("{} stops applying: {how}", crate::customize::label_of(&b.entity, &b.op)));
    }
    out
}

/// The workspace's menus a pack marks for the portal: Studio's generic page shows them (docs/builder/portal.md).
fn portal_menus(pack: &Pack) -> Vec<&str> {
    pack.customizations
        .iter()
        .filter_map(|c| match &c.op {
            OverlayOp::DefineModule { module } if module.portal => Some(module.name.as_str()),
            _ => None,
        })
        .collect()
}

/// The packages whose entities a pack names (or whose roles it adds rules to), at the versions installed
/// here; Studio when it marks a menu for the portal.
fn requirements(ctx: &Ctx, pack: &Pack, role_packages: &BTreeSet<String>, installed: &[(String, String)]) -> BTreeMap<String, String> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    for c in &pack.customizations {
        names.insert(c.entity.clone());
        match &c.op {
            OverlayOp::DefineEntity { entity } | OverlayOp::RedefineEntity { entity } => names.extend(entity.fields.iter().filter_map(|f| f.to.clone())),
            OverlayOp::AddField { field, .. } => names.extend(field.to.clone()),
            // What a configured action's effects make.
            OverlayOp::AddAction { action } => names.extend(action.effects.iter().filter_map(|x| x.get("entity").and_then(Value::as_str).map(str::to_string))),
            // The entities a menu shows.
            OverlayOp::DefineModule { module } => names.extend(module.items.iter().filter(|i| !i.starts_with("report:")).cloned()),
            _ => {}
        }
    }
    names.extend(pack.roles.iter().flat_map(|r| r.permissions.iter().map(|p| p.entity.clone())));
    for a in &pack.automations {
        names.insert(a.entity.clone());
        names.extend(a.actions.as_array().into_iter().flatten().filter_map(|x| x.get("entity").and_then(Value::as_str).map(str::to_string)));
    }
    for s in &pack.studio {
        names.insert(s.entity.clone());
        names.extend(s.record.get("spec").and_then(|v| v.get("entity")).and_then(Value::as_str).map(str::to_string));
    }
    for r in &pack.records {
        names.insert(r.entity.clone());
        let links = ctx.schema.entity(&r.entity).map(|e| e.def.fields.iter().filter_map(|f| f.to.clone()).collect::<Vec<_>>()).unwrap_or_default();
        names.extend(links);
    }
    let studio = (!portal_menus(pack).is_empty()).then(|| "studio".to_string());
    let packages: BTreeSet<String> = names
        .iter()
        .filter_map(|n| ctx.schema.entity(n))
        .map(|e| e.package.clone())
        .filter(|p| p != "tenant")
        .chain(role_packages.iter().cloned())
        .chain(studio)
        .collect();
    installed.iter().filter(|(n, _)| packages.contains(n)).cloned().collect()
}

/// What this workspace lacks of what a pack requires: packages, and the calendars its targets name that it
/// neither carries nor finds here.
async fn lacking(tx: &mut Tx, tenant: Uuid, pack: &Pack) -> Result<Vec<String>> {
    let requires = &pack.requires;
    let carried: BTreeSet<&str> = pack.calendars.iter().map(|c| c.name.as_str()).collect();
    let wanted: Vec<String> = target_calendars(pack).into_iter().filter(|n| !carried.contains(n.as_str())).collect();
    let here: Vec<String> =
        sqlx::query_scalar("select name from logic_calendar where tenant_id = $1 and name = any($2)").bind(tenant).bind(&wanted).fetch_all(&mut **tx).await?;
    let calendars = wanted.into_iter().filter(|n| !here.contains(n)).map(|n| format!("the business calendar {n} is not here, and the pack does not carry it"));
    let have: HashMap<String, String> =
        sqlx::query_as("select name, version from installed_package where tenant_id = $1").bind(tenant).fetch_all(&mut **tx).await?.into_iter().collect();
    let older = crate::tenant::package_behind;
    // A portal menu is shown by Studio's generic page, whether the pack names Studio or not.
    let portal = portal_menus(pack);
    let studio = (!portal.is_empty() && !have.contains_key("studio") && !requires.packages.contains_key("studio")).then_some(("studio", ""));
    let why = |p: &str| {
        if p == "studio" && !portal.is_empty() { format!(": the portal shows the menu {} on Studio's page", portal.join(", ")) } else { String::new() }
    };
    Ok(requires
        .packages
        .iter()
        .map(|(p, want)| (p.as_str(), want.as_str()))
        .chain(studio)
        .filter_map(|(p, want)| match have.get(p) {
            None if want.is_empty() => Some(format!("the package {p} is not installed here{}", why(p))),
            None => Some(format!("the package {p} ({want} or later) is not installed here{}", why(p))),
            Some(v) if older(v, want) => Some(format!("the package {p} is at {v} here; the pack needs {want} or later")),
            _ => None,
        })
        .chain(calendars)
        .collect())
}

impl Kernel {
    // ───────────────────────────── export ─────────────────────────────

    /// `GET /api/admin/pack`: this workspace's configuration as a pack. Deterministic: two exports of an
    /// unchanged workspace differ only in `exported_at`.
    pub async fn export_pack(&self, ctx: &Ctx, o: &ExportOptions) -> Result<Value> {
        require_admin(ctx)?;
        check_header(&o.name, &o.label, o.version)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let overlays: Vec<(String, Value, Option<String>)> =
            sqlx::query_as("select entity, op, note from meta_overlay where tenant_id = $1 and is_active order by seq")
                .bind(ctx.tenant)
                .fetch_all(&mut *tx)
                .await?;
        let customizations: Vec<Customization> =
            overlays.into_iter().filter_map(|(entity, op, note)| Some(Customization { entity, op: serde_json::from_value(op).ok()?, note })).collect();
        let mut calendars = Vec::new();
        for name in ctx.schema.entities.values().flat_map(|e| e.def.targets.iter().filter_map(|t| t.calendar.clone())).collect::<BTreeSet<_>>() {
            if let Some(c) = crate::load_calendar(&mut tx, ctx.tenant, &name).await? {
                calendars.push(PackCalendar { name, zone: c.zone, weekends: c.weekends, holidays: c.holidays, hours: c.hours, exceptions: c.exceptions });
            }
        }
        let installed: Vec<(String, String)> =
            sqlx::query_as("select name, version from installed_package where tenant_id = $1 order by name").bind(ctx.tenant).fetch_all(&mut *tx).await?;
        let (roles, role_packages) = self.export_roles(ctx, &mut tx, &installed).await?;
        let automations = export_automations(ctx, &mut tx).await?;
        let studio = self.export_studio(ctx, &mut tx).await?;
        let (messages, channels) = crate::messages::for_pack(&mut tx, ctx.tenant).await?;
        let documents = crate::documents::for_pack(&mut tx, ctx.tenant).await?;
        let document_defaults = crate::documents::defaults_for_pack(&mut tx, ctx.tenant).await?;
        let connections = crate::connections::for_pack(&mut tx, ctx.tenant).await?;
        let mut records = Vec::new();
        let mut asked: Vec<&String> = Vec::new();
        for e in &o.records {
            if !asked.contains(&e) {
                asked.push(e);
                records.push(self.export_records(ctx, &mut tx, e).await?);
            }
        }
        let running: Vec<Uuid> =
            sqlx::query_scalar("select release_id from logic_activation where tenant_id = $1 order by solution").bind(ctx.tenant).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let mut solutions = Vec::new();
        for release in running {
            let mut s = self.solution_pack(ctx, release).await?;
            if let Some(m) = s.as_object_mut() {
                m.remove("exported_at");
            }
            solutions.push(s);
        }
        let mut pack = Pack {
            format: FORMAT.into(),
            name: o.name.clone(),
            label: o.label.trim().to_string(),
            version: o.version,
            exported_at: Some(Utc::now()),
            requires: Requires::default(),
            calendars,
            customizations,
            roles,
            automations,
            studio,
            solutions,
            records,
            messages,
            channels,
            documents,
            document_defaults,
            connections,
        };
        pack.requires.packages = requirements(ctx, &pack, &role_packages, &installed);
        let out = serde_json::to_value(&pack).map_err(Error::internal)?;
        let size = serde_json::to_vec(&out).map_err(Error::internal)?.len();
        if size > MAX_PACK {
            return Err(Error::invalid(format!("this pack would be {size} bytes, more than {MAX_PACK} (5 MiB): name fewer records")));
        }
        Ok(out)
    }

    /// The roles this workspace made, with their rules, and the rules it added to packaged roles (with their
    /// portal mark when it changed it); and the packages of those packaged roles. A role's `package` column
    /// says a package seeded it; its seeded rules are those every installed package's manifest gives a role of
    /// that name (several packages may seed one role).
    async fn export_roles(&self, ctx: &Ctx, tx: &mut Tx, installed: &[(String, String)]) -> Result<(Vec<RoleDef>, BTreeSet<String>)> {
        let roles: Vec<(String, Option<String>, Option<String>, bool)> =
            sqlx::query_as("select name, description, package, portal from role where tenant_id = $1 order by name")
                .bind(ctx.tenant)
                .fetch_all(&mut **tx)
                .await?;
        let mut rules: BTreeMap<String, Vec<PermissionDef>> = BTreeMap::new();
        for r in rules_of(tx, ctx.tenant, None).await? {
            rules.entry(r.role.clone()).or_default().push(r.def());
        }
        let catalog = self.registry.catalog();
        let (mut out, mut packages) = (Vec::new(), BTreeSet::new());
        for (name, description, package, portal) in roles {
            if MACHINE_ROLES.iter().any(|p| name.starts_with(p)) {
                continue;
            }
            let permissions = rules.remove(&name).unwrap_or_default();
            match package {
                None => out.push(RoleDef { name, description, permissions, portal }),
                Some(pkg) => {
                    let seeded = catalog.package(&pkg).and_then(|p| p.manifest.roles.iter().find(|r| r.name == name).cloned());
                    let kept: Vec<PermissionDef> = installed
                        .iter()
                        .filter_map(|(p, _)| catalog.package(p))
                        .flat_map(|p| p.manifest.roles.iter().filter(|r| r.name == name))
                        .flat_map(|r| r.permissions.iter().map(as_kept))
                        .collect();
                    let added: Vec<PermissionDef> = permissions.into_iter().filter(|p| !kept.contains(p)).collect();
                    if added.is_empty() && seeded.as_ref().map(|r| r.portal) == Some(portal) {
                        continue;
                    }
                    packages.insert(pkg);
                    out.push(RoleDef { name, description: None, permissions: added, portal });
                }
            }
        }
        Ok((out, packages))
    }

    /// Studio's items this workspace made (not a package's standard ones), by entity and label.
    async fn export_studio(&self, ctx: &Ctx, tx: &mut Tx) -> Result<Vec<StudioItem>> {
        let index = self.studio_index(ctx, tx).await?;
        let mut keys = HashMap::new();
        let mut out = Vec::new();
        for entity in STUDIO {
            let Some(ent) = ctx.schema.entity(entity).cloned() else { continue };
            let mut items: Vec<(String, Map<String, Value>)> = Vec::new();
            for doc in self.all_records(ctx, tx, entity).await? {
                if doc.get("is_standard") == Some(&json!(true)) {
                    continue;
                }
                let label = doc.get("label").and_then(Value::as_str).unwrap_or_default().to_string();
                let owner = format!("{} '{label}'", ent.def.label);
                let mut record = self.portable(ctx, tx, &ent, &doc, &mut keys).await?;
                if let Some(v) = refs_field(entity).and_then(|f| record.get_mut(f)) {
                    rewrite_refs(v, |e, id| label_of_id(&index, e, id, &owner))?;
                }
                items.push((label, record));
            }
            items.sort_by(|a, b| a.0.cmp(&b.0));
            if let Some(w) = items.windows(2).find(|w| w[0].0 == w[1].0) {
                return Err(Error::Conflict(format!(
                    "two {} are called '{}': a pack matches Studio's items by name, so rename one",
                    ent.def.plural_label(),
                    w[0].0
                )));
            }
            out.extend(items.into_iter().map(|(name, record)| StudioItem { entity: entity.into(), name, record }));
        }
        Ok(out)
    }

    /// The rows of a reference entity, keyed and sorted by key.
    async fn export_records(&self, ctx: &Ctx, tx: &mut Tx, entity: &str) -> Result<Records> {
        let ent = reference_entity(ctx, entity)?;
        let key = key_of(&ent).ok_or_else(|| {
            Error::field("records", format!("records of {entity} cannot travel in a pack: it has no unique key or title field to match them by"))
        })?;
        let mut keys = HashMap::new();
        let mut rows: Vec<(String, Map<String, Value>)> = Vec::new();
        for doc in self.all_records(ctx, tx, entity).await? {
            let row = self.portable(ctx, tx, &ent, &doc, &mut keys).await?;
            let k = key_value(key.iter().map(|f| row.get(f).cloned().unwrap_or_default()).collect());
            rows.push((ferrum_logic::canonical::canonical(&ferrum_logic::capability::digestible(&k)).unwrap_or_default(), row));
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some(w) = rows.windows(2).find(|w| w[0].0 == w[1].0) {
            return Err(Error::Conflict(format!("two {entity} records share the key {}: a pack matches records by it", w[0].0)));
        }
        Ok(Records { entity: entity.into(), key, rows: rows.into_iter().map(|(_, r)| r).collect() })
    }

    /// Every record of an entity, with its lines; refused beyond [`MAX_ROWS`].
    async fn all_records(&self, ctx: &Ctx, tx: &mut Tx, entity: &str) -> Result<Vec<Doc>> {
        let found = self.find_tx(ctx, tx, entity, &[], None, MAX_ROWS as u32 + 1).await?;
        if found.len() > MAX_ROWS {
            return Err(Error::invalid(format!("{entity} has more than {MAX_ROWS} records: a pack carries at most that many")));
        }
        let mut out = Vec::with_capacity(found.len());
        for d in found {
            out.push(self.get_tx(ctx, tx, entity, crate::store::doc_id(&d)?, false).await?);
        }
        Ok(out)
    }

    /// Studio's items that a layout or blocks can name.
    async fn studio_index(&self, ctx: &Ctx, tx: &mut Tx) -> Result<StudioIndex> {
        let mut out = Vec::new();
        for (_, entity) in STUDIO_REFS {
            if ctx.schema.entity(entity).is_none() {
                continue;
            }
            for d in self.find_tx(ctx, tx, entity, &[], None, 100_000).await? {
                let text = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                out.push((entity.to_string(), text("id"), text("label"), d.get("is_standard") == Some(&json!(true))));
            }
        }
        Ok(out)
    }

    /// A record as a pack writes it: each carried field, a link by the key of the record it reaches, and its
    /// tables' rows likewise.
    async fn portable(&self, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &Doc, keys: &mut HashMap<(String, String), Value>) -> Result<Map<String, Value>> {
        let mut out = self.portable_values(ctx, tx, ent, doc, keys).await?;
        for f in ent.def.fields.iter().filter(|f| f.ftype == FieldType::Table && carried(f)) {
            let child =
                f.of.as_deref()
                    .and_then(|c| ctx.schema.entity(c))
                    .cloned()
                    .ok_or_else(|| Error::internal(format!("{}.{} has no rows' entity", ent.def.name, f.name)))?;
            let mut rows = Vec::new();
            for r in doc.get(&f.name).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object) {
                // A line with lines of its own (BD-06) carries them the same way.
                rows.push(Value::Object(Box::pin(self.portable(ctx, tx, &child, r, keys)).await?));
            }
            out.insert(f.name.clone(), Value::Array(rows));
        }
        Ok(out)
    }

    async fn portable_values(
        &self,
        ctx: &Ctx,
        tx: &mut Tx,
        ent: &EffEntity,
        doc: &Doc,
        keys: &mut HashMap<(String, String), Value>,
    ) -> Result<Map<String, Value>> {
        let mut out = Map::new();
        for f in ent.def.fields.iter().filter(|f| f.ftype != FieldType::Table && carried(f)) {
            let v = doc.get(&f.name).cloned().unwrap_or_default();
            let v = match (&v, f.to.as_deref()) {
                (Value::String(id), Some(to)) if f.ftype == FieldType::Link => {
                    let at = (to.to_string(), id.clone());
                    match keys.get(&at) {
                        Some(k) => k.clone(),
                        None => {
                            let k = self.key_for(ctx, tx, ent, f, to, id).await?;
                            keys.insert(at, k.clone());
                            k
                        }
                    }
                }
                _ => v,
            };
            out.insert(f.name.clone(), v);
        }
        Ok(out)
    }

    /// The key of the record a link reaches, refused when it holds people's details or has no key.
    async fn key_for(&self, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, f: &FieldDef, to: &str, id: &str) -> Result<Value> {
        let target = ctx.schema.entity(to).cloned().ok_or_else(|| Error::internal(format!("{to} is not an entity")))?;
        let field = format!("{}.{}", ent.def.name, f.name);
        if ferrum_meta::holds_personal_data(&target.def, |n| ctx.schema.personal_data.iter().any(|d| d.entity == n)) {
            return Err(Error::invalid(format!("{field} links to {to}, which holds people's details: a pack carries none, so clear it before exporting")));
        }
        let key =
            key_of(&target).ok_or_else(|| Error::invalid(format!("{field} links to {to}, which has no unique key or title field to name its records by")))?;
        let id: Uuid = id.parse().map_err(|_| Error::internal(format!("{field} holds {id}, not an id")))?;
        let doc = self.get_tx(ctx, tx, to, id, false).await?;
        Ok(key_value(key.iter().map(|k| doc.get(k).cloned().unwrap_or_default()).collect()))
    }

    // ───────────────────────────── install ─────────────────────────────

    /// A record of a pack as this workspace saves it: links found by key.
    async fn resolve(&self, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, rec: &Map<String, Value>, what: &str) -> Result<Doc> {
        let mut doc = self.resolve_values(ctx, tx, ent, rec, what).await?;
        for (name, v) in rec.iter().filter(|(n, _)| ent.field(n).is_some_and(|f| f.ftype == FieldType::Table)) {
            let child = ent.field(name).and_then(|f| f.of.as_deref()).and_then(|c| ctx.schema.entity(c)).cloned();
            let child = child.ok_or_else(|| Error::internal(format!("{}.{name} has no rows' entity", ent.def.name)))?;
            let mut rows = Vec::new();
            for r in v.as_array().into_iter().flatten() {
                let r = r.as_object().ok_or_else(|| Error::invalid(format!("{what}: a row of {name} is not an object")))?;
                rows.push(Value::Object(Box::pin(self.resolve(ctx, tx, &child, r, what)).await?));
            }
            doc.insert(name.clone(), Value::Array(rows));
        }
        Ok(doc)
    }

    async fn resolve_values(&self, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, rec: &Map<String, Value>, what: &str) -> Result<Doc> {
        let mut doc = Doc::new();
        for (name, v) in rec {
            let f = ent
                .field(name)
                .filter(|f| carried(f))
                .ok_or_else(|| Error::invalid(format!("{what}: {} has no field {name} a pack may set here", ent.def.label)))?;
            let v = match (f.ftype, f.to.as_deref()) {
                (FieldType::Table, _) => continue,
                (FieldType::Link, Some(to)) if !v.is_null() => json!(self.id_for(ctx, tx, to, v, &format!("{what}: {name}")).await?.to_string()),
                _ => v.clone(),
            };
            doc.insert(name.clone(), v);
        }
        Ok(doc)
    }

    /// The one record of `to` with this key here.
    async fn id_for(&self, ctx: &Ctx, tx: &mut Tx, to: &str, key: &Value, what: &str) -> Result<Uuid> {
        let target = ctx.schema.entity(to).cloned().ok_or_else(|| Error::invalid(format!("{what}: {to} is not an entity here")))?;
        let fields = key_of(&target).ok_or_else(|| Error::invalid(format!("{what}: {to} has no key to find its records by")))?;
        let values: Vec<Value> = match key {
            Value::Array(a) if fields.len() > 1 => a.clone(),
            v => vec![v.clone()],
        };
        if values.len() != fields.len() {
            return Err(Error::invalid(format!("{what}: {} is not a key of {to} ({})", key_text(key), fields.join(", "))));
        }
        let filters: Vec<Filter> = fields.iter().zip(values).map(|(f, v)| Filter::eq(f, v)).collect();
        let found = self.find_tx(ctx, tx, to, &filters, None, 2).await?;
        match found.as_slice() {
            [one] => crate::store::doc_id(one),
            [] => Err(Error::invalid(format!("{what}: there is no {to} {} here", key_text(key)))),
            _ => Err(Error::invalid(format!("{what}: more than one {to} is {} here", key_text(key)))),
        }
    }

    /// Everything but the solutions, in the caller's transaction: what each item did.
    async fn apply(&self, ctx: &Ctx, tx: &mut Tx, pack: &Pack) -> Result<Summary> {
        let mut summary = Summary::new();
        // Calendars first: the targets that name them are checked against what is here. One of the same name
        // here is left as it is.
        for c in &pack.calendars {
            let what = format!("calendar {}", c.name);
            let status = match crate::load_calendar(tx, ctx.tenant, &c.name).await? {
                Some(_) => "unchanged",
                None => {
                    crate::process::set_calendar_in(tx, ctx, &c.calendar()).await.map_err(|e| about(&what, e))?;
                    "created"
                }
            };
            summary.entry("calendars").or_default().add(c.name.clone(), status);
        }
        // What the customizations name is there before them: the pack's roles by name (their rules, which name
        // entities, come after) and its message and document templates, kept as they come until they are saved below.
        let here: BTreeSet<String> =
            sqlx::query_scalar("select name from role where tenant_id = $1").bind(ctx.tenant).fetch_all(&mut **tx).await?.into_iter().collect();
        for role in pack.roles.iter().filter(|r| !here.contains(&r.name)) {
            sqlx::query("insert into role (tenant_id, name, description) values ($1, $2, $3)")
                .bind(ctx.tenant)
                .bind(&role.name)
                .bind(&role.description)
                .execute(&mut **tx)
                .await?;
        }
        const KEEP: [(&str, &str); 2] = [
            (
                "insert into message_template (tenant_id, key, def) values ($1, $2, $3) on conflict do nothing",
                "delete from message_template where tenant_id = $1 and key = $2",
            ),
            (
                "insert into document_template (tenant_id, key, def) values ($1, $2, $3) on conflict do nothing",
                "delete from document_template where tenant_id = $1 and key = $2",
            ),
        ];
        let messages = pack.messages.iter().map(|m| (KEEP[0], &m.key, serde_json::to_value(m)));
        let documents = pack.documents.iter().map(|d| (KEEP[1], &d.key, serde_json::to_value(d)));
        let mut early = Vec::new();
        for ((insert, delete), key, def) in messages.chain(documents) {
            let made = sqlx::query(insert).bind(ctx.tenant).bind(key).bind(def.map_err(Error::internal)?).execute(&mut **tx).await?;
            if made.rows_affected() == 1 {
                early.push((delete, key));
            }
        }
        let items = pack.customizations.iter().map(|c| NewCustomization { entity: c.entity.clone(), op: c.op.clone(), note: c.note.clone() }).collect();
        let (saved, schema) = self.merge_customizations(ctx, tx, items).await.map_err(|e| about("customizations", e))?;
        let section = summary.entry("customizations").or_default();
        for (name, status) in saved {
            section.add(name, status);
        }
        // What follows sees the entities and fields just defined.
        let ctx = &Ctx { schema: Arc::new(schema), ..ctx.clone() };
        for role in &pack.roles {
            let status = self.install_role(ctx, tx, role).await.map_err(|e| about(&format!("role {}", role.name), e))?;
            let status = if here.contains(&role.name) { status } else { "created" };
            summary.entry("roles").or_default().add(role.name.clone(), status);
        }
        for a in &pack.automations {
            let status = install_automation(ctx, tx, a).await.map_err(|e| about(&format!("automation {}", a.name), e))?;
            summary.entry("automations").or_default().add(a.name.clone(), status);
        }
        self.install_studio(ctx, tx, &pack.studio, summary.entry("studio").or_default()).await?;
        // The templates kept early are saved now, checked, as any other.
        for (delete, key) in early {
            sqlx::query(delete).bind(ctx.tenant).bind(key).execute(&mut **tx).await?;
        }
        for m in &pack.messages {
            let status = crate::messages::save_template(self, tx, ctx, m).await.map_err(|e| about(&format!("message {}", m.key), e))?;
            summary.entry("messages").or_default().add(m.key.clone(), status);
        }
        for d in &pack.documents {
            let status = crate::documents::save_template(self, tx, ctx, d).await.map_err(|e| about(&format!("document {}", d.key), e))?;
            summary.entry("documents").or_default().add(d.key.clone(), status);
        }
        for (entity, key) in &pack.document_defaults {
            let status = crate::documents::install_default(tx, ctx, entity, key).await.map_err(|e| about(&format!("default document of {entity}"), e))?;
            summary.entry("document_defaults").or_default().add(entity.clone(), status);
        }
        for c in &pack.channels {
            let status = crate::messages::install_channel(self, tx, ctx, c).await.map_err(|e| about(&format!("channel {}", c.name), e))?;
            summary.entry("channels").or_default().add(c.name.clone(), status);
        }
        for c in &pack.connections {
            let status =
                crate::connections::install(self, tx, ctx, c, &format!("pack:{}", pack.name)).await.map_err(|e| about(&format!("connection {}", c.name), e))?;
            summary.entry("connections").or_default().add(c.name.clone(), status);
        }
        for sec in &pack.records {
            let section = summary.entry("records").or_default();
            self.install_records(ctx, tx, sec, section).await?;
        }
        Ok(summary)
    }

    async fn install_role(&self, ctx: &Ctx, tx: &mut Tx, role: &RoleDef) -> Result<&'static str> {
        if MACHINE_ROLES.iter().any(|p| role.name.starts_with(p)) || role.name.trim().is_empty() || role.name.len() > 100 {
            return Err(Error::invalid("not a role a pack may carry"));
        }
        let found: Option<(Option<String>, Option<String>, bool)> =
            sqlx::query_as("select description, package, portal from role where tenant_id = $1 and name = $2")
                .bind(ctx.tenant)
                .bind(&role.name)
                .fetch_optional(&mut **tx)
                .await?;
        let have: Vec<PermissionDef> = rules_of(tx, ctx.tenant, Some(role.name.as_str())).await?.into_iter().map(Rule::def).collect();
        let new: Vec<&PermissionDef> = role.permissions.iter().filter(|p| !have.contains(&as_kept(p))).collect();
        let describe = found.as_ref().is_some_and(|(d, pkg, _)| pkg.is_none() && role.description.is_some() && *d != role.description);
        let portal = found.as_ref().map_or(role.portal, |f| f.2 != role.portal);
        let status = match &found {
            None => "created",
            Some(_) if new.is_empty() && !describe && !portal => return Ok("unchanged"),
            Some(_) => "changed",
        };
        sqlx::query("insert into role (tenant_id, name, description) values ($1, $2, $3) on conflict do nothing")
            .bind(ctx.tenant)
            .bind(&role.name)
            .bind(&role.description)
            .execute(&mut **tx)
            .await?;
        if describe {
            sqlx::query("update role set description = $3 where tenant_id = $1 and name = $2 and package is null")
                .bind(ctx.tenant)
                .bind(&role.name)
                .bind(&role.description)
                .execute(&mut **tx)
                .await?;
        }
        // A portal role is marked before its rules, which are then checked as a portal role's; a member
        // role's rules come first.
        if portal && role.portal {
            self.set_role_portal_in(ctx, tx, &role.name, true).await?;
        }
        for p in new {
            self.set_role_permission_in(ctx, tx, &role.name, p).await?;
        }
        if portal && !role.portal {
            self.set_role_portal_in(ctx, tx, &role.name, false).await?;
        }
        Ok(status)
    }

    async fn install_studio(&self, ctx: &Ctx, tx: &mut Tx, items: &[StudioItem], section: &mut Section) -> Result<()> {
        if let Some(odd) = items.iter().find(|i| !STUDIO.contains(&i.entity.as_str())) {
            return Err(Error::field("studio", format!("{} is not one of Studio's items ({})", odd.entity, STUDIO.join(", "))));
        }
        let mut keys = HashMap::new();
        let mut blocks: Vec<(Uuid, Value, String)> = Vec::new();
        for entity in STUDIO {
            for item in items.iter().filter(|i| i.entity == entity) {
                let what = format!("{entity} '{}'", item.name);
                let ent = ctx.schema.entity(entity).cloned().ok_or_else(|| Error::invalid(format!("{what}: Studio is not installed here")))?;
                let index = self.studio_index(ctx, tx).await?;
                let label = Filter::eq("label", item.name.clone());
                let found: Vec<Doc> =
                    self.find_tx(ctx, tx, entity, &[label], None, 100).await?.into_iter().filter(|d| d.get("is_standard") != Some(&json!(true))).collect();
                let existing = match found.as_slice() {
                    [] => None,
                    [one] => Some(self.get_tx(ctx, tx, entity, crate::store::doc_id(one)?, true).await?),
                    _ => return Err(Error::invalid(format!("{what}: this workspace has more than one of that name"))),
                };
                if let Some(doc) = &existing {
                    let mut now = self.portable(ctx, tx, &ent, doc, &mut keys).await?;
                    if let Some(v) = refs_field(entity).and_then(|f| now.get_mut(f)) {
                        rewrite_refs(v, |e, id| label_of_id(&index, e, id, &what))?;
                    }
                    if item.record.iter().all(|(k, v)| now.get(k) == Some(v)) {
                        section.add(what, "unchanged");
                        continue;
                    }
                }
                let mut record = item.record.clone();
                record.insert("label".into(), json!(item.name));
                // A space's blocks name items written after it: they are set once everything is there.
                let later = if entity == "Space" { record.remove("blocks") } else { None };
                if let Some(v) = refs_field(entity).and_then(|f| record.get_mut(f)) {
                    rewrite_refs(v, |e, label| id_of_label(&index, e, label, &what))?;
                }
                let doc = self.resolve(ctx, tx, &ent, &record, &what).await?;
                let id = match &existing {
                    None => crate::store::doc_id(&self.insert_tx(ctx, tx, entity, doc).await.map_err(|e| about(&what, e))?)?,
                    Some(old) => {
                        let id = crate::store::doc_id(old)?;
                        self.update_tx(ctx, tx, entity, id, doc, None).await.map_err(|e| about(&what, e))?;
                        id
                    }
                };
                if let Some(b) = later {
                    blocks.push((id, b, what.clone()));
                }
                section.add(what, if existing.is_some() { "changed" } else { "created" });
            }
        }
        if !blocks.is_empty() {
            let index = self.studio_index(ctx, tx).await?;
            for (id, mut b, what) in blocks {
                rewrite_refs(&mut b, |e, label| id_of_label(&index, e, label, &what))?;
                let patch = Doc::from_iter([("blocks".to_string(), b)]);
                self.update_tx(ctx, tx, "Space", id, patch, None).await.map_err(|e| about(&what, e))?;
            }
        }
        Ok(())
    }

    /// Reference records, matched by key: created or updated, never deleted.
    async fn install_records(&self, ctx: &Ctx, tx: &mut Tx, sec: &Records, section: &mut Section) -> Result<()> {
        let ent = reference_entity(ctx, &sec.entity)?;
        if one_record(&ent) {
            return self.install_settings(ctx, tx, &ent, sec, section).await;
        }
        if sec.key.is_empty() || sec.key.iter().any(|k| ent.field(k).is_none_or(|f| !carried(f) || f.ftype == FieldType::Table)) {
            return Err(Error::field("records", format!("records of {}: {} is not a key of its fields here", sec.entity, sec.key.join(", "))));
        }
        let mut keys = HashMap::new();
        for row in &sec.rows {
            let key = key_value(sec.key.iter().map(|k| row.get(k).cloned().unwrap_or_default()).collect());
            let what = format!("{} {}", sec.entity, key_text(&key));
            let doc = self.resolve(ctx, tx, &ent, row, &what).await?;
            let filters: Vec<Filter> = sec.key.iter().map(|k| Filter::eq(k, doc.get(k).cloned().unwrap_or_default())).collect();
            let found = self.find_tx(ctx, tx, &sec.entity, &filters, None, 2).await?;
            match found.as_slice() {
                [] => {
                    self.insert_tx(ctx, tx, &sec.entity, doc).await.map_err(|e| about(&what, e))?;
                    section.add(what, "created");
                }
                [one] => {
                    let id = crate::store::doc_id(one)?;
                    let old = self.get_tx(ctx, tx, &sec.entity, id, true).await?;
                    let now = self.portable(ctx, tx, &ent, &old, &mut keys).await?;
                    if row.iter().all(|(k, v)| now.get(k) == Some(v)) {
                        section.add(what, "unchanged");
                        continue;
                    }
                    self.update_tx(ctx, tx, &sec.entity, id, doc, None).await.map_err(|e| about(&what, e))?;
                    section.add(what, "changed");
                }
                _ => return Err(Error::invalid(format!("{what}: more than one record here has this key"))),
            }
        }
        Ok(())
    }

    /// A workspace's settings (BD-17): the pack's values when the workspace has no settings record yet; a record it
    /// has keeps its own values.
    async fn install_settings(&self, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, sec: &Records, section: &mut Section) -> Result<()> {
        let what = ent.def.label.clone();
        let [row] = sec.rows.as_slice() else {
            return Err(Error::field("records", format!("records of {}: settings travel as one record, and the pack has {}", sec.entity, sec.rows.len())));
        };
        if !self.find_tx(ctx, tx, &sec.entity, &[], None, 1).await?.is_empty() {
            section.add(what, "unchanged");
            return Ok(());
        }
        let doc = self.resolve(ctx, tx, ent, row, &what).await?;
        self.insert_tx(ctx, tx, &sec.entity, doc).await.map_err(|e| about(&what, e))?;
        section.add(what, "created");
        Ok(())
    }

    /// Whether a solution of a pack is here already: a release of the same source, or a draft holding it.
    async fn solution_here(&self, ctx: &Ctx, s: &Value) -> Result<bool> {
        let text = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let here: bool = sqlx::query_scalar(
            "select exists (select 1 from logic_release where tenant_id = $1 and solution = $2 and source_digest = $3) \
             or exists (select 1 from logic_draft where tenant_id = $1 and solution = $2 and source = $4)",
        )
        .bind(ctx.tenant)
        .bind(text("solution"))
        .bind(text("source_digest"))
        .bind(text("source"))
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(here)
    }

    /// `POST /api/admin/pack/plan`: what installing would do here, doing nothing: the install runs in a
    /// transaction that is rolled back.
    pub async fn plan_pack(&self, ctx: &Ctx, raw: &Value) -> Result<Value> {
        require_admin(ctx)?;
        let pack = parse(raw)?;
        let digest = digest(&pack)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let missing = lacking(&mut tx, ctx.tenant, &pack).await?;
        let mut out = json!({
            "name": pack.name, "label": pack.label, "version": pack.version, "digest": digest,
            "requires": pack.requires, "missing": missing, "can_install": missing.is_empty(),
        });
        if !missing.is_empty() {
            tx.rollback().await?;
            return Ok(out);
        }
        let before = Self::overlay_rows(&mut tx, ctx.tenant).await?;
        let mut summary = self.apply(ctx, &mut tx, &pack).await?;
        let mut warnings = stops_applying(&pack, &before, &Self::overlay_rows(&mut tx, ctx.tenant).await?);
        tx.rollback().await?;
        warnings.extend(publishes(ctx, &pack));
        for a in pack.automations.iter().filter(|a| a.schedule.is_some()) {
            let touched = summary.get("automations").is_some_and(|s| s.created.contains(&a.name) || s.changed.contains(&a.name));
            if touched {
                warnings.push(format!("The automation {} runs on a schedule from now: no moment already past fires.", a.name));
            }
        }
        let section = summary.entry("solutions").or_default();
        for s in &pack.solutions {
            let name = s.get("solution").and_then(Value::as_str).unwrap_or_default().to_string();
            if self.solution_here(ctx, s).await? {
                section.add(name, "unchanged");
            } else {
                warnings.push(format!(
                    "The solution {name} arrives as a draft: it runs only once it is kept as a release, reviewed by a second administrator and activated."
                ));
                section.add(name, "created");
            }
        }
        out["sections"] = serde_json::to_value(&summary).map_err(Error::internal)?;
        out["warnings"] = json!(warnings);
        Ok(out)
    }

    /// `POST /api/admin/pack/install`: the pack whose plan has `digest`, in one transaction but for its
    /// solutions, which then arrive as drafts one by one; a solution that fails is named and the rest stand.
    pub async fn install_pack(&self, ctx: &Ctx, raw: &Value, given: &str) -> Result<Value> {
        require_admin(ctx)?;
        let pack = parse(raw)?;
        let digest = digest(&pack)?;
        if given != digest {
            return Err(Error::field("digest", "is not this pack's: plan the pack again and install what that plan shows"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        // One install at a time in a workspace: what a pack brings is matched by name, and two installs
        // at once would each find the name free. Held to the end of this transaction.
        sqlx::query("select pg_advisory_xact_lock($1, hashtext($2))").bind(INSTALL_LOCK).bind(ctx.tenant.to_string()).execute(&mut *tx).await?;
        let missing = lacking(&mut tx, ctx.tenant, &pack).await?;
        if !missing.is_empty() {
            return Err(Error::Conflict(format!("this workspace cannot take the pack: {}", missing.join("; "))));
        }
        let before = Self::overlay_rows(&mut tx, ctx.tenant).await?;
        let mut summary = self.apply(ctx, &mut tx, &pack).await?;
        let warnings = stops_applying(&pack, &before, &Self::overlay_rows(&mut tx, ctx.tenant).await?);
        let id = Uuid::now_v7();
        sqlx::query("insert into pack_install (tenant_id, id, name, version, digest, installed_by, summary) values ($1, $2, $3, $4, $5, $6, $7)")
            .bind(ctx.tenant)
            .bind(id)
            .bind(&pack.name)
            .bind(pack.version)
            .bind(&digest)
            .bind((!ctx.user.is_nil()).then_some(ctx.user))
            .bind(serde_json::to_value(&summary).map_err(Error::internal)?)
            .execute(&mut *tx)
            .await?;
        crate::audit::record_admin(&mut tx, ctx, "Pack", id, "install", None, json!({"name": pack.name, "version": pack.version, "digest": digest})).await?;
        tx.commit().await?;
        self.registry.invalidate(ctx.tenant);
        self.invalidate_tenant_users(ctx.tenant);
        // Solutions, after the commit, each its own: a draft that a person keeps, a second administrator
        // reviews and someone activates.
        let fresh = if ctx.user.is_nil() {
            Ctx::system(ctx.tenant, self.registry.schema(ctx.tenant).await?)
        } else {
            self.load_ctx(ctx.tenant, ctx.user, None, ctx.request_id.clone()).await?
        };
        let mut failed = Vec::new();
        let section = summary.entry("solutions").or_default();
        for s in &pack.solutions {
            let name = s.get("solution").and_then(Value::as_str).unwrap_or_default().to_string();
            if self.solution_here(&fresh, s).await? {
                section.add(name, "unchanged");
                continue;
            }
            match self.import_solution_pack(&fresh, s).await {
                Ok(_) => section.add(name, "created"),
                Err(e) => failed.push(json!({"solution": name, "error": e.to_string()})),
            }
        }
        let summary = serde_json::to_value(&summary).map_err(Error::internal)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("update pack_install set summary = $3 where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).bind(&summary).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(json!({"id": id, "name": pack.name, "version": pack.version, "digest": digest, "summary": summary, "failed": failed, "warnings": warnings}))
    }

    /// `GET /api/admin/packs`: the packs installed here, the latest first.
    pub async fn installed_packs(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', id, 'name', name, 'version', version, 'digest', digest, 'installed_by', installed_by, \
               'installed_at', installed_at, 'summary', summary) from pack_install where tenant_id = $1 order by installed_at desc, id desc limit 200",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }
}

/// The automations administrators saved (an app's, saved by no one, stay its own), by name.
async fn export_automations(ctx: &Ctx, tx: &mut Tx) -> Result<Vec<Automation>> {
    let rows: Vec<Automation> = sqlx::query_as(
        "select name, entity, event, schedule, condition, actions, is_active from automation where tenant_id = $1 and created_by is not null order by name, id",
    )
    .bind(ctx.tenant)
    .fetch_all(&mut **tx)
    .await?;
    if let Some(w) = rows.windows(2).find(|w| w[0].name == w[1].name) {
        return Err(Error::Conflict(format!("two automations are called {}: a pack matches automations by name, so rename one", w[0].name)));
    }
    for a in &rows {
        let to_person = a.actions.as_array().into_iter().flatten().any(|x| x.get("to").and_then(Value::as_str).is_some_and(|t| t.starts_with("user:")));
        if to_person {
            return Err(Error::invalid(format!(
                "the automation {} notifies a person by id: a pack carries no people, so notify a role or a field instead",
                a.name
            )));
        }
    }
    Ok(rows)
}

/// An automation matched by name: saved as the installer, through the admin API's own save.
async fn install_automation(ctx: &Ctx, tx: &mut Tx, a: &Automation) -> Result<&'static str> {
    let found: Option<(Uuid, String, String, String, Option<Value>, Option<String>, Value, bool)> = sqlx::query_as(
        "select id, name, entity, event, schedule, condition, actions, is_active from automation where tenant_id = $1 and name = $2 order by created_at, id limit 1",
    )
    .bind(ctx.tenant)
    .bind(&a.name)
    .fetch_optional(&mut **tx)
    .await?;
    let mut input = serde_json::to_value(a).map_err(Error::internal)?.as_object().cloned().unwrap_or_default();
    let status = match found {
        Some((id, name, entity, event, schedule, condition, actions, is_active)) => {
            if (Automation { name, entity, event, schedule, condition, actions, is_active }) == *a {
                return Ok("unchanged");
            }
            input.insert("id".into(), json!(id));
            "changed"
        }
        None => "created",
    };
    crate::automation::save_in(ctx, tx, &input).await?;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(extra: Value) -> Value {
        let mut p = json!({"format": FORMAT, "name": "gym", "label": "Gym", "version": 1});
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            p[k] = v;
        }
        p
    }

    #[test]
    fn a_pack_is_read_strictly() {
        assert!(parse(&pack(json!({}))).is_ok());
        for (bad, says) in [
            (pack(json!({"code": "rm -rf"})), "unknown field `code`"),
            (pack(json!({"format": "ferrum.changeset/1"})), "not a ferrum.pack/1 pack"),
            (pack(json!({"name": "Gym!"})), "lower-case"),
            (
                pack(json!({"automations": [{"name": "a", "entity": "X", "event": "created", "actions": [], "is_active": true, "sql": "x"}]})),
                "unknown field `sql`",
            ),
            (pack(json!({"records": [{"entity": "X", "key": ["k"], "rows": [], "ids": []}]})), "unknown field `ids`"),
            (pack(json!({"padding": "x".repeat(MAX_PACK)})), "at most 5242880 bytes"),
        ] {
            let e = parse(&bad).unwrap_err().to_string();
            assert!(e.contains(says), "{says}: {e}");
        }
    }

    #[test]
    fn the_digest_leaves_out_when_it_was_exported() {
        let a = parse(&pack(json!({"exported_at": "2026-11-10T10:00:00Z"}))).unwrap();
        let b = parse(&pack(json!({"exported_at": "2026-11-11T10:00:00Z"}))).unwrap();
        assert_eq!(digest(&a).unwrap(), digest(&b).unwrap());
        let c = parse(&pack(json!({"label": "Gym 2"}))).unwrap();
        assert_ne!(digest(&a).unwrap(), digest(&c).unwrap(), "a changed word changes it");
    }

    #[test]
    fn studio_references_are_rewritten_wherever_they_are() {
        let mut blocks = json!([{"type": "chart", "chart": "c1"}, {"type": "links", "label": "L", "items": [{"label": "r", "report": "r1"}, {"label": "d", "dashboard": "d1"}]},
            {"type": "markdown", "content": "chart"}]);
        rewrite_refs(&mut blocks, |e, id| Ok(format!("{e}:{id}"))).unwrap();
        assert_eq!(blocks[0]["chart"], json!("DashboardChart:c1"));
        assert_eq!(blocks[1]["items"][0]["report"], json!("Report:r1"));
        assert_eq!(blocks[1]["items"][1]["dashboard"], json!("Dashboard:d1"));
        assert_eq!(blocks[2]["content"], json!("chart"));
    }
}
