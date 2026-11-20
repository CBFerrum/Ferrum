//! What a package declares about callers outside the workspace (gapclose/design/A11-app-apis.md 2.3.1, API-1): who may
//! call an operation, how often, behind which guard, with which handle, and what an entity's records show them. Pure
//! declarations, checked when a package loads ([`check_package`]); the doors that honour them come in later slices.

use crate::{ActionDef, EntityDef, EntityKind, FieldType, OverlayOp, Package};
use ferrum_logic::capability::Effect;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Who may call: ordered, each widening the one before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    #[default]
    Staff,
    Customer,
    Anyone,
}

/// An entity's record reads for callers outside the workspace. Absent: staff only.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityApi {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<ReadApi>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<ListApi>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadApi {
    pub audience: Audience,
    pub fields: Vec<String>,
    /// A table field's columns shown, 1 to 8.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub columns: BTreeMap<String, Vec<String>>,
    /// Seconds an anonymous answer may be cached, 1 to 300.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<u16>,
}

fn default_limit() -> u32 {
    24
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListApi {
    pub audience: Audience,
    pub fields: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub columns: BTreeMap<String, Vec<String>>,
    /// A subset of `fields`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<String>,
    /// `"<field> asc|desc"`, fields of `fields`; the first is the default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sort: Vec<String>,
    /// The most one page holds, 1 to 100.
    #[serde(default = "default_limit")]
    pub limit: u32,
    /// `q` over the entity's search fields that are in `fields`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub search: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<u16>,
}

/// Whose calls a rate counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RatePer {
    /// The caller's network.
    Address,
    /// The signed-in customer (a `customer` operation only).
    Customer,
    /// The key (a staff operation only).
    Key,
    /// The handle (an action a handle grants).
    Handle,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallRate {
    pub per: RatePer,
    /// 1 to 100,000 calls ...
    pub max: u32,
    /// ... in this many seconds, 1 to 3,600.
    pub seconds: u16,
}

/// The challenge, proof of work and counts every anonymous create passes (S4-P5): `scope` names its counters;
/// `address` a parameter whose messaged address is counted too.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardDecl {
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// A handle given once in the answer: the record `field` links to, for `days`, letting its holder run `grants`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandleDecl {
    pub field: String,
    pub days: u16,
    pub grants: Vec<String>,
}

/// On a floor entity, in its own package: what another app's work may call while an outside caller's request runs,
/// for the record `via` names (`[entity-name field, record-id field]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutsideCalls {
    pub via: (String, String),
    pub operations: Vec<String>,
}

/// The gate codes a refusal's own words may replace (docs/logic/capabilities.md, "Refusals").
pub const REFUSAL_CODES: &[&str] = &["schema_invalid", "forbidden", "capability_unavailable", "input_changed", "not_found", "conflict", "invalid"];

/// What an operation's `audience` is: its contract's, or staff.
pub fn audience_of(a: &ActionDef) -> Audience {
    a.contract.as_ref().map(|c| c.audience).unwrap_or_default()
}

fn effect_of(a: &ActionDef) -> Effect {
    a.contract.as_ref().and_then(|c| c.effect).unwrap_or(if a.read_only { Effect::Query } else { Effect::TransactionalAction })
}

/// What a package loading may hold of every other: each entity by name with its package, and each package.
pub(crate) struct Loaded<'a> {
    pub entities: &'a BTreeMap<String, (String, EntityDef)>,
    pub packages: &'a BTreeMap<String, Package>,
}

impl Loaded<'_> {
    /// A package holding an `internal` entity: staff only, whatever it declares (2.9 R3; its money part is API-1b's).
    fn floor_package(&self, package: &str) -> bool {
        self.packages.get(package).is_some_and(|p| p.entities.iter().any(|e| e.internal))
    }

    /// An entity no caller outside the workspace reaches (A11 2.3.1 rule 1).
    fn floor(&self, e: &EntityDef) -> bool {
        matches!(e.kind, EntityKind::Ledger | EntityKind::Child) || self.entities.get(&e.name).is_some_and(|(owner, _)| self.floor_package(owner))
    }

    fn personal(&self, e: &EntityDef) -> bool {
        self.packages.values().any(|p| p.manifest.personal_data.iter().any(|d| d.entity == e.name))
    }

    /// The actions `package` declares on `entity`: its own entity's, or those its overlays add.
    fn declared_by(&self, package: &str, entity: &str) -> Vec<&ActionDef> {
        let Some(p) = self.packages.get(package) else { return vec![] };
        let own = p.entities.iter().filter(|e| e.name == entity).flat_map(|e| &e.actions);
        let added = p.manifest.overlays.iter().filter(|o| o.entity == entity).flat_map(|o| &o.ops).filter_map(|op| match op {
            OverlayOp::AddAction { action } => Some(action),
            _ => None,
        });
        own.chain(added).collect()
    }
}

/// A11 2.3.1's rules for package `name` as it loads: its entities' `api` and `called_for_outside`, its actions' audience,
/// cache, guard, handle, rate, refusals and sunset (its own and those its overlays add), and its roles' visitor rules and
/// rules naming collection actions.
pub(crate) fn check_package(all: &Loaded, name: &str, p: &Package) -> Result<(), String> {
    // Every action a handle of this package grants, by entity: a `per: handle` rate is only on one of them.
    let mut granted: BTreeSet<(String, String)> = BTreeSet::new();
    let mut actions: Vec<(&EntityDef, &ActionDef, bool)> = Vec::new();
    for e in &p.entities {
        actions.extend(e.actions.iter().map(|a| (e, a, false)));
    }
    for ov in &p.manifest.overlays {
        let Some((_, target)) = all.entities.get(&ov.entity) else { continue };
        for op in &ov.ops {
            if let OverlayOp::AddAction { action } = op {
                actions.push((target, action, true));
            }
        }
    }
    for (_, a, _) in &actions {
        if let Some(h) = a.contract.as_ref().and_then(|c| c.handle.as_ref()) {
            let to = link_of(a, &h.field).unwrap_or_default();
            granted.extend(h.grants.iter().map(|g| (to.clone(), g.clone())));
        }
    }
    for e in &p.entities {
        if let Some(api) = &e.api {
            if all.floor(e) {
                return Err(format!("{}: an entity of a package with internal entities, a ledger or a line is for staff only: no api", e.name));
            }
            entity_api(all, e, api).map_err(|m| format!("{}: api: {m}", e.name))?;
        }
        if let Some(o) = &e.called_for_outside {
            outside_calls(all, name, e, o).map_err(|m| format!("{}: called_for_outside: {m}", e.name))?;
        }
    }
    for (e, a, overlay) in actions {
        if audience_of(a) != Audience::Staff && (all.floor(e) || all.floor_package(name)) {
            return Err(format!(
                "{}action {} on {}: an operation of a package with internal entities, or on a ledger or a line, faces staff only",
                if overlay { format!("overlay on {}: ", e.name) } else { String::new() },
                a.name,
                e.name
            ));
        }
        action(all, name, e, a, &granted).map_err(|m| format!("{}: action {}: {m}", e.name, a.name))?;
    }
    for role in &p.manifest.roles {
        for perm in &role.permissions {
            let def = all.entities.get(&perm.entity).map(|(_, d)| d);
            if let Some(d) = def {
                for a in d.actions.iter().filter(|a| a.collection && perm.actions.contains(&a.name)) {
                    if !perm.condition.is_empty() || perm.via.is_some() || perm.if_owner || perm.shared.is_some() {
                        return Err(format!(
                            "role {} on {}: a rule naming collection action {} takes no where, via, if_owner or shared: there is no record",
                            role.name, perm.entity, a.name
                        ));
                    }
                }
            }
            if role.visitor {
                if def.is_some_and(|d| all.floor(d)) {
                    return Err(format!("role {} on {}: a visitor role never reaches an entity for staff only", role.name, perm.entity));
                }
                if let Some(why) = perm.visitor_problem(def, def.is_some_and(|d| all.personal(d) || crate::holds_personal_data(d, |_| false))) {
                    return Err(format!("visitor role {}: {why}", role.name));
                }
            }
        }
    }
    Ok(())
}

/// The entity a property of `a`'s returns links to (`x-ferrum-link`).
fn link_of(a: &ActionDef, field: &str) -> Option<String> {
    let returns = a.contract.as_ref()?.returns.as_ref()?;
    returns.get("properties")?.get(field)?.get("x-ferrum-link")?.as_str().map(str::to_string)
}

fn action(all: &Loaded, package: &str, e: &EntityDef, a: &ActionDef, granted: &BTreeSet<(String, String)>) -> Result<(), String> {
    if a.collection {
        if !a.docstatus.is_empty() || a.condition.is_some() || !a.effects.is_empty() {
            return Err("a collection action takes no docstatus, condition or effects: there is no record".into());
        }
        if !(a.permission == "create" || a.permission == "read" && a.read_only) {
            return Err("a collection action's permission is create, or read with read_only".into());
        }
    }
    let Some(c) = &a.contract else { return Ok(()) };
    let audience = c.audience;
    let effect = effect_of(a);
    match (&c.deprecated, c.deprecated_since, c.sunset) {
        (_, None, None) => {}
        (None, _, _) => return Err("deprecated_since and sunset come with deprecated".into()),
        (Some(_), Some(since), Some(sunset)) if sunset < since + chrono::Days::new(90) => {
            return Err("sunset is at least 90 days after deprecated_since".into());
        }
        (Some(_), Some(_), Some(_)) => {}
        _ => return Err("deprecated_since and sunset go together".into()),
    }
    if c.rate.len() > 3 {
        return Err("at most three rates".into());
    }
    for r in &c.rate {
        if !(1..=100_000).contains(&r.max) {
            return Err("a rate's max is 1 to 100,000".into());
        }
        if !(1..=3600).contains(&r.seconds) {
            return Err("a rate's seconds are 1 to 3,600: the counts are kept an hour".into());
        }
        let fits = match r.per {
            RatePer::Address => true,
            RatePer::Customer => audience == Audience::Customer,
            RatePer::Key => audience == Audience::Staff,
            RatePer::Handle => granted.contains(&(e.name.clone(), a.name.clone())),
        };
        if !fits {
            return Err(format!(
                "a rate per {:?} fits only {}",
                r.per,
                match r.per {
                    RatePer::Customer => "a customer operation",
                    RatePer::Key => "a staff operation (per key)",
                    _ => "an action a handle of this package grants (per handle)",
                }
            )
            .to_lowercase());
        }
    }
    for (code, words) in &c.refusals {
        if !REFUSAL_CODES.contains(&code.as_str()) || words.chars().count() > 200 || words.contains(['{', '}']) || words.trim().is_empty() {
            return Err(format!(
                "refusals: {code} is a gate code ({}) and its words are at most 200 characters, with no placeholders",
                REFUSAL_CODES.join(", ")
            ));
        }
    }
    if c.cache.is_some_and(|n| !(1..=300).contains(&n) || audience != Audience::Anyone || effect.changes()) {
        return Err("cache is 1 to 300 seconds, on an anyone query only".into());
    }
    if audience == Audience::Staff {
        if c.guard.is_some() || c.handle.is_some() {
            return Err("a guard or a handle is for an operation outside callers use: give it an audience".into());
        }
        return Ok(());
    }
    if effect != Effect::ExternalEffect && effect != Effect::TransactionalAction && effect != Effect::Query {
        return Err(format!("an operation outside callers use is a query, a transactional action or an external effect, not {effect:?}"));
    }
    if c.completion.as_deref().is_none_or(|s| s.trim().is_empty()) {
        return Err("an operation outside callers use says its completion".into());
    }
    let returns = c.returns.as_ref().ok_or("an operation outside callers use declares its returns")?;
    if returns.get("type") != Some(&Value::String("object".into())) {
        return Err("returns is an object".into());
    }
    closed(returns, "").map_err(|m| format!("returns{m}"))?;
    let anonymous_create = audience == Audience::Anyone && (a.collection && effect.changes() || effect == Effect::ExternalEffect);
    match &c.guard {
        None if anonymous_create => return Err("an anonymous create or outside effect needs a guard".into()),
        None => {}
        Some(g) => {
            let scope = regex::Regex::new(r"^[a-z0-9_.-]{1,60}$").expect("pattern");
            if !scope.is_match(&g.scope) {
                return Err(format!("guard scope {} is 1 to 60 of a-z, 0-9, _, . and -", g.scope));
            }
            if let Some(addr) = &g.address
                && !a.params.iter().any(|p| &p.name == addr && p.ptype == "data")
            {
                return Err(format!("guard address {addr} is no data parameter of the action"));
            }
        }
    }
    if let Some(h) = &c.handle {
        if !effect.changes() {
            return Err("a handle is given by an operation that changes something".into());
        }
        let to = link_of(a, &h.field).ok_or_else(|| format!("handle field {} is no property of returns carrying x-ferrum-link", h.field))?;
        let (_, target) = all.entities.get(&to).ok_or_else(|| format!("handle field {} links to {to}, which is no entity", h.field))?;
        if all.floor(target) {
            return Err(format!("handle field {} links to {to}, which is for staff only", h.field));
        }
        if !(1..=90).contains(&h.days) {
            return Err("handle days are 1 to 90".into());
        }
        if !(1..=20).contains(&h.grants.len()) {
            return Err("handle grants name 1 to 20 actions".into());
        }
        let ours = all.declared_by(package, &to);
        for g in &h.grants {
            if !ours.iter().any(|x| &x.name == g && !x.collection && audience_of(x) == Audience::Anyone) {
                return Err(format!("handle grant {g} is no action of {to} this package declares for anyone, bound to a record"));
            }
        }
    }
    Ok(())
}

/// Refused anywhere in a customer-facing `returns`: what the output allow-list cannot prune (A11 2.14.5).
fn closed(v: &Value, at: &str) -> Result<(), String> {
    let o = v.as_object().ok_or_else(|| format!("{at}: a schema is an object"))?;
    for k in ["anyOf", "oneOf", "allOf", "not", "$ref", "patternProperties"] {
        if o.contains_key(k) {
            return Err(format!("{at}: {k} is not allowed facing callers outside"));
        }
    }
    if o.get("additionalProperties").is_some_and(|x| x != &Value::Bool(false)) {
        return Err(format!("{at}: additionalProperties is false or absent"));
    }
    let ty = match o.get("type") {
        Some(Value::String(t)) => t.as_str(),
        Some(Value::Array(ts)) if ts.len() == 2 && ts.iter().any(|t| t == "null") => {
            ts.iter().find_map(|t| t.as_str().filter(|t| *t != "null")).unwrap_or("null")
        }
        Some(_) => return Err(format!("{at}: a type is one type, or one and null")),
        None => return Err(format!("{at}: every schema says its type")),
    };
    match ty {
        "object" => {
            let props = o.get("properties").and_then(Value::as_object).ok_or_else(|| format!("{at}: an object lists its properties"))?;
            props.iter().try_for_each(|(k, p)| closed(p, &format!("{at}/{k}")))
        }
        "array" => closed(o.get("items").ok_or_else(|| format!("{at}: an array says its items"))?, &format!("{at}/items")),
        _ => Ok(()),
    }
}

fn entity_api(all: &Loaded, e: &EntityDef, api: &EntityApi) -> Result<(), String> {
    let personal = all.personal(e);
    let shown = |what: &str, audience: Audience, fields: &[String], columns: &BTreeMap<String, Vec<String>>, cache: Option<u16>| -> Result<(), String> {
        if audience == Audience::Staff {
            return Err(format!("{what}: audience is customer or anyone"));
        }
        if cache.is_some_and(|n| !(1..=300).contains(&n) || audience != Audience::Anyone) {
            return Err(format!("{what}: cache is 1 to 300 seconds, for anyone only"));
        }
        if audience == Audience::Anyone && personal {
            return Err(format!("{what}: {} holds declared personal data: not for anyone", e.name));
        }
        for name in fields {
            let f = e.fields.iter().find(|f| &f.name == name).ok_or_else(|| format!("{what}: {name} is not a field of {}", e.name))?;
            if f.ftype == FieldType::Secret || f.perm_group.is_some() || f.no_export {
                return Err(format!("{what}: field {name} is a secret, in a field group or not exported"));
            }
            let linked = f.fetch_from.is_some() || f.rollup.is_some() || f.computed.as_deref().is_some_and(|x| reads_a_link(e, x));
            if audience == Audience::Anyone && (matches!(f.ftype, FieldType::Email | FieldType::Phone) || linked) {
                return Err(format!("{what}: field {name} is personal, or read from linked records: not for anyone"));
            }
        }
        for (table, cols) in columns {
            let f = e.fields.iter().find(|f| &f.name == table && f.ftype == FieldType::Table && fields.contains(table));
            let child = f.and_then(|f| f.of.as_deref()).and_then(|c| all.entities.get(c)).map(|(_, d)| d);
            let child = child.ok_or_else(|| format!("{what}: columns {table} is no table field shown"))?;
            if !(1..=8).contains(&cols.len())
                || cols.iter().any(|c| !child.fields.iter().any(|f| &f.name == c && f.ftype != FieldType::Secret && f.perm_group.is_none()))
            {
                return Err(format!("{what}: columns {table} are 1 to 8 fields of {}", child.name));
            }
        }
        Ok(())
    };
    if let Some(r) = &api.read {
        shown("read", r.audience, &r.fields, &r.columns, r.cache)?;
    }
    if let Some(l) = &api.list {
        shown("list", l.audience, &l.fields, &l.columns, l.cache)?;
        if let Some(f) = l.filters.iter().find(|f| !l.fields.contains(f)) {
            return Err(format!("list: filter {f} is not a field shown"));
        }
        for s in &l.sort {
            let (f, dir) = s.split_once(' ').unwrap_or((s, "asc"));
            if !l.fields.iter().any(|x| x == f) {
                return Err(format!("list: sort {s}: {f} is not a field shown"));
            }
            if !["asc", "desc"].contains(&dir) {
                return Err(format!("list: sort {s} is \"<field> asc\" or \"<field> desc\""));
            }
        }
        if !(1..=100).contains(&l.limit) {
            return Err("list: limit is 1 to 100".into());
        }
        if l.search && !e.search_fields.iter().any(|f| l.fields.contains(f)) {
            return Err("list: search needs one of the entity's search fields shown".into());
        }
    }
    Ok(())
}

/// Whether expression `x` reads a linked record's value (`doc.<link>.…`).
fn reads_a_link(e: &EntityDef, x: &str) -> bool {
    e.fields.iter().filter(|f| f.ftype == FieldType::Link).any(|f| x.contains(&format!("doc.{}.", f.name)))
}

fn outside_calls(all: &Loaded, package: &str, e: &EntityDef, o: &OutsideCalls) -> Result<(), String> {
    if !all.floor_package(package) {
        return Err("only on an entity of a package with internal entities (a floor package)".into());
    }
    let field = |n: &str| e.fields.iter().find(|f| f.name == n);
    if field(&o.via.0).is_none_or(|f| f.ftype != FieldType::Data) {
        return Err(format!("via {} is no data field holding an entity's name", o.via.0));
    }
    if field(&o.via.1).is_none() {
        return Err(format!("via {} is no field holding a record's id", o.via.1));
    }
    for op in &o.operations {
        if !(op == "create" || op == "read" || e.actions.iter().any(|a| &a.name == op)) {
            return Err(format!("operation {op} is create, read or an action of {} its package declares", e.name));
        }
    }
    Ok(())
}
