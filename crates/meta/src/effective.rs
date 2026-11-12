//! The effective (per-tenant) schema: installed packages + package overlays + tenant overlays,
//! merged deterministically and compiled. Broken tenant customizations are isolated into
//! `Schema::warnings` instead of taking the tenant down (upgrade safety).

use crate::catalog::{Catalog, OverlaySource, validate_entity, validate_workflow};
use crate::expr::Expr;
use crate::types::*;
use crate::{MetaError, child_table, invalid, is_entity_name, is_ident, table_for};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;

pub const CUSTOM_RECORD_TABLE: &str = "custom_record";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Storage {
    /// A real typed column.
    Column,
    /// A key in the row's `ext` JSONB column (customizations; no DDL).
    Ext,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChildInfo {
    pub entity: String,
    pub table: String,
}

impl ChildInfo {
    /// What tells this table's rows apart where they share `custom_record` with every other
    /// custom record (` and <alias>.entity = '<child>'`); empty for a table of its own. Every
    /// reader and writer of child rows adds it.
    pub fn predicate(&self, alias: &str) -> String {
        if self.table == CUSTOM_RECORD_TABLE { format!(" and {alias}.entity = '{}'", self.entity) } else { String::new() }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Exprs {
    pub default_expr: BTreeMap<String, Expr>,
    pub computed: BTreeMap<String, Expr>,
    /// The order `computed` is worked out in: a field after those it reads (`doc.<field>`), by name otherwise.
    pub computed_order: Vec<String>,
    pub visible_if: BTreeMap<String, Expr>,
    pub required_if: BTreeMap<String, Expr>,
    pub read_only_if: BTreeMap<String, Expr>,
    pub rules: Vec<(RuleDef, Expr)>,
    pub action_conditions: BTreeMap<String, Expr>,
    /// A configured action's `refuse_if`, compiled, with their messages, in order.
    pub action_refusals: BTreeMap<String, Vec<(Expr, String)>>,
    /// Each service target's expressions, by its name.
    pub targets: BTreeMap<String, TargetExprs>,
}

impl Exprs {
    /// Every expression it holds.
    pub fn all(&self) -> impl Iterator<Item = &Expr> {
        let maps = [&self.default_expr, &self.computed, &self.visible_if, &self.required_if, &self.read_only_if, &self.action_conditions];
        let targets =
            self.targets.values().flat_map(|t| [t.start_when.as_ref(), Some(&t.within), t.pause_when.as_ref(), Some(&t.met_when)].into_iter().flatten());
        maps.into_iter()
            .flat_map(|m| m.values())
            .chain(self.rules.iter().map(|(_, e)| e))
            .chain(self.action_refusals.values().flatten().map(|(e, _)| e))
            .chain(targets)
    }
}

/// A service target's expressions, compiled.
#[derive(Debug, Clone)]
pub struct TargetExprs {
    pub start_when: Option<Expr>,
    pub within: Expr,
    pub pause_when: Option<Expr>,
    pub met_when: Expr,
}

#[derive(Debug, Clone)]
pub struct CompiledWorkflow {
    pub def: WorkflowDef,
    pub guards: BTreeMap<(String, String), Expr>,
    /// What its approval policies say (`when`, approvers' and escalations' `expr`): the settings they read are loaded.
    pub approvals: Vec<Expr>,
}

impl CompiledWorkflow {
    /// Its guards and its approval policies' expressions.
    pub fn exprs(&self) -> impl Iterator<Item = &Expr> {
        self.guards.values().chain(&self.approvals)
    }
}

/// An approval policy on submitting or cancelling an entity's documents (`set_approval`, BD-19), with what its
/// expressions say (the settings they read are loaded).
#[derive(Debug, Clone)]
pub struct DocumentApproval {
    pub policy: ApprovalPolicy,
    pub exprs: Vec<Expr>,
}

/// Why an approval cannot stand on action `name` of `def`, if it cannot: the entity has no such action, or it
/// changes nothing (`read_only`), so there is nothing to agree to.
pub(crate) fn action_approval_problem(def: &EntityDef, name: &str) -> Option<String> {
    match def.actions.iter().find(|a| a.name == name) {
        None => Some(format!("{} has no action {name}", def.label)),
        Some(a) if a.read_only => Some(format!("the action {name} changes nothing, so there is nothing to approve")),
        Some(_) => None,
    }
}

/// What an approval policy's expressions say: each step's `when`, its approvers' and its escalation's `expr`.
fn policy_exprs(p: &ApprovalPolicy) -> Result<Vec<Expr>, MetaError> {
    let mut out = Vec::new();
    for s in &p.steps {
        for x in [&s.when, &s.approvers.expr].into_iter().chain(s.escalate_to.as_ref().map(|e| &e.expr)).flatten() {
            out.push(Expr::compile(x).map_err(MetaError::Invalid)?);
        }
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct EffEntity {
    pub def: EntityDef,
    /// Owning package, or `tenant` for custom entities.
    pub package: String,
    /// Physical table; empty for child templates (their tables are per parent field).
    pub table: String,
    /// Stored in the generic `custom_record` table.
    pub custom: bool,
    pub storage: BTreeMap<String, Storage>,
    pub children: BTreeMap<String, ChildInfo>,
    pub exprs: Exprs,
    pub workflow: Option<CompiledWorkflow>,
    pub print_template: Option<String>,
    /// Approval before its documents are submitted or cancelled, outside a workflow (BD-19).
    pub document_approvals: BTreeMap<DocumentAction, DocumentApproval>,
}

impl EffEntity {
    pub fn name(&self) -> &str {
        &self.def.name
    }

    pub fn field(&self, name: &str) -> Option<&FieldDef> {
        self.def.fields.iter().find(|f| f.name == name)
    }

    /// Headed by a link (an appointment by its patient): its title is the linked record's.
    pub fn headed_by_link(&self) -> bool {
        self.def.title_field.as_deref().and_then(|t| self.field(t)).is_some_and(|f| f.ftype == FieldType::Link)
    }

    /// The service target that keeps `field` (its due moment or its state), if one does.
    pub fn target_keeping(&self, field: &str) -> Option<&TargetDef> {
        self.def.targets.iter().find(|t| t.due_field == field || t.state_field == field)
    }

    pub fn storage_of(&self, field: &str) -> Option<Storage> {
        self.storage.get(field).copied()
    }

    pub fn is_submittable(&self) -> bool {
        self.def.submittable
    }

    /// Fields that hold a value (everything but `table` fields).
    pub fn value_fields(&self) -> impl Iterator<Item = &FieldDef> {
        self.def.fields.iter().filter(|f| f.ftype.is_stored())
    }

    pub fn secret_fields(&self) -> impl Iterator<Item = &FieldDef> {
        self.def.fields.iter().filter(|f| f.ftype == FieldType::Secret)
    }

    pub fn table_fields(&self) -> impl Iterator<Item = &FieldDef> {
        self.def.fields.iter().filter(|f| f.ftype == FieldType::Table)
    }

    /// The entity's unique keys: each `unique: true` field on its own, then each `unique` group.
    pub fn unique_keys(&self) -> Vec<Vec<String>> {
        let mut keys: Vec<Vec<String>> = self.def.fields.iter().filter(|f| f.unique).map(|f| vec![f.name.clone()]).collect();
        for g in &self.def.unique {
            if !keys.contains(g) {
                keys.push(g.clone());
            }
        }
        keys
    }

    /// The document as CEL sees it: decimals become floats (comparison only; money math is native).
    pub fn cel_view(&self, doc: &Value, schema: &Schema) -> Value {
        let Value::Object(map) = doc else { return doc.clone() };
        let mut out = serde_json::Map::with_capacity(map.len());
        // Empty numbers and checks read as 0 / false, so `doc.qty > 0.0` works on fresh documents.
        let empty = |t: FieldType| match t {
            FieldType::Int => Some(Value::from(0)),
            FieldType::Check => Some(Value::Bool(false)),
            t if t.is_decimal() => Some(Value::from(0.0)),
            _ => None,
        };
        // Which of them were empty, for `isEmpty` and `coalesce` (expressions do not see this list).
        let mut left_empty = Vec::new();
        for (k, v) in map {
            let converted = match self.field(k) {
                Some(f) if v.is_null() && empty(f.ftype).is_some() => {
                    left_empty.push(Value::String(k.clone()));
                    empty(f.ftype).unwrap_or_default()
                }
                Some(f) if f.ftype.is_decimal() => match v {
                    Value::String(s) => {
                        Decimal::from_str(s).ok().and_then(|d| d.to_f64()).and_then(serde_json::Number::from_f64).map(Value::Number).unwrap_or(Value::Null)
                    }
                    other => other.clone(),
                },
                Some(f) if f.ftype == FieldType::Table => {
                    let child = f.of.as_deref().and_then(|c| schema.entities.get(c));
                    match (v, child) {
                        (Value::Array(rows), Some(c)) => Value::Array(rows.iter().map(|r| c.cel_view(r, schema)).collect()),
                        _ => v.clone(),
                    }
                }
                _ => v.clone(),
            };
            out.insert(k.clone(), converted);
        }
        // Expressions may reference any declared field even when the payload omits it.
        for f in self.value_fields() {
            if !out.contains_key(&f.name) {
                if empty(f.ftype).is_some() {
                    left_empty.push(Value::String(f.name.clone()));
                }
                out.insert(f.name.clone(), empty(f.ftype).unwrap_or(Value::Null));
            }
        }
        if !left_empty.is_empty() {
            out.insert(crate::expr::EMPTY_FIELDS.into(), Value::Array(left_empty));
        }
        for f in self.table_fields() {
            out.entry(f.name.clone()).or_insert_with(|| Value::Array(vec![]));
        }
        Value::Object(out)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaWarning {
    pub entity: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct Schema {
    pub version: i64,
    pub entities: BTreeMap<String, Arc<EffEntity>>,
    pub packages: Vec<String>,
    pub modules: Vec<ModuleDef>,
    pub roles: Vec<RoleDef>,
    /// Where the installed packages keep people's personal data.
    pub personal_data: Vec<PersonalDataDef>,
    /// Installed packages that declare no personal data, so requests do not search them.
    pub undeclared_personal_data: Vec<String>,
    pub handover: Vec<HandoverDef>,
    pub export_tables: Vec<String>,
    pub merge_links: Vec<MergeLinkDef>,
    pub warnings: Vec<SchemaWarning>,
    /// The tenant's fiscal year start month (1-12), for contexts that act without a user.
    pub fy_start_month: u32,
    /// The tenant's IANA time zone (`Asia/Kolkata`); "today" and "now" are local to it.
    pub timezone: String,
    /// The tenant's people use the shell and apps with interfaces of their own (docs/plan/apps-ui.md):
    /// every workspace since U4b, whatever its `shell_v2` setting says (kept for the release before).
    pub shell_v2: bool,
    /// The workspace's published lists (`publish_list`, BD-15), each checked against its entity.
    pub published_lists: Vec<PublishedList>,
    /// The installed packages' message templates, as they ship them (BD-20).
    pub messages: Vec<MessageDef>,
    /// The installed packages' document templates, as they ship them (BD-21).
    pub documents: Vec<DocumentDef>,
    /// Each installed package's direct dependencies (its manifest's `depends`): which package's default document wins.
    pub package_depends: BTreeMap<String, Vec<String>>,
}

impl Schema {
    /// Whether package `a` depends on package `b`, directly or through others.
    pub fn package_depends_on(&self, a: &str, b: &str) -> bool {
        let mut seen = std::collections::BTreeSet::new();
        let mut todo = vec![a];
        while let Some(p) = todo.pop() {
            for d in self.package_depends.get(p).into_iter().flatten() {
                if d == b {
                    return true;
                }
                if seen.insert(d.as_str()) {
                    todo.push(d);
                }
            }
        }
        false
    }

    /// What the schema gives a workspace, without who defines it: equal before and after
    /// entities or fields move between packages if nothing a workspace sees or stores changes
    /// (`ferrum task deps_report schema:…`). Roles are merged by name, as installing them does.
    pub fn canonical(&self) -> Value {
        let entities: BTreeMap<&str, Value> = self
            .entities
            .iter()
            .map(|(n, e)| {
                let v = serde_json::json!({
                    "def": e.def, "table": e.table, "custom": e.custom, "storage": e.storage, "children": e.children,
                    "workflow": e.workflow.as_ref().map(|w| &w.def), "print_template": e.print_template,
                });
                (n.as_str(), v)
            })
            .collect();
        let sorted = |mut v: Vec<Value>| {
            v.sort_by_key(|x| x.to_string());
            v
        };
        let mut roles: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
        for r in &self.roles {
            roles.entry(&r.name).or_default().extend(r.permissions.iter().map(|p| serde_json::to_value(p).unwrap_or_default()));
        }
        let roles: BTreeMap<&str, Vec<Value>> = roles.into_iter().map(|(k, v)| (k, sorted(v))).collect();
        let personal = sorted(self.personal_data.iter().map(|p| serde_json::to_value(p).unwrap_or_default()).collect());
        // What people get, whichever package defines a module.
        let modules: Vec<ModuleDef> = self.modules.iter().map(|m| ModuleDef { package: String::new(), ..m.clone() }).collect();
        serde_json::json!({"entities": entities, "modules": modules, "roles": roles, "personal_data": personal})
    }
}

/// An empty schema (for tests); build literals with `..Default::default()` so new fields don't
/// break callers in other repositories.
impl Default for Schema {
    fn default() -> Self {
        Schema {
            version: 0,
            entities: BTreeMap::new(),
            packages: vec![],
            modules: vec![],
            roles: vec![],
            personal_data: vec![],
            undeclared_personal_data: vec![],
            handover: vec![],
            export_tables: vec![],
            merge_links: vec![],
            warnings: vec![],
            fy_start_month: 1,
            timezone: "UTC".into(),
            shell_v2: false,
            published_lists: vec![],
            messages: vec![],
            documents: vec![],
            package_depends: BTreeMap::new(),
        }
    }
}

impl Schema {
    pub fn entity(&self, name: &str) -> Option<&Arc<EffEntity>> {
        self.entities.get(name)
    }

    /// What is wrong with the settings `e` reads as `settings.<Entity>.<field>` (BD-17), the first thing: each is a
    /// field of an entity of the settings kind with one record for the workspace, and not a secret.
    pub fn settings_problem(&self, e: &Expr) -> Option<String> {
        e.settings().iter().find_map(|(name, field)| {
            let what = format!("settings.{name}.{field}");
            let Some(ent) = self.entity(name) else { return Some(format!("{what}: there is no entity {name}")) };
            if ent.def.kind != EntityKind::Settings || ent.def.company_scoped {
                return Some(format!("{what}: {name} is not the workspace's settings (an entity of the settings kind, one record for the workspace)"));
            }
            match ent.field(field) {
                None => Some(format!("{what}: {} has no field {field}", ent.def.label)),
                Some(f) if f.ftype == FieldType::Secret => Some(format!("{what}: {field} is a secret, which no expression reads")),
                Some(_) => None,
            }
        })
    }

    /// The settings entities the expressions of `ent` and of its lines read: what its save loads (BD-17).
    pub fn settings_read(&self, ent: &EffEntity) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut next = vec![ent];
        while let Some(e) = next.pop() {
            let guards = e.workflow.iter().flat_map(|w| w.exprs()).chain(e.document_approvals.values().flat_map(|a| &a.exprs));
            out.extend(e.exprs.all().chain(guards).flat_map(|x| x.settings().iter().map(|(n, _)| n.clone())));
            next.extend(e.children.values().filter_map(|c| self.entity(&c.entity)).map(|c| &**c));
        }
        out
    }

    /// The published list named `name`, if the workspace has one.
    pub fn published_list(&self, name: &str) -> Option<&PublishedList> {
        self.published_lists.iter().find(|l| l.list.name == name)
    }
}

fn as_bool(v: &Value, prop: &str) -> Result<bool, String> {
    v.as_bool().ok_or_else(|| format!("{prop} must be a boolean"))
}

fn as_opt_string(v: &Value, prop: &str) -> Result<Option<String>, String> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) if s.trim().is_empty() => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        _ => Err(format!("{prop} must be a string")),
    }
}

/// What `op` replaces in `e` as it stands: `None` if it replaces nothing, or what it names is gone.
/// A workflow or print template replaces the packages' own, or their having none (`null`).
pub fn replaced_value(e: &EffEntity, op: &OverlayOp) -> Option<Value> {
    let def = &e.def;
    match op {
        OverlayOp::SetProperty { field, property, .. } => {
            let f = def.fields.iter().find(|f| &f.name == field)?;
            Some(serde_json::to_value(f).ok()?.get(property).cloned().unwrap_or(Value::Null))
        }
        OverlayOp::SetEntityProperty { property, .. } => Some(serde_json::to_value(def).ok()?.get(property).cloned().unwrap_or(Value::Null)),
        OverlayOp::SetWorkflow { .. } => Some(e.workflow.as_ref().and_then(|w| serde_json::to_value(&w.def).ok()).unwrap_or(Value::Null)),
        OverlayOp::SetPrintTemplate { .. } => Some(e.print_template.clone().map_or(Value::Null, Value::String)),
        OverlayOp::SetApproval { on, .. } => Some(e.document_approvals.get(on).and_then(|a| serde_json::to_value(&a.policy).ok()).unwrap_or(Value::Null)),
        OverlayOp::SetTransitionApproval { action, from, .. } => {
            let t = e.workflow.as_ref()?.def.transitions.iter().find(|t| &t.action == action && &t.from == from)?;
            Some(t.approval.as_ref().and_then(|p| serde_json::to_value(p).ok()).unwrap_or(Value::Null))
        }
        _ => None,
    }
}

/// Applies one overlay op to an entity definition. Returns the name of a field it added.
pub(crate) fn apply_op(def: &mut EntityDef, op: &OverlayOp, source: OverlaySource) -> Result<Option<String>, String> {
    match op {
        OverlayOp::AddAction { action } => {
            if source == OverlaySource::Tenant && action.effects.is_empty() {
                return Err(format!(
                    "action {}: a workspace's action says what it does (effects); one without effects needs code behind it, which only packages add",
                    action.name
                ));
            }
            match def.actions.iter().position(|a| a.name == action.name) {
                // The workspace's own, from an earlier customization (`build` refuses a package's): replaced where it stands.
                Some(i) if source == OverlaySource::Tenant => def.actions[i] = action.clone(),
                Some(_) => return Err(format!("action {} already exists", action.name)),
                None => def.actions.push(action.clone()),
            }
            Ok(None)
        }
        OverlayOp::RemoveAction { name } => {
            if source != OverlaySource::Tenant {
                return Err("this operation is only valid as a tenant customization".into());
            }
            let i = def.actions.iter().position(|a| &a.name == name).ok_or(format!("there is no action {name} to remove"))?;
            def.actions.remove(i);
            Ok(None)
        }
        OverlayOp::AddTarget { target } => {
            if source != OverlaySource::Tenant {
                return Err("this operation is only valid as a tenant customization".into());
            }
            match def.targets.iter().position(|t| t.name == target.name) {
                Some(i) => def.targets[i] = target.clone(),
                None => def.targets.push(target.clone()),
            }
            validate_targets(def)?;
            Ok(None)
        }
        OverlayOp::RemoveTarget { name } => {
            if source != OverlaySource::Tenant {
                return Err("this operation is only valid as a tenant customization".into());
            }
            let i = def.targets.iter().position(|t| &t.name == name).ok_or(format!("there is no target {name} to remove"))?;
            def.targets.remove(i);
            Ok(None)
        }
        OverlayOp::AddField { field, after, column } => {
            if def.fields.iter().any(|f| f.name == field.name) {
                return Err(format!("field {} already exists", field.name));
            }
            if *column && source == OverlaySource::Tenant {
                return Err("only packages add columns to an entity's table".into());
            }
            if field.ftype == FieldType::Secret && source == OverlaySource::Tenant {
                return Err("secret fields come with packages (they need code that uses them)".into());
            }
            if source == OverlaySource::Tenant && !field.name.starts_with("x_") {
                return Err(format!("custom field {} must start with 'x_'", field.name));
            }
            if field.unique && !*column {
                return Err("custom fields cannot be unique (use a validation rule)".into());
            }
            let pos = match after {
                Some(a) => def.fields.iter().position(|f| &f.name == a).map(|i| i + 1).ok_or(format!("unknown field {a}"))?,
                None => def.fields.len(),
            };
            def.fields.insert(pos, field.clone());
            // A column the entity's form does not place goes into the section of the field it
            // follows (§13); one the form places keeps that place.
            if *column
                && let Some(a) = after
                && !def.form.iter().any(|s| s.fields.contains(&field.name))
                && let Some(s) = def.form.iter_mut().find(|s| s.fields.contains(a))
            {
                let i = s.fields.iter().position(|f| f == a).map_or(s.fields.len(), |i| i + 1);
                s.fields.insert(i, field.name.clone());
            }
            Ok(Some(field.name.clone()))
        }
        OverlayOp::SetProperty { field, property, value } => {
            let f = def.fields.iter_mut().find(|f| &f.name == field).ok_or(format!("unknown field {field}"))?;
            match property.as_str() {
                "label" => f.label = as_opt_string(value, property)?,
                "description" => f.description = as_opt_string(value, property)?,
                "required" => {
                    let v = as_bool(value, property)?;
                    if f.locked && f.required && !v {
                        return Err(format!("{field} is locked as required"));
                    }
                    f.required = v;
                }
                "hidden" => {
                    let v = as_bool(value, property)?;
                    if v && f.required && f.default.is_none() && f.default_expr.is_none() {
                        return Err(format!("{field} is required without a default and cannot be hidden"));
                    }
                    f.hidden = v;
                }
                "read_only" => f.read_only = as_bool(value, property)?,
                "in_list" => f.in_list = as_bool(value, property)?,
                "in_filter" => f.in_filter = as_bool(value, property)?,
                "default" => f.default = if value.is_null() { None } else { Some(value.clone()) },
                "default_expr" => f.default_expr = as_opt_string(value, property)?,
                "visible_if" => f.visible_if = as_opt_string(value, property)?,
                "required_if" => f.required_if = as_opt_string(value, property)?,
                "read_only_if" => f.read_only_if = as_opt_string(value, property)?,
                "read_only_message" => f.read_only_message = as_opt_string(value, property)?,
                "max_length" => {
                    let v = value.as_u64().filter(|v| *v > 0 && *v <= 1_000_000).ok_or("max_length must be 1..1000000")? as u32;
                    if f.locked && f.max_length.is_some_and(|m| v > m) {
                        return Err(format!("{field} max_length is locked"));
                    }
                    f.max_length = Some(v);
                }
                "min" | "max" => {
                    let d = match value {
                        Value::Null => None,
                        Value::String(s) => Some(Decimal::from_str(s).map_err(|e| e.to_string())?),
                        Value::Number(n) => Some(Decimal::from_str(&n.to_string()).map_err(|e| e.to_string())?),
                        _ => return Err(format!("{property} must be a number")),
                    };
                    if f.locked {
                        return Err(format!("{field} bounds are locked"));
                    }
                    if property == "min" { f.min = d } else { f.max = d }
                }
                other => return Err(format!("property {other} cannot be customized")),
            }
            Ok(None)
        }
        OverlayOp::AddSection { section, after } => {
            if source == OverlaySource::Tenant {
                return Err("customizations change the form with the form layout".into());
            }
            if let Some(f) = section.fields.iter().find(|f| !def.fields.iter().any(|d| &d.name == *f)) {
                return Err(format!("section field {f} is not a field of the entity"));
            }
            for s in &mut def.form {
                s.fields.retain(|f| !section.fields.contains(f));
            }
            let pos = after.as_ref().and_then(|a| def.form.iter().position(|s| s.fields.contains(a))).map_or(def.form.len(), |i| i + 1);
            def.form.insert(pos, section.clone());
            Ok(None)
        }
        OverlayOp::AddRule { rule } => {
            if def.rules.iter().any(|r| r.name == rule.name) {
                return Err(format!("rule {} already exists", rule.name));
            }
            def.rules.push(rule.clone());
            Ok(None)
        }
        OverlayOp::AddOptions { field, options } => {
            let f = def.fields.iter_mut().find(|f| &f.name == field).ok_or(format!("unknown field {field}"))?;
            if f.ftype != FieldType::Select {
                return Err(format!("{field} is not a select field"));
            }
            for o in options {
                if o.trim().is_empty() {
                    return Err("empty option".into());
                }
                if !f.options.contains(o) {
                    f.options.push(o.clone());
                }
            }
            Ok(None)
        }
        OverlayOp::SetEntityProperty { property, value } => {
            match property.as_str() {
                "label" => def.label = as_opt_string(value, property)?.ok_or("label cannot be empty")?,
                "label_plural" => def.label_plural = as_opt_string(value, property)?,
                "description" => def.description = as_opt_string(value, property)?,
                "icon" => def.icon = as_opt_string(value, property)?,
                "title_field" => def.title_field = as_opt_string(value, property)?,
                "reference" => def.reference = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?,
                "list" => def.list = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?,
                "search_fields" => def.search_fields = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?,
                "form" => {
                    let form: Vec<SectionDef> = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
                    validate_form(def, &form)?;
                    def.form = form;
                }
                "numbering" => {
                    let n: crate::types::NumberingDef = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
                    if !n.template.contains("{seq") || n.template.chars().count() > 60 {
                        return Err("a number series needs {seq:<width>} in a template of at most 60 characters".into());
                    }
                    if let Some(f) = n.date_field.as_ref().filter(|f| !def.fields.iter().any(|d| &d.name == *f)) {
                        return Err(format!("the series' date field {f} is not a field of the entity"));
                    }
                    if def.numbering.as_ref().is_some_and(|o| o.gapless) && !n.gapless {
                        return Err("this series is gapless (as invoice numbers must be): keep gapless".into());
                    }
                    def.numbering = Some(n);
                }
                other => return Err(format!("entity property {other} cannot be customized")),
            }
            Ok(None)
        }
        OverlayOp::DefineEntity { .. }
        | OverlayOp::RedefineEntity { .. }
        | OverlayOp::SetWorkflow { .. }
        | OverlayOp::SetPrintTemplate { .. }
        | OverlayOp::SetApproval { .. }
        | OverlayOp::SetTransitionApproval { .. }
        | OverlayOp::DefineModule { .. }
        | OverlayOp::RemoveModule { .. }
        | OverlayOp::PublishList { .. }
        | OverlayOp::UnpublishList { .. } => Err("this operation is only valid as a tenant customization".into()),
    }
}

/// At most this many items in a workspace's menu.
pub const MAX_MENU_ITEMS: usize = 50;

/// What a workspace's own menu may say, before its items are looked up.
fn validate_menu(m: &ModuleDef) -> Result<(), String> {
    let n = &m.name;
    if !n.starts_with("x_") || n.len() < 3 || n.len() > 40 || !is_ident(n) {
        return Err(format!("menu {n}: its name is x_ then lowercase letters, digits and underscores, at most 40 in all"));
    }
    let long = |what: &str, s: &str, max: usize| if s.chars().count() > max { Err(format!("menu {n}: {what} is at most {max} characters")) } else { Ok(()) };
    if m.label.trim().is_empty() {
        return Err(format!("menu {n}: it needs a label"));
    }
    long("its label", &m.label, 60)?;
    long("its description", m.description.as_deref().unwrap_or_default(), 200)?;
    if m.icon.as_deref().is_some_and(|i| i.is_empty() || i.len() > 40 || !i.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')) {
        return Err(format!("menu {n}: an icon is a short name such as clipboard-list"));
    }
    if m.color.as_deref().is_some_and(|c| c.len() != 7 || !c.starts_with('#') || !c[1..].bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err(format!("menu {n}: a colour is #rrggbb"));
    }
    if m.items.len() > MAX_MENU_ITEMS {
        return Err(format!("menu {n}: at most {MAX_MENU_ITEMS} items"));
    }
    let mut seen = HashSet::new();
    for i in &m.items {
        let ok = match i.strip_prefix("report:") {
            Some(r) => !r.is_empty() && r.len() <= 100 && r.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b)),
            None => is_entity_name(i),
        };
        if !ok {
            return Err(format!("menu {n}: {i} is neither an entity's name nor report:<name>"));
        }
        if !seen.insert(i.as_str()) {
            return Err(format!("menu {n}: {i} is listed twice"));
        }
    }
    let mut offered = HashSet::new();
    for c in &m.create {
        if c.starts_with("report:") || !m.items.contains(c) || !offered.insert(c.as_str()) {
            return Err(format!("menu {n}: create offers {c}, which is not one of its entities (or is offered twice)"));
        }
    }
    if let Some(a) = &m.assistant {
        if a.instructions.trim().is_empty() {
            return Err(format!("menu {n}: the assistant's instructions are empty"));
        }
        long("the assistant's instructions", &a.instructions, 2000)?;
        if a.suggestions.len() > 6 {
            return Err(format!("menu {n}: at most 6 suggestions"));
        }
        for s in &a.suggestions {
            long("a suggestion", s, 200)?;
        }
    }
    Ok(())
}

struct Building {
    package: String,
    def: EntityDef,
    storage: BTreeMap<String, Storage>,
    custom: bool,
    workflow: Option<WorkflowDef>,
    print_template: Option<String>,
    approvals: BTreeMap<DocumentAction, ApprovalPolicy>,
}

fn validate_custom_entity(e: &EntityDef) -> Result<(), String> {
    if !is_entity_name(&e.name) || !e.name.starts_with('X') {
        return Err(format!("custom entity {} must be PascalCase and start with 'X'", e.name));
    }
    if !matches!(e.kind, EntityKind::Master | EntityKind::Document | EntityKind::Child | EntityKind::Settings) {
        return Err("custom entities must be masters, documents, settings or child (line) entities".into());
    }
    // A workspace's own settings are one record for the workspace (BD-17), read as `settings.<Entity>.<field>`.
    if e.kind == EntityKind::Settings && e.company_scoped {
        return Err(format!("{} holds the workspace's settings: one record for the workspace, not one per company", e.name));
    }
    if e.table.is_some() || !e.indexes.is_empty() || !e.exclusion.is_empty() {
        return Err("custom entities cannot declare tables, indexes or exclusions".into());
    }
    if e.kind == EntityKind::Child && (!e.unique.is_empty() || e.fields.iter().any(|f| f.unique)) {
        return Err(format!("{} is a child entity: its lines cannot have unique keys", e.name));
    }
    for f in &e.fields {
        if f.ftype == FieldType::Secret {
            return Err(format!("custom entity field {} cannot be a secret", f.name));
        }
    }
    Ok(())
}

/// A custom entity's lines, and those a customization adds to a package's entity (an `x_` table field), are rows
/// of a custom child entity, and each custom child entity is the lines of one table field only: its rows share
/// `custom_record`, where the child's name alone tells one table's rows from another's.
fn validate_custom_lines(name: &str, building: &BTreeMap<String, Building>) -> Result<(), String> {
    let Some(b) = building.get(name) else { return Ok(()) };
    for f in b.def.fields.iter().filter(|f| f.ftype == FieldType::Table && (b.custom || f.name.starts_with("x_"))) {
        let of = f.of.as_deref().unwrap_or_default();
        if !building.get(of).is_some_and(|c| c.custom && c.def.kind == EntityKind::Child) {
            return Err(format!("table field {}: the lines a workspace adds must be a custom child entity, and {of} is not one", f.name));
        }
        // A custom entity's table first: where one meets a package's entity's, the package's entity is the one that
        // falls back to its package's definition (a custom entity that fails goes from the schema altogether).
        let first = building
            .iter()
            .filter(|(_, o)| o.custom)
            .chain(building.iter().filter(|(_, o)| !o.custom))
            .flat_map(|(n, o)| o.def.fields.iter().filter(|g| g.ftype == FieldType::Table && g.of.as_deref() == Some(of)).map(move |g| (n, &g.name)))
            .next();
        if let Some((owner, table)) = first.filter(|(n, t)| (n.as_str(), t.as_str()) != (name, f.name.as_str())) {
            return Err(format!("table field {}: {of} is already the lines of {owner}.{table}; a child entity belongs to one table field", f.name));
        }
        // Lines within lines go two levels deep: a record, its lines, their lines.
        if b.def.kind == EntityKind::Child && building.get(of).is_some_and(|c| c.def.fields.iter().any(|g| g.ftype == FieldType::Table)) {
            return Err(format!("table field {}: lines go two levels deep (a record, its lines, their lines), and {of} has lines of its own", f.name));
        }
        // On a package's entity, one level: its lines live under a record of the package's table.
        if !b.custom && building.get(of).is_some_and(|c| c.def.fields.iter().any(|g| g.ftype == FieldType::Table)) {
            return Err(format!(
                "table field {}: the lines a workspace adds to {name}, which comes with a package, go one level deep, and {of} has lines of its own",
                f.name
            ));
        }
    }
    Ok(())
}

/// Whether `op`, the tenant's customization at `at`, is part of what a redefinition of its entity at `r`
/// replaced: a change to the definition made before it, an earlier redefinition included (a workflow and a print
/// template are not part of the definition, and the `define_entity` that made the entity is what lets it be
/// redefined at all). `build` applies none of these.
pub fn replaced_by_redefinition(op: &OverlayOp, at: usize, r: usize) -> bool {
    at < r
        && !matches!(
            op,
            OverlayOp::SetWorkflow { .. }
                | OverlayOp::SetPrintTemplate { .. }
                | OverlayOp::SetApproval { .. }
                | OverlayOp::SetTransitionApproval { .. }
                | OverlayOp::DefineEntity { .. }
                | OverlayOp::PublishList { .. }
                | OverlayOp::UnpublishList { .. }
        )
}

/// For each of the tenant's customizations (one operation each, as stored), the place of the later
/// redefinition of its entity that replaced it, if one did.
pub fn superseded(tenant: &[Overlay]) -> Vec<Option<usize>> {
    let entity = |ov: &Overlay| match ov.ops.first() {
        Some(OverlayOp::DefineEntity { entity } | OverlayOp::RedefineEntity { entity }) => entity.name.clone(),
        _ => ov.entity.clone(),
    };
    let latest: HashMap<String, usize> =
        tenant.iter().enumerate().filter(|(_, ov)| matches!(ov.ops.first(), Some(OverlayOp::RedefineEntity { .. }))).map(|(at, ov)| (entity(ov), at)).collect();
    tenant
        .iter()
        .enumerate()
        .map(|(at, ov)| latest.get(&entity(ov)).copied().filter(|r| ov.ops.iter().all(|op| replaced_by_redefinition(op, at, *r))))
        .collect()
}

/// Builds the effective schema for one tenant.
pub fn build(catalog: &Catalog, installed: &[String], tenant: &[Overlay], version: i64) -> Result<Schema, MetaError> {
    let packages = catalog.closure(installed)?;
    let set: HashSet<&str> = packages.iter().map(String::as_str).collect();
    let mut building: BTreeMap<String, Building> = BTreeMap::new();
    let mut modules = Vec::new();
    let mut roles = Vec::new();
    let mut personal_data = Vec::new();
    let mut undeclared_personal_data = Vec::new();
    let mut handover = Vec::new();
    let mut export_tables = Vec::new();
    let mut merge_links = Vec::new();
    let mut messages = Vec::new();
    let mut documents = Vec::new();
    let mut package_depends = BTreeMap::new();
    let mut warnings = Vec::new();

    for pkg in catalog.packages().filter(|p| set.contains(p.name())) {
        for e in &pkg.entities {
            let storage = e.fields.iter().filter(|f| f.ftype.is_stored()).map(|f| (f.name.clone(), Storage::Column)).collect();
            let print_template = pkg.print_formats.iter().find(|(n, _)| n == &e.name).map(|(_, t)| t.clone());
            building.insert(
                e.name.clone(),
                Building { package: pkg.name().into(), def: e.clone(), storage, custom: false, workflow: None, print_template, approvals: BTreeMap::new() },
            );
        }
        modules.extend(pkg.manifest.modules.iter().map(|m| ModuleDef { package: pkg.name().into(), ..m.clone() }));
        roles.extend(pkg.manifest.roles.iter().cloned());
        personal_data.extend(pkg.manifest.personal_data.iter().cloned());
        handover.extend(pkg.manifest.handover.iter().cloned());
        export_tables.extend(pkg.manifest.export_tables.iter().cloned());
        merge_links.extend(pkg.manifest.merge_links.iter().cloned());
        messages.extend(pkg.manifest.messages.iter().cloned());
        documents.extend(pkg.manifest.documents.iter().cloned());
        package_depends.insert(pkg.name().to_string(), pkg.manifest.depends.clone());
        if pkg.manifest.personal_data.is_empty() {
            undeclared_personal_data.push(pkg.manifest.package.clone());
        }
    }
    // A package may print an entity of a package it depends on its own way (India's tax invoice);
    // the last in dependency order wins.
    for pkg in catalog.packages().filter(|p| set.contains(p.name())) {
        for (name, template) in &pkg.print_formats {
            if let Some(b) = building.get_mut(name).filter(|b| b.package != pkg.name()) {
                b.print_template = Some(template.clone());
            }
        }
    }
    // Package overlays and workflows, in dependency order (validated by the catalog already).
    for pkg in catalog.packages().filter(|p| set.contains(p.name())) {
        for ov in &pkg.manifest.overlays {
            let Some(b) = building.get_mut(&ov.entity) else { continue };
            for op in &ov.ops {
                if let Some(added) = apply_op(&mut b.def, op, OverlaySource::Package).map_err(|m| MetaError::Invalid(format!("{}: {m}", ov.entity)))? {
                    match op {
                        OverlayOp::AddField { field, column: true, .. } => {
                            if field.ftype.is_stored() {
                                b.storage.insert(added, Storage::Column);
                            }
                        }
                        _ => {
                            b.storage.insert(added, Storage::Ext);
                        }
                    }
                }
            }
        }
        for wf in &pkg.manifest.workflows {
            if let Some(b) = building.get_mut(&wf.entity) {
                b.workflow = Some(wf.clone());
            }
        }
        // Its approvals on its documents: defaults the workspace's `set_approval` comes over.
        for a in &pkg.manifest.approvals {
            if let Some(b) = building.get_mut(&a.entity) {
                b.approvals.insert(a.on.clone(), a.policy.clone());
            }
        }
    }

    // An entity's layout (form and list) may place the fields other packages add to it, such as
    // the columns they take over (§13); where such a package is not installed its fields drop
    // out, and so do sections left without fields. (`Catalog::unknown_form_fields` finds names
    // nothing adds.)
    for b in building.values_mut() {
        let known: HashSet<&str> = b.def.fields.iter().map(|f| f.name.as_str()).chain(crate::RESERVED_FIELDS.iter().copied()).collect();
        let keep = |f: &String| known.contains(f.as_str());
        b.def.list.columns.retain(|c| keep(c));
        if b.def.form.iter().any(|s| s.fields.iter().any(|f| !keep(f))) {
            let keep: Vec<Vec<String>> = b.def.form.iter().map(|s| s.fields.iter().filter(|f| keep(f)).cloned().collect()).collect();
            let mut sections = std::mem::take(&mut b.def.form).into_iter().zip(keep).collect::<Vec<_>>();
            sections.retain(|(s, kept)| !kept.is_empty() || s.fields.is_empty());
            b.def.form = sections
                .into_iter()
                .map(|(mut s, kept)| {
                    s.fields = kept;
                    s
                })
                .collect();
        }
    }

    // What a tenant-broken entity falls back to: its packages' definition, overlays included.
    let packaged: HashMap<String, (EntityDef, BTreeMap<String, Storage>)> =
        building.iter().map(|(k, b)| (k.clone(), (b.def.clone(), b.storage.clone()))).collect();

    // Tenant: new entities first, so other customizations can target them; a redefinition replaces
    // what the definitions before it gave, in order.
    let mut redefined: HashMap<&str, usize> = HashMap::new();
    for (at, ov) in tenant.iter().enumerate() {
        for op in &ov.ops {
            match op {
                OverlayOp::DefineEntity { entity } => {
                    let res = validate_custom_entity(entity)
                        .and_then(|_| if building.contains_key(&entity.name) { Err(format!("entity {} already exists", entity.name)) } else { Ok(()) });
                    match res {
                        Ok(()) => {
                            let storage = entity.fields.iter().map(|f| (f.name.clone(), Storage::Ext)).collect();
                            building.insert(
                                entity.name.clone(),
                                Building {
                                    package: "tenant".into(),
                                    def: (**entity).clone(),
                                    storage,
                                    custom: true,
                                    workflow: None,
                                    print_template: None,
                                    approvals: BTreeMap::new(),
                                },
                            );
                        }
                        Err(m) => warnings.push(SchemaWarning { entity: entity.name.clone(), message: m }),
                    }
                }
                OverlayOp::RedefineEntity { entity } => {
                    let res = match building.get_mut(&entity.name) {
                        Some(b) if b.custom => validate_custom_entity(entity).map(|_| b),
                        Some(_) => Err(format!("{} comes with a package: only the workspace's own entities can be redefined", entity.name)),
                        None => Err(format!("there is no custom entity {} to redefine (define_entity makes a new one)", entity.name)),
                    };
                    match res {
                        Ok(b) => {
                            b.def = (**entity).clone();
                            b.storage = entity.fields.iter().map(|f| (f.name.clone(), Storage::Ext)).collect();
                            redefined.insert(entity.name.as_str(), at);
                        }
                        Err(m) => warnings.push(SchemaWarning { entity: entity.name.clone(), message: m }),
                    }
                }
                _ => {}
            }
        }
    }
    // The workspace's own menus, in the order they were first defined; their items are looked up once
    // the entities are settled.
    let mut menus: Vec<ModuleDef> = Vec::new();
    // The workspace's published lists, by name across the workspace; checked against their entities once
    // those are settled.
    let mut lists: Vec<PublishedList> = Vec::new();
    for (at, ov) in tenant.iter().enumerate() {
        for op in &ov.ops {
            if matches!(op, OverlayOp::DefineEntity { .. } | OverlayOp::RedefineEntity { .. }) {
                continue;
            }
            let menu = match op {
                // Never a package's module: those take no x_ names (the catalog's check).
                OverlayOp::DefineModule { module } => Some(validate_menu(module).map(|_| match menus.iter().position(|m| m.name == module.name) {
                    // As a workspace's action: replaced where it stands.
                    Some(i) => menus[i] = ModuleDef { package: "tenant".into(), ..module.clone() },
                    None => menus.push(ModuleDef { package: "tenant".into(), ..module.clone() }),
                })),
                OverlayOp::RemoveModule { name } => Some(match menus.iter().position(|m| &m.name == name) {
                    Some(i) => {
                        menus.remove(i);
                        Ok(())
                    }
                    None => Err(format!("there is no menu {name} of this workspace to remove")),
                }),
                OverlayOp::PublishList { list } => Some(crate::published::collect(&mut lists, &ov.entity, list)),
                OverlayOp::UnpublishList { name } => Some(match lists.iter().position(|l| &l.list.name == name && l.entity == ov.entity) {
                    Some(i) => {
                        lists.remove(i);
                        Ok(())
                    }
                    None => Err(format!("there is no published list {name} of {} to take down", ov.entity)),
                }),
                _ => None,
            };
            if let Some(res) = menu {
                if let Err(m) = res {
                    warnings.push(SchemaWarning { entity: ov.entity.clone(), message: m });
                }
                continue;
            }
            // A change to the definition made before its latest redefinition is part of what that
            // replaced (its workflow and print template are not part of the definition).
            if redefined.get(ov.entity.as_str()).is_some_and(|r| replaced_by_redefinition(op, at, *r)) {
                continue;
            }
            let Some(b) = building.get_mut(&ov.entity) else {
                warnings.push(SchemaWarning { entity: ov.entity.clone(), message: "customization targets an entity that is not installed".into() });
                continue;
            };
            let snapshot = (b.def.clone(), b.storage.clone());
            let res = match op {
                OverlayOp::SetWorkflow { workflow } => {
                    if workflow.entity != ov.entity {
                        Err("workflow entity mismatch".to_string())
                    } else {
                        b.workflow = Some(workflow.clone());
                        Ok(None)
                    }
                }
                OverlayOp::SetApproval { on, approval } => {
                    match approval {
                        Some(p) => b.approvals.insert(on.clone(), p.clone()),
                        None => b.approvals.remove(on),
                    };
                    Ok(None)
                }
                OverlayOp::SetTransitionApproval { action, from, approval } => {
                    match b.workflow.as_mut().and_then(|w| w.transitions.iter_mut().find(|t| &t.action == action && &t.from == from)) {
                        Some(t) => {
                            t.approval = approval.clone();
                            Ok(None)
                        }
                        None => Err(format!("its workflow has no transition {action} from {from}")),
                    }
                }
                OverlayOp::SetPrintTemplate { template } => {
                    if template.len() > 512 * 1024 {
                        Err("print template larger than 512 KiB".into())
                    } else {
                        b.print_template = Some(template.clone());
                        Ok(None)
                    }
                }
                // A package's action stays as its package says: the workspace replaces or removes only its own.
                OverlayOp::AddAction { action: ActionDef { name, .. } } | OverlayOp::RemoveAction { name }
                    if packaged.get(&ov.entity).is_some_and(|(d, _)| d.actions.iter().any(|a| &a.name == name)) =>
                {
                    Err(format!("action {name} comes with the package of {}: a customization does not replace or remove it", ov.entity))
                }
                // A packaged entity's actions are its packages' names; the workspace's own start with x_.
                OverlayOp::AddAction { action } if !b.custom && !action.name.starts_with("x_") => {
                    Err(format!("action {}: an action added to {}, which comes with a package, is named x_…", action.name, ov.entity))
                }
                // A target keeps fields the workspace added, never a package's own.
                OverlayOp::AddTarget { target: t } if !b.custom && ![&t.due_field, &t.state_field].iter().all(|f| f.starts_with("x_")) => {
                    Err(format!("target {}: {} comes with a package, so the target keeps fields the workspace added (x_…)", t.name, ov.entity))
                }
                // Lines a customization adds go on a package's records, one level (BD-06): a custom entity's come with its
                // definition, and a package's lines or ledger take none.
                OverlayOp::AddField { field, .. } if field.ftype == FieldType::Table && b.custom => {
                    Err(format!("table field {}: a custom entity's tables come with its definition (redefine_entity)", field.name))
                }
                OverlayOp::AddField { field, .. } if field.ftype == FieldType::Table && matches!(b.def.kind, EntityKind::Child | EntityKind::Ledger) => {
                    Err(format!("table field {}: {} is the lines of another entity or a ledger, and takes no lines", field.name, ov.entity))
                }
                _ => apply_op(&mut b.def, op, OverlaySource::Tenant),
            };
            match res {
                Ok(Some(added)) => {
                    b.storage.insert(added, Storage::Ext);
                }
                Ok(None) => {}
                Err(m) => {
                    (b.def, b.storage) = snapshot;
                    warnings.push(SchemaWarning { entity: ov.entity.clone(), message: m });
                }
            }
        }
    }

    // Validate merged entities; a tenant-broken entity falls back to its uncustomized definition.
    let defs_snapshot: HashMap<String, EntityDef> = building.iter().map(|(k, b)| (k.clone(), b.def.clone())).collect();
    let all: HashMap<&str, &EntityDef> = defs_snapshot.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let mut broken = Vec::new();
    for (name, b) in &building {
        if let Err(m) = validate_entity(&b.def, &all, b.custom).and_then(|_| validate_custom_lines(name, &building)) {
            if b.custom {
                broken.push(name.clone());
            } else if let Some((base, _)) = packaged.get(name) {
                if validate_entity(base, &all, false).is_err() {
                    return invalid(format!("{name}: {m}"));
                }
                broken.push(name.clone());
            } else {
                return invalid(format!("{name}: {m}"));
            }
            warnings.push(SchemaWarning { entity: name.clone(), message: format!("customizations ignored: {m}") });
        }
    }
    for name in broken {
        if let Some(b) = building.get_mut(&name) {
            if b.custom {
                building.remove(&name);
            } else if let Some((def, storage)) = packaged.get(&name) {
                b.def = def.clone();
                b.storage = storage.clone();
            }
        }
    }
    // A menu names entities the workspace has (not lines); one that went (its app uninstalled) drops
    // out of the menu, which keeps the rest.
    for mut m in menus {
        m.items.retain(|i| {
            let gone = match building.get(i) {
                _ if i.starts_with("report:") => None,
                Some(b) if b.def.kind == EntityKind::Child => Some("is the lines of another entity"),
                Some(_) => None,
                None => Some("is not an entity here"),
            };
            if let Some(why) = gone {
                warnings.push(SchemaWarning { entity: m.name.clone(), message: format!("menu {}: {i} {why}, so it is left out", m.name) });
            }
            gone.is_none()
        });
        let items = m.items.clone();
        m.create.retain(|c| items.contains(c));
        modules.push(m);
    }

    // Workflows: validate against the merged entities; drop invalid tenant ones with a warning.
    let entity_map: BTreeMap<String, (String, EntityDef)> = building.iter().map(|(k, b)| (k.clone(), (b.package.clone(), b.def.clone()))).collect();
    for (name, b) in building.iter_mut() {
        if let Some(wf) = &b.workflow
            && let Err(m) = validate_workflow(wf, &entity_map)
        {
            warnings.push(SchemaWarning { entity: name.clone(), message: format!("workflow ignored: {m}") });
            b.workflow = None;
        }
        // Approval before a document is submitted or cancelled: on a submittable entity no workflow submits.
        let mut ignored = Vec::new();
        for (on, p) in &b.approvals {
            let what = format!("{}: approval", on.key());
            let checked = if let DocumentAction::Action(name) = on {
                action_approval_problem(&b.def, name).map_or_else(|| crate::catalog::validate_approval(&b.def, &what, None, p), |m| Err(format!("{what}: {m}")))
            } else if !b.def.submittable {
                Err(format!("{what}: {} is not submittable", b.def.label))
            } else if b.workflow.is_some() {
                Err(format!("{what}: its workflow submits and cancels it, so the approval goes on the workflow's transition"))
            } else {
                crate::catalog::validate_approval(&b.def, &what, None, p)
            };
            if let Err(m) = checked {
                warnings.push(SchemaWarning { entity: name.clone(), message: format!("approval ignored: {m}") });
                ignored.push(on.clone());
            }
        }
        for on in ignored {
            b.approvals.remove(&on);
        }
    }

    // Each published list as its entity now stands: one that cannot stand goes, and a field that may no
    // longer be published drops out of it (each with a warning; a new one with any is refused).
    let declared = |e: &str| personal_data.iter().filter(|d| d.entity == e).collect::<Vec<_>>();
    let lookup = |e: &str| building.get(e).map(|b| (&b.def, b.custom, declared(e)));
    let published_lists: Vec<PublishedList> = lists
        .into_iter()
        .filter_map(|l| {
            let (kept, dropped) = crate::published::settle(&l, &lookup);
            warnings.extend(dropped.into_iter().map(|m| SchemaWarning { entity: l.entity.clone(), message: format!("published list {}: {m}", l.list.name) }));
            kept
        })
        .collect();

    let custom: HashSet<String> = building.iter().filter(|(_, b)| b.custom).map(|(n, _)| n.clone()).collect();
    let mut entities = BTreeMap::new();
    for (name, b) in building {
        let table = if b.def.kind == EntityKind::Child {
            String::new()
        } else if b.custom {
            CUSTOM_RECORD_TABLE.to_string()
        } else {
            b.def.table.clone().unwrap_or_else(|| table_for(&name))
        };
        let children = b
            .def
            .fields
            .iter()
            .filter(|f| f.ftype == FieldType::Table)
            .map(|f| {
                // A custom child entity's rows are custom records (`ChildInfo::predicate`), under a custom record or,
                // added by a customization, a package's record.
                let rows = if f.of.as_deref().is_some_and(|c| custom.contains(c)) { CUSTOM_RECORD_TABLE.to_string() } else { child_table(&table, &f.name) };
                (f.name.clone(), ChildInfo { entity: f.of.clone().unwrap_or_default(), table: rows })
            })
            .collect();
        let exprs = compile_exprs(&b.def).map_err(|m| MetaError::Invalid(format!("{name}: {m}")))?;
        let workflow = match b.workflow {
            Some(def) => {
                let mut guards = BTreeMap::new();
                let mut approvals = Vec::new();
                for t in &def.transitions {
                    if let Some(c) = &t.condition {
                        guards.insert((t.action.clone(), t.from.clone()), Expr::compile(c).map_err(MetaError::Invalid)?);
                    }
                    if let Some(p) = &t.approval {
                        approvals.extend(policy_exprs(p)?);
                    }
                }
                Some(CompiledWorkflow { def, guards, approvals })
            }
            None => None,
        };
        entities.insert(
            name,
            Arc::new(EffEntity {
                def: b.def,
                package: b.package,
                table,
                custom: b.custom,
                storage: b.storage,
                children,
                exprs,
                workflow,
                print_template: b.print_template,
                document_approvals: b
                    .approvals
                    .into_iter()
                    .map(|(on, policy)| Ok((on, DocumentApproval { exprs: policy_exprs(&policy)?, policy })))
                    .collect::<Result<_, MetaError>>()?,
            }),
        );
    }
    modules.sort_by(|a, b| a.order.cmp(&b.order).then(a.name.cmp(&b.name)));
    let mut schema = Schema {
        version,
        entities,
        packages,
        modules,
        roles,
        personal_data,
        undeclared_personal_data,
        handover,
        export_tables,
        merge_links,
        warnings,
        fy_start_month: 1,
        timezone: "UTC".into(),
        shell_v2: false,
        published_lists,
        messages,
        documents,
        package_depends,
    };
    // What expressions read as `settings` is there (BD-17); otherwise it is said, and a customization saying it refused.
    let wrong: Vec<SchemaWarning> = schema
        .entities
        .values()
        .flat_map(|e| {
            let approvals = e.document_approvals.values().flat_map(|a| &a.exprs);
            e.exprs.all().chain(e.workflow.iter().flat_map(|w| w.exprs())).chain(approvals).map(move |x| (e, x))
        })
        .filter_map(|(e, x)| schema.settings_problem(x).map(|m| SchemaWarning { entity: e.def.name.clone(), message: m }))
        .collect();
    schema.warnings.extend(wrong);
    Ok(schema)
}

fn compile_exprs(def: &EntityDef) -> Result<Exprs, String> {
    let mut x = Exprs::default();
    let c = |s: &str| Expr::compile(s);
    for f in &def.fields {
        if let Some(s) = &f.default_expr {
            x.default_expr.insert(f.name.clone(), c(s)?);
        }
        if let Some(s) = &f.computed {
            x.computed.insert(f.name.clone(), c(s)?);
        }
        if let Some(s) = &f.visible_if {
            x.visible_if.insert(f.name.clone(), c(s)?);
        }
        if let Some(s) = &f.required_if {
            x.required_if.insert(f.name.clone(), c(s)?);
        }
        if let Some(s) = &f.read_only_if {
            x.read_only_if.insert(f.name.clone(), c(s)?);
        }
    }
    x.computed_order = computed_order(&x.computed)?;
    for r in &def.rules {
        if let Some(f) = r.field.as_deref().filter(|f| !def.fields.iter().any(|d| &d.name == f)) {
            return Err(format!("rule {}: {f} is not a field of {}", r.name, def.name));
        }
        x.rules.push((r.clone(), c(&r.check)?));
    }
    for a in &def.actions {
        if let Some(s) = &a.condition {
            x.action_conditions.insert(a.name.clone(), c(s)?);
        }
        if !a.refuse_if.is_empty() {
            let refusals = a.refuse_if.iter().map(|r| Ok((c(&r.when)?, r.message.clone()))).collect::<Result<Vec<_>, String>>()?;
            x.action_refusals.insert(a.name.clone(), refusals);
        }
    }
    for t in &def.targets {
        let opt = |s: &Option<String>| s.as_deref().map(c).transpose();
        let exprs = TargetExprs { start_when: opt(&t.start_when)?, within: c(&t.within)?, pause_when: opt(&t.pause_when)?, met_when: c(&t.met_when)? };
        x.targets.insert(t.name.clone(), exprs);
    }
    Ok(x)
}

/// Computed fields in the order they are worked out: each after the computed fields it reads, by name where
/// nothing decides (as before, when name order was the only order); a field reading itself reads its stored value.
/// Fields that read each other in a circle are refused.
fn computed_order(computed: &BTreeMap<String, Expr>) -> Result<Vec<String>, String> {
    let deps: BTreeMap<&str, Vec<String>> =
        computed.iter().map(|(n, e)| (n.as_str(), e.doc_fields().into_iter().filter(|d| d != n && computed.contains_key(d)).collect())).collect();
    let mut order: Vec<String> = Vec::with_capacity(computed.len());
    while order.len() < computed.len() {
        let next = deps.iter().find(|(n, d)| !order.iter().any(|o| o == *n) && d.iter().all(|d| order.contains(d)));
        let Some((n, _)) = next else {
            let left: Vec<&str> = deps.keys().copied().filter(|n| !order.iter().any(|o| o == n)).collect();
            return Err(format!("computed fields read each other in a circle: {}", left.join(", ")));
        };
        order.push(n.to_string());
    }
    Ok(order)
}

/// Service targets an entity holds at most.
pub const MAX_TARGETS: usize = 8;

/// An entity's service targets: on an entity whose records are saved on their own (not lines, not a ledger), at
/// most [`MAX_TARGETS`], each keeping a datetime field and a data or select field (one offering the five words)
/// that no other target keeps and nothing else works out, with expressions that compile.
pub(crate) fn validate_targets(def: &EntityDef) -> Result<(), String> {
    if def.targets.is_empty() {
        return Ok(());
    }
    if matches!(def.kind, EntityKind::Child | EntityKind::Ledger) {
        return Err(format!("{} holds lines or ledger rows: a target is set on an entity whose records are saved on their own", def.name));
    }
    if def.targets.len() > MAX_TARGETS {
        return Err(format!("at most {MAX_TARGETS} targets on one entity"));
    }
    let mut kept = HashSet::new();
    let mut names = HashSet::new();
    for t in &def.targets {
        let at = |m: String| format!("target {}: {m}", t.name);
        if !crate::is_ident(&t.name) || !names.insert(t.name.as_str()) {
            return Err(at("a target's name is lower_snake_case and one of a kind on the entity".into()));
        }
        if t.label.trim().is_empty() {
            return Err(at("a target needs a label".into()));
        }
        if let Some(c) = &t.calendar
            && !(c.len() <= 64
                && c.starts_with(|x: char| x.is_ascii_lowercase())
                && c.chars().all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '_'))
        {
            return Err(at(format!("{c} is not a calendar's name")));
        }
        for (role, name) in [("due_field", &t.due_field), ("state_field", &t.state_field)] {
            let f = def.fields.iter().find(|f| &f.name == name).ok_or_else(|| at(format!("{role} {name} is not a field of {}", def.name)))?;
            let (fits, what) = match role {
                "due_field" => (f.ftype == FieldType::Datetime, "a date-time field"),
                _ => (matches!(f.ftype, FieldType::Data | FieldType::Select), "a data or select field"),
            };
            if !fits {
                return Err(at(format!("{role} {name} must be {what}")));
            }
            if f.computed.is_some() || f.rollup.is_some() || f.fetch_from.is_some() {
                return Err(at(format!("{role} {name} is worked out already: a target keeps a field of its own")));
            }
            if f.required {
                return Err(at(format!("{role} {name} is required: a field a target keeps is empty while the record has no such target")));
            }
            if !kept.insert(name.as_str()) {
                return Err(at(format!("{name} is kept by another target (or twice by this one)")));
            }
            if f.ftype == FieldType::Select
                && let Some(w) = crate::TARGET_STATES.iter().find(|w| !f.options.iter().any(|o| o == *w))
            {
                return Err(at(format!("{role} {name} offers no {w}: a select field it keeps offers {}", crate::TARGET_STATES.join(", "))));
            }
        }
        for (what, src) in [
            ("start_when", t.start_when.as_deref()),
            ("within", Some(t.within.as_str())),
            ("pause_when", t.pause_when.as_deref()),
            ("met_when", Some(t.met_when.as_str())),
        ] {
            crate::catalog::compile(def, &at(what.into()), src, true)?;
        }
    }
    Ok(())
}

/// A form layout may only place existing fields, each once, in at most 4 columns per section,
/// with valid section conditions.
pub(crate) fn validate_form(def: &EntityDef, form: &[SectionDef]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for s in form {
        if s.columns.is_some_and(|c| !(1..=4).contains(&c)) {
            return Err("a section has 1 to 4 columns".into());
        }
        crate::catalog::compile(def, &format!("section condition '{}'", s.visible_if.as_deref().unwrap_or_default()), s.visible_if.as_deref(), false)?;
        for f in &s.fields {
            if !def.fields.iter().any(|x| &x.name == f) {
                return Err(format!("the layout places unknown field {f}"));
            }
            if !seen.insert(f.as_str()) {
                return Err(format!("the layout places {f} twice"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalog() -> Catalog {
        let entities: Vec<EntityDef> = serde_yaml::from_str(
            r#"
- name: Company
  label: Company
  kind: master
  fields: [{name: company_name, type: data, required: true}]
- name: Customer
  label: Customer
  kind: master
  fields:
    - {name: customer_name, type: data, required: true, locked: true}
    - {name: tier, type: select, options: [Gold, Silver]}
    - {name: credit_limit, type: currency}
"#,
        )
        .unwrap();
        Catalog::from_packages(vec![Package {
            manifest: serde_yaml::from_str("{package: core, version: '1'}").unwrap(),
            entities,
            sql: vec![],
            print_formats: vec![],
        }])
        .unwrap()
    }

    fn ov(entity: &str, ops: serde_json::Value) -> Overlay {
        Overlay { entity: entity.into(), ops: serde_json::from_value(ops).unwrap() }
    }

    fn package(yaml: &str, entities: &str) -> Package {
        Package { manifest: serde_yaml::from_str(yaml).unwrap(), entities: serde_yaml::from_str(entities).unwrap(), sql: vec![], print_formats: vec![] }
    }

    /// A field another package takes over (§13) keeps its column and its place in the form, and
    /// is left out where that package is not installed.
    #[test]
    fn columns_taken_over_by_another_package() {
        let parties = package(
            "{package: parties, version: '1'}",
            "[{name: Customer, label: Customer, kind: master, fields: [{name: customer_name, type: data}, {name: phone, type: phone}],
               form: [{label: Customer, fields: [customer_name, phone]}, {label: Accounts, fields: [receivable_account]}]},
              {name: Account, label: Account, kind: master, fields: [{name: account_name, type: data}]}]",
        );
        let accounting = |after: &str| {
            package(
                &format!(
                    "{{package: accounting, version: '1', depends: [parties], overlays: [{{entity: Customer, ops: [{{op: add_field, column: true, after: {after}, field: {{name: receivable_account, type: link, to: Account}}}}]}}]}}"
                ),
                "[]",
            )
        };
        let catalog = Catalog::from_packages(vec![parties.clone(), accounting("customer_name")]).unwrap();
        let alone = build(&catalog, &["parties".into()], &[], 1).unwrap();
        let c = alone.entity("Customer").unwrap();
        assert!(c.field("receivable_account").is_none());
        assert_eq!(c.def.form.len(), 1, "the section left empty drops out");
        let both = build(&catalog, &["accounting".into()], &[], 1).unwrap();
        let c = both.entity("Customer").unwrap();
        assert_eq!(c.def.fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["customer_name", "receivable_account", "phone"]);
        assert_eq!(c.storage_of("receivable_account"), Some(Storage::Column));
        assert_eq!(c.def.form[1].fields, ["receivable_account"]);
        // A customization cannot add a column, and a name no package adds is still an error.
        let w = build(
            &catalog,
            &["accounting".into()],
            &[ov("Customer", json!([{"op": "add_field", "column": true, "field": {"name": "x_col", "type": "data"}}]))],
            2,
        )
        .unwrap();
        assert!(w.warnings.iter().any(|w| w.message.contains("only packages add columns")));
        assert!(catalog.unknown_form_fields().is_empty());
        // A column the form does not place goes into the section of the field it follows.
        let crm = package(
            "{package: crm, version: '1', depends: [accounting], overlays: [{entity: Customer, ops: [{op: add_field, column: true, after: receivable_account, field: {name: lead, type: data}}]}]}",
            "[]",
        );
        let catalog = Catalog::from_packages(vec![parties.clone(), accounting("customer_name"), crm]).unwrap();
        let c = build(&catalog, &["crm".into()], &[], 1).unwrap();
        assert_eq!(c.entity("Customer").unwrap().def.form[1].fields, ["receivable_account", "lead"]);
        let mut typo = parties.clone();
        typo.entities[0].form[0].fields.push("phnoe".into());
        assert_eq!(Catalog::from_packages(vec![typo, accounting("customer_name")]).unwrap().unknown_form_fields(), ["Customer.phnoe"]);
        // Two packages may not add the same field.
        let other = package(
            "{package: other, version: '1', depends: [parties], field_prefix: o_, overlays: [{entity: Customer, ops: [{op: add_field, column: true, field: {name: receivable_account, type: data}}]}]}",
            "[]",
        );
        let e = Catalog::from_packages(vec![parties.clone(), accounting("customer_name"), other]).unwrap_err().to_string();
        assert!(e.contains("too"), "{e}");
    }

    #[test]
    fn only_packages_add_actions_without_effects() {
        let mut def: EntityDef = serde_yaml::from_str("{name: Project, label: Project, kind: master, fields: [{name: title, type: data}]}").unwrap();
        let op: OverlayOp =
            serde_json::from_value(json!({"op": "add_action", "action": {"name": "make_sales_invoice", "label": "Make Sales Invoice"}})).unwrap();
        assert!(apply_op(&mut def.clone(), &op, OverlaySource::Tenant).is_err(), "actions without effects need code");
        apply_op(&mut def, &op, OverlaySource::Package).unwrap();
        assert_eq!(def.actions[0].permission, "write");
        assert!(apply_op(&mut def, &op, OverlaySource::Package).is_err(), "no duplicates");
        let configured: OverlayOp = serde_json::from_value(json!({"op": "add_action", "action": {"name": "x_close", "label": "Close",
            "roles": ["Manager"], "refuse_if": [{"when": "doc.title == ''", "message": "a title first"}],
            "effects": [{"type": "set_fields", "values": {"title": "'closed'"}}]}}))
        .unwrap();
        apply_op(&mut def, &configured, OverlaySource::Tenant).expect("an action with effects is the workspace's to add");
        assert_eq!(compile_exprs(&def).unwrap().action_refusals["x_close"][0].1, "a title first");
    }

    #[test]
    fn configured_actions_are_named_and_bounded() {
        let cat = catalog();
        let action = |name: &str, effects: usize| json!({"op": "add_action", "action": {"name": name, "label": "A", "effects": vec![json!({"type": "notify", "to": "owner", "title": "'t'"}); effects]}});
        let warned = |entity: &str, ops: Vec<Value>| {
            let tenant: Vec<Overlay> = ops.into_iter().map(|o| ov(entity, json!([o]))).collect();
            let defines = ov(
                "XThing",
                json!([{"op": "define_entity", "entity": {"name": "XThing", "label": "Thing", "kind": "master", "fields": [{"name": "a", "type": "data"}]}}]),
            );
            build(&cat, &["core".into()], &[vec![defines], tenant].concat(), 1).unwrap().warnings.iter().map(|w| w.message.clone()).collect::<Vec<_>>()
        };
        assert!(warned("Customer", vec![action("close", 1)])[0].contains("is named x_"), "a packaged entity's");
        assert!(warned("Customer", vec![action("x_close", 1)]).is_empty());
        assert!(warned("XThing", vec![action("close", 1)]).is_empty(), "a custom entity's are all the workspace's");
        assert!(warned("XThing", vec![action("close", 11)])[0].contains("at most 10 effects"));
        assert!(warned("XThing", (0..21).map(|i| action(&format!("a{i}"), 1)).collect())[0].contains("at most 20"));
        let ro = json!({"op": "add_action", "action": {"name": "peek", "label": "P", "read_only": true, "effects": [{"type": "notify", "to": "owner", "title": "'t'"}]}});
        assert!(warned("XThing", vec![ro])[0].contains("read-only"));
        let bad = json!({"op": "add_action", "action": {"name": "x", "label": "X", "refuse_if": [{"when": "doc.a ==", "message": "m"}], "effects": [{"type": "notify", "to": "owner", "title": "'t'"}]}});
        assert!(warned("XThing", vec![bad])[0].contains("refuse_if"));
    }

    #[test]
    fn workspace_actions_are_replaced_and_removed() {
        let cat = Catalog::from_packages(vec![package(
            "{package: core, version: '1'}",
            "[{name: Customer, label: Customer, kind: master, fields: [{name: n, type: data}], actions: [{name: block, label: Block}]}]",
        )])
        .unwrap();
        let act = |name: &str, label: &str| json!({"op": "add_action", "action": {"name": name, "label": label, "effects": [{"type": "notify", "to": "owner", "title": "'t'"}]}});
        let remove = |name: &str| json!({"op": "remove_action", "name": name});
        let built = |ops: Vec<Value>| build(&cat, &["core".into()], &ops.into_iter().map(|o| ov("Customer", json!([o]))).collect::<Vec<_>>(), 1).unwrap();
        let actions = |s: &Schema| s.entity("Customer").unwrap().def.actions.iter().map(|a| format!("{}:{}", a.name, a.label)).collect::<Vec<_>>();
        let s = built(vec![act("x_a", "A"), act("x_b", "B"), act("x_a", "A2")]);
        assert_eq!((actions(&s), s.warnings.len()), (vec!["block:Block".into(), "x_a:A2".into(), "x_b:B".into()], 0), "replaced where it stands");
        assert_eq!(actions(&built(vec![act("x_a", "A"), remove("x_a")])), ["block:Block"]);
        for op in [act("block", "Mine"), remove("block")] {
            assert!(built(vec![op]).warnings[0].message.contains("comes with the package"));
        }
        assert!(built(vec![remove("x_z")]).warnings[0].message.contains("no action x_z"));
        let mut def = cat.packages().next().unwrap().entities[0].clone();
        assert!(apply_op(&mut def, &serde_json::from_value(remove("block")).unwrap(), OverlaySource::Package).is_err(), "a workspace's operation");
    }

    #[test]
    fn workspace_menus_are_defined_replaced_removed_and_bounded() {
        let cat = Catalog::from_packages(vec![package(
            "{package: core, version: '1', modules: [{name: selling, label: Selling, items: [Customer]}]}",
            "[{name: Customer, label: Customer, kind: master, fields: [{name: n, type: data}, {name: lines, type: table, of: Line}]},
              {name: Line, label: Line, kind: child, fields: [{name: a, type: data}]},
              {name: Order, label: Order, kind: master, fields: [{name: n, type: data}]}]",
        )])
        .unwrap();
        let menu = |extra: Value| {
            let mut m = json!({"name": "x_sales", "label": "Sales", "items": ["Customer", "report:sales_by_month"]});
            m.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            json!({"op": "define_module", "module": m})
        };
        let built = |ops: Vec<Value>| {
            let tenant: Vec<Overlay> =
                ops.into_iter().map(|o| ov(o["module"]["name"].as_str().or(o["name"].as_str()).unwrap_or("x_sales"), json!([o]))).collect();
            build(&cat, &["core".into()], &tenant, 1).unwrap()
        };
        let warned = |op: Value| built(vec![op]).warnings.iter().map(|w| w.message.clone()).collect::<Vec<_>>().join("; ");
        let ours = |s: &Schema| s.modules.iter().filter(|m| m.package == "tenant").map(|m| format!("{}:{}", m.name, m.label)).collect::<Vec<_>>();

        let s = built(vec![menu(json!({
            "icon": "briefcase", "color": "#7c3aed", "description": "Who buys", "order": 5, "create": ["Customer"],
            "assistant": {"instructions": "A customer buys.", "suggestions": ["Who bought most?"]}
        }))]);
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        let m = s.modules.iter().find(|m| m.name == "x_sales").unwrap();
        assert_eq!((m.package.as_str(), m.create.clone(), m.assistant.as_ref().unwrap().suggestions.len()), ("tenant", vec!["Customer".to_string()], 1));
        assert_eq!(s.modules.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["selling", "x_sales"], "sorted with the packages' by order");
        // Replaced where it stands; removed; what is not there to remove is said.
        let other = json!({"op": "define_module", "module": {"name": "x_buy", "label": "Buying", "items": ["Order"]}});
        let s = built(vec![menu(json!({})), other.clone(), menu(json!({"label": "Sales 2"}))]);
        assert_eq!((ours(&s), s.warnings.len()), (vec!["x_buy:Buying".into(), "x_sales:Sales 2".into()], 0));
        let s = built(vec![menu(json!({})), other, json!({"op": "remove_module", "name": "x_sales"})]);
        assert_eq!(ours(&s), ["x_buy:Buying"]);
        assert!(warned(json!({"op": "remove_module", "name": "x_none"})).contains("no menu x_none"));
        assert!(warned(json!({"op": "remove_module", "name": "selling"})).contains("no menu selling"), "a package's is not the workspace's to remove");
        // Every refusal.
        for (extra, says) in [
            (json!({"name": "sales"}), "its name is x_"),
            (json!({"name": "x_Sales"}), "its name is x_"),
            (json!({"name": format!("x_{}", "a".repeat(39))}), "at most 40"),
            (json!({"label": " "}), "needs a label"),
            (json!({"label": "L".repeat(61)}), "label is at most 60"),
            (json!({"description": "d".repeat(201)}), "description is at most 200"),
            (json!({"icon": "Big Icon"}), "a short name"),
            (json!({"color": "purple"}), "#rrggbb"),
            (json!({"items": (0..51).map(|i| format!("report:r{i}")).collect::<Vec<_>>()}), "at most 50 items"),
            (json!({"items": ["Customer", "Customer"]}), "listed twice"),
            (json!({"items": ["customer"]}), "neither an entity"),
            (json!({"create": ["Order"]}), "create offers Order"),
            (json!({"create": ["report:sales_by_month"]}), "create offers report"),
            (json!({"assistant": {"instructions": ""}}), "instructions are empty"),
            (json!({"assistant": {"instructions": "i".repeat(2001)}}), "at most 2000"),
            (json!({"assistant": {"instructions": "i", "suggestions": vec!["s"; 7]}}), "at most 6 suggestions"),
            (json!({"assistant": {"instructions": "i", "suggestions": ["s".repeat(201)]}}), "a suggestion is at most 200"),
            (json!({"items": ["Nowhere"]}), "Nowhere is not an entity here"),
            (json!({"items": ["Line"]}), "Line is the lines of another entity"),
        ] {
            assert!(warned(menu(extra.clone())).contains(says), "{extra}: {}", warned(menu(extra.clone())));
        }
        assert!(serde_json::from_value::<OverlayOp>(menu(json!({"badge": 1}))).is_err(), "unknown properties are refused");
        // An entity that goes (its app uninstalled) drops out of the menu, with a warning; the rest stays.
        let s = built(vec![menu(json!({"items": ["Customer", "Gone"], "create": ["Customer", "Gone"]}))]);
        let m = s.modules.iter().find(|m| m.name == "x_sales").unwrap();
        assert_eq!((m.items.clone(), m.create.clone()), (vec!["Customer".to_string()], vec!["Customer".to_string()]));
        assert_eq!(s.warnings.len(), 1);
        // A package's module: no x_ name, none of a menu's properties; never a workspace's operation.
        let bad = |modules: &str| {
            Catalog::from_packages(vec![package(&format!("{{package: core, version: '1', modules: [{modules}]}}"), "[]")]).unwrap_err().to_string()
        };
        assert!(bad("{name: x_mine, label: M}").contains("x_ names are a workspace's"));
        assert!(bad("{name: mine, label: M, color: '#000000'}").contains("a workspace's own menu's"));
        assert!(bad("{name: mine, label: M, assistant: {instructions: i}}").contains("a workspace's own menu's"));
        assert!(bad("{name: mine, label: M, portal: true}").contains("a workspace's own menu's"), "the portal's mark is a workspace's menu's");
        // Marked for the portal: kept in the schema and written back as it was given; unmarked, not written.
        let s = built(vec![menu(json!({"portal": true}))]);
        let m = s.modules.iter().find(|m| m.name == "x_sales").unwrap();
        assert!(m.portal);
        assert_eq!(serde_json::to_value(serde_json::from_value::<OverlayOp>(menu(json!({"portal": true}))).unwrap()).unwrap()["module"]["portal"], json!(true));
        assert!(serde_json::to_value(serde_json::from_value::<OverlayOp>(menu(json!({}))).unwrap()).unwrap()["module"].get("portal").is_none());
        let mut def = cat.packages().next().unwrap().entities[0].clone();
        assert!(apply_op(&mut def, &serde_json::from_value(menu(json!({}))).unwrap(), OverlaySource::Package).is_err());
    }

    #[test]
    fn form_layouts_are_validated() {
        let mut def: EntityDef =
            serde_yaml::from_str("{name: Project, label: Project, kind: master, fields: [{name: title, type: data}, {name: notes, type: text}]}").unwrap();
        let set = |form: Value| -> OverlayOp { serde_json::from_value(json!({"op": "set_entity_property", "property": "form", "value": form})).unwrap() };
        let ok = json!([{"label": "Main", "fields": ["title"], "tab": "Details"}, {"fields": ["notes"], "visible_if": "doc.title != ''"}]);
        apply_op(&mut def, &set(ok), OverlaySource::Tenant).unwrap();
        assert_eq!(def.form[0].tab.as_deref(), Some("Details"));
        for bad in [
            json!([{"fields": ["nope"]}]),
            json!([{"fields": ["title"]}, {"fields": ["title"]}]),
            json!([{"fields": ["title"], "columns": 5}]),
            json!([{"fields": ["title"], "visible_if": "doc.title ==="}]),
        ] {
            assert!(apply_op(&mut def.clone(), &set(bad.clone()), OverlaySource::Tenant).is_err(), "{bad}");
        }
    }

    #[test]
    fn tenant_overlays_merge_and_isolate_failures() {
        let cat = catalog();
        let tenant = vec![
            ov("Customer", json!([{"op": "add_field", "field": {"name": "x_gstin", "type": "data"}, "after": "customer_name"}])),
            ov("Customer", json!([{"op": "add_options", "field": "tier", "options": ["Platinum"]}])),
            ov("Customer", json!([{"op": "set_property", "field": "customer_name", "property": "required", "value": false}])),
            ov("Customer", json!([{"op": "add_field", "field": {"name": "gstin", "type": "data"}}])),
            ov("Ghost", json!([{"op": "add_rule", "rule": {"name": "r", "check": "true", "message": "m"}}])),
            ov(
                "XWarranty",
                json!([{"op": "define_entity", "entity": {"name": "XWarranty", "label": "Warranty", "kind": "master",
                "fields": [{"name": "serial", "type": "data", "required": true}, {"name": "customer", "type": "link", "to": "Customer"}]}}]),
            ),
        ];
        let s = build(&cat, &["core".into()], &tenant, 7).unwrap();
        let c = s.entity("Customer").unwrap();
        assert_eq!(c.def.fields[1].name, "x_gstin");
        assert_eq!(c.storage_of("x_gstin"), Some(Storage::Ext));
        assert_eq!(c.storage_of("customer_name"), Some(Storage::Column));
        assert!(c.field("tier").unwrap().options.contains(&"Platinum".to_string()));
        assert!(c.field("customer_name").unwrap().required, "locked field stays required");
        assert_eq!(s.warnings.len(), 3, "{:?}", s.warnings);
        let w = s.entity("XWarranty").unwrap();
        assert!(w.custom && w.table == CUSTOM_RECORD_TABLE);
        assert_eq!(s.version, 7);
    }

    #[test]
    fn redefinitions_replace_custom_definitions_in_order() {
        let cat = catalog();
        let def = |fields: Value| json!({"name": "XThing", "label": "Thing", "kind": "master", "fields": fields});
        let one = def(json!([{"name": "a", "type": "data"}, {"name": "b", "type": "int"}]));
        let two = def(json!([{"name": "a", "type": "text"}, {"name": "c", "type": "date"}]));
        let mut tenant = vec![
            ov("XThing", json!([{"op": "define_entity", "entity": one}])),
            ov("XThing", json!([{"op": "set_property", "field": "b", "property": "label", "value": "Bee"}])),
            ov("XThing", json!([{"op": "redefine_entity", "entity": two}])),
            ov("XThing", json!([{"op": "set_property", "field": "c", "property": "label", "value": "Sea"}])),
        ];
        let s = build(&cat, &["core".into()], &tenant, 1).unwrap();
        assert!(s.warnings.is_empty(), "a change before the redefinition is part of what it replaced: {:?}", s.warnings);
        let e = s.entity("XThing").unwrap();
        assert_eq!(e.def.fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!((e.field("a").unwrap().ftype, e.field("c").unwrap().label.as_deref()), (FieldType::Text, Some("Sea")));
        assert_eq!(e.storage_of("c"), Some(Storage::Ext));
        tenant.remove(2);
        tenant.pop();
        let s = build(&cat, &["core".into()], &tenant, 1).unwrap();
        assert_eq!(s.entity("XThing").unwrap().field("b").unwrap().label.as_deref(), Some("Bee"), "without it, the definition before it");
        for (target, why) in [("Company", "comes with a package"), ("XNone", "there is no custom entity XNone")] {
            let e = json!({"name": target, "label": "T", "kind": "master", "fields": [{"name": "a", "type": "data"}]});
            let s = build(&cat, &["core".into()], &[ov(target, json!([{"op": "redefine_entity", "entity": e}]))], 1).unwrap();
            assert!(s.warnings.iter().any(|w| w.message.contains(why)), "{target}: {:?}", s.warnings);
        }
    }

    #[test]
    fn safe_type_changes() {
        use FieldType::*;
        for (from, to) in [(Data, Text), (Markdown, Url), (Select, Data), (Int, Decimal), (Currency, Percent)] {
            assert!(from.converts_to(to), "{from:?} -> {to:?}");
        }
        for (from, to) in [(Text, Select), (Decimal, Int), (Date, Datetime), (Data, Json), (Link, Data), (Data, Table), (Check, Int)] {
            assert!(!from.converts_to(to), "{from:?} -> {to:?}");
        }
    }

    #[test]
    fn cel_view_converts_decimals() {
        let s = build(&catalog(), &["core".into()], &[], 1).unwrap();
        let c = s.entity("Customer").unwrap();
        let v = c.cel_view(&json!({"credit_limit": "1500.50"}), &s);
        assert_eq!(v["credit_limit"], json!(1500.5));
        assert_eq!(v["tier"], Value::Null);
    }

    /// Empty is told from 0 only where asked (BL-21): `isEmpty` and `coalesce` see a field left empty, every other
    /// expression reads it as 0 as before, and the record CEL sees has no new key.
    #[test]
    fn empty_is_told_from_zero_only_when_asked() {
        let s = build(&catalog(), &["core".into()], &[], 1).unwrap();
        let c = s.entity("Customer").unwrap();
        let (empty, zero) = (c.cel_view(&json!({"credit_limit": null}), &s), c.cel_view(&json!({"credit_limit": "0"}), &s));
        let omitted = c.cel_view(&json!({}), &s);
        let user = json!({});
        let eval = |src: &str, doc: &Value| {
            crate::Expr::compile(src).unwrap().eval_with(&crate::Scope { doc, old: None, parent: None, user: &user }, &[("linked", doc)]).unwrap()
        };
        for (doc, was_empty) in [(&empty, true), (&omitted, true), (&zero, false)] {
            assert_eq!(eval("doc.credit_limit == 0.0 && doc.credit_limit + 1.0 == 1.0", doc), json!(true), "read as 0, as before");
            assert_eq!(eval("isEmpty(doc.credit_limit)", doc), json!(was_empty));
            assert_eq!(eval("isEmpty(linked.credit_limit)", doc), json!(was_empty), "any record bound, by its path");
            assert_eq!(eval("coalesce(doc.credit_limit, 500.0)", doc), json!(if was_empty { 500.0 } else { 0.0 }));
            assert_eq!(eval("coalesce(doc.tier, doc.customer_name, 'none')", doc), json!("none"), "null and '' are empty");
            assert_eq!(eval("isEmpty(doc.credit_limit + 0.0)", doc), json!(false), "only a field given directly");
        }
        assert_eq!(eval("size(doc)", &empty), eval("size(doc)", &zero), "the note of what was empty is not part of the record");
        assert_eq!(eval("doc == linked", &empty), json!(true));
        assert_eq!(eval("[isEmpty(''), isEmpty([]), isEmpty(0), coalesce(null, ''), coalesce('', 'x')]", &zero), json!([true, true, false, null, "x"]));
    }

    /// Computed fields are worked out after those they read, by name otherwise; a circle is refused.
    #[test]
    fn computed_fields_in_dependency_order() {
        let def = |computed: Value| {
            let fields: Vec<Value> = computed
                .as_object()
                .unwrap()
                .iter()
                .map(|(n, c)| json!({"name": n, "type": "data", "computed": c}))
                .chain([json!({"name": "base", "type": "data"})])
                .collect();
            compile_exprs(&serde_json::from_value::<EntityDef>(json!({"name": "XT", "label": "T", "kind": "master", "fields": fields})).unwrap())
        };
        let order = |c: Value| def(c).map(|x| x.computed_order);
        assert_eq!(order(json!({"b": "doc.base", "a": "'x'", "c": "'y'"})).unwrap(), ["a", "b", "c"], "nothing to decide: by name");
        assert_eq!(order(json!({"a": "doc.z + doc.b", "b": "doc.base", "z": "doc.b + '!'"})).unwrap(), ["b", "z", "a"]);
        assert_eq!(order(json!({"a": "doc.a != null ? doc.a : doc.base"})).unwrap(), ["a"], "reading itself reads what is stored");
        assert!(order(json!({"a": "doc.b", "b": "doc.c", "c": "doc.a", "d": "'x'"})).unwrap_err().contains("in a circle: a, b, c"));
    }

    #[test]
    fn service_targets() {
        let cat = catalog();
        let target = |name: &str, extra: Value| {
            let mut t = json!({"name": name, "label": "Respond", "within": "doc.priority == 'High' ? 'PT1H' : 'PT8H'", "met_when": "doc.status == 'Closed'",
                "due_field": "respond_by", "state_field": "respond_state"});
            for (k, v) in extra.as_object().cloned().unwrap_or_default() {
                t[k] = v;
            }
            json!([{"op": "add_target", "target": t}])
        };
        let fields = json!([{"name": "subject", "type": "data"}, {"name": "priority", "type": "data"}, {"name": "status", "type": "data"},
            {"name": "respond_by", "type": "datetime"}, {"name": "respond_state", "type": "select", "options": ["running", "paused", "met", "met_late", "breached"]},
            {"name": "resolve_by", "type": "datetime"}, {"name": "resolve_state", "type": "data"}, {"name": "notes", "type": "text"},
            {"name": "total", "type": "int", "computed": "1"}, {"name": "shown", "type": "data", "computed": "doc.subject"}, {"name": "lines", "type": "table", "of": "XTicketLine"}]);
        let base = vec![
            ov(
                "XTicketLine",
                json!([{"op": "define_entity", "entity": {"name": "XTicketLine", "label": "Line", "kind": "child",
                "fields": [{"name": "at", "type": "datetime"}, {"name": "word", "type": "data"}]}}]),
            ),
            ov("XTicket", json!([{"op": "define_entity", "entity": {"name": "XTicket", "label": "Ticket", "kind": "master", "fields": fields}}])),
            ov("Customer", json!([{"op": "add_field", "field": {"name": "x_due", "type": "datetime"}}])),
            ov("Customer", json!([{"op": "add_field", "field": {"name": "x_state", "type": "data"}}])),
        ];
        let with = |more: &[Overlay]| build(&cat, &["core".into()], &[base.clone(), more.to_vec()].concat(), 1).unwrap();
        assert!(with(&[]).warnings.is_empty(), "{:?}", with(&[]).warnings);
        let good = ov("XTicket", target("respond", json!({"calendar": "office", "pause_when": "doc.status == 'Waiting'"})));
        let s = with(std::slice::from_ref(&good));
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        let e = s.entity("XTicket").unwrap();
        assert_eq!(e.target_keeping("respond_state").map(|t| t.name.as_str()), Some("respond"));
        assert!(e.exprs.targets["respond"].pause_when.is_some() && e.exprs.targets["respond"].start_when.is_none());

        // Every refusal is a warning naming why; the schema stands without it.
        let line = ov(
            "XTicketLine",
            json!([{"op": "add_target", "target": {"name": "t", "label": "T", "within": "60", "met_when": "true", "due_field": "at", "state_field": "word"}}]),
        );
        let packaged = ov(
            "Customer",
            json!([{"op": "add_target", "target": {"name": "t", "label": "T", "within": "60", "met_when": "true", "due_field": "x_due", "state_field": "customer_name"}}]),
        );
        let mut nine = Vec::new();
        for i in 0..9 {
            let (d, st) = (format!("x_d{i}"), format!("x_s{i}"));
            nine.push(ov("XTicket", json!([{"op": "add_field", "field": {"name": d, "type": "datetime"}}])));
            nine.push(ov("XTicket", json!([{"op": "add_field", "field": {"name": st, "type": "data"}}])));
            nine.push(ov("XTicket", target(&format!("t{i}"), json!({"due_field": d, "state_field": st}))));
        }
        for (more, why) in [
            (vec![line], "holds lines"),
            (vec![ov("XTicket", target("t", json!({"due_field": "nothing"})))], "due_field nothing is not a field"),
            (vec![ov("XTicket", target("t", json!({"due_field": "subject"})))], "must be a date-time field"),
            (vec![ov("XTicket", target("t", json!({"state_field": "notes"})))], "must be a data or select field"),
            (vec![ov("XTicket", target("t", json!({"state_field": "total"})))], "must be a data or select field"),
            (vec![ov("XTicket", target("t", json!({"state_field": "shown"})))], "worked out already"),
            (vec![good.clone(), ov("XTicket", target("other", json!({"state_field": "resolve_state"})))], "kept by another target"),
            (vec![packaged], "comes with a package"),
            (nine, "at most 8 targets"),
            (vec![ov("XTicket", target("t", json!({"met_when": "doc.status ==" })))], "met_when"),
            (vec![ov("XTicket", target("t", json!({"within": ""})))], "within"),
            (vec![ov("XTicket", target("t", json!({"calendar": "Office Hours"})))], "not a calendar's name"),
            (vec![ov("XTicket", json!([{"op": "remove_target", "name": "respond"}]))], "no target respond to remove"),
        ] {
            let s = with(&more);
            assert!(s.warnings.iter().any(|w| w.message.contains(why)), "{why}: {:?}", s.warnings);
        }
        // The workspace's own fields on a packaged entity are fine.
        let own = ov(
            "Customer",
            json!([{"op": "add_target", "target": {"name": "t", "label": "T", "within": "60", "met_when": "true", "due_field": "x_due", "state_field": "x_state"}}]),
        );
        assert!(with(std::slice::from_ref(&own)).warnings.is_empty());

        // The same name again replaces it where it stands; remove takes it away.
        let resolve = ov("XTicket", target("resolve", json!({"due_field": "resolve_by", "state_field": "resolve_state"})));
        let s = with(&[good.clone(), resolve, ov("XTicket", target("respond", json!({"within": "'PT2H'"})))]);
        let names: Vec<(&str, &str)> = s.entity("XTicket").unwrap().def.targets.iter().map(|t| (t.name.as_str(), t.within.as_str())).collect();
        assert_eq!(names, [("respond", "'PT2H'"), ("resolve", "doc.priority == 'High' ? 'PT1H' : 'PT8H'")], "{:?}", s.warnings);
        let s = with(&[good.clone(), ov("XTicket", json!([{"op": "remove_target", "name": "respond"}]))]);
        assert!(s.warnings.is_empty() && s.entity("XTicket").unwrap().def.targets.is_empty());

        // A target added before a redefinition is part of what it replaced; the definition carries targets.
        let mut def: Value = serde_json::to_value(&with(std::slice::from_ref(&good)).entity("XTicket").unwrap().def).unwrap();
        assert_eq!(def["targets"][0]["name"], json!("respond"), "the definition carries its targets");
        def["targets"] = json!([]);
        let s = with(&[good.clone(), ov("XTicket", json!([{"op": "redefine_entity", "entity": def.clone()}]))]);
        assert!(s.warnings.is_empty() && s.entity("XTicket").unwrap().def.targets.is_empty(), "{:?}", s.warnings);
        let tenant = [base.clone(), vec![good.clone(), ov("XTicket", json!([{"op": "redefine_entity", "entity": def}]))]].concat();
        assert_eq!(superseded(&tenant)[base.len()], Some(base.len() + 1));
    }
}
