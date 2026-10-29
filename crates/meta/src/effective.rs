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

#[derive(Debug, Clone, Default)]
pub struct Exprs {
    pub default_expr: BTreeMap<String, Expr>,
    pub computed: BTreeMap<String, Expr>,
    pub visible_if: BTreeMap<String, Expr>,
    pub required_if: BTreeMap<String, Expr>,
    pub read_only_if: BTreeMap<String, Expr>,
    pub rules: Vec<(RuleDef, Expr)>,
    pub action_conditions: BTreeMap<String, Expr>,
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
    pub warnings: Vec<SchemaWarning>,
    /// The tenant's fiscal year start month (1-12), for contexts that act without a user.
    pub fy_start_month: u32,
    /// The tenant's IANA time zone (`Asia/Kolkata`); "today" and "now" are local to it.
    pub timezone: String,
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
            warnings: vec![],
            fy_start_month: 1,
            timezone: "UTC".into(),
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

/// Applies one overlay op to an entity definition. Returns the name of a field it added.
pub(crate) fn apply_op(def: &mut EntityDef, op: &OverlayOp, source: OverlaySource) -> Result<Option<String>, String> {
    match op {
        OverlayOp::AddAction { action } => {
            if source == OverlaySource::Tenant {
                return Err("actions need code behind them: only packages can add them".into());
            }
            if def.actions.iter().any(|a| a.name == action.name) {
                return Err(format!("action {} already exists", action.name));
            }
            def.actions.push(action.clone());
            Ok(None)
        }
        OverlayOp::AddField { field, after } => {
            if def.fields.iter().any(|f| f.name == field.name) {
                return Err(format!("field {} already exists", field.name));
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
            if field.unique {
                return Err("custom fields cannot be unique (use a validation rule)".into());
            }
            let pos = match after {
                Some(a) => def.fields.iter().position(|f| &f.name == a).map(|i| i + 1).ok_or(format!("unknown field {a}"))?,
                None => def.fields.len(),
            };
            def.fields.insert(pos, field.clone());
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
                "list" => def.list = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?,
                "search_fields" => def.search_fields = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?,
                "form" => {
                    let form: Vec<SectionDef> = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
                    validate_form(def, &form)?;
                    def.form = form;
                }
                other => return Err(format!("entity property {other} cannot be customized")),
            }
            Ok(None)
        }
        OverlayOp::DefineEntity { .. } | OverlayOp::SetWorkflow { .. } | OverlayOp::SetPrintTemplate { .. } => {
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
    if !matches!(e.kind, EntityKind::Master | EntityKind::Document) {
        return Err("custom entities must be masters or documents".into());
    }
    if e.table.is_some() || !e.unique.is_empty() || !e.indexes.is_empty() || !e.exclusion.is_empty() {
        return Err("custom entities cannot declare tables, unique keys, indexes or exclusions".into());
    }
    for f in &e.fields {
        if matches!(f.ftype, FieldType::Table | FieldType::Secret) || f.unique {
            return Err(format!("custom entity field {} cannot be a table, a secret or unique", f.name));
        }
    }
    Ok(())
}

/// Builds the effective schema for one tenant.
pub fn build(catalog: &Catalog, installed: &[String], tenant: &[Overlay], version: i64) -> Result<Schema, MetaError> {
    let packages = catalog.closure(installed)?;
    let set: HashSet<&str> = packages.iter().map(String::as_str).collect();
    let mut building: BTreeMap<String, Building> = BTreeMap::new();
    let mut modules = Vec::new();
    let mut roles = Vec::new();
    let mut personal_data = Vec::new();
    let mut warnings = Vec::new();

    for pkg in catalog.packages().filter(|p| set.contains(p.name())) {
        for e in &pkg.entities {
            let storage = e.fields.iter().filter(|f| f.ftype.is_stored()).map(|f| (f.name.clone(), Storage::Column)).collect();
            let print_template = pkg.print_formats.iter().find(|(n, _)| n == &e.name).map(|(_, t)| t.clone());
            building.insert(e.name.clone(), Building { package: pkg.name().into(), def: e.clone(), storage, custom: false, workflow: None, print_template });
        }
        modules.extend(pkg.manifest.modules.iter().cloned());
        roles.extend(pkg.manifest.roles.iter().cloned());
        personal_data.extend(pkg.manifest.personal_data.iter().cloned());
    }
    // Package overlays and workflows, in dependency order (validated by the catalog already).
    for pkg in catalog.packages().filter(|p| set.contains(p.name())) {
        for ov in &pkg.manifest.overlays {
            let Some(b) = building.get_mut(&ov.entity) else { continue };
            for op in &ov.ops {
                if let Some(added) = apply_op(&mut b.def, op, OverlaySource::Package).map_err(|m| MetaError::Invalid(format!("{}: {m}", ov.entity)))? {
                    b.storage.insert(added, Storage::Ext);
                }
            }
        }
        for wf in &pkg.manifest.workflows {
            if let Some(b) = building.get_mut(&wf.entity) {
                b.workflow = Some(wf.clone());
            }
        }
    }

    // Tenant: new entities first, so other customizations can target them.
    for ov in tenant {
        for op in &ov.ops {
            if let OverlayOp::DefineEntity { entity } = op {
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
        }
    }
    for ov in tenant {
        for op in &ov.ops {
            if matches!(op, OverlayOp::DefineEntity { .. }) {
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
        if let Err(m) = validate_entity(&b.def, &all) {
            if b.custom {
                broken.push(name.clone());
            } else if let Some(base) = catalog.entity(name) {
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
            } else if let Some(base) = catalog.entity(&name) {
                b.def = base.clone();
                b.storage = base.fields.iter().filter(|f| f.ftype.is_stored()).map(|f| (f.name.clone(), Storage::Column)).collect();
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
        let table = if b.custom {
            CUSTOM_RECORD_TABLE.to_string()
        } else if b.def.kind == EntityKind::Child {
            String::new()
        } else {
            b.def.table.clone().unwrap_or_else(|| table_for(&name))
        };
        let children = b
            .def
            .fields
            .iter()
            .filter(|f| f.ftype == FieldType::Table)
            .map(|f| (f.name.clone(), ChildInfo { entity: f.of.clone().unwrap_or_default(), table: child_table(&table, &f.name) }))
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
    Ok(Schema { version, entities, packages, modules, roles, personal_data, warnings, fy_start_month: 1, timezone: "UTC".into() })
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

    #[test]
    fn only_packages_add_actions() {
        let mut def: EntityDef = serde_yaml::from_str("{name: Project, label: Project, kind: master, fields: [{name: title, type: data}]}").unwrap();
        let op: OverlayOp =
            serde_json::from_value(json!({"op": "add_action", "action": {"name": "make_sales_invoice", "label": "Make Sales Invoice"}})).unwrap();
        assert!(apply_op(&mut def.clone(), &op, OverlaySource::Tenant).is_err(), "actions need code");
        apply_op(&mut def, &op, OverlaySource::Package).unwrap();
        assert_eq!(def.actions[0].permission, "write");
        assert!(apply_op(&mut def, &op, OverlaySource::Package).is_err(), "no duplicates");
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
    fn cel_view_converts_decimals() {
        let s = build(&catalog(), &["core".into()], &[], 1).unwrap();
        let c = s.entity("Customer").unwrap();
        let v = c.cel_view(&json!({"credit_limit": "1500.50"}), &s);
        assert_eq!(v["credit_limit"], json!(1500.5));
        assert_eq!(v["tier"], Value::Null);
    }
}
