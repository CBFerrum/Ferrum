//! Active releases' rules, ready to run on records (docs/logic/rules.md). Pure like the rest of this crate:
//! the kernel hands in the record, the person, the time and the linked records rules declared, and applies
//! what comes back, so the builder's simulator and the server run one evaluator (L-05).
//!
//! * Derivations of an entity, from every active release, run in the order their reads and writes need: a
//!   rule reading a field another fills runs after it. A cycle is refused when rules are prepared, which is
//!   when a release is activated.
//! * A value or a check that cannot be worked out refuses the save and says so: a rule that cannot run as it
//!   was approved does not run some other way, and does not quietly not run.
//! * A rule reads its record's unrestricted fields, the person saving, the time, its parameters and the
//!   linked records it declared, nothing else; it writes only the customer's own fields.

use crate::compile::{EntityShape, Release, parameter_type};
use crate::expr::{self, Env, Facts, Ty, Val};
use crate::solution::{Aggregate, AggregateOp, Condition, Phase, Rule};
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Linked records one rule may read, and fields of each.
pub const MAX_LOOKUPS: usize = 4;
pub const MAX_LOOKUP_FIELDS: usize = 16;
/// Linked records one save reads for all its entity's rules together; and, apart, sets of records it counts
/// or sums.
pub const MAX_READS_PER_SAVE: usize = 8;
/// Sets of records one rule counts or sums, and conditions of each.
pub const MAX_AGGREGATES: usize = 4;
pub const MAX_FILTER: usize = 8;

/// The person, as rules read them.
pub fn user_type() -> Ty {
    Ty::Record(BTreeMap::from([("id".into(), Ty::Text), ("email".into(), Ty::Text), ("roles".into(), Ty::List(Box::new(Ty::Text)))]))
}

/// The record's fields a rule may read: restricted ones are not there to name.
pub fn readable(shape: &EntityShape) -> Ty {
    Ty::Record(shape.fields.iter().filter(|(_, f)| !f.restricted).map(|(k, f)| (k.clone(), f.ty.clone())).collect())
}

/// What a rule's expressions may name. `old`, the stored version, only in validation: a derivation computes
/// from the record as it is.
pub fn env(shape: &EntityShape, phase: Phase, params: &Ty, lookups: &BTreeMap<String, Ty>, aggregates: &BTreeMap<String, Ty>) -> Env {
    let doc = readable(shape);
    let env = Env::default()
        .with("doc", doc.clone())
        .with("parameters", params.clone())
        .with("user", user_type())
        .with("today", Ty::Date)
        .with("now", Ty::Instant)
        .with("lookups", Ty::Record(lookups.clone()))
        .with("aggregates", Ty::Record(aggregates.clone()));
    if phase == Phase::Validate { env.with("old", doc) } else { env }
}

/// A type as a release's lock writes it down.
pub fn lock_name(ty: &Ty) -> String {
    format!("{ty:?}")
}

/// The linked records a rule declared, typed, or what is wrong with them.
pub fn lookup_types(rule: &Rule, shape: &EntityShape, target: &dyn Fn(&str) -> Option<EntityShape>) -> Result<BTreeMap<String, Ty>, String> {
    if rule.lookups.len() > MAX_LOOKUPS {
        return Err(format!("{}: a rule reads at most {MAX_LOOKUPS} linked records", rule.id));
    }
    let mut out = BTreeMap::new();
    for l in &rule.lookups {
        let Some(to) = shape.fields.get(&l.link).and_then(|f| f.link.clone()) else {
            return Err(format!("{}: {} is not a link of {}", rule.id, l.link, rule.entity));
        };
        if shape.fields.get(&l.link).is_some_and(|f| f.restricted) {
            return Err(format!("{}: {} is restricted, so a rule may not follow it", rule.id, l.link));
        }
        let Some(linked) = target(&to) else { return Err(format!("{}: {to} is not an entity a rule can read", rule.id)) };
        if l.fields.is_empty() || l.fields.len() > MAX_LOOKUP_FIELDS {
            return Err(format!("{}: a lookup names between 1 and {MAX_LOOKUP_FIELDS} fields", rule.id));
        }
        let mut fields = BTreeMap::new();
        for f in &l.fields {
            match linked.fields.get(f) {
                Some(s) if s.restricted => return Err(format!("{}: {to}.{f} is restricted: a rule may not read it", rule.id)),
                Some(s) => {
                    fields.insert(f.clone(), s.ty.clone());
                }
                None => return Err(format!("{}: {to} has no field {f}", rule.id)),
            }
        }
        if out.insert(l.name.clone(), Ty::Record(fields)).is_some() {
            return Err(format!("{}: two lookups are called {}", rule.id, l.name));
        }
    }
    Ok(out)
}

/// The sets of records a rule counts or sums, typed (`count` a whole number, `exists` yes/no, `sum` a decimal),
/// or what is wrong with them.
pub fn aggregate_types(rule: &Rule, shape: &EntityShape, target: &dyn Fn(&str) -> Option<EntityShape>) -> Result<BTreeMap<String, Ty>, String> {
    let id = &rule.id;
    if rule.aggregates.len() > MAX_AGGREGATES {
        return Err(format!("{id}: a rule counts or sums at most {MAX_AGGREGATES} sets of records"));
    }
    let mut out = BTreeMap::new();
    for a in &rule.aggregates {
        if rule.lookups.iter().any(|l| l.name == a.name) || out.contains_key(&a.name) {
            return Err(format!("{id}: two lookups or aggregates are called {}", a.name));
        }
        let pointed = match a.to_link() {
            None => rule.entity.clone(),
            Some(to) => match shape.fields.get(to) {
                Some(f) if f.restricted => return Err(format!("{id}: {to} is restricted, so a rule may not follow it")),
                Some(f) if f.link.is_some() => f.link.clone().unwrap_or_default(),
                _ => return Err(format!("{id}: {to} is not a link of {}", rule.entity)),
            },
        };
        let Some(of) = target(&a.entity) else { return Err(format!("{id}: {} is not an entity a rule can read", a.entity)) };
        let field = |f: &str| match of.fields.get(f) {
            Some(s) if s.restricted => Err(format!("{id}: {}.{f} is restricted: a rule may not read it", a.entity)),
            Some(s) => Ok(s.clone()),
            None => Err(format!("{id}: {} has no field {f}", a.entity)),
        };
        if field(&a.link)?.link.as_deref() != Some(pointed.as_str()) {
            return Err(format!("{id}: {}.{} is not a link to {pointed}", a.entity, a.link));
        }
        let ty = match (a.op, &a.field) {
            (AggregateOp::Sum, Some(f)) => match field(f)?.ty {
                Ty::Int | Ty::Decimal => Ty::Decimal,
                other => return Err(format!("{id}: {}.{f} is {other}: a sum adds up numbers", a.entity)),
            },
            (AggregateOp::Sum, None) => return Err(format!("{id}: the sum {} names the field it adds up", a.name)),
            (_, Some(_)) => return Err(format!("{id}: only a sum names a field ({})", a.name)),
            (AggregateOp::Count, None) => Ty::Int,
            (AggregateOp::Exists, None) => Ty::Bool,
        };
        if a.filter.len() > MAX_FILTER {
            return Err(format!("{id}: an aggregate's filter has at most {MAX_FILTER} conditions"));
        }
        for Condition(f, op, v) in &a.filter {
            let fty = field(f)?.ty;
            if !condition_fits(&fty, op, v)? {
                return Err(format!("{id}: [{f}, {op}, {v}] does not fit {}.{f}, which is {fty}", a.entity));
            }
        }
        out.insert(a.name.clone(), ty);
    }
    Ok(out)
}

/// Whether a filter's operator and literal value fit a field of this type, as a list's filters take them.
fn condition_fits(ty: &Ty, op: &str, v: &Value) -> Result<bool, String> {
    // Literal values only, and no floats: a decimal is written as text.
    let scalar = |v: &Value| match v {
        Value::Number(n) if !(n.is_i64() || n.is_u64()) => false,
        Value::Number(_) | Value::String(_) | Value::Bool(_) => !Val::from_json(v, ty).is_unknown(),
        _ => false,
    };
    let ordered = matches!(ty, Ty::Int | Ty::Decimal | Ty::Date | Ty::Instant | Ty::Text);
    let list = |n: Option<usize>| v.as_array().is_some_and(|a| n.is_none_or(|n| a.len() == n) && a.len() <= 1000 && a.iter().all(scalar));
    Ok(match op.trim().to_lowercase().as_str() {
        "=" | "==" | "eq" | "!=" | "<>" | "ne" => v.is_null() || scalar(v),
        ">" | "gt" | ">=" | "gte" | "<" | "lt" | "<=" | "lte" => ordered && scalar(v),
        "in" | "not in" | "not_in" => list(None),
        "between" => ordered && list(Some(2)),
        "like" | "not like" | "not_like" => *ty == Ty::Text && v.is_string(),
        "is" => matches!(v.as_str(), Some("set" | "not set")),
        other => return Err(format!("'{other}' is not a filter operator")),
    })
}

/// Where a rule came from, for its refusals and their evidence (FLC-COMP-010).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub release: String,
    pub solution: String,
    pub version: String,
    pub artifact_digest: String,
}

#[derive(Debug)]
struct Prepared {
    origin: Arc<Origin>,
    id: String,
    /// The rule in its release's source.
    pointer: String,
    when: Option<expr::Compiled>,
    writes: Vec<(String, expr::Compiled)>,
    check: Option<expr::Compiled>,
    message: String,
    field: Option<String>,
    audit: bool,
    params: Val,
    /// name, link field, the declared fields' type.
    lookups: Vec<(String, String, Ty)>,
    /// What it counts or sums, and its type.
    aggregates: Vec<(Aggregate, Ty)>,
    /// The record's fields its expressions read.
    reads: BTreeSet<String>,
}

#[derive(Debug)]
struct EntityRules {
    doc: Ty,
    derives: Vec<Prepared>,
    validates: Vec<Prepared>,
    /// Linked records to read first, per phase: link field → (entity, fields).
    reads: BTreeMap<Phase, BTreeMap<String, (String, BTreeSet<String>)>>,
    /// Sets of records to count or sum first, per phase, by what they read.
    counts: BTreeMap<Phase, BTreeMap<String, Aggregate>>,
}

/// Every active release's rules in a workspace, by entity. An entity whose rules cannot run as approved (a
/// field they read changed since, or two releases fill one field) holds why, and its saves are refused.
#[derive(Debug, Default)]
pub struct Active {
    entities: BTreeMap<String, Result<EntityRules, String>>,
}

/// A linked record the kernel reads, with the saver's rights, before rules run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read {
    pub link: String,
    pub entity: String,
    pub fields: BTreeSet<String>,
}

/// What a save gives the rules.
pub struct Record<'a> {
    pub doc: &'a Value,
    pub old: Option<&'a Value>,
    /// `{id, email, roles}`.
    pub user: &'a Value,
    pub now: DateTime<Utc>,
    pub today: NaiveDate,
    /// By link field: the linked record as read for the rules, or null.
    pub linked: &'a BTreeMap<String, Value>,
    /// By [`Aggregate::key`]: the count (a whole number), the sum (a decimal as text) or whether any exist.
    pub aggregated: &'a BTreeMap<String, Value>,
}

/// Why a save does not go ahead.
#[derive(Debug, Clone, PartialEq)]
pub enum Stop {
    Refused(Box<Refused>),
    /// The entity's rules cannot run as approved.
    Stale(String),
}

/// A rule's refusal.
#[derive(Debug, Clone, PartialEq)]
pub struct Refused {
    pub origin: Arc<Origin>,
    pub rule: String,
    pub pointer: String,
    pub message: String,
    pub field: Option<String>,
    /// The check did not hold (`false`), or could not be worked out (`unknown`).
    pub outcome: &'static str,
    /// Whether the rule keeps evidence of its refusals.
    pub audit: bool,
}

impl Prepared {
    fn refused(&self, outcome: &'static str, message: String) -> Stop {
        Stop::Refused(Box::new(Refused {
            origin: self.origin.clone(),
            rule: self.id.clone(),
            pointer: self.pointer.clone(),
            message,
            field: self.field.clone(),
            outcome,
            audit: self.audit,
        }))
    }
}

/// The parameters of a release, as values and as their type.
fn parameters(release: &Release) -> Result<(Ty, Val), String> {
    let mut tys = BTreeMap::new();
    let mut vals = BTreeMap::new();
    for (name, p) in &release.ir.parameters {
        let ty = parameter_type(&p.ty).ok_or_else(|| format!("parameter {name} has an unknown type"))?;
        vals.insert(name.clone(), Val::from_json(&p.value, &ty));
        tys.insert(name.clone(), ty);
    }
    Ok((Ty::Record(tys), Val::Record(vals)))
}

fn prepare_rule(
    origin: &Arc<Origin>,
    index: usize,
    rule: &Rule,
    release: &Release,
    shape: &EntityShape,
    entities: &dyn Fn(&str) -> Option<EntityShape>,
) -> Result<Prepared, String> {
    let who = format!("{} {} (rule {})", origin.solution, origin.version, rule.id);
    // What the release was approved against: a field it reads or fills that has changed since stops it.
    let prefix = format!("{}.", rule.entity);
    for (path, was) in release.fields.iter().filter(|(p, _)| p.starts_with(&prefix)) {
        let name = &path[prefix.len()..];
        match shape.fields.get(name) {
            Some(f) if lock_name(&f.ty) == *was => {}
            Some(f) => return Err(format!("{who} was approved when {path} was {was}; it is {}: compile and review it again", lock_name(&f.ty))),
            None => return Err(format!("{who} reads {path}, which is gone: compile and review it again")),
        }
    }
    let (params_ty, params) = parameters(release)?;
    let lookups = lookup_types(rule, shape, entities).map_err(|e| format!("{who}: {e}"))?;
    let aggregates = aggregate_types(rule, shape, entities).map_err(|e| format!("{who}: {e}"))?;
    let env = env(shape, rule.phase, &params_ty, &lookups, &aggregates);
    let compile = |src: &str, want: &Ty| expr::compile_as(src, &env, want).map_err(|e| format!("{who}: {e}"));
    let when = rule.when.as_deref().map(|w| compile(w, &Ty::Bool)).transpose()?;
    let mut writes = vec![];
    for (field, src) in &rule.writes {
        let f = shape.fields.get(field).filter(|f| f.customer).ok_or_else(|| format!("{who} fills {field}, which is not the customer's"))?;
        writes.push((field.clone(), compile(src, &f.ty)?));
    }
    let check = rule.check.as_deref().map(|c| compile(c, &Ty::Bool)).transpose()?;
    let mut reads = BTreeSet::new();
    for c in when.iter().chain(writes.iter().map(|(_, c)| c)).chain(check.iter()) {
        reads.extend(c.references().into_iter().filter_map(|p| p.strip_prefix("doc.").map(|f| f.split('.').next().unwrap_or(f).to_string())));
    }
    Ok(Prepared {
        origin: origin.clone(),
        id: rule.id.clone(),
        pointer: format!("/rules/{index}"),
        when,
        writes,
        check,
        message: rule.message.clone().unwrap_or_default(),
        field: rule.field.clone(),
        audit: rule.audit_refusals,
        params,
        lookups: rule.lookups.iter().map(|l| (l.name.clone(), l.link.clone(), lookups[&l.name].clone())).collect(),
        aggregates: rule.aggregates.iter().map(|a| (a.clone(), aggregates[&a.name].clone())).collect(),
        reads,
    })
}

/// Derivations in the order their reads need, or the rules that wait for each other.
fn ordered(mut derives: Vec<Prepared>) -> Result<Vec<Prepared>, String> {
    let mut out = Vec::with_capacity(derives.len());
    while !derives.is_empty() {
        let filled_later = |i: usize| -> BTreeSet<&str> {
            derives.iter().enumerate().filter(|(j, _)| *j != i).flat_map(|(_, d)| d.writes.iter().map(|(f, _)| f.as_str())).collect()
        };
        let Some(next) = (0..derives.len()).find(|&i| derives[i].reads.iter().all(|r| !filled_later(i).contains(r.as_str()))) else {
            let names: Vec<String> = derives.iter().map(|d| format!("{} ({})", d.id, d.origin.solution)).collect();
            return Err(format!("these rules each read what another fills: {}", names.join(", ")));
        };
        out.push(derives.remove(next));
    }
    Ok(out)
}

impl Active {
    /// Prepares the rules of `releases` against the workspace's entities as they are now. Problems stay with
    /// the entity they concern; [`Active::problems`] lists them, and an activation that would cause any is
    /// refused.
    pub fn prepare(releases: &[(Origin, Release)], entities: &dyn Fn(&str) -> Option<EntityShape>) -> Active {
        let mut by_entity: BTreeMap<String, Result<(Vec<Prepared>, Vec<Prepared>), String>> = BTreeMap::new();
        for (origin, release) in releases {
            let origin = Arc::new(origin.clone());
            for (i, rule) in release.ir.rules.iter().enumerate() {
                let slot = by_entity.entry(rule.entity.clone()).or_insert_with(|| Ok((vec![], vec![])));
                let Ok((derives, validates)) = slot else { continue };
                let Some(shape) = entities(&rule.entity) else {
                    *slot = Err(format!("{} {} has rules on {}, which is not in this workspace any more", origin.solution, origin.version, rule.entity));
                    continue;
                };
                match prepare_rule(&origin, i, rule, release, &shape, entities) {
                    Ok(p) if rule.phase == Phase::Derive => derives.push(p),
                    Ok(p) => validates.push(p),
                    Err(e) => *slot = Err(e),
                }
            }
        }
        let mut out = Active::default();
        for (entity, rules) in by_entity {
            let prepared = rules.and_then(|(derives, validates)| {
                let mut writer: BTreeMap<&str, &Prepared> = BTreeMap::new();
                for d in &derives {
                    for (f, _) in &d.writes {
                        if let Some(other) = writer.insert(f, d) {
                            return Err(format!(
                                "{} ({} {}) and {} ({} {}) both fill {entity}.{f}: one writer per field",
                                other.id, other.origin.solution, other.origin.version, d.id, d.origin.solution, d.origin.version
                            ));
                        }
                    }
                }
                let mut reads: BTreeMap<Phase, BTreeMap<String, (String, BTreeSet<String>)>> = BTreeMap::new();
                let mut counts: BTreeMap<Phase, BTreeMap<String, Aggregate>> = BTreeMap::new();
                for (phase, list) in [(Phase::Derive, &derives), (Phase::Validate, &validates)] {
                    let here = counts.entry(phase).or_default();
                    here.extend(list.iter().flat_map(|r| r.aggregates.iter().map(|(a, _)| (a.key(), a.clone()))));
                    if here.len() > MAX_READS_PER_SAVE {
                        return Err(format!("{entity}'s rules count or sum {} sets of records in one save; at most {MAX_READS_PER_SAVE}", here.len()));
                    }
                    let here = reads.entry(phase).or_default();
                    for r in list.iter() {
                        for (_, link, ty) in &r.lookups {
                            let Ty::Record(fields) = ty else { continue };
                            let to = entities(&entity).and_then(|s| s.fields.get(link).and_then(|f| f.link.clone())).unwrap_or_default();
                            here.entry(link.clone()).or_insert_with(|| (to, BTreeSet::new())).1.extend(fields.keys().cloned());
                        }
                    }
                    if here.len() > MAX_READS_PER_SAVE {
                        return Err(format!("{entity}'s rules read {} linked records in one save; at most {MAX_READS_PER_SAVE}", here.len()));
                    }
                }
                let doc = entities(&entity).map(|s| readable(&s)).unwrap_or(Ty::Any);
                Ok(EntityRules { doc, derives: ordered(derives)?, validates, reads, counts })
            });
            out.entities.insert(entity, prepared);
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// What stops these rules from running as approved, by entity.
    pub fn problems(&self) -> Vec<String> {
        self.entities.values().filter_map(|r| r.as_ref().err().cloned()).collect()
    }

    /// Whether `entity` has rules for this phase (so that a save without any reads nothing for them).
    pub fn has(&self, entity: &str, phase: Phase) -> bool {
        match self.entities.get(entity) {
            Some(Ok(r)) => !(if phase == Phase::Derive { &r.derives } else { &r.validates }).is_empty(),
            Some(Err(_)) => true,
            None => false,
        }
    }

    /// The linked records to read before this phase's rules run.
    pub fn reads(&self, entity: &str, phase: Phase) -> Result<Vec<Read>, Stop> {
        match self.entities.get(entity) {
            None => Ok(vec![]),
            Some(Err(e)) => Err(Stop::Stale(e.clone())),
            Some(Ok(r)) => Ok(r
                .reads
                .get(&phase)
                .into_iter()
                .flatten()
                .map(|(link, (to, fields))| Read { link: link.clone(), entity: to.clone(), fields: fields.clone() })
                .collect()),
        }
    }

    /// The sets of records to count or sum before this phase's rules run, by [`Aggregate::key`].
    pub fn aggregates(&self, entity: &str, phase: Phase) -> Result<Vec<(String, Aggregate)>, Stop> {
        match self.entities.get(entity) {
            None => Ok(vec![]),
            Some(Err(e)) => Err(Stop::Stale(e.clone())),
            Some(Ok(r)) => Ok(r.counts.get(&phase).into_iter().flatten().map(|(k, a)| (k.clone(), a.clone())).collect()),
        }
    }

    fn facts(rules: &EntityRules, r: &Prepared, doc: &Value, rec: &Record) -> Facts {
        let lookups = r.lookups.iter().map(|(name, link, ty)| (name.clone(), Val::from_json(rec.linked.get(link).unwrap_or(&Value::Null), ty))).collect();
        let aggregates =
            r.aggregates.iter().map(|(a, ty)| (a.name.clone(), Val::from_json(rec.aggregated.get(&a.key()).unwrap_or(&Value::Null), ty))).collect();
        let facts = Facts::default()
            .with("doc", Val::from_json(doc, &rules.doc))
            .with("parameters", r.params.clone())
            .with("user", Val::from_json(rec.user, &user_type()))
            .with("today", Val::Date(rec.today))
            .with("now", Val::Instant(rec.now))
            .with("lookups", Val::Record(lookups))
            .with("aggregates", Val::Record(aggregates));
        match rec.old {
            Some(old) => facts.with("old", Val::from_json(old, &rules.doc)),
            None => facts.with("old", Val::Null),
        }
    }

    /// Fills the customer's fields an entity's derivations compute, in order, each seeing what came before.
    pub fn derive(&self, entity: &str, doc: &mut Map<String, Value>, rec: &Record) -> Result<(), Stop> {
        let rules = match self.entities.get(entity) {
            None => return Ok(()),
            Some(Err(e)) => return Err(Stop::Stale(e.clone())),
            Some(Ok(r)) => r,
        };
        for r in &rules.derives {
            let facts = Self::facts(rules, r, &Value::Object(doc.clone()), rec);
            if let Some(w) = &r.when {
                match w.decide(&facts) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(_) => return Err(r.refused("unknown", format!("{}: whether this applies could not be worked out", r.id))),
                }
            }
            for (field, c) in &r.writes {
                let v = c.eval(&facts);
                if v.is_unknown() {
                    return Err(r.refused("unknown", format!("{}: {field} could not be worked out", r.id)));
                }
                doc.insert(field.clone(), v.to_json());
            }
        }
        Ok(())
    }

    /// Checks an entity's validations against the record as it would be stored.
    pub fn validate(&self, entity: &str, rec: &Record) -> Result<(), Stop> {
        let rules = match self.entities.get(entity) {
            None => return Ok(()),
            Some(Err(e)) => return Err(Stop::Stale(e.clone())),
            Some(Ok(r)) => r,
        };
        for r in &rules.validates {
            let facts = Self::facts(rules, r, rec.doc, rec);
            if let Some(w) = &r.when {
                match w.decide(&facts) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(_) => return Err(r.refused("unknown", format!("{} (it could not be checked)", r.message))),
                }
            }
            match r.check.as_ref().map(|c| c.decide(&facts)) {
                Some(Ok(true)) | None => {}
                Some(Ok(false)) => return Err(r.refused("false", r.message.clone())),
                Some(Err(_)) => return Err(r.refused("unknown", format!("{} (it could not be checked)", r.message))),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::FieldShape;
    use serde_json::json;

    fn shape() -> EntityShape {
        let f = |ty: Ty, customer: bool, restricted: bool| FieldShape { ty, customer, restricted, link: None };
        EntityShape {
            fields: BTreeMap::from([
                ("company".into(), f(Ty::Text, false, false)),
                ("customer".into(), FieldShape { link: Some("Customer".into()), ..f(Ty::Text, false, false) }),
                ("grand_total".into(), f(Ty::Decimal, false, false)),
                ("x_band".into(), f(Ty::Text, true, false)),
                ("x_flag".into(), f(Ty::Bool, true, false)),
                ("salary".into(), f(Ty::Decimal, false, true)),
            ]),
        }
    }

    fn entities(name: &str) -> Option<EntityShape> {
        match name {
            "SalesOrder" => Some(shape()),
            "Customer" => Some(EntityShape {
                fields: BTreeMap::from([
                    ("credit_limit".into(), FieldShape { ty: Ty::Decimal, customer: false, restricted: false, link: None }),
                    ("bank_account".into(), FieldShape { ty: Ty::Text, customer: false, restricted: true, link: None }),
                ]),
            }),
            "Invoice" => Some(EntityShape {
                fields: BTreeMap::from([
                    ("customer".into(), FieldShape { ty: Ty::Ref("Customer".into()), customer: false, restricted: false, link: Some("Customer".into()) }),
                    ("order".into(), FieldShape { ty: Ty::Ref("SalesOrder".into()), customer: false, restricted: false, link: Some("SalesOrder".into()) }),
                    ("outstanding".into(), FieldShape { ty: Ty::Decimal, customer: false, restricted: false, link: None }),
                ]),
            }),
            _ => None,
        }
    }

    fn release(rules: Value) -> Release {
        let mut r: Release = serde_json::from_value(json!({
            "solution": "acme.orders", "version": "1.0.0", "compiler": "t", "profile": "core", "source_digest": "s", "artifact_digest": "a",
            "lock": [], "fields": {}, "ir": {"parameters": {"limit": {"type": "decimal", "value": "1000"}}, "rules": [], "decisions": [], "processes": []}
        }))
        .unwrap();
        r.ir.rules = serde_json::from_value(rules).unwrap();
        r
    }

    fn origin(s: &str) -> Origin {
        Origin { release: format!("r-{s}"), solution: s.into(), version: "1.0.0".into(), artifact_digest: "a".into() }
    }

    fn rec<'a>(doc: &'a Value, linked: &'a BTreeMap<String, Value>, user: &'a Value) -> Record<'a> {
        static NONE: BTreeMap<String, Value> = BTreeMap::new();
        Record { doc, old: None, user, now: Utc::now(), today: NaiveDate::from_ymd_opt(2026, 11, 8).unwrap(), linked, aggregated: &NONE }
    }

    #[test]
    fn derivations_run_in_the_order_their_reads_need() {
        // x_flag reads x_band, which another rule (listed after it) fills: the band comes first.
        let r = release(json!([
            {"id": "flag", "entity": "SalesOrder", "phase": "derive", "writes": {"x_flag": "doc.x_band == \"large\""}},
            {"id": "band", "entity": "SalesOrder", "phase": "derive", "writes": {"x_band": "doc.grand_total > parameters.limit ? \"large\" : \"small\""}}
        ]));
        let active = Active::prepare(&[(origin("a"), r)], &entities);
        assert!(active.problems().is_empty(), "{:?}", active.problems());
        let mut doc = json!({"grand_total": "5000"}).as_object().unwrap().clone();
        let (linked, user) = (BTreeMap::new(), json!({"id": "u", "email": "e", "roles": []}));
        let snapshot = Value::Object(doc.clone());
        active.derive("SalesOrder", &mut doc, &rec(&snapshot, &linked, &user)).unwrap();
        assert_eq!((doc["x_band"].clone(), doc["x_flag"].clone()), (json!("large"), json!(true)));
    }

    #[test]
    fn cycles_and_competing_writers_are_found_when_prepared() {
        let r = release(json!([
            {"id": "a", "entity": "SalesOrder", "phase": "derive", "writes": {"x_flag": "doc.x_band == \"x\""}},
            {"id": "b", "entity": "SalesOrder", "phase": "derive", "writes": {"x_band": "doc.x_flag ? \"x\" : \"y\""}}
        ]));
        let active = Active::prepare(&[(origin("a"), r)], &entities);
        assert!(active.problems()[0].contains("each read what another fills"), "{:?}", active.problems());
        let one = release(json!([{"id": "a", "entity": "SalesOrder", "phase": "derive", "writes": {"x_band": "\"x\""}}]));
        let two = release(json!([{"id": "b", "entity": "SalesOrder", "phase": "derive", "writes": {"x_band": "\"y\""}}]));
        let active = Active::prepare(&[(origin("one"), one), (origin("two"), two)], &entities);
        assert!(active.problems()[0].contains("one writer per field"), "{:?}", active.problems());
        let mut doc = Map::new();
        let (linked, user, snap) = (BTreeMap::new(), json!({}), json!({}));
        assert!(matches!(active.derive("SalesOrder", &mut doc, &rec(&snap, &linked, &user)), Err(Stop::Stale(_))), "a conflicted entity refuses its saves");
    }

    #[test]
    fn validations_refuse_false_and_unknown_alike() {
        let r = release(json!([
            {"id": "positive", "entity": "SalesOrder", "phase": "validate", "check": "doc.grand_total > dec(\"0\")", "message": "An order has a total", "field": "grand_total", "audit_refusals": true}
        ]));
        let active = Active::prepare(&[(origin("a"), r)], &entities);
        let (linked, user) = (BTreeMap::new(), json!({}));
        let ok = json!({"grand_total": "10"});
        assert!(active.validate("SalesOrder", &rec(&ok, &linked, &user)).is_ok());
        let zero = json!({"grand_total": "0"});
        let Err(Stop::Refused(r)) = active.validate("SalesOrder", &rec(&zero, &linked, &user)) else { panic!() };
        assert_eq!(
            (r.outcome, r.message.as_str(), r.field.as_deref(), r.audit, r.pointer.as_str()),
            ("false", "An order has a total", Some("grand_total"), true, "/rules/0")
        );
        let missing = json!({});
        let Err(Stop::Refused(r)) = active.validate("SalesOrder", &rec(&missing, &linked, &user)) else { panic!() };
        assert_eq!(r.outcome, "unknown", "a missing total is not a pass");
    }

    #[test]
    fn linked_records_are_declared_bounded_and_unrestricted() {
        let r = release(json!([{
            "id": "credit", "entity": "SalesOrder", "phase": "validate", "message": "Over the customer's credit limit",
            "lookups": [{"name": "customer", "link": "customer", "fields": ["credit_limit"]}],
            "check": "doc.grand_total <= lookups.customer.credit_limit"
        }]));
        let active = Active::prepare(&[(origin("a"), r)], &entities);
        assert!(active.problems().is_empty(), "{:?}", active.problems());
        assert_eq!(
            active.reads("SalesOrder", Phase::Validate).unwrap(),
            vec![Read { link: "customer".into(), entity: "Customer".into(), fields: BTreeSet::from(["credit_limit".to_string()]) }]
        );
        assert!(active.reads("SalesOrder", Phase::Derive).unwrap().is_empty());
        let user = json!({});
        let doc = json!({"grand_total": "600"});
        let linked = BTreeMap::from([("customer".to_string(), json!({"credit_limit": "500", "bank_account": "secret"}))]);
        assert!(matches!(active.validate("SalesOrder", &rec(&doc, &linked, &user)), Err(Stop::Refused(_))));
        let bank = release(json!([{
            "id": "bank", "entity": "SalesOrder", "phase": "validate", "message": "m", "check": "true",
            "lookups": [{"name": "customer", "link": "customer", "fields": ["bank_account"]}]
        }]));
        assert!(Active::prepare(&[(origin("b"), bank)], &entities).problems()[0].contains("restricted"));
    }

    #[test]
    fn aggregates_are_read_first_and_given_to_the_rules() {
        let owed = json!({"name": "owed", "entity": "Invoice", "link": "customer", "to": "customer", "filter": [["outstanding", ">", "0"]], "op": "sum", "field": "outstanding"});
        let r = release(json!([
            {"id": "unpaid", "entity": "SalesOrder", "phase": "validate", "message": "The customer has unpaid invoices",
             "aggregates": [owed, {"name": "invoices", "entity": "Invoice", "link": "order", "op": "count"}],
             "check": "aggregates.owed == dec(\"0\") || aggregates.invoices > 0"},
            // The same sum under another name: read once.
            {"id": "band", "entity": "SalesOrder", "phase": "derive", "aggregates": [{"name": "due", "entity": "Invoice", "link": "customer", "to": "customer", "filter": [["outstanding", ">", "0"]], "op": "sum", "field": "outstanding"}],
             "writes": {"x_band": "aggregates.due > dec(\"0.3\") ? \"owing\" : \"clear\""}}
        ]));
        let active = Active::prepare(&[(origin("a"), r.clone())], &entities);
        assert!(active.problems().is_empty(), "{:?}", active.problems());
        let validate = active.aggregates("SalesOrder", Phase::Validate).unwrap();
        assert_eq!(validate.len(), 2);
        let derive = active.aggregates("SalesOrder", Phase::Derive).unwrap();
        assert_eq!(derive.len(), 1);
        let key = r.ir.rules[0].aggregates[0].key();
        assert_eq!(derive[0].0, key, "one read whatever the rules call it");
        let count = r.ir.rules[0].aggregates[1].key();
        let (linked, user, doc) = (BTreeMap::new(), json!({}), json!({"grand_total": "1"}));
        // A simulation hands the values in, as the server would have read them.
        let run = |owed: Value, invoices: Value| {
            let aggregated = BTreeMap::from([(key.clone(), owed), (count.clone(), invoices)]);
            active.validate("SalesOrder", &Record { aggregated: &aggregated, ..rec(&doc, &linked, &user) })
        };
        assert!(run(json!("0"), json!(0)).is_ok());
        assert!(run(json!("0.30"), json!(1)).is_ok());
        let Err(Stop::Refused(refused)) = run(json!("0.30"), json!(0)) else { panic!("unpaid and no invoice of its own") };
        assert_eq!(refused.outcome, "false");
        assert!(run(Value::Null, json!(0)).is_err(), "not given is not zero");
        // Exact: 0.1 + 0.2 summed by the server is 0.3, not more.
        let mut out = Map::new();
        let aggregated = BTreeMap::from([(key.clone(), json!("0.3"))]);
        active.derive("SalesOrder", &mut out, &Record { aggregated: &aggregated, ..rec(&doc, &linked, &user) }).unwrap();
        assert_eq!(out["x_band"], json!("clear"));
        // Pointing at the wrong record is found when prepared.
        let wrong = release(json!([{"id": "w", "entity": "SalesOrder", "phase": "validate", "message": "m", "check": "true",
            "aggregates": [{"name": "x", "entity": "Invoice", "link": "customer", "op": "count"}]}]));
        assert!(Active::prepare(&[(origin("w"), wrong)], &entities).problems()[0].contains("not a link to SalesOrder"));
    }

    #[test]
    fn a_release_approved_against_other_fields_does_not_run() {
        let mut r = release(json!([{"id": "band", "entity": "SalesOrder", "phase": "derive", "writes": {"x_band": "\"x\""}}]));
        r.fields.insert("SalesOrder.x_band".into(), lock_name(&Ty::Int));
        let active = Active::prepare(&[(origin("a"), r)], &entities);
        assert!(active.problems()[0].contains("compile and review it again"), "{:?}", active.problems());
    }
}
