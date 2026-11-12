//! Loading and validating the set of packages available on this cluster.

use crate::expr::Expr;
use crate::types::*;
use crate::{MetaError, RESERVED_FIELDS, invalid, is_entity_name, is_ident, is_package_name};
use serde_json::Value;
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
    for (sub, ext) in [("entities", "yaml"), ("sql", "sql"), ("print", "html"), ("print", "json")] {
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
    let mut manifest: PackageManifest = serde_yaml::from_str(manifest_text).map_err(|e| MetaError::Yaml { path: "package.yaml".into(), msg: e.to_string() })?;
    // A layout format's file (`print/<name>.layout.json`) read into its document (BD-28).
    for d in manifest.documents.iter_mut() {
        let Some(file) = d.layout_file.take() else { continue };
        if !file.starts_with("print/") || !file.ends_with(".layout.json") || file[6..].contains('/') {
            return invalid(format!("document {}: layout_file is print/<name>.layout.json, not {file}", d.key));
        }
        let text = files.get(&file).ok_or_else(|| MetaError::Invalid(format!("document {}: {file} is not in the package", d.key)))?;
        if text.len() > crate::layout::MAX_LAYOUT_BYTES {
            return invalid(format!("document {}: {file} is larger than 256 KB", d.key));
        }
        let layout: crate::layout::Layout = serde_json::from_str(text).map_err(|e| MetaError::Yaml { path: file.clone(), msg: e.to_string() })?;
        if let Some((id, msg)) = layout.problems(false).into_iter().next() {
            return invalid(format!("document {}: {file}: {}{msg}", d.key, if id.is_empty() { String::new() } else { format!("{id}: ") }));
        }
        d.layout = Some(layout);
    }
    let (mut entities, mut sql, mut print_formats) = (Vec::new(), Vec::new(), Vec::new());
    for (path, text) in files {
        if let Some(name) = path.strip_prefix("entities/").filter(|n| n.ends_with(".yaml") && !n.contains('/')) {
            let yaml_err = |e: serde_yaml::Error| MetaError::Yaml { path: format!("entities/{name}"), msg: e.to_string() };
            // A list of entities or one, whatever comes first (a comment, `---`); parsed again as
            // what it is, so that errors keep their lines.
            if serde_yaml::from_str::<serde_yaml::Value>(text).is_ok_and(|v| v.is_sequence()) {
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
        // Each entity is validated with its package's dependencies installed (validate_packages),
        // where the fields other packages add to the entities it names are there too.
        let mut cat = Catalog { packages, order, entities, fingerprint: String::new() };
        cat.validate_packages()?;
        cat.fingerprint = cat.compute_fingerprint();
        Ok(cat)
    }

    /// Whether a package declares an entity's records as people's by an address or a number, not
    /// by a link: what a declaration that links to it reaches through.
    fn found_directly(&self, entity: &str) -> bool {
        self.packages.values().flat_map(|p| &p.manifest.personal_data).any(|d| {
            d.entity == entity
                && self.entities.get(entity).and_then(|(_, e)| e.fields.iter().find(|f| f.name == d.match_field)).is_some_and(|f| f.ftype != FieldType::Link)
        })
    }

    /// A package's approvals and automations (and its workflows' approvals) are on its own entities, and name only
    /// roles, message and document templates and settings entities it or a package it depends on ships, and no
    /// business calendar (a workspace's own).
    fn validate_defaults(&self, name: &str, p: &Package, deps: &[String]) -> Result<(), MetaError> {
        let ours = |e: &str| self.entities.get(e).filter(|(owner, _)| owner == name).map(|(_, d)| d);
        let theirs = || deps.iter().filter_map(|d| self.packages.get(d));
        let roles: HashSet<&str> = theirs().flat_map(|q| &q.manifest.roles).map(|r| r.name.as_str()).collect();
        let templates: HashSet<&str> =
            theirs().flat_map(|q| q.manifest.messages.iter().map(|m| m.key.as_str()).chain(q.manifest.documents.iter().map(|d| d.key.as_str()))).collect();
        let settings: HashSet<&str> =
            self.entities.iter().filter(|(_, (owner, e))| deps.contains(owner) && e.kind == EntityKind::Settings).map(|(n, _)| n.as_str()).collect();
        let check = |what: String, v: &Value| -> Result<(), MetaError> {
            match foreign(v, &roles, &templates, &settings) {
                Some(m) => invalid(format!("{name}: {what} names {m}, which neither {name} nor a package it depends on ships")),
                None => Ok(()),
            }
        };
        for wf in &p.manifest.workflows {
            for t in &wf.transitions {
                if let Some(a) = &t.approval {
                    check(format!("{}: transition {} from {}: approval", wf.entity, t.action, t.from), &serde_json::to_value(a).unwrap_or_default())?;
                }
            }
        }
        for a in &p.manifest.approvals {
            let what = format!("{}: approval on {}", a.entity, a.on.key());
            let Some(e) = ours(&a.entity) else { return invalid(format!("{name}: {what}: {} is not this package's entity", a.entity)) };
            if let DocumentAction::Action(action) = &a.on {
                if let Some(m) = crate::effective::action_approval_problem(e, action) {
                    return invalid(format!("{name}: {what}: {m}"));
                }
            } else if !e.submittable || p.manifest.workflows.iter().any(|w| w.entity == a.entity) {
                return invalid(format!("{name}: {what}: only a submittable entity without a workflow asks approval on submit or cancel"));
            }
            validate_approval(e, &format!("{name}: {what}"), None, &a.policy).map_err(MetaError::Invalid)?;
            check(what, &serde_json::to_value(&a.policy).unwrap_or_default())?;
        }
        let mut names = HashSet::new();
        const EVENTS: [&str; 8] = ["created", "updated", "submitted", "cancelled", "deleted", "transitioned", "scheduled", "opened"];
        for a in &p.manifest.automations {
            let what = format!("{}: automation {}", a.entity, a.name);
            if !is_ident(&a.name) || !names.insert(a.name.as_str()) {
                return invalid(format!("{name}: {what}: an automation's name is an identifier of its own"));
            }
            if ours(&a.entity).is_none() {
                return invalid(format!("{name}: {what}: {} is not this package's entity", a.entity));
            }
            if !EVENTS.contains(&a.event.as_str()) || (a.event == "scheduled") != a.schedule.is_some() || a.actions.is_empty() {
                return invalid(format!("{name}: {what}: it needs an event ({}), a schedule exactly when scheduled, and actions", EVENTS.join(", ")));
            }
            check(what, &a.definition())?;
        }
        Ok(())
    }

    /// The entities packages `deps` add fields to, with those fields (in dependency order, as a workspace has them).
    fn seen_by(&self, deps: &[String]) -> HashMap<String, EntityDef> {
        let mut out: HashMap<String, EntityDef> = HashMap::new();
        for p in self.order.iter().filter(|n| deps.contains(n)).filter_map(|n| self.packages.get(n)) {
            for ov in &p.manifest.overlays {
                for op in &ov.ops {
                    let OverlayOp::AddField { field, .. } = op else { continue };
                    let Some((_, base)) = self.entities.get(&ov.entity) else { continue };
                    let e = out.entry(ov.entity.clone()).or_insert_with(|| base.clone());
                    if !e.fields.iter().any(|f| f.name == field.name) {
                        e.fields.push(field.clone());
                    }
                }
            }
        }
        out
    }

    fn validate_packages(&self) -> Result<(), MetaError> {
        // Fields added to an entity, and by whom: two packages adding the same one would break
        // every workspace that installs both.
        let mut added: HashMap<(&str, &str), &str> = HashMap::new();
        for name in &self.order {
            let p = &self.packages[name];
            let deps = self.closure(std::slice::from_ref(name))?;
            if let Some(prefix) = &p.manifest.field_prefix
                && (!is_ident(prefix) || !prefix.ends_with('_') || prefix == "x_")
            {
                return invalid(format!("{name}: field_prefix must be an identifier ending in '_' (not 'x_')"));
            }
            let mut events = HashSet::new();
            for ev in &p.manifest.events {
                let well_formed = ev.name.starts_with(|c: char| c.is_ascii_lowercase())
                    && ev.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.');
                if !well_formed || !events.insert(ev.name.as_str()) {
                    return invalid(format!("{name}: event '{}' must be unique and written [a-z][a-z0-9_.]*", ev.name));
                }
                if let Some(entity) = &ev.entity
                    && !self.entities.get(entity).is_some_and(|(owner, _)| deps.contains(owner))
                {
                    return invalid(format!("{name}: event {} is about {entity}, which is not this package's or a dependency's", ev.name));
                }
                if ev.payload.as_ref().is_some_and(|v| !v.is_object()) {
                    return invalid(format!("{name}: event {}'s payload must be a JSON Schema object", ev.name));
                }
            }
            let mut messages = HashSet::new();
            for m in &p.manifest.messages {
                if let Some(problem) = m.problem() {
                    return invalid(format!("{name}: {problem}"));
                }
                if !m.key.strip_prefix(name.as_str()).is_some_and(|r| r.starts_with('.')) || !messages.insert(m.key.as_str()) {
                    return invalid(format!("{name}: message {} must be unique and start with '{name}.'", m.key));
                }
                if let Some(entity) = &m.entity
                    && !self.entities.get(entity).is_some_and(|(owner, _)| deps.contains(owner))
                {
                    return invalid(format!("{name}: message {} is about {entity}, which is not this package's or a dependency's", m.key));
                }
            }
            let mut documents = HashSet::new();
            for d in &p.manifest.documents {
                if let Some(problem) = d.problem() {
                    return invalid(format!("{name}: {problem}"));
                }
                if !d.key.strip_prefix(name.as_str()).is_some_and(|r| r.starts_with('.')) || !documents.insert(d.key.as_str()) {
                    return invalid(format!("{name}: document {} must be unique and start with '{name}.'", d.key));
                }
                if !self.entities.get(&d.entity).is_some_and(|(owner, _)| deps.contains(owner)) {
                    return invalid(format!("{name}: document {} is for {}, which is not this package's or a dependency's", d.key, d.entity));
                }
            }
            for ov in &p.manifest.overlays {
                let Some((owner, _)) = self.entities.get(&ov.entity) else {
                    return invalid(format!("{name}: overlay targets unknown entity {}", ov.entity));
                };
                if !deps.contains(owner) {
                    return invalid(format!("{name}: overlay on {} requires depending on package {owner}", ov.entity));
                }
                for op in &ov.ops {
                    if let OverlayOp::AddField { field, column, .. } = op {
                        // A column taken over from the entity keeps its name; anything else carries
                        // the package's prefix.
                        let ok = *column || p.manifest.field_prefix.as_deref().is_some_and(|px| field.name.starts_with(px));
                        if !ok {
                            return invalid(format!("{name}: field '{}' added to {} must start with the package field_prefix", field.name, ov.entity));
                        }
                        if let Some(other) = added.insert((ov.entity.as_str(), field.name.as_str()), name.as_str()) {
                            return invalid(format!("{name}: field '{}' is added to {} by {other} too", field.name, ov.entity));
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
                }
            }
            // Entities as this package sees them: with the fields it and the packages it depends on add to them.
            let seen = self.seen_by(&deps);
            let entity = |e: &str| seen.get(e).or_else(|| self.entities.get(e).map(|(_, d)| d));
            for role in &p.manifest.roles {
                if role.name.trim().is_empty() {
                    return invalid(format!("{name}: role without a name"));
                }
                for perm in &role.permissions {
                    if perm.entity != "*" && !self.entities.contains_key(&perm.entity) {
                        return invalid(format!("{name}: role {} references unknown entity {}", role.name, perm.entity));
                    }
                    let def = entity(&perm.entity);
                    perm.check_condition(|f| def.and_then(|e| e.fields.iter().find(|x| x.name == f)))
                        .map_err(|m| MetaError::Invalid(format!("{name}: role {} on {}: {m}", role.name, perm.entity)))?;
                    if let Some(via) = &perm.via {
                        crate::via::resolve(&perm.entity, via, entity)
                            .map_err(|m| MetaError::Invalid(format!("{name}: role {} on {}: {m}", role.name, perm.entity)))?;
                    }
                    if let Some(why) = perm.unknown_action(def) {
                        return invalid(format!("{name}: role {} on {}: {why}", role.name, perm.entity));
                    }
                    let personal = def.is_some_and(|e| {
                        crate::holds_personal_data(e, |n| self.packages.values().any(|p| p.manifest.personal_data.iter().any(|d| d.entity == n)))
                    });
                    if let Some(why) = role.portal.then(|| perm.portal_problem(personal)).flatten() {
                        return invalid(format!("{name}: portal role {}: {why}", role.name));
                    }
                }
            }
            for wf in &p.manifest.workflows {
                validate_workflow(wf, &self.entities).map_err(|m| MetaError::Invalid(format!("{name}: workflow {}: {m}", wf.name)))?;
            }
            self.validate_defaults(name, p, &deps)?;
            for pd in &p.manifest.personal_data {
                let Some(e) = entity(&pd.entity) else {
                    return invalid(format!("{name}: personal data of unknown entity {}", pd.entity));
                };
                if e.kind == EntityKind::Child {
                    return invalid(format!("{name}: personal data of {}: a child table's rows are not records of their own", pd.entity));
                }
                let field = |f: &str| e.fields.iter().find(|x| x.name == f);
                if pd.erase.is_empty() == pd.retain.is_none() {
                    return invalid(format!("{name}: personal data of {}: list the fields to erase, or say in `retain` why the records stay", pd.entity));
                }
                if let Some(c) = pd.condition.keys().find(|c| field(c).is_none()) {
                    return invalid(format!("{name}: personal data of {}: {c} is not a field", pd.entity));
                }
                // People held in a child table's rows: `table.field` names the table and the field
                // with their address. Only those rows are the person's: erasure removes them (the
                // table in `erase`) or blanks their fields (`table.field`), and leaves the record.
                if let Some((table, at)) = pd.match_field.split_once('.') {
                    let rows = field(table).filter(|f| f.ftype == FieldType::Table).and_then(|f| f.of.as_deref()).and_then(|c| self.entities.get(c));
                    let Some((_, rows)) = rows else {
                        return invalid(format!("{name}: personal data of {}: {table} is not a table of it", pd.entity));
                    };
                    let row_field = |f: &str| rows.fields.iter().find(|x| x.name == f);
                    if row_field(at).is_none_or(|f| !matches!(f.ftype, FieldType::Email | FieldType::Data)) {
                        return invalid(format!("{name}: personal data of {}: {at} is not an email (or data) field of {table}'s rows", pd.entity));
                    }
                    if pd.phone.is_some() || pd.name.is_some() {
                        return invalid(format!("{name}: personal data of {}: rows matched in a table take no phone or name", pd.entity));
                    }
                    let erasable = |f: &String| {
                        f == table
                            || f.strip_prefix(table)
                                .and_then(|r| r.strip_prefix('.'))
                                .and_then(row_field)
                                .is_some_and(|x| crate::types::ERASABLE_TYPES.contains(&x.ftype) && x.ftype != FieldType::Table)
                    };
                    if let Some(f) = pd.erase.iter().find(|f| !erasable(f)) {
                        return invalid(format!("{name}: personal data of {}: erase {f}: name {table} (its rows go) or {table}.<field> (blanked)", pd.entity));
                    }
                    continue;
                }
                // Records that hold no address are matched by the phone field they name, or by a link
                // to a record another declaration finds by address or number.
                let by_phone = pd.phone.as_deref() == Some(pd.match_field.as_str());
                if let Some(to) = field(&pd.match_field).filter(|f| f.ftype == FieldType::Link).and_then(|f| f.to.as_deref()) {
                    if !self.found_directly(to) {
                        return invalid(format!(
                            "{name}: personal data of {}: match links to {to}, which no declaration finds by address or number",
                            pd.entity
                        ));
                    }
                } else if field(&pd.match_field)
                    .is_none_or(|f| !(matches!(f.ftype, FieldType::Email | FieldType::Data) || by_phone && f.ftype == FieldType::Phone))
                {
                    return invalid(format!(
                        "{name}: personal data of {}: match must be an email (or data) field holding the address, the phone field where there is none, \
                         or a link to a record that is declared",
                        pd.entity
                    ));
                }
                for f in &pd.erase {
                    if field(f).is_none_or(|x| !crate::types::ERASABLE_TYPES.contains(&x.ftype)) {
                        return invalid(format!("{name}: personal data of {}: {f} is not a text, contact, date, JSON or table field", pd.entity));
                    }
                }
                if pd.phone.as_deref().is_some_and(|f| field(f).is_none_or(|x| !matches!(x.ftype, FieldType::Phone | FieldType::Data))) {
                    return invalid(format!("{name}: personal data of {}: phone must be a phone (or data) field", pd.entity));
                }
                // A name, or the names a field lists (the people a photograph shows), which may run long.
                if pd.name.as_deref().is_some_and(|f| field(f).is_none_or(|x| !matches!(x.ftype, FieldType::Data | FieldType::Text))) {
                    return invalid(format!("{name}: personal data of {}: name must be a data or text field", pd.entity));
                }
            }
            for m in &p.manifest.merge_links {
                if !is_ident(&m.table) || !is_ident(&m.column) || !self.entities.get(&m.to).is_some_and(|(_, e)| e.mergeable) {
                    return invalid(format!("{name}: merge_links: {}.{} must point at a mergeable entity, not {}", m.table, m.column, m.to));
                }
            }
            let mut backfills = HashSet::new();
            for b in &p.manifest.backfills {
                let ident = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
                if !ident(&b.name) || !backfills.insert(b.name.as_str()) {
                    return invalid(format!("{name}: backfill {:?} needs a name of its own, in lowercase letters, digits and _", b.name));
                }
                match (&b.table, &b.entity) {
                    (Some(t), None) if ident(t) => {}
                    (None, Some(e)) if self.entities.contains_key(e) => {
                        // A child's rows are in its parent's lines table: one parent field must carry it.
                        let is_child = self.entities.get(e).is_some_and(|(_, d)| d.kind == EntityKind::Child);
                        let parents = self
                            .entities
                            .values()
                            .flat_map(|(_, d)| d.fields.iter())
                            .filter(|f| f.ftype == FieldType::Table && f.of.as_deref() == Some(e.as_str()))
                            .count();
                        if is_child && parents != 1 {
                            return invalid(format!(
                                "{name}: backfill {}: {e} is a child entity held by {parents} lines fields here, not one: name the table instead",
                                b.name
                            ));
                        }
                    }
                    _ => return invalid(format!("{name}: backfill {}: name one table, or one entity this catalog has", b.name)),
                }
                if b.needs.trim().is_empty() || b.set.trim().is_empty() || b.batch.is_some_and(|n| !(1..=10_000).contains(&n)) {
                    return invalid(format!("{name}: backfill {}: say which rows need it, what it sets, and at most 10000 rows a batch", b.name));
                }
            }
            for t in &p.manifest.export_tables {
                if t.is_empty() || !t.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                    return invalid(format!("{name}: export_tables: {t:?} is not a table name"));
                }
            }
            for h in &p.manifest.handover {
                let Some((_, e)) = self.entities.get(&h.entity) else {
                    return invalid(format!("{name}: handover of unknown entity {}", h.entity));
                };
                let target = match h.field.split_once('.') {
                    None => e.fields.iter().find(|x| x.name == h.field),
                    Some((table, f)) => e
                        .fields
                        .iter()
                        .find(|x| x.name == table && x.ftype == FieldType::Table)
                        .and_then(|t| t.of.as_ref())
                        .and_then(|child| self.entities.get(child))
                        .and_then(|(_, c)| c.fields.iter().find(|x| x.name == f)),
                };
                if target.is_none_or(|f| !matches!(f.ftype, FieldType::Email | FieldType::Data)) {
                    return invalid(format!("{name}: handover of {}: {} must be an email (or data) field", h.entity, h.field));
                }
                if let Some(c) = h.condition.keys().find(|c| !e.fields.iter().any(|x| &x.name == *c)) {
                    return invalid(format!("{name}: handover of {}: {c} is not a field", h.entity));
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
                if !is_ident(&m.name) || m.name.starts_with("x_") {
                    return invalid(format!("{name}: invalid module name {} (x_ names are a workspace's own menus)", m.name));
                }
                // One way to do each thing: an app's tile, quick create and assistant notes are in its manifest.
                if m.has_menu_properties() {
                    return invalid(format!(
                        "{name}: module {}: color, description, create, assistant and portal are a workspace's own menu's (an app gives them in its manifest)",
                        m.name
                    ));
                }
            }
        }
        // Two packages setting one thing differently: the later would win by accident of order.
        // One that depends on the other has seen it, and means to.
        let mut set: BTreeMap<(String, String, String), (String, Value)> = BTreeMap::new();
        for name in &self.order {
            let needs = self.closure(std::slice::from_ref(name))?;
            for ov in &self.packages[name].manifest.overlays {
                for op in &ov.ops {
                    let (field, property, value) = match op {
                        OverlayOp::SetProperty { field, property, value } => (field.as_str(), property, value),
                        OverlayOp::SetEntityProperty { property, value } => ("", property, value),
                        _ => continue,
                    };
                    let key = (ov.entity.clone(), field.to_string(), property.clone());
                    match set.get(&key) {
                        Some((other, seen)) if seen != value && !needs.contains(other) => {
                            return invalid(format!("{name} and {other} set {} {field} {property} differently, and neither depends on the other", ov.entity));
                        }
                        _ => {
                            set.insert(key, (name.clone(), value.clone()));
                        }
                    }
                }
            }
        }
        // Every package works with only its dependencies installed: what it links to, overlays
        // (in dependency order, so it may place fields after its dependencies' ones), runs
        // workflows on and seeds is within reach.
        for name in &self.order {
            let schema = crate::effective::build(self, std::slice::from_ref(name), &[], 0)
                .map_err(|e| MetaError::Invalid(format!("{name} with only its dependencies: {e}")))?;
            if let Some(w) = schema.warnings.first() {
                return invalid(format!("{name} with only its dependencies: {}: {}", w.entity, w.message));
            }
            let p = &self.packages[name];
            let reach = |e: &str| schema.entities.contains_key(e);
            if let Some(s) = p.manifest.seed.iter().find(|s| !reach(&s.entity)) {
                return invalid(format!("{name}: seeds {}, which is not among its dependencies' entities", s.entity));
            }
            if let Some(d) = p.manifest.personal_data.iter().find(|d| !reach(&d.entity)) {
                return invalid(format!("{name}: personal data of {}, which is not among its dependencies' entities", d.entity));
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

    /// Links a role of `package` may fill in and whose target it may not read: a form field that
    /// cannot search. Roles that only read are not asked, because a title shows without that.
    pub fn links_out_of_reach(&self, package: &str) -> Vec<String> {
        let mut out = Vec::new();
        for role in self.packages.get(package).map(|p| p.manifest.roles.as_slice()).unwrap_or_default() {
            let reads = |e: &str| role.permissions.iter().any(|p| p.read && (p.entity == e || p.entity == "*"));
            for p in role.permissions.iter().filter(|p| p.write || p.create) {
                let mut pending: Vec<&EntityDef> = self.entity(&p.entity).into_iter().collect();
                while let Some(e) = pending.pop() {
                    for f in e.fields.iter().filter(|f| !f.read_only) {
                        match (f.ftype, &f.to, &f.of) {
                            (FieldType::Link, Some(to), _) if !reads(to) => out.push(format!("{}: {}.{} needs {to}", role.name, e.name, f.name)),
                            (FieldType::Table, _, Some(of)) => pending.extend(self.entity(of)),
                            _ => {}
                        }
                    }
                }
            }
        }
        out
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

/// One package naming an entity another package defines (`ferrum task deps_report`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct Reference {
    pub from: String,
    pub to: String,
    /// `link`, `table`, `overlay`, `role`, `seed`, `workflow`, `personal_data`, `print` or `module`.
    pub kind: &'static str,
    pub what: String,
}

impl Catalog {
    /// Names in entities' forms and list columns that neither the entity nor any package's overlay
    /// defines (`Entity.field`): typos, since a layout may place the fields other packages add
    /// (§13).
    pub fn unknown_form_fields(&self) -> Vec<String> {
        let mut added: HashSet<(&str, &str)> = HashSet::new();
        for p in self.packages.values() {
            for ov in &p.manifest.overlays {
                for op in &ov.ops {
                    if let OverlayOp::AddField { field, .. } = op {
                        added.insert((ov.entity.as_str(), field.name.as_str()));
                    }
                }
            }
        }
        let mut out = Vec::new();
        for (name, (_, e)) in &self.entities {
            let list = e.list.columns.iter().filter(|c| !crate::RESERVED_FIELDS.contains(&c.as_str()));
            for f in e.form.iter().flat_map(|s| &s.fields).chain(list) {
                if !e.fields.iter().any(|x| &x.name == f) && !added.contains(&(name.as_str(), f.as_str())) {
                    out.push(format!("{name}.{f}"));
                }
            }
        }
        out
    }

    /// Everything a package names that another package defines: what ties packages together, and
    /// what moving an entity or field between packages has to carry along.
    pub fn references(&self) -> Vec<Reference> {
        let mut out = Vec::new();
        for name in &self.order {
            let p = &self.packages[name];
            let mut add = |kind: &'static str, entity: &str, what: String| {
                if let Some((to, _)) = self.entities.get(entity).filter(|(to, _)| to != name) {
                    out.push(Reference { from: name.clone(), to: to.clone(), kind, what });
                }
            };
            let fields = |add: &mut dyn FnMut(&'static str, &str, String), entity: &str, f: &FieldDef| {
                if let Some(t) = &f.to {
                    add("link", t, format!("{entity}.{} → {t}", f.name));
                }
                if let Some(t) = &f.of {
                    add("table", t, format!("{entity}.{} → {t}", f.name));
                }
            };
            for e in &p.entities {
                for f in &e.fields {
                    fields(&mut add, &e.name, f);
                }
            }
            for ov in &p.manifest.overlays {
                for op in &ov.ops {
                    let kind = serde_json::to_value(op).ok().and_then(|v| v.get("op").and_then(|o| o.as_str()).map(str::to_string)).unwrap_or_default();
                    match op {
                        OverlayOp::AddField { field, column, .. } => {
                            add("overlay", &ov.entity, format!("{}.{}: {kind}{}", ov.entity, field.name, if *column { " (column)" } else { "" }));
                            fields(&mut add, &ov.entity, field);
                        }
                        _ => add("overlay", &ov.entity, format!("{}: {kind}", ov.entity)),
                    }
                }
            }
            for r in &p.manifest.roles {
                for perm in &r.permissions {
                    add("role", &perm.entity, format!("{}: {}", r.name, perm.entity));
                }
            }
            for s in &p.manifest.seed {
                add("seed", &s.entity, format!("{} ({} records)", s.entity, s.records.len()));
            }
            for w in &p.manifest.workflows {
                add("workflow", &w.entity, format!("{}: {}", w.name, w.entity));
            }
            for d in &p.manifest.personal_data {
                add("personal_data", &d.entity, d.entity.clone());
            }
            for (e, _) in &p.print_formats {
                add("print", e, e.clone());
            }
            for m in &p.manifest.modules {
                for i in &m.items {
                    add("module", i, format!("{}: {i}", m.name));
                }
            }
        }
        out
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

/// Compiles an expression of `on`, refusing what it reads through `links` unless `reads` (docs/builder/decisions.md
/// BD-14): there, each `links.<name>` is a link field of `on`, and `on` is not lines. Without `reads` the expression
/// is worked out where no transaction is at hand (and in the browser), so it reads no linked record.
pub(crate) fn compile(on: &EntityDef, what: &str, src: Option<&str>, reads: bool) -> Result<(), String> {
    let Some(s) = src else { return Ok(()) };
    let e = Expr::compile(s).map_err(|e| format!("{what}: {e}"))?;
    let Some(name) = e.links().first() else { return Ok(()) };
    if !reads {
        return Err(format!(
            "{what}: links.{name}: this is worked out without the linked records (also in the browser); rules, conditions, refusals, workflow guards, automations and targets read them"
        ));
    }
    if on.kind == EntityKind::Child {
        return Err(format!("{what}: links.{name}: a line's rules do not read linked records; write the rule on the record that holds the lines"));
    }
    if let Some(n) = e.links().iter().find(|n| !on.fields.iter().any(|f| &f.name == *n && f.ftype == FieldType::Link)) {
        return Err(format!("{what}: links.{n}: {n} is not a link field of {}", on.name));
    }
    Ok(())
}

/// Structural validation of one (possibly overlaid) entity against all known entities.
/// `custom`: a workspace's own entity, whose line entity may hold lines of its own (`effective::validate_custom_lines`
/// bounds them at two levels); a package's child entity holds no table.
pub(crate) fn validate_entity(e: &EntityDef, all: &HashMap<&str, &EntityDef>, custom: bool) -> Result<(), String> {
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
        validate_field(e, f, all, custom)?;
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
        compile(e, &format!("rule {}", r.name), Some(&r.check), true)?;
    }
    let mut action_names = HashSet::new();
    for a in &e.actions {
        // The record operations' names stay theirs, so a capability's id names one thing.
        if !is_ident(&a.name) || ["submit", "cancel", "amend", "delete", "create", "read", "list", "update"].contains(&a.name.as_str()) {
            return Err(format!("invalid action name '{}'", a.name));
        }
        if !action_names.insert(a.name.as_str()) {
            return Err(format!("duplicate action '{}'", a.name));
        }
        if !["read", "write", "create", "submit", "cancel"].contains(&a.permission.as_str()) {
            return Err(format!("action {}: invalid permission {}", a.name, a.permission));
        }
        compile(e, &format!("action {}", a.name), a.condition.as_deref(), true)?;
        if let Some(p) = a.params.iter().find(|p| {
            let files = p.ptype == "files";
            (p.max_files.is_some() || p.max_bytes.is_some() || !p.accept.is_empty()) && !files
                || files
                    && (p.max_files.is_some_and(|n| n == 0 || n > crate::MAX_PARAM_FILES) || p.max_bytes.is_some_and(|b| b == 0 || b > crate::MAX_PARAM_BYTES))
        }) {
            return Err(format!(
                "action {}: parameter '{}': limits on files belong to a files parameter (1 to {} files, each at most {} MB)",
                a.name,
                p.name,
                crate::MAX_PARAM_FILES,
                crate::MAX_PARAM_BYTES >> 20
            ));
        }
        if let Some(p) = a.params.iter().find(|p| p.secret && !["data", "text"].contains(&p.ptype.as_str())) {
            return Err(format!("action {}: secret parameter '{}' is data or text", a.name, p.name));
        }
        if let Some(c) = &a.contract {
            validate_contract(e, a, c)?;
        }
        validate_configured(e, a)?;
    }
    if e.actions.iter().filter(|a| !a.effects.is_empty()).count() > MAX_CONFIGURED_ACTIONS {
        return Err(format!("at most {MAX_CONFIGURED_ACTIONS} configured actions on one entity"));
    }
    if let Some(n) = &e.numbering {
        validate_numbering(e, n)?;
    }
    crate::effective::validate_targets(e)
}

/// Configured actions one entity holds, and effects and refusals one of them holds.
pub const MAX_CONFIGURED_ACTIONS: usize = 20;
pub const MAX_EFFECTS: usize = 10;

/// A configured action's own bounds; what its effects write is the kernel's to check (it knows them).
fn validate_configured(e: &EntityDef, a: &ActionDef) -> Result<(), String> {
    if a.effects.is_empty() {
        return if a.roles.is_empty() && a.refuse_if.is_empty() { Ok(()) } else { Err(format!("action {}: roles and refuse_if go with effects", a.name)) };
    }
    if a.effects.len() > MAX_EFFECTS {
        return Err(format!("action {}: at most {MAX_EFFECTS} effects", a.name));
    }
    if a.refuse_if.len() > MAX_EFFECTS {
        return Err(format!("action {}: at most {MAX_EFFECTS} refuse_if", a.name));
    }
    if a.read_only {
        return Err(format!("action {}: a read-only action has no effects", a.name));
    }
    if a.roles.iter().any(|r| r.trim().is_empty()) {
        return Err(format!("action {}: a role without a name", a.name));
    }
    for (i, r) in a.refuse_if.iter().enumerate() {
        if r.message.trim().is_empty() {
            return Err(format!("action {}: refuse_if {} says nothing", a.name, i + 1));
        }
        compile(e, &format!("action {}: refuse_if {}", a.name, i + 1), Some(&r.when), true)?;
    }
    Ok(())
}

/// An action's contract must agree with what the action is: a read-only one cannot change anything, any
/// other cannot claim to be pure, and what undoes it must be another action of the same entity.
fn validate_contract(e: &EntityDef, a: &ActionDef, c: &ActionContract) -> Result<(), String> {
    use ferrum_logic::capability::Effect;
    if let Some(effect) = c.effect {
        if a.read_only && effect.changes() {
            return Err(format!("action {}: a read-only action cannot be {effect:?}", a.name));
        }
        if !a.read_only && !effect.changes() {
            return Err(format!("action {}: an action that writes cannot be {effect:?}; mark it read_only", a.name));
        }
        if effect == Effect::HumanTask {
            return Err(format!("action {}: a person's task is the platform's, not an action's", a.name));
        }
    }
    if let Some(returns) = &c.returns
        && !returns.is_object()
    {
        return Err(format!("action {}: returns must be a JSON Schema object", a.name));
    }
    if let Some(undo) = &c.compensate_with
        && (undo == &a.name || !e.actions.iter().any(|x| &x.name == undo))
    {
        return Err(format!("action {}: compensate_with names {undo}, which is not another action of {}", a.name, e.name));
    }
    if c.completion.as_deref().is_some_and(|t| t.trim().is_empty()) {
        return Err(format!("action {}: completion says nothing", a.name));
    }
    if let Some(once) = &c.answer_once {
        let declared = c.returns.as_ref().and_then(|r| r.get("properties")).and_then(Value::as_object);
        if once.fields.is_empty() || once.fields.iter().any(|f| !is_ident(f) || declared.is_some_and(|d| !d.contains_key(f))) {
            return Err(format!("action {}: answer_once names fields of what it returns", a.name));
        }
    }
    Ok(())
}

/// A rollup totals one of this entity's own tables: a sum of a numeric row field into a numeric field
/// that can hold it (decimals only into a decimal field), or a count into any numeric field.
fn validate_rollup(e: &EntityDef, f: &FieldDef, r: &RollupDef, all: &HashMap<&str, &EntityDef>) -> Result<(), String> {
    let fname = &f.name;
    if !f.ftype.is_numeric() {
        return Err(format!("rollup field '{fname}' must be a number field"));
    }
    if f.computed.is_some() {
        return Err(format!("field '{fname}' cannot be both computed and a rollup"));
    }
    let table = e.fields.iter().find(|t| t.name == r.table && t.ftype == FieldType::Table);
    let child = table.and_then(|t| all.get(t.of.as_deref()?)).ok_or_else(|| format!("rollup field '{fname}': {} is not a table of {}", r.table, e.name))?;
    match (r.op, r.field.as_deref()) {
        (RollupOp::Count, None) => Ok(()),
        (RollupOp::Count, Some(_)) => Err(format!("rollup field '{fname}': count takes no field")),
        (RollupOp::Sum, None) => Err(format!("rollup field '{fname}': sum needs the row field it adds up")),
        (RollupOp::Sum, Some(rf)) => {
            let row = child.fields.iter().find(|c| c.name == rf).ok_or_else(|| format!("rollup field '{fname}': {rf} is not a field of {}", child.name))?;
            if !row.ftype.is_numeric() {
                return Err(format!("rollup field '{fname}': {}.{rf} is not a number", child.name));
            }
            if row.ftype.is_decimal() && !f.ftype.is_decimal() {
                return Err(format!("rollup field '{fname}': a sum of decimals needs a decimal field"));
            }
            Ok(())
        }
    }
}

fn validate_field(e: &EntityDef, f: &FieldDef, all: &HashMap<&str, &EntityDef>, custom: bool) -> Result<(), String> {
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
            if e.kind == EntityKind::Child && !custom {
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
    if let Some(r) = &f.rollup {
        validate_rollup(e, f, r, all)?;
    }
    if f.to.is_some() && f.ftype != FieldType::Link {
        return Err(format!("field '{fname}': 'to' is only for links"));
    }
    if let Some(w) = f.widget.as_deref()
        && !(w == "studio_query" && f.ftype == FieldType::Json)
    {
        return Err(format!("field '{fname}': widget '{w}' is not one forms know (studio_query, on a json field)"));
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
    compile(e, &format!("{fname}.default_expr"), f.default_expr.as_deref(), false)?;
    compile(e, &format!("{fname}.computed"), f.computed.as_deref(), false)?;
    compile(e, &format!("{fname}.visible_if"), f.visible_if.as_deref(), false)?;
    compile(e, &format!("{fname}.required_if"), f.required_if.as_deref(), false)?;
    compile(e, &format!("{fname}.read_only_if"), f.read_only_if.as_deref(), false)?;
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
        if let Some(f) = s.editable.iter().chain(&s.required).find(|f| !e.fields.iter().any(|x| &x.name == *f)) {
            return Err(format!("state {}: {f} is not a field", s.name));
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
        compile(e, &format!("transition {}", t.action), t.condition.as_deref(), true)?;
        // What it asks for and does (BD-19): a button's bounds; what effects write is the kernel's to check.
        if t.effects.len() > MAX_EFFECTS {
            return Err(format!("transition {}: at most {MAX_EFFECTS} effects", t.action));
        }
        let mut names = HashSet::new();
        if let Some(p) = t.params.iter().find(|p| p.name.trim().is_empty() || !names.insert(p.name.as_str())) {
            return Err(format!("transition {}: parameter '{}' is unnamed or named twice", t.action, p.name));
        }
        if let Some(p) = t.params.iter().find(|p| p.secret) {
            return Err(format!("transition {}: parameter '{}' cannot be secret: a transition keeps what it is given", t.action, p.name));
        }
        if let Some(p) = &t.approval {
            validate_approval(e, &format!("transition {}: approval", t.action), Some((wf, &t.from)), p)?;
        }
        for path in &t.not_creator_of {
            let link = |fields: &[crate::types::FieldDef], name: &str| fields.iter().any(|f| f.name == name && f.ftype == FieldType::Link && f.to.is_some());
            let ok = match path.split_once('.') {
                None => link(&e.fields, path),
                Some((table, field)) => e
                    .fields
                    .iter()
                    .find(|f| f.name == table && f.ftype == FieldType::Table)
                    .and_then(|f| f.of.as_deref())
                    .and_then(|child| entities.get(child))
                    .is_some_and(|(_, c)| link(&c.fields, field)),
            };
            if !ok {
                return Err(format!("transition {}: {path} is not a link (a field, or table.field)", t.action));
            }
        }
    }
    Ok(())
}

/// The first role, message or document template, settings entity or business calendar `v` (a policy or an automation
/// as JSON) names that is not among those given: `role X`, `template X`, `settings entity X` or `calendar X`.
fn foreign(v: &Value, roles: &HashSet<&str>, templates: &HashSet<&str>, settings: &HashSet<&str>) -> Option<String> {
    let text = |s: &str| -> Option<String> {
        if let Some(r) = s.strip_prefix("role:").filter(|r| !roles.contains(r)) {
            return Some(format!("role {r}"));
        }
        s.match_indices("settings.").find_map(|(i, _)| {
            let n: String = s[i + 9..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            (n.starts_with(|c: char| c.is_ascii_uppercase()) && !settings.contains(n.as_str())).then(|| format!("settings entity {n}"))
        })
    };
    match v {
        Value::String(s) => text(s),
        Value::Array(a) => a.iter().find_map(|x| foreign(x, roles, templates, settings)),
        Value::Object(o) => {
            let named = |k: &str| o.get(k).and_then(Value::as_str);
            if let Some(r) = named("role").filter(|r| !roles.contains(r)) {
                return Some(format!("role {r}"));
            }
            if let Some(c) = named("calendar") {
                return Some(format!("calendar {c}"));
            }
            // A template by its key (`message` of a policy or an e-mail, `template` of a message or a document); an
            // e-mail's own `template` is a body, never a key.
            let key = |t: &&str| !t.contains(|c: char| c.is_whitespace() || c == '{');
            let keyed = named("message").or(named("template")).filter(key);
            if let Some(t) = keyed.filter(|t| !templates.contains(t)) {
                return Some(format!("template {t}"));
            }
            o.values().find_map(|x| foreign(x, roles, templates, settings))
        }
        _ => None,
    }
}

/// Steps one approval policy holds.
pub const MAX_APPROVAL_STEPS: usize = 10;

/// An approval policy (`what` says where) on a transition of `wf` from a state, or else on a document's submit or
/// cancel (BD-19): what it names is there, what it says compiles, each fault said with its step's place. Roles and
/// business calendars are the workspace's, checked by the kernel when it is saved.
pub(crate) fn validate_approval(e: &EntityDef, what: &str, wf: Option<(&WorkflowDef, &str)>, p: &crate::types::ApprovalPolicy) -> Result<(), String> {
    if p.steps.is_empty() || p.steps.len() > MAX_APPROVAL_STEPS {
        return Err(format!("{what}: 1 to {MAX_APPROVAL_STEPS} steps"));
    }
    if let Some(f) = p.reset_on.iter().find(|f| !e.fields.iter().any(|x| &x.name == *f)) {
        return Err(format!("{what}: reset_on: {f} is not a field"));
    }
    if let Some(f) = p.show.iter().find(|f| !e.fields.iter().any(|x| &x.name == *f)) {
        return Err(format!("{what}: show: {f} is not a field"));
    }
    match (&p.on_reject, wf) {
        (Some(s), Some((wf, from))) => {
            let status = |name: &str| wf.states.iter().find(|x| x.name == name).map(|x| x.docstatus);
            if status(s).is_none() || status(s) != status(from) {
                return Err(format!("{what}: on_reject: {s} is not a state of the same document status as {from}"));
            }
        }
        (Some(_), None) => return Err(format!("{what}: on_reject: a rejected submit or cancel leaves the document as it is")),
        _ => {}
    }
    let who = |at: &str, a: &crate::types::Approvers| -> Result<(), String> {
        if [&a.role, &a.field, &a.expr].iter().filter(|x| x.is_some()).count() != 1 {
            return Err(format!("{at}: names exactly one of role, field and expr"));
        }
        if let Some(f) = a.field.as_ref().filter(|f| !e.fields.iter().any(|x| &x.name == *f)) {
            return Err(format!("{at}: {f} is not a field"));
        }
        compile(e, at, a.expr.as_deref(), false)
    };
    for (i, s) in p.steps.iter().enumerate() {
        let at = format!("{what} step {}", i + 1);
        if s.label.trim().is_empty() {
            return Err(format!("{at}: a step needs a label"));
        }
        who(&format!("{at} approvers"), &s.approvers)?;
        if s.need == crate::types::ApprovalNeed::Count(0) {
            return Err(format!("{at}: needs at least one approval"));
        }
        if let crate::types::ApprovalNeed::Expr(x) = &s.need {
            compile(e, &format!("{at} need"), Some(x), false)?;
        }
        compile(e, &format!("{at} when"), s.when.as_deref(), false)?;
        if s.escalate_to.is_some() != s.escalate_after.is_some() {
            return Err(format!("{at}: escalate_after and escalate_to go together"));
        }
        if let Some(to) = &s.escalate_to {
            who(&format!("{at} escalate_to"), to)?;
        }
        for d in [&s.remind_after, &s.escalate_after].into_iter().flatten() {
            match ferrum_logic::target::within(&serde_json::Value::String(d.clone())) {
                Ok(Some(_)) => {}
                _ => return Err(format!("{at}: {d} is not a time (seconds or an ISO 8601 duration such as PT48H)")),
            }
        }
        if let Some(a) = s.not_who_took.iter().find(|a| !wf.is_some_and(|(wf, _)| wf.transitions.iter().any(|x| &x.action == *a))) {
            return Err(format!("{at}: not_who_took: {a} is not a transition of this workflow"));
        }
    }
    if p.steps.last().is_some_and(|s| s.with_next) {
        return Err(format!("{what} step {}: with_next, but no step comes after it", p.steps.len()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Linked records in expressions (BD-14): read where a transaction is at hand, by link fields named in the
    /// expression; refused when saved anywhere else.
    #[test]
    fn linked_records_are_read_where_they_may_be() {
        let def = |extra: serde_json::Value| -> EntityDef {
            let mut v = serde_json::json!({"name": "XApp", "label": "Application", "kind": "document", "fields": [
                {"name": "type", "type": "link", "to": "XType"}, {"name": "office", "type": "link", "to": "XType"}, {"name": "note", "type": "data"}]});
            for (k, x) in extra.as_object().unwrap() {
                if k == "field" {
                    v["fields"].as_array_mut().unwrap().push(x.clone());
                } else {
                    v[k] = x.clone();
                }
            }
            serde_json::from_value(v).unwrap()
        };
        let target: EntityDef = serde_json::from_value(serde_json::json!({"name": "XType", "label": "Type", "kind": "master", "fields": []})).unwrap();
        let all = HashMap::from([("XType", &target)]);
        let check = |extra: serde_json::Value| validate_entity(&def(extra), &all, true);
        let rule = |c: &str| serde_json::json!({"rules": [{"name": "r", "check": c, "message": "m"}]});
        check(rule("links.type == null || links.type.documents.all(d, d.document != '') && links.office != null")).expect("two link fields");
        check(serde_json::json!({"actions": [{"name": "x_go", "label": "Go", "condition": "links.type != null",
            "refuse_if": [{"when": "links.office == null", "message": "an office"}], "effects": [{"type": "notify", "to": "owner", "title": "'t'"}]}]}))
        .expect("a condition and a refusal");
        let e = check(rule("links.note != null")).unwrap_err();
        assert!(e.contains("links.note: note is not a link field of XApp"), "{e}");
        let e = check(rule("links['type'] != null")).unwrap_err();
        assert!(e.contains("links.<link field>"), "{e}");
        let six = "links.a == null && links.b == null && links.c == null && links.d == null && links.e == null && links.f == null";
        assert!(check(rule(six)).unwrap_err().contains("at most 5"));
        for (prop, src) in [("computed", "links.type.name"), ("required_if", "links.type != null"), ("default_expr", "links.type.name")] {
            let e = check(serde_json::json!({"field": {"name": "x", "type": "data", prop: src}})).unwrap_err();
            assert!(e.contains("x.") && e.contains("without the linked records"), "{prop}: {e}");
        }
        let e = check(serde_json::json!({"form": [{"fields": ["note"], "visible_if": "links.type != null"}]})).unwrap_err();
        assert!(e.contains("without the linked records"), "{e}");
        let mut line = def(rule("links.type != null"));
        line.kind = EntityKind::Child;
        assert!(validate_entity(&line, &all, true).unwrap_err().contains("a line's rules do not read linked records"));
        let wf: WorkflowDef = serde_json::from_value(serde_json::json!({"name": "w", "entity": "XApp", "initial_state": "A",
            "states": [{"name": "A"}, {"name": "B"}], "transitions": [{"action": "go", "from": "A", "to": "B", "condition": "links.note != null"}]}))
        .unwrap();
        let entities = BTreeMap::from([("XApp".to_string(), ("tenant".to_string(), def(serde_json::json!({}))))]);
        assert!(validate_workflow(&wf, &entities).unwrap_err().contains("note is not a link field"));
    }

    #[test]
    fn an_entity_file_may_start_with_a_comment() {
        let files: BTreeMap<String, String> = [
            ("package.yaml", "package: inns\nversion: 0.1.0\ndepends: [core]\n"),
            ("entities/both.yaml", "# Two of them.\n\n- {name: InnRoom, label: Room, kind: master, fields: [{name: number, type: data}]}\n- {name: InnGuest, label: Guest, kind: master, fields: [{name: name, type: data}]}\n"),
            ("entities/one.yaml", "# Just one.\nname: InnStay\nlabel: Stay\nkind: master\nfields: [{name: guest, type: data}]\n"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let p = load_package_files(&files).expect("both files read");
        let mut names: Vec<&str> = p.entities.iter().map(|e| e.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["InnGuest", "InnRoom", "InnStay"]);
    }

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
    fn a_studio_query_is_built_only_in_a_json_field() {
        let with = |field: &str| {
            CORE.replace(
                "    - {name: customer_name, type: data, required: true}",
                &format!("    - {{name: customer_name, type: data, required: true}}\n    - {field}"),
            )
        };
        let core = |s: &str| Catalog::from_packages(vec![pkg("{package: core, version: '1'}", s)]);
        let cat = core(&with("{name: spec, type: json, widget: studio_query}")).unwrap();
        assert_eq!(cat.entity("Customer").unwrap().fields.iter().find(|f| f.name == "spec").unwrap().widget.as_deref(), Some("studio_query"));
        let err = core(&with("{name: spec, type: data, widget: studio_query}")).unwrap_err();
        assert!(err.to_string().contains("widget 'studio_query' is not one forms know"), "{err}");
        assert!(core(&with("{name: spec, type: json, widget: chart}")).is_err());
    }

    #[test]
    fn package_overlays_need_prefix_and_dependency() {
        let ok = pkg(
            "{package: india, version: '1', depends: [core], field_prefix: in_, overlays: [{entity: Customer, ops: [{op: add_field, field: {name: in_gstin, type: data}}]}]}",
            "[]",
        );
        assert!(Catalog::from_packages(vec![pkg("{package: core, version: '1'}", CORE), ok]).is_ok());

        // Two packages setting one thing differently need one to have seen the other.
        let overlay = |name: &str, depends: &str, label: &str| {
            pkg(
                &format!(
                    "{{package: {name}, version: '1', depends: [{depends}], overlays: [{{entity: Customer, ops: [{{op: set_property, field: customer_name, property: label, value: {label}}}]}}]}}"
                ),
                "",
            )
        };
        let core = || pkg("{package: core, version: '1'}", CORE);
        let conflict = Catalog::from_packages(vec![core(), overlay("one", "core", "One"), overlay("two", "core", "Two")]);
        assert!(conflict.is_err_and(|e| e.to_string().contains("differently")));
        assert!(Catalog::from_packages(vec![core(), overlay("one", "core", "Same"), overlay("two", "core", "Same")]).is_ok(), "they agree");
        assert!(Catalog::from_packages(vec![core(), overlay("one", "core", "One"), overlay("two", "one", "Two")]).is_ok(), "one builds on the other");
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

    /// A package with one document whose actions are `actions` (YAML flow sequence).
    fn with_actions(actions: &str) -> Result<Catalog, MetaError> {
        let entities = format!("- {{name: Hold, label: Hold, kind: document, submittable: true, fields: [{{name: qty, type: int}}], actions: {actions}}}");
        Catalog::from_packages(vec![pkg("{package: core, version: '1'}", CORE), pkg("{package: holds, version: '0.1.0', depends: [core]}", &entities)])
    }

    #[test]
    fn an_action_contract_agrees_with_the_action() {
        assert!(with_actions("[{name: place, label: Place, contract: {effect: transactional_action, completion: The hold is placed}}]").is_ok());
        assert!(with_actions("[{name: peek, label: Peek, read_only: true, contract: {effect: query}}]").is_ok());
        let refused = |actions: &str| with_actions(actions).unwrap_err().to_string();
        assert!(refused("[{name: peek, label: Peek, read_only: true, contract: {effect: transactional_action}}]").contains("read-only"));
        assert!(refused("[{name: place, label: Place, contract: {effect: pure}}]").contains("writes"), "a write cannot call itself pure");
        assert!(refused("[{name: place, label: Place, contract: {effect: human_task}}]").contains("platform"));
        assert!(refused("[{name: place, label: Place, contract: {compensate_with: release}}]").contains("release"), "an undo that does not exist");
        assert!(refused("[{name: place, label: Place, contract: {compensate_with: place}}]").contains("place"), "an action cannot undo itself");
        assert!(with_actions("[{name: place, label: Place, contract: {compensate_with: release}}, {name: release, label: Release}]").is_ok());
        assert!(refused("[{name: place, label: Place, contract: {returns: 3}}]").contains("returns"));
        // A secret parameter is text; what is given once is a field of what it returns.
        assert!(with_actions("[{name: redeem, label: Redeem, params: [{name: pin, label: PIN, secret: true}]}]").is_ok());
        assert!(refused("[{name: redeem, label: Redeem, params: [{name: pin, label: PIN, type: int, secret: true}]}]").contains("data or text"));
        let once = "contract: {returns: {type: object, properties: {code: {type: string}}}, answer_once: {fields: [code]}}";
        assert!(with_actions(&format!("[{{name: issue, label: Issue, {once}}}]")).is_ok());
        assert!(refused(&format!("[{{name: issue, label: Issue, {}}}]", once.replace("[code]", "[pin]"))).contains("answer_once"));
        let unknown = serde_yaml::from_str::<ActionDef>("{name: place, label: Place, contract: {effect: query, sideways: true}}");
        assert!(unknown.unwrap_err().to_string().contains("sideways"), "unknown keys are refused when the package is read");
    }

    #[test]
    fn the_record_operations_keep_their_names() {
        for name in ["create", "read", "list", "update", "delete", "submit"] {
            assert!(with_actions(&format!("[{{name: {name}, label: X}}]")).is_err(), "{name}");
        }
    }

    #[test]
    fn declared_events_are_checked() {
        let with_events = |events: &str| {
            Catalog::from_packages(vec![
                pkg("{package: core, version: '1'}", CORE),
                pkg(&format!("{{package: supply, version: '0.1.0', depends: [core], events: {events}}}"), "[]"),
            ])
        };
        assert!(with_events("[{name: order_acknowledged, label: Order acknowledged, entity: Invoice, payload: {type: object}}]").is_ok());
        assert!(with_events("[{name: Order-Ack, label: X}]").is_err(), "a badly written name");
        assert!(with_events("[{name: a, label: X}, {name: a, label: Y}]").is_err(), "twice");
        assert!(with_events("[{name: a, label: X, entity: Nowhere}]").is_err(), "about an entity nobody has");
        assert!(with_events("[{name: a, label: X, payload: 3}]").is_err());
    }
}
