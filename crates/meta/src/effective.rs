//! The effective (per-tenant) schema: installed packages + package overlays + tenant overlays,
//! merged deterministically and compiled. Broken tenant customizations are isolated into
//! `Schema::warnings` instead of taking the tenant down (upgrade safety).

use crate::catalog::{Catalog, OverlaySource, validate_entity, validate_workflow};
use crate::expr::Expr;
use crate::types::*;
use crate::{MetaError, child_table, invalid, is_entity_name, table_for};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
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
    pub visible_if: BTreeMap<String, Expr>,
    pub required_if: BTreeMap<String, Expr>,
    pub read_only_if: BTreeMap<String, Expr>,
    pub rules: Vec<(RuleDef, Expr)>,
    pub action_conditions: BTreeMap<String, Expr>,
    /// A configured action's `refuse_if`, compiled, with their messages, in order.
    pub action_refusals: BTreeMap<String, Vec<(Expr, String)>>,
}

#[derive(Debug, Clone)]
pub struct CompiledWorkflow {
    pub def: WorkflowDef,
    pub guards: BTreeMap<(String, String), Expr>,
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
        for (k, v) in map {
            let converted = match self.field(k) {
                Some(f) if v.is_null() && empty(f.ftype).is_some() => empty(f.ftype).unwrap_or_default(),
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
            out.entry(f.name.clone()).or_insert_with(|| empty(f.ftype).unwrap_or(Value::Null));
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
}

impl Schema {
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
        }
    }
}

impl Schema {
    pub fn entity(&self, name: &str) -> Option<&Arc<EffEntity>> {
        self.entities.get(name)
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
        OverlayOp::AddField { field, after, column } => {
            if def.fields.iter().any(|f| f.name == field.name) {
                return Err(format!("field {} already exists", field.name));
            }
            if *column && source == OverlaySource::Tenant {
                return Err("only packages add columns to an entity's table".into());
            }
            if field.ftype == FieldType::Table && source == OverlaySource::Tenant {
                return Err("customizations cannot add child tables".into());
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
        OverlayOp::DefineEntity { .. } | OverlayOp::RedefineEntity { .. } | OverlayOp::SetWorkflow { .. } | OverlayOp::SetPrintTemplate { .. } => {
            Err("this operation is only valid as a tenant customization".into())
        }
    }
}

struct Building {
    package: String,
    def: EntityDef,
    storage: BTreeMap<String, Storage>,
    custom: bool,
    workflow: Option<WorkflowDef>,
    print_template: Option<String>,
}

fn validate_custom_entity(e: &EntityDef) -> Result<(), String> {
    if !is_entity_name(&e.name) || !e.name.starts_with('X') {
        return Err(format!("custom entity {} must be PascalCase and start with 'X'", e.name));
    }
    if !matches!(e.kind, EntityKind::Master | EntityKind::Document | EntityKind::Child) {
        return Err("custom entities must be masters, documents or child (line) entities".into());
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

/// A custom entity's lines are rows of a custom child entity, and each custom child entity is the
/// lines of one table field only: its rows share `custom_record`, where the child's name alone tells
/// one table's rows from another's.
fn validate_custom_lines(name: &str, building: &BTreeMap<String, Building>) -> Result<(), String> {
    let Some(b) = building.get(name).filter(|b| b.custom) else { return Ok(()) };
    for f in b.def.fields.iter().filter(|f| f.ftype == FieldType::Table) {
        let of = f.of.as_deref().unwrap_or_default();
        if !building.get(of).is_some_and(|c| c.custom && c.def.kind == EntityKind::Child) {
            return Err(format!("table field {}: the lines of a custom entity must be a custom child entity, and {of} is not one", f.name));
        }
        let first = building
            .iter()
            .filter(|(_, o)| o.custom)
            .flat_map(|(n, o)| o.def.fields.iter().filter(|g| g.ftype == FieldType::Table && g.of.as_deref() == Some(of)).map(move |g| (n, &g.name)))
            .next();
        if let Some((owner, table)) = first.filter(|(n, t)| (n.as_str(), t.as_str()) != (name, f.name.as_str())) {
            return Err(format!("table field {}: {of} is already the lines of {owner}.{table}; a child entity belongs to one table field", f.name));
        }
    }
    Ok(())
}

/// Whether `op`, the tenant's customization at `at`, is part of what a redefinition of its entity at `r`
/// replaced: a change to the definition made before it, an earlier redefinition included (a workflow and a print
/// template are not part of the definition, and the `define_entity` that made the entity is what lets it be
/// redefined at all). `build` applies none of these.
pub fn replaced_by_redefinition(op: &OverlayOp, at: usize, r: usize) -> bool {
    at < r && !matches!(op, OverlayOp::SetWorkflow { .. } | OverlayOp::SetPrintTemplate { .. } | OverlayOp::DefineEntity { .. })
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
    let mut warnings = Vec::new();

    for pkg in catalog.packages().filter(|p| set.contains(p.name())) {
        for e in &pkg.entities {
            let storage = e.fields.iter().filter(|f| f.ftype.is_stored()).map(|f| (f.name.clone(), Storage::Column)).collect();
            let print_template = pkg.print_formats.iter().find(|(n, _)| n == &e.name).map(|(_, t)| t.clone());
            building.insert(e.name.clone(), Building { package: pkg.name().into(), def: e.clone(), storage, custom: false, workflow: None, print_template });
        }
        modules.extend(pkg.manifest.modules.iter().map(|m| ModuleDef { package: pkg.name().into(), ..m.clone() }));
        roles.extend(pkg.manifest.roles.iter().cloned());
        personal_data.extend(pkg.manifest.personal_data.iter().cloned());
        handover.extend(pkg.manifest.handover.iter().cloned());
        export_tables.extend(pkg.manifest.export_tables.iter().cloned());
        merge_links.extend(pkg.manifest.merge_links.iter().cloned());
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
                                Building { package: "tenant".into(), def: (**entity).clone(), storage, custom: true, workflow: None, print_template: None },
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
    for (at, ov) in tenant.iter().enumerate() {
        for op in &ov.ops {
            if matches!(op, OverlayOp::DefineEntity { .. } | OverlayOp::RedefineEntity { .. }) {
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
        if let Err(m) = validate_entity(&b.def, &all).and_then(|_| validate_custom_lines(name, &building)) {
            if b.custom {
                broken.push(name.clone());
            } else if let Some((base, _)) = packaged.get(name) {
                if validate_entity(base, &all).is_err() {
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

    // Workflows: validate against the merged entities; drop invalid tenant ones with a warning.
    let entity_map: BTreeMap<String, (String, EntityDef)> = building.iter().map(|(k, b)| (k.clone(), (b.package.clone(), b.def.clone()))).collect();
    for (name, b) in building.iter_mut() {
        if let Some(wf) = &b.workflow
            && let Err(m) = validate_workflow(wf, &entity_map)
        {
            warnings.push(SchemaWarning { entity: name.clone(), message: format!("workflow ignored: {m}") });
            b.workflow = None;
        }
    }

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
                // A custom entity's lines are custom records too (`ChildInfo::predicate`).
                let rows = if b.custom { CUSTOM_RECORD_TABLE.to_string() } else { child_table(&table, &f.name) };
                (f.name.clone(), ChildInfo { entity: f.of.clone().unwrap_or_default(), table: rows })
            })
            .collect();
        let exprs = compile_exprs(&b.def).map_err(|m| MetaError::Invalid(format!("{name}: {m}")))?;
        let workflow = match b.workflow {
            Some(def) => {
                let mut guards = BTreeMap::new();
                for t in &def.transitions {
                    if let Some(c) = &t.condition {
                        guards.insert((t.action.clone(), t.from.clone()), Expr::compile(c).map_err(MetaError::Invalid)?);
                    }
                }
                Some(CompiledWorkflow { def, guards })
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
            }),
        );
    }
    modules.sort_by(|a, b| a.order.cmp(&b.order).then(a.name.cmp(&b.name)));
    Ok(Schema {
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
    })
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
    for r in &def.rules {
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
    Ok(x)
}

/// A form layout may only place existing fields, each once, in at most 4 columns per section,
/// with valid section conditions.
pub(crate) fn validate_form(def: &EntityDef, form: &[SectionDef]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for s in form {
        if s.columns.is_some_and(|c| !(1..=4).contains(&c)) {
            return Err("a section has 1 to 4 columns".into());
        }
        if let Some(c) = &s.visible_if {
            crate::expr::Expr::compile(c).map_err(|e| format!("section condition '{c}': {e}"))?;
        }
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
}
