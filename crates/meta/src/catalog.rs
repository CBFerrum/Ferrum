//! Loading and validating the set of packages available on this cluster.

use crate::expr::Expr;
use crate::types::*;
use crate::{MetaError, RESERVED_FIELDS, invalid, is_entity_name, is_ident, is_package_name};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Catalog {
    packages: BTreeMap<String, Package>,
    order: Vec<String>,
    entities: BTreeMap<String, (String, EntityDef)>,
    fingerprint: String,
}

fn read(path: &Path) -> Result<String, MetaError> {
    fs::read_to_string(path).map_err(|e| MetaError::Io { path: path.display().to_string(), msg: e.to_string() })
}

fn sorted_files(dir: &Path, ext: &str) -> Result<Vec<std::path::PathBuf>, MetaError> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: Vec<_> = fs::read_dir(dir)
        .map_err(|e| MetaError::Io { path: dir.display().to_string(), msg: e.to_string() })?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == ext))
        .collect();
    files.sort();
    Ok(files)
}

/// Reads `package.yaml`, `entities/*.yaml` (one entity or a list per file), `sql/*.sql`, `print/<Entity>.html`.
pub fn load_package_dir(dir: &Path) -> Result<Package, MetaError> {
    let mut files = BTreeMap::new();
    files.insert("package.yaml".to_string(), read(&dir.join("package.yaml"))?);
    for (sub, ext) in [("entities", "yaml"), ("sql", "sql"), ("print", "html")] {
        for f in sorted_files(&dir.join(sub), ext)? {
            let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            files.insert(format!("{sub}/{name}"), read(&f)?);
        }
    }
    load_package_files(&files).map_err(|e| match e {
        MetaError::Yaml { path, msg } => MetaError::Yaml { path: dir.join(path).display().to_string(), msg },
        other => other,
    })
}

/// A package from its files, keyed by path relative to the package directory (`package.yaml`,
/// `entities/x.yaml`, `sql/001.sql`, `print/Entity.html`) — as shipped inside app bundles, stored
/// in the database or embedded in native apps. Other paths are ignored.
pub fn load_package_files(files: &BTreeMap<String, String>) -> Result<Package, MetaError> {
    let manifest_text = files.get("package.yaml").ok_or_else(|| MetaError::Invalid("package.yaml is missing".into()))?;
    let manifest: PackageManifest = serde_yaml::from_str(manifest_text).map_err(|e| MetaError::Yaml { path: "package.yaml".into(), msg: e.to_string() })?;
    let (mut entities, mut sql, mut print_formats) = (Vec::new(), Vec::new(), Vec::new());
    for (path, text) in files {
        if let Some(name) = path.strip_prefix("entities/").filter(|n| n.ends_with(".yaml") && !n.contains('/')) {
            let yaml_err = |e: serde_yaml::Error| MetaError::Yaml { path: format!("entities/{name}"), msg: e.to_string() };
            if text.trim_start().starts_with('-') {
                let list: Vec<EntityDef> = serde_yaml::from_str(text).map_err(yaml_err)?;
                entities.extend(list);
            } else {
                entities.push(serde_yaml::from_str(text).map_err(yaml_err)?);
            }
        } else if let Some(name) = path.strip_prefix("sql/").filter(|n| n.ends_with(".sql") && !n.contains('/')) {
            sql.push((name.to_string(), text.clone()));
        } else if let Some(name) = path.strip_prefix("print/").and_then(|n| n.strip_suffix(".html")).filter(|n| !n.contains('/')) {
            print_formats.push((name.to_string(), text.clone()));
        }
    }
    Ok(Package { manifest, entities, sql, print_formats })
}

/// Packages embedded in a binary as `(package, path inside the package, content)` — the platform's
/// own and native apps', generated at build time from their `packages/` directories.
pub fn load_embedded(files: &[(&str, &str, &str)]) -> Result<Vec<Package>, MetaError> {
    let mut by_package: BTreeMap<&str, BTreeMap<String, String>> = BTreeMap::new();
    for (pkg, path, text) in files {
        by_package.entry(pkg).or_default().insert(path.to_string(), text.to_string());
    }
    by_package
        .into_iter()
        .map(|(name, files)| {
            let p = load_package_files(&files).map_err(|e| MetaError::Invalid(format!("embedded package {name}: {e}")))?;
            if p.name() != name {
                return invalid(format!("embedded directory {name} holds package {}", p.name()));
            }
            Ok(p)
        })
        .collect()
}

impl Catalog {
    /// Loads every sub-directory of `dir` that contains a `package.yaml`.
    pub fn load_dir(dir: &Path) -> Result<Catalog, MetaError> {
        let mut pkgs = Vec::new();
        let mut dirs: Vec<_> = fs::read_dir(dir)
            .map_err(|e| MetaError::Io { path: dir.display().to_string(), msg: e.to_string() })?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.join("package.yaml").is_file())
            .collect();
        dirs.sort();
        for d in dirs {
            pkgs.push(load_package_dir(&d)?);
        }
        Catalog::from_packages(pkgs)
    }

    pub fn from_packages(pkgs: Vec<Package>) -> Result<Catalog, MetaError> {
        let mut packages = BTreeMap::new();
        for p in pkgs {
            let name = p.name().to_string();
            if !is_package_name(&name) {
                return invalid(format!("invalid package name '{name}'"));
            }
            if p.manifest.version.trim().is_empty() {
                return invalid(format!("package '{name}' has no version"));
            }
            if packages.insert(name.clone(), p).is_some() {
                return invalid(format!("duplicate package '{name}'"));
            }
        }
        let order = topo_order(&packages)?;
        let mut entities = BTreeMap::new();
        for name in &order {
            for e in &packages[name].entities {
                if !is_entity_name(&e.name) {
                    return invalid(format!("invalid entity name '{}' in package {name}", e.name));
                }
                if let Some((other, _)) = entities.insert(e.name.clone(), (name.clone(), e.clone())) {
                    return invalid(format!("entity '{}' defined by both {other} and {name}", e.name));
                }
            }
        }
        let all: HashMap<&str, &EntityDef> = entities.iter().map(|(k, (_, e))| (k.as_str(), e)).collect();
        for (pkg, e) in entities.values() {
            validate_entity(e, &all).map_err(|m| MetaError::Invalid(format!("{pkg}/{}: {m}", e.name)))?;
        }
        let mut cat = Catalog { packages, order, entities, fingerprint: String::new() };
        cat.validate_packages()?;
        cat.fingerprint = cat.compute_fingerprint();
        Ok(cat)
    }

    fn validate_packages(&self) -> Result<(), MetaError> {
        for name in &self.order {
            let p = &self.packages[name];
            let deps = self.closure(std::slice::from_ref(name))?;
            if let Some(prefix) = &p.manifest.field_prefix
                && (!is_ident(prefix) || !prefix.ends_with('_') || prefix == "x_")
            {
                return invalid(format!("{name}: field_prefix must be an identifier ending in '_' (not 'x_')"));
            }
            for ov in &p.manifest.overlays {
                let Some((owner, base)) = self.entities.get(&ov.entity) else {
                    return invalid(format!("{name}: overlay targets unknown entity {}", ov.entity));
                };
                if !deps.contains(owner) {
                    return invalid(format!("{name}: overlay on {} requires depending on package {owner}", ov.entity));
                }
                let mut merged = base.clone();
                for op in &ov.ops {
                    if let OverlayOp::AddField { field, .. } = op {
                        let ok = p.manifest.field_prefix.as_deref().is_some_and(|px| field.name.starts_with(px));
                        if !ok {
                            return invalid(format!("{name}: field '{}' added to {} must start with the package field_prefix", field.name, ov.entity));
                        }
                        // A child table added to another package's entity: its rows' entity must be
                        // a child entity this package defines or depends on.
                        if field.ftype == FieldType::Table {
                            let of = field.of.as_deref().unwrap_or_default();
                            match self.entities.get(of) {
                                Some((owner, child)) if child.kind == EntityKind::Child && deps.contains(owner) => {}
                                _ => {
                                    return invalid(format!(
                                        "{name}: table field '{}' on {} needs 'of' naming a child entity of this package or its dependencies",
                                        field.name, ov.entity
                                    ));
                                }
                            }
                        }
                    }
                    crate::effective::apply_op(&mut merged, op, OverlaySource::Package)
                        .map_err(|m| MetaError::Invalid(format!("{name}: overlay on {}: {m}", ov.entity)))?;
                }
            }
            for role in &p.manifest.roles {
                if role.name.trim().is_empty() {
                    return invalid(format!("{name}: role without a name"));
                }
                for perm in &role.permissions {
                    if perm.entity != "*" && !self.entities.contains_key(&perm.entity) {
                        return invalid(format!("{name}: role {} references unknown entity {}", role.name, perm.entity));
                    }
                    let def = self.entities.get(&perm.entity).map(|(_, e)| e);
                    perm.check_condition(|f| def.and_then(|e| e.fields.iter().find(|x| x.name == f)))
                        .map_err(|m| MetaError::Invalid(format!("{name}: role {} on {}: {m}", role.name, perm.entity)))?;
                }
            }
            for wf in &p.manifest.workflows {
                validate_workflow(wf, &self.entities).map_err(|m| MetaError::Invalid(format!("{name}: workflow {}: {m}", wf.name)))?;
            }
            for pd in &p.manifest.personal_data {
                let Some((_, e)) = self.entities.get(&pd.entity) else {
                    return invalid(format!("{name}: personal data of unknown entity {}", pd.entity));
                };
                let field = |f: &str| e.fields.iter().find(|x| x.name == f);
                if field(&pd.match_field).is_none_or(|f| f.ftype != FieldType::Email) {
                    return invalid(format!("{name}: personal data of {}: match must be an email field", pd.entity));
                }
                if pd.erase.is_empty() {
                    return invalid(format!("{name}: personal data of {}: list the fields to erase", pd.entity));
                }
                for f in &pd.erase {
                    if field(f).is_none_or(|x| !crate::types::ERASABLE_TYPES.contains(&x.ftype)) {
                        return invalid(format!("{name}: personal data of {}: {f} is not a text, contact, date, JSON or table field", pd.entity));
                    }
                }
                if let Some(c) = pd.condition.keys().find(|c| field(c).is_none()) {
                    return invalid(format!("{name}: personal data of {}: {c} is not a field", pd.entity));
                }
            }
            for seed in &p.manifest.seed {
                let Some((_, e)) = self.entities.get(&seed.entity) else {
                    return invalid(format!("{name}: seed for unknown entity {}", seed.entity));
                };
                for k in &seed.key {
                    if !e.fields.iter().any(|f| &f.name == k) {
                        return invalid(format!("{name}: seed key {k} is not a field of {}", seed.entity));
                    }
                }
            }
            for m in &p.manifest.modules {
                if !is_ident(&m.name) {
                    return invalid(format!("{name}: invalid module name {}", m.name));
                }
            }
        }
        Ok(())
    }

    fn compute_fingerprint(&self) -> String {
        let mut h = Sha256::new();
        for name in &self.order {
            let p = &self.packages[name];
            h.update(name.as_bytes());
            h.update(p.manifest.version.as_bytes());
            h.update(serde_json::to_vec(&p.manifest).unwrap_or_default());
            h.update(serde_json::to_vec(&p.entities).unwrap_or_default());
            for (f, s) in &p.sql {
                h.update(f.as_bytes());
                h.update(s.as_bytes());
            }
        }
        hex::encode(h.finalize())
    }

    /// Stable hash of every definition; changes whenever any package changes.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn package(&self, name: &str) -> Option<&Package> {
        self.packages.get(name)
    }

    /// Packages in dependency order (dependencies first).
    pub fn packages(&self) -> impl Iterator<Item = &Package> {
        self.order.iter().map(|n| &self.packages[n])
    }

    pub fn entity(&self, name: &str) -> Option<&EntityDef> {
        self.entities.get(name).map(|(_, e)| e)
    }

    pub fn entity_package(&self, name: &str) -> Option<&str> {
        self.entities.get(name).map(|(p, _)| p.as_str())
    }

    /// `(package, entity)` in dependency order.
    pub fn entities(&self) -> impl Iterator<Item = (&str, &EntityDef)> {
        self.order.iter().flat_map(move |p| self.packages[p].entities.iter().map(move |e| (p.as_str(), e)))
    }

    /// The requested packages plus all transitive dependencies, in dependency order.
    pub fn closure(&self, requested: &[String]) -> Result<Vec<String>, MetaError> {
        let mut want = BTreeSet::new();
        let mut stack: Vec<String> = requested.to_vec();
        while let Some(n) = stack.pop() {
            let Some(p) = self.packages.get(&n) else {
                return invalid(format!("unknown package '{n}'"));
            };
            if want.insert(n.clone()) {
                stack.extend(p.manifest.depends.iter().cloned());
            }
        }
        Ok(self.order.iter().filter(|n| want.contains(*n)).cloned().collect())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlaySource {
    Package,
    Tenant,
}

fn topo_order(packages: &BTreeMap<String, Package>) -> Result<Vec<String>, MetaError> {
    let mut indeg: BTreeMap<&str, usize> = packages.keys().map(|k| (k.as_str(), 0)).collect();
    for (name, p) in packages {
        for d in &p.manifest.depends {
            if !packages.contains_key(d) {
                return invalid(format!("package '{name}' depends on unknown package '{d}'"));
            }
            *indeg.get_mut(name.as_str()).unwrap() += 1;
        }
    }
    let mut ready: Vec<&str> = indeg.iter().filter(|(_, d)| **d == 0).map(|(k, _)| *k).collect();
    let mut order = Vec::new();
    while let Some(n) = ready.pop() {
        order.push(n.to_string());
        for (name, p) in packages {
            if p.manifest.depends.iter().any(|d| d == n) {
                let d = indeg.get_mut(name.as_str()).unwrap();
                *d -= 1;
                if *d == 0 {
                    ready.push(name.as_str());
                    ready.sort_by(|a, b| b.cmp(a));
                }
            }
        }
    }
    if order.len() != packages.len() {
        return invalid("package dependency cycle detected");
    }
    Ok(order)
}

fn compile(what: &str, src: &Option<String>) -> Result<(), String> {
    if let Some(s) = src {
        Expr::compile(s).map_err(|e| format!("{what}: {e}"))?;
    }
    Ok(())
}

/// Structural validation of one (possibly overlaid) entity against all known entities.
pub(crate) fn validate_entity(e: &EntityDef, all: &HashMap<&str, &EntityDef>) -> Result<(), String> {
    if e.label.trim().is_empty() {
        return Err("label is required".into());
    }
    match e.kind {
        EntityKind::Ledger if e.table.is_none() => return Err("ledger entities must name their engine table".into()),
        EntityKind::Child if e.submittable || e.numbering.is_some() => return Err("child entities cannot be submittable or numbered".into()),
        EntityKind::Master | EntityKind::Settings | EntityKind::Ledger if e.submittable => return Err("only document entities can be submittable".into()),
        _ => {}
    }
    if let Some(t) = &e.table
        && !is_ident(t)
    {
        return Err(format!("invalid table name {t}"));
    }
    let mut names = HashSet::new();
    for f in &e.fields {
        if !is_ident(&f.name) {
            return Err(format!("invalid field name '{}'", f.name));
        }
        if RESERVED_FIELDS.contains(&f.name.as_str()) {
            return Err(format!("field name '{}' is reserved", f.name));
        }
        if !names.insert(f.name.as_str()) {
            return Err(format!("duplicate field '{}'", f.name));
        }
        validate_field(e, f, all)?;
    }
    let field = |n: &str| e.fields.iter().find(|f| f.name == n);
    let storable = |n: &str| field(n).is_some_and(|f| f.ftype.is_stored());
    if e.company_scoped && !field("company").is_some_and(|f| f.ftype == FieldType::Link && f.to.as_deref() == Some("Company")) {
        return Err("company_scoped entities need a 'company' link to Company".into());
    }
    if let Some(t) = &e.title_field
        && !storable(t)
    {
        return Err(format!("title_field '{t}' is not a field"));
    }
    let emailish = |f: &FieldDef| matches!(f.ftype, FieldType::Data | FieldType::Email);
    for o in &e.owner_fields {
        let ok = match o.split_once('.') {
            None => field(o).is_some_and(emailish),
            Some((t, cf)) => field(t)
                .filter(|f| f.ftype == FieldType::Table)
                .and_then(|f| f.of.as_deref())
                .and_then(|child| all.get(child))
                .is_some_and(|c| c.fields.iter().any(|f| f.name == cf && emailish(f))),
        };
        if !ok {
            return Err(format!("owner field '{o}' must be a data/email field holding a user's email, or table.field of one"));
        }
    }
    for group in e.unique.iter().chain(e.indexes.iter()) {
        if group.is_empty() {
            return Err("empty unique/index definition".into());
        }
        for n in group {
            if !storable(n) {
                return Err(format!("unique/index field '{n}' is not a stored field"));
            }
        }
    }
    for x in &e.exclusion {
        for n in x.fields.iter().chain([&x.from, &x.to]) {
            if !storable(n) {
                return Err(format!("exclusion field '{n}' is not a stored field"));
            }
        }
        let kind = |n: &str| field(n).map(|f| f.ftype);
        match (kind(&x.from), kind(&x.to)) {
            (Some(FieldType::Date), Some(FieldType::Date)) | (Some(FieldType::Datetime), Some(FieldType::Datetime)) => {}
            _ => return Err(format!("exclusion range fields '{}' and '{}' must both be dates or both date-times", x.from, x.to)),
        }
        for (n, values) in &x.when_not {
            if !storable(n) || !field(n).is_some_and(|f| matches!(f.ftype, FieldType::Select | FieldType::Data)) {
                return Err(format!("exclusion when_not field '{n}' must be a stored select or data field"));
            }
            let plain = |v: &String| !v.is_empty() && v.len() <= 60 && v.chars().all(|c| c.is_alphanumeric() || " _-.".contains(c));
            if values.is_empty() || !values.iter().all(plain) {
                return Err(format!("exclusion when_not values of '{n}' must be plain words (letters, digits, space, _ - .)"));
            }
        }
    }
    const SYSTEM_COLUMNS: &[&str] = &["number", "created_at", "updated_at", "docstatus", "workflow_state"];
    for n in e.list.columns.iter().chain(e.search_fields.iter()) {
        if !storable(n) && !SYSTEM_COLUMNS.contains(&n.as_str()) {
            return Err(format!("list/search field '{n}' is not a stored field"));
        }
    }
    if let Some(sort) = &e.list.sort {
        let mut parts = sort.split_whitespace();
        let col = parts.next().unwrap_or_default();
        let dir = parts.next().unwrap_or("asc");
        let system = ["created_at", "updated_at", "number"].contains(&col);
        if !(system || storable(col)) || !["asc", "desc"].contains(&dir) || parts.next().is_some() {
            return Err(format!("invalid list sort '{sort}'"));
        }
    }
    crate::effective::validate_form(e, &e.form)?;
    let mut rule_names = HashSet::new();
    for r in &e.rules {
        if !rule_names.insert(r.name.as_str()) {
            return Err(format!("duplicate rule '{}'", r.name));
        }
        Expr::compile(&r.check).map_err(|m| format!("rule {}: {m}", r.name))?;
    }
    let mut action_names = HashSet::new();
    for a in &e.actions {
        if !is_ident(&a.name) || ["submit", "cancel", "amend", "delete"].contains(&a.name.as_str()) {
            return Err(format!("invalid action name '{}'", a.name));
        }
        if !action_names.insert(a.name.as_str()) {
            return Err(format!("duplicate action '{}'", a.name));
        }
        if !["read", "write", "create", "submit", "cancel"].contains(&a.permission.as_str()) {
            return Err(format!("action {}: invalid permission {}", a.name, a.permission));
        }
        compile(&format!("action {}", a.name), &a.condition)?;
    }
    if let Some(n) = &e.numbering {
        validate_numbering(e, n)?;
    }
    Ok(())
}

fn validate_field(e: &EntityDef, f: &FieldDef, all: &HashMap<&str, &EntityDef>) -> Result<(), String> {
    let fname = &f.name;
    if f.ftype == FieldType::Secret {
        if e.kind == EntityKind::Child {
            return Err(format!("secret field '{fname}' must be on a top-level entity of a package"));
        }
        if f.in_list || f.in_filter || f.unique || f.default.is_some() || f.default_expr.is_some() || f.computed.is_some() || f.fetch_from.is_some() {
            return Err(format!("secret field '{fname}' cannot be listed, filtered, unique, defaulted, computed or fetched"));
        }
        if e.search_fields.contains(fname) {
            return Err(format!("secret field '{fname}' cannot be searched"));
        }
    }
    match f.ftype {
        FieldType::Link => {
            let to = f.to.as_deref().ok_or_else(|| format!("link field '{fname}' needs 'to'"))?;
            let target = all.get(to).ok_or_else(|| format!("link field '{fname}' targets unknown entity {to}"))?;
            if matches!(target.kind, EntityKind::Child) {
                return Err(format!("link field '{fname}' cannot target a child entity"));
            }
            if let Some(filters) = &f.link_filters {
                for k in filters.keys() {
                    if !target.fields.iter().any(|tf| &tf.name == k) {
                        return Err(format!("link field '{fname}': filter on unknown field {to}.{k}"));
                    }
                }
            }
        }
        FieldType::Table => {
            let of = f.of.as_deref().ok_or_else(|| format!("table field '{fname}' needs 'of'"))?;
            let child = all.get(of).ok_or_else(|| format!("table field '{fname}' uses unknown entity {of}"))?;
            if child.kind != EntityKind::Child {
                return Err(format!("table field '{fname}': {of} is not a child entity"));
            }
            if e.kind == EntityKind::Child {
                return Err(format!("table field '{fname}': child entities cannot nest tables"));
            }
            if f.unique || f.index || f.default.is_some() || f.computed.is_some() {
                return Err(format!("table field '{fname}' cannot be unique/indexed/defaulted/computed"));
            }
        }
        FieldType::Select if f.options.is_empty() => return Err(format!("select field '{fname}' needs options")),
        FieldType::Currency if f.computed.is_some() => {
            return Err(format!("currency field '{fname}' cannot be CEL-computed (use a native hook)"));
        }
        _ => {}
    }
    if f.to.is_some() && f.ftype != FieldType::Link {
        return Err(format!("field '{fname}': 'to' is only for links"));
    }
    if f.of.is_some() && f.ftype != FieldType::Table {
        return Err(format!("field '{fname}': 'of' is only for tables"));
    }
    if f.precision.is_some_and(|p| p > 9) {
        return Err(format!("field '{fname}': precision must be ≤ 9"));
    }
    if f.precision.is_some() && !f.ftype.is_decimal() {
        return Err(format!("field '{fname}': precision is only for decimal types"));
    }
    if f.max_length.is_some_and(|m| m == 0) {
        return Err(format!("field '{fname}': max_length must be > 0"));
    }
    if let (Some(min), Some(max)) = (f.min, f.max)
        && min > max
    {
        return Err(format!("field '{fname}': min > max"));
    }
    if let Some(cur) = &f.currency {
        let local = cur.strip_prefix("parent.").unwrap_or(cur);
        if !is_ident(local) {
            return Err(format!("field '{fname}': invalid currency reference {cur}"));
        }
        if !cur.starts_with("parent.") && !e.fields.iter().any(|x| &x.name == cur) {
            return Err(format!("field '{fname}': currency field {cur} does not exist"));
        }
    }
    if let Some(ff) = &f.fetch_from {
        let (link, target) = ff.split_once('.').ok_or_else(|| format!("field '{fname}': fetch_from must be link.field"))?;
        let lf = e
            .fields
            .iter()
            .find(|x| x.name == link && x.ftype == FieldType::Link)
            .ok_or_else(|| format!("field '{fname}': fetch_from link '{link}' not found"))?;
        let te = all.get(lf.to.as_deref().unwrap_or_default()).ok_or("fetch_from target missing")?;
        if !te.fields.iter().any(|x| x.name == target) && !["number"].contains(&target) {
            return Err(format!("field '{fname}': fetch_from target field {target} not found"));
        }
    }
    compile(&format!("{fname}.default_expr"), &f.default_expr)?;
    compile(&format!("{fname}.computed"), &f.computed)?;
    compile(&format!("{fname}.visible_if"), &f.visible_if)?;
    compile(&format!("{fname}.required_if"), &f.required_if)?;
    compile(&format!("{fname}.read_only_if"), &f.read_only_if)?;
    if let Some(g) = &f.perm_group
        && !is_ident(g)
    {
        return Err(format!("field '{fname}': invalid perm_group"));
    }
    Ok(())
}

fn validate_numbering(e: &EntityDef, n: &NumberingDef) -> Result<(), String> {
    let t = &n.template;
    if t.is_empty() || t.len() > 100 {
        return Err("numbering template must be 1..100 chars".into());
    }
    let mut has_seq = false;
    let mut rest = t.as_str();
    while let Some(start) = rest.find('{') {
        let end = rest[start..].find('}').ok_or("unclosed '{' in numbering template")? + start;
        let token = &rest[start + 1..end];
        match token {
            "YYYY" | "YY" | "MM" | "DD" | "FY" => {}
            _ if token.starts_with("seq:") => {
                let w: u32 = token[4..].parse().map_err(|_| "seq width must be a number")?;
                if !(1..=12).contains(&w) {
                    return Err("seq width must be 1..12".into());
                }
                has_seq = true;
            }
            _ if token.starts_with("field:") => {
                let f = &token[6..];
                if !e.fields.iter().any(|x| x.name == f && x.ftype.is_stored()) {
                    return Err(format!("numbering field {f} not found"));
                }
            }
            _ => return Err(format!("unknown numbering token {{{token}}}")),
        }
        rest = &rest[end + 1..];
    }
    if !has_seq {
        return Err("numbering template needs a {seq:N} token".into());
    }
    if let Some(df) = &n.date_field
        && !e.fields.iter().any(|x| &x.name == df && matches!(x.ftype, FieldType::Date | FieldType::Datetime))
    {
        return Err(format!("numbering date_field {df} must be a date field"));
    }
    Ok(())
}

pub(crate) fn validate_workflow(wf: &WorkflowDef, entities: &BTreeMap<String, (String, EntityDef)>) -> Result<(), String> {
    let (_, e) = entities.get(&wf.entity).ok_or_else(|| format!("unknown entity {}", wf.entity))?;
    if e.kind != EntityKind::Document && e.kind != EntityKind::Master {
        return Err("workflows apply to documents and masters".into());
    }
    let states: HashSet<&str> = wf.states.iter().map(|s| s.name.as_str()).collect();
    if states.len() != wf.states.len() || states.is_empty() {
        return Err("states must be non-empty and unique".into());
    }
    if !states.contains(wf.initial_state.as_str()) {
        return Err("initial_state is not a state".into());
    }
    for s in &wf.states {
        if !(0..=2).contains(&s.docstatus) || (s.docstatus != 0 && !e.submittable) {
            return Err(format!("state {}: invalid docstatus for this entity", s.name));
        }
    }
    let mut seen = HashSet::new();
    for t in &wf.transitions {
        if !states.contains(t.from.as_str()) || !states.contains(t.to.as_str()) {
            return Err(format!("transition {} uses unknown states", t.action));
        }
        if !seen.insert((t.action.as_str(), t.from.as_str())) {
            return Err(format!("duplicate transition {} from {}", t.action, t.from));
        }
        compile(&format!("transition {}", t.action), &t.condition)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(yaml_manifest: &str, entities: &str) -> Package {
        Package {
            manifest: serde_yaml::from_str(yaml_manifest).unwrap(),
            entities: serde_yaml::from_str(entities).unwrap(),
            sql: vec![],
            print_formats: vec![],
        }
    }

    const CORE: &str = r#"
- name: Company
  label: Company
  kind: master
  title_field: company_name
  fields:
    - {name: company_name, type: data, required: true, unique: true}
- name: Customer
  label: Customer
  kind: master
  fields:
    - {name: customer_name, type: data, required: true}
    - {name: company, type: link, to: Company}
- name: InvoiceItem
  label: Invoice Item
  kind: child
  fields:
    - {name: item_name, type: data}
    - {name: qty, type: decimal}
- name: Invoice
  label: Invoice
  kind: document
  submittable: true
  company_scoped: true
  numbering: {template: "INV-{YYYY}-{seq:5}", gapless: true, on: submit, date_field: posting_date}
  fields:
    - {name: company, type: link, to: Company, required: true}
    - {name: customer, type: link, to: Customer, required: true}
    - {name: posting_date, type: date, required: true}
    - {name: items, type: table, of: InvoiceItem}
  rules:
    - {name: has_items, check: "size(doc.items) > 0", message: "Add at least one item", on: submit}
"#;

    #[test]
    fn loads_valid_catalog() {
        let cat = Catalog::from_packages(vec![pkg("{package: core, version: '1.0'}", CORE)]).unwrap();
        assert!(cat.entity("Invoice").is_some());
        assert_eq!(cat.closure(&["core".into()]).unwrap(), vec!["core"]);
        assert_eq!(cat.fingerprint().len(), 64);
    }

    #[test]
    fn rejects_broken_definitions() {
        let bad_link = CORE.replace("to: Customer, required", "to: Nope, required");
        assert!(Catalog::from_packages(vec![pkg("{package: core, version: '1'}", &bad_link)]).is_err());
        let reserved = CORE.replace("name: customer_name", "name: version");
        assert!(Catalog::from_packages(vec![pkg("{package: core, version: '1'}", &reserved)]).is_err());
        let bad_rule = CORE.replace("size(doc.items) > 0", "size(doc.items >");
        assert!(Catalog::from_packages(vec![pkg("{package: core, version: '1'}", &bad_rule)]).is_err());
        let cyc = vec![pkg("{package: a, version: '1', depends: [b]}", "[]"), pkg("{package: b, version: '1', depends: [a]}", "[]")];
        assert!(Catalog::from_packages(cyc).is_err());
    }

    #[test]
    fn package_overlays_need_prefix_and_dependency() {
        let ok = pkg(
            "{package: india, version: '1', depends: [core], field_prefix: in_, overlays: [{entity: Customer, ops: [{op: add_field, field: {name: in_gstin, type: data}}]}]}",
            "[]",
        );
        assert!(Catalog::from_packages(vec![pkg("{package: core, version: '1'}", CORE), ok]).is_ok());
        let no_prefix = pkg(
            "{package: india, version: '1', depends: [core], field_prefix: in_, overlays: [{entity: Customer, ops: [{op: add_field, field: {name: gstin, type: data}}]}]}",
            "[]",
        );
        assert!(Catalog::from_packages(vec![pkg("{package: core, version: '1'}", CORE), no_prefix]).is_err());
        let no_dep = pkg(
            "{package: india, version: '1', field_prefix: in_, overlays: [{entity: Customer, ops: [{op: add_field, field: {name: in_gstin, type: data}}]}]}",
            "[]",
        );
        assert!(Catalog::from_packages(vec![pkg("{package: core, version: '1'}", CORE), no_dep]).is_err());
    }
}
