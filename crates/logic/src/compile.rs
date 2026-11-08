//! The compiler (package MASTER_PLAN §7): a solution's source in, an immutable release out, or every fault
//! found, each with a stable code and a pointer into the source.
//!
//! Passes, in order: parse (strict, in [`crate::solution`]); resolve the capabilities asked for at their exact
//! versions; type every expression and mapping; check each process's graph (identities, routes, reachability,
//! no unbounded cycles); check effects (a query reads, an action changes, a rule touches nothing outside its
//! record); authority (only what was asked for, and asking grants nothing); data flow (a rule writes only the
//! customer's own fields and reads nothing restricted); resources (steps, sizes); time (every wait has a
//! deadline and a route for it); recovery (an effect that can end unknown has a route for that, and neither an
//! error nor an unknown outcome may be routed to success); profile (processes need the Full Process profile);
//! and admission (the solution's own tests pass).
//!
//! What it cannot prove, it says: a decision table written as expressions is "coverage unproven", never
//! covered. Business correctness is the tests' and the reviewers'.

use crate::canonical;
use crate::capability::{Capability, Effect, Profile, UnknownOutcome, digestible};
use crate::decision::{self, Outcome};
use crate::expr::{self, Env, Facts, Ty, Val};
use crate::refusal::{Code, Refusal};
use crate::solution::{CapRef, EndOutcome, Node, Phase, Process, Solution};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The compiler's version, part of every artifact digest: a new compiler is a new artifact.
pub const COMPILER: &str = concat!("ferrum-logic/", env!("CARGO_PKG_VERSION"));
pub const SOURCE_SCHEMA: &str = "ferrum.solution.source/1";
pub const ARTIFACT_SCHEMA: &str = "ferrum.solution.artifact/1";
const MAX_PROCESSES: usize = 16;
const MAX_NODES: usize = 128;
const MAX_RULES: usize = 128;
const MAX_DECISIONS: usize = 64;
const MAX_WRITES: usize = 32;
const MAX_STEPS: u32 = 100_000;
/// Names an expression already has, which no step may take.
const SCOPE_NAMES: &[&str] = &["trigger", "parameters", "doc", "old", "user", "today", "now", "item", "lookups", "true", "false", "null", "in", "has"];

/// A name an expression can refer to: lower_snake_case, at most 64 characters.
fn name_ok(s: &str) -> bool {
    s.len() <= 64 && s.chars().next().is_some_and(|c| c.is_ascii_lowercase()) && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// An id as the package writes them (`acme.purchase_exception`).
fn id_ok(s: &str) -> bool {
    (2..=101).contains(&s.len())
        && s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-'))
}

/// `major.minor.patch`.
fn version_ok(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.len() <= 9 && p.chars().all(|c| c.is_ascii_digit()))
}

/// Fields no rule may write, whoever's they look like.
pub const PROTECTED: &[&str] = &[
    "id",
    "name",
    "tenant_id",
    "company",
    "owner",
    "docstatus",
    "workflow_state",
    "version",
    "created_by",
    "created_at",
    "updated_by",
    "updated_at",
    "amended_from",
];

/// What the compiler asks of the workspace it compiles for.
pub trait Resolver {
    /// A capability at exactly this version.
    fn capability(&self, id: &str, version: &str) -> Option<Capability>;
    /// The version installed, to tell "not installed" from "another version".
    fn installed(&self, id: &str) -> Option<String>;
    /// An event's payload type.
    fn event(&self, id: &str) -> Option<Ty>;
    /// An entity's fields.
    fn entity(&self, name: &str) -> Option<EntityShape>;
    /// Whether the workspace has a business calendar of this name.
    fn calendar(&self, name: &str) -> bool {
        let _ = name;
        false
    }
}

#[derive(Debug, Clone, Default)]
pub struct EntityShape {
    pub fields: BTreeMap<String, FieldShape>,
}

#[derive(Debug, Clone)]
pub struct FieldShape {
    pub ty: Ty,
    /// The customer's own field (added by a customization or a solution's model), which a rule may fill.
    pub customer: bool,
    /// In a permission group, masked or secret: a rule may not read it.
    pub restricted: bool,
    /// For a link: the entity it points to.
    pub link: Option<String>,
}

/// Where a release may run (decisions L-06).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeProfile {
    Core,
    FullProcess,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Diagnostic {
    pub code: Code,
    pub message: String,
    /// A JSON pointer into the source.
    pub pointer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// A fault refuses the release; a note does not.
    pub fault: bool,
}

/// A capability as the release is bound to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Locked {
    pub id: String,
    pub version: String,
    pub digest: String,
    pub effect: Effect,
}

/// What runs: the checked source, with every capability resolved to its digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ir {
    pub parameters: BTreeMap<String, crate::solution::Parameter>,
    pub rules: Vec<crate::solution::Rule>,
    pub decisions: Vec<decision::Table>,
    pub processes: Vec<crate::solution::Process>,
}

/// An immutable, executable release.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Release {
    pub solution: String,
    pub version: String,
    pub compiler: String,
    pub profile: RuntimeProfile,
    pub source_digest: String,
    pub artifact_digest: String,
    pub lock: Vec<Locked>,
    /// The fields its rules read or fill, `Entity.field` → type, as they were when it was compiled.
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
    pub ir: Ir,
    /// Notes that do not refuse it, for the builder to show.
    #[serde(default)]
    pub notes: Vec<Value>,
}

impl Release {
    /// Whether one of its derivations fills `entity.field`.
    pub fn writes(&self, entity: &str, field: &str) -> bool {
        self.ir.rules.iter().any(|r| r.entity == entity && r.writes.contains_key(field))
    }
}

struct Pass<'a> {
    out: Vec<Diagnostic>,
    resolver: &'a dyn Resolver,
}

impl Pass<'_> {
    fn fault(&mut self, code: Code, pointer: String, node: Option<&str>, message: impl Into<String>) {
        self.out.push(Diagnostic { code, message: message.into(), pointer, node: node.map(str::to_string), fault: true });
    }

    fn note(&mut self, code: Code, pointer: String, node: Option<&str>, message: impl Into<String>) {
        self.out.push(Diagnostic { code, message: message.into(), pointer, node: node.map(str::to_string), fault: false });
    }

    fn refusal(&mut self, r: Refusal, pointer: String, node: Option<&str>) {
        let pointer = r.pointer.map(|p| format!("{pointer}{p}")).unwrap_or(pointer);
        self.out.push(Diagnostic { code: r.code, message: r.message, pointer, node: node.map(str::to_string), fault: true });
    }
}

/// A parameter's declared type: `money:INR`, `quantity:kg`, `decimal`, `int`, `text`, `bool`, `date`.
pub fn parameter_type(t: &str) -> Option<Ty> {
    Some(match t.split_once(':') {
        Some(("money", c)) => Ty::Money(Some(c.to_string())),
        Some(("quantity", u)) => Ty::Quantity(Some(u.to_string())),
        _ => match t {
            "money" => Ty::Money(None),
            "decimal" => Ty::Decimal,
            "int" => Ty::Int,
            "text" => Ty::Text,
            "bool" => Ty::Bool,
            "date" => Ty::Date,
            _ => return None,
        },
    })
}

/// A JSON Schema as a type, as far as one can be read from it. An object of exactly an amount and a currency
/// is money, and of a value and a unit a quantity, as [`Val::from_json`] reads them; a `const` currency or
/// unit is part of the type.
pub fn schema_type(s: &Value) -> Ty {
    let kind = s.get("type");
    let has = |t: &str| kind.is_some_and(|k| k == t || k.as_array().is_some_and(|a| a.iter().any(|x| x == t)));
    if has("object")
        && let Some(props) = s.get("properties").and_then(Value::as_object)
    {
        let fixed = |k: &str| props.get(k).and_then(|p| p.get("const")).and_then(Value::as_str).map(str::to_string);
        let keys: Vec<&str> = props.keys().map(String::as_str).collect();
        return match keys.as_slice() {
            ["amount", "currency"] => Ty::Money(fixed("currency")),
            ["unit", "value"] => Ty::Quantity(fixed("unit")),
            _ => Ty::Record(props.iter().map(|(k, v)| (k.clone(), schema_type(v))).collect()),
        };
    }
    if has("array") {
        return Ty::List(Box::new(s.get("items").map(schema_type).unwrap_or(Ty::Any)));
    }
    if has("string") && s.get("format").and_then(Value::as_str) == Some("date") {
        return Ty::Date;
    }
    if has("string") && has("number") {
        return Ty::Decimal;
    }
    if has("integer") {
        return Ty::Int;
    }
    if has("number") {
        return Ty::Decimal;
    }
    if has("boolean") {
        return Ty::Bool;
    }
    if has("string") {
        return Ty::Text;
    }
    Ty::Any
}

/// The required fields an input mapping leaves out: those of the capability's input, and of each object the
/// mapping fills field by field.
fn missing_inputs(schema: &Value, input: &BTreeMap<String, String>) -> Vec<String> {
    let given = |path: &str| input.keys().any(|k| k == path || k.starts_with(&format!("{path}.")));
    let required = |s: &Value| -> Vec<String> {
        s.get("required").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()
    };
    let mut out: Vec<String> = required(schema).into_iter().filter(|r| !given(r)).collect();
    let parents: BTreeSet<&str> = input.keys().flat_map(|k| k.match_indices('.').map(move |(i, _)| &k[..i])).collect();
    for parent in parents {
        if let Some(s) = parent.split('.').try_fold(schema, |s, part| s.get("properties")?.get(part)) {
            out.extend(required(s).into_iter().map(|r| format!("{parent}.{r}")).filter(|r| !given(r)));
        }
    }
    out
}

/// What a decided task gives later steps besides its form: the decision, its record, who and when.
pub const TASK_FIELDS: &[&str] = &["decision", "decision_id", "by", "at"];

fn task_output(form: &[crate::solution::FormField]) -> Ty {
    let mut out = BTreeMap::from([("decision".into(), Ty::Text), ("decision_id".into(), Ty::Text), ("by".into(), Ty::Text), ("at".into(), Ty::Instant)]);
    for f in form {
        out.entry(f.name.clone()).or_insert_with(|| parameter_type(&f.ty).unwrap_or(Ty::Any));
    }
    Ty::Record(out)
}

/// Compiles a solution for a workspace.
pub fn compile(src: &Solution, resolver: &dyn Resolver, profile: RuntimeProfile) -> Result<Release, Vec<Diagnostic>> {
    let mut p = Pass { out: vec![], resolver };
    // Sizes first: nothing else should run over an oversized solution.
    if src.processes.len() > MAX_PROCESSES || src.rules.len() > MAX_RULES || src.decisions.len() > MAX_DECISIONS {
        p.fault(
            Code::SchemaInvalid,
            String::new(),
            None,
            format!("a solution holds at most {MAX_PROCESSES} processes, {MAX_RULES} rules and {MAX_DECISIONS} decisions"),
        );
        return Err(p.out);
    }

    if !id_ok(&src.id) {
        p.fault(Code::SchemaInvalid, "/id".into(), None, format!("'{}' is not an id: lower case letters, digits, _ . -", src.id));
    }
    if !version_ok(&src.version) {
        p.fault(Code::SchemaInvalid, "/version".into(), None, format!("'{}' is not a version (major.minor.patch)", src.version));
    }
    if src.label.trim().is_empty() {
        p.fault(Code::SchemaInvalid, "/label".into(), None, "a solution has a label");
    }
    let mut process_ids = BTreeSet::new();
    for (pi, proc_) in src.processes.iter().enumerate() {
        if !process_ids.insert(proc_.id.as_str()) || !id_ok(&proc_.id) {
            p.fault(Code::SchemaInvalid, format!("/processes/{pi}/id"), None, format!("'{}' is not a process id, or another process has it", proc_.id));
        }
    }
    for (kind, ids) in
        [("decision", src.decisions.iter().map(|d| d.id.as_str()).collect::<Vec<_>>()), ("test", src.tests.iter().map(|t| t.id.as_str()).collect())]
    {
        for (i, id) in ids.iter().enumerate() {
            if !name_ok(id) {
                p.fault(Code::SchemaInvalid, format!("/{kind}s/{i}/id"), None, format!("'{id}' cannot name a {kind}: lower_snake_case"));
            }
        }
    }

    // Resolve: every capability asked for, at its exact version, once.
    let mut caps: BTreeMap<String, Capability> = BTreeMap::new();
    for (i, r) in src.requires.iter().enumerate() {
        let at = format!("/requires/{i}");
        if !version_ok(&r.version) {
            p.fault(Code::SchemaInvalid, format!("{at}/version"), None, format!("'{}' is not a version (major.minor.patch)", r.version));
            continue;
        }
        if caps.contains_key(&r.id) {
            p.fault(Code::SchemaInvalid, at, None, format!("{} is asked for twice", r.id));
            continue;
        }
        match resolver.capability(&r.id, &r.version) {
            Some(c) => {
                caps.insert(r.id.clone(), c);
            }
            None => match resolver.installed(&r.id) {
                Some(v) => p.fault(Code::CapabilityUnavailable, at, None, format!("{} is installed at version {v}, not {}", r.id, r.version)),
                None => p.fault(Code::CapabilityUnavailable, at, None, format!("{} is not installed in this workspace", r.id)),
            },
        }
    }
    let asked: BTreeSet<CapRef> = src.requires.iter().cloned().collect();

    // Parameters.
    let mut params = BTreeMap::new();
    let mut param_values = BTreeMap::new();
    for (name, prm) in &src.parameters {
        let at = format!("/parameters/{name}");
        match parameter_type(&prm.ty) {
            Some(t) => {
                let v = Val::from_json(&prm.value, &t);
                if v.is_unknown() || matches!(v, Val::Null) {
                    p.fault(Code::TypeMismatch, format!("{at}/value"), None, format!("{name}'s value is not {t}"));
                }
                params.insert(name.clone(), t);
                param_values.insert(name.clone(), v);
            }
            None => p.fault(Code::SchemaInvalid, format!("{at}/type"), None, format!("{name} has an unknown type '{}'", prm.ty)),
        }
    }
    let params_ty = Ty::Record(params.clone());
    let params_val = Val::Record(param_values);

    let fields = rules(&mut p, src, &params_ty);
    let decisions: BTreeMap<String, &decision::Table> = src.decisions.iter().map(|d| (d.id.clone(), d)).collect();
    if decisions.len() != src.decisions.len() {
        p.fault(Code::SchemaInvalid, "/decisions".into(), None, "two decisions share an id");
    }
    let mut decision_scopes: BTreeMap<String, Env> = BTreeMap::new();
    // Callees before callers, so a subprocess's output is typed where it is used; a process that calls itself,
    // directly or not, is refused where it does.
    let processes: BTreeMap<String, &Process> = src.processes.iter().map(|p| (p.id.clone(), p)).collect();
    let callees: BTreeMap<String, BTreeSet<String>> = src
        .processes
        .iter()
        .map(|p| {
            let mut c = BTreeSet::new();
            calls(&p.nodes, &mut c);
            (p.id.clone(), c)
        })
        .collect();
    let reaches = |from: &str, to: &str| -> bool {
        let mut seen = BTreeSet::new();
        let mut todo: Vec<String> = callees.get(from).into_iter().flatten().cloned().collect();
        while let Some(next) = todo.pop() {
            if next == to {
                return true;
            }
            if seen.insert(next.clone()) {
                todo.extend(callees.get(&next).into_iter().flatten().cloned());
            }
        }
        false
    };
    let recursive: BTreeSet<String> = processes.keys().filter(|id| reaches(id, id)).cloned().collect();
    let mut order: Vec<usize> = (0..src.processes.len()).collect();
    order.sort_by_key(|&i| src.processes.iter().filter(|q| reaches(&src.processes[i].id, &q.id)).count());
    let mut outputs: BTreeMap<String, Ty> = BTreeMap::new();
    for pi in order {
        let proc_ = &src.processes[pi];
        let t = process(&mut p, pi, proc_, &caps, &asked, &decisions, &processes, &outputs, &recursive, &params_ty, profile, &mut decision_scopes);
        outputs.insert(proc_.id.clone(), t);
    }

    // Decisions no process uses are still checked, in a scope of the parameters alone.
    for (di, d) in src.decisions.iter().enumerate() {
        let scope = decision_scopes.get(&d.id).cloned().unwrap_or_else(|| Env::default().with("parameters", params_ty.clone()));
        match decision::compile(d, &scope) {
            Ok(c) => {
                for f in &c.findings {
                    let at = format!("/decisions/{di}");
                    if f.fault {
                        p.fault(Code::SchemaInvalid, at, f.row.as_deref(), f.message.clone());
                    } else {
                        p.note(Code::SchemaInvalid, at, f.row.as_deref(), f.message.clone());
                    }
                }
            }
            Err(e) => p.refusal(e, format!("/decisions/{di}"), None),
        }
    }

    // Admission: the solution's own tests, run on the decisions they name.
    for (ti, t) in src.tests.iter().enumerate() {
        let at = format!("/tests/{ti}");
        let Some(d) = decisions.get(&t.decision) else {
            p.fault(Code::SchemaInvalid, at, None, format!("test {} names {}, which is not a decision of this solution", t.id, t.decision));
            continue;
        };
        let scope = decision_scopes.get(&t.decision).cloned().unwrap_or_else(|| Env::default().with("parameters", params_ty.clone()));
        let Ok(c) = decision::compile(d, &scope) else { continue };
        let mut facts = Facts::default().with("parameters", params_val.clone());
        for (name, v) in &t.given {
            let ty = scope.vars.get(name).cloned().unwrap_or(Ty::Any);
            facts = facts.with(name, Val::from_json(v, &ty));
        }
        let got = match c.decide(&facts) {
            Outcome::Decided { outcome, .. } => outcome,
            Outcome::Unknown(_) => json!("unknown"),
        };
        if got != t.expect {
            p.fault(Code::SchemaInvalid, at, None, format!("test {} expected {} and the decision gave {got}", t.id, t.expect));
        }
    }

    if p.out.iter().any(|d| d.fault) {
        return Err(p.out);
    }
    let lock: Vec<Locked> =
        caps.values().map(|c| Locked { id: c.id.clone(), version: c.version.clone(), digest: c.digest.clone(), effect: c.effect }).collect();
    let ir = Ir { parameters: src.parameters.clone(), rules: src.rules.clone(), decisions: src.decisions.clone(), processes: src.processes.clone() };
    let source_digest = match canonical::digest(SOURCE_SCHEMA, &digestible(&src.semantic())) {
        Ok(d) => d,
        Err(e) => return Err(vec![Diagnostic { code: Code::SchemaInvalid, message: e.to_string(), pointer: String::new(), node: None, fault: true }]),
    };
    let body = json!({"compiler": COMPILER, "profile": profile, "solution": src.id, "version": src.version, "lock": lock, "fields": fields, "ir": ir});
    let artifact_digest = match canonical::digest(ARTIFACT_SCHEMA, &digestible(&body)) {
        Ok(d) => d,
        Err(e) => return Err(vec![Diagnostic { code: Code::SchemaInvalid, message: e.to_string(), pointer: String::new(), node: None, fault: true }]),
    };
    let release = Release {
        solution: src.id.clone(),
        version: src.version.clone(),
        compiler: COMPILER.into(),
        profile,
        source_digest,
        artifact_digest,
        lock,
        fields,
        ir,
        notes: p.out.iter().map(|d| serde_json::to_value(d).unwrap_or(Value::Null)).collect(),
    };
    // The rules as the server will run them: derivations ordered by what they read, and none waiting for
    // another in a circle, linked reads within one save's budget.
    let origin = crate::rules::Origin {
        release: String::new(),
        solution: release.solution.clone(),
        version: release.version.clone(),
        artifact_digest: release.artifact_digest.clone(),
    };
    let problems = crate::rules::Active::prepare(&[(origin, release.clone())], &|name| resolver.entity(name)).problems();
    if !problems.is_empty() {
        return Err(problems.into_iter().map(|m| Diagnostic { code: Code::Conflict, message: m, pointer: "/rules".into(), node: None, fault: true }).collect());
    }
    Ok(release)
}

/// Checks the rules, and returns the fields they read or fill, with their types, as the release's lock
/// writes them down (FLC-COMP-005): a field that changes under an active release stops it.
fn rules(p: &mut Pass, src: &Solution, params: &Ty) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let mut writers: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for (i, r) in src.rules.iter().enumerate() {
        let at = format!("/rules/{i}");
        if !ids.insert(r.id.clone()) {
            p.fault(Code::SchemaInvalid, format!("{at}/id"), None, format!("two rules are called {}", r.id));
        }
        if !name_ok(&r.id) {
            p.fault(Code::SchemaInvalid, format!("{at}/id"), None, format!("'{}' cannot name a rule: lower_snake_case", r.id));
        }
        if r.writes.len() > MAX_WRITES {
            p.fault(Code::SchemaInvalid, format!("{at}/writes"), None, format!("a rule writes at most {MAX_WRITES} fields"));
        }
        let Some(shape) = p.resolver.entity(&r.entity) else {
            p.fault(Code::SchemaInvalid, format!("{at}/entity"), None, format!("{} is not an entity a rule can be on", r.entity));
            continue;
        };
        let resolver = p.resolver;
        let lookups = match crate::rules::lookup_types(r, &shape, &|name| resolver.entity(name)) {
            Ok(l) => l,
            Err(e) => {
                let code = if e.contains("restricted") { Code::PermissionDenied } else { Code::SchemaInvalid };
                p.fault(code, format!("{at}/lookups"), None, e);
                BTreeMap::new()
            }
        };
        for l in &r.lookups {
            let to = shape.fields.get(&l.link).and_then(|f| f.link.clone()).unwrap_or_default();
            if let Some(linked) = resolver.entity(&to) {
                for f in &l.fields {
                    if let Some(t) = linked.fields.get(f) {
                        fields.insert(format!("{to}.{f}"), crate::rules::lock_name(&t.ty));
                    }
                }
            }
        }
        // A rule sees the record as fields it may read: restricted ones are not there to read.
        let restricted: Vec<&String> = shape.fields.iter().filter(|(_, f)| f.restricted).map(|(k, _)| k).collect();
        let env = crate::rules::env(&shape, r.phase, params, &lookups);
        let check =
            |p: &mut Pass, fields: &mut BTreeMap<String, String>, src_expr: &str, want: &Ty, pointer: String| match expr::compile_as(src_expr, &env, want) {
                Ok(c) => {
                    for path in c.references() {
                        if let Some(f) = path.strip_prefix("doc.").or_else(|| path.strip_prefix("old.")) {
                            let f = f.split('.').next().unwrap_or(f);
                            if let Some(shaped) = shape.fields.get(f) {
                                fields.insert(format!("{}.{f}", r.entity), crate::rules::lock_name(&shaped.ty));
                            }
                        }
                    }
                }
                Err(e) => {
                    let mentions_restricted = restricted.iter().any(|f| src_expr.contains(&format!("doc.{f}")) || src_expr.contains(&format!("old.{f}")));
                    if mentions_restricted {
                        p.fault(Code::PermissionDenied, pointer, None, format!("a rule may not read a restricted field of {}: '{src_expr}'", r.entity));
                    } else {
                        p.refusal(e, pointer, None);
                    }
                }
            };
        if let Some(w) = &r.when {
            check(p, &mut fields, w, &Ty::Bool, format!("{at}/when"));
        }
        match r.phase {
            Phase::Derive => {
                if r.writes.is_empty() || r.check.is_some() || r.message.is_some() {
                    p.fault(Code::SchemaInvalid, at.clone(), None, format!("{}: a derive rule writes fields and checks nothing", r.id));
                }
                for (field, e) in &r.writes {
                    let wp = format!("{at}/writes/{field}");
                    if PROTECTED.contains(&field.as_str()) {
                        p.fault(Code::PermissionDenied, wp, None, format!("{field} is the platform's: no rule writes it"));
                        continue;
                    }
                    let Some(f) = shape.fields.get(field) else {
                        p.fault(Code::SchemaInvalid, wp, None, format!("{} has no field {field}", r.entity));
                        continue;
                    };
                    if !f.customer {
                        p.fault(
                            Code::PermissionDenied,
                            wp,
                            None,
                            format!("{field} belongs to {}'s app: a rule fills only the customer's own fields", r.entity),
                        );
                        continue;
                    }
                    fields.insert(format!("{}.{field}", r.entity), crate::rules::lock_name(&f.ty));
                    check(p, &mut fields, e, &f.ty, wp.clone());
                    if let Some(other) = writers.insert((r.entity.clone(), field.clone()), r.id.clone()) {
                        p.fault(Code::Conflict, wp, None, format!("{other} and {} both write {}.{field}: one writer per field", r.id, r.entity));
                    }
                }
            }
            Phase::Validate => {
                if !r.writes.is_empty() {
                    p.fault(Code::SchemaInvalid, format!("{at}/writes"), None, format!("{}: a validate rule writes nothing", r.id));
                }
                match &r.check {
                    Some(c) => check(p, &mut fields, c, &Ty::Bool, format!("{at}/check")),
                    None => p.fault(Code::SchemaInvalid, at.clone(), None, format!("{}: a validate rule says what must hold (check)", r.id)),
                }
                if r.message.as_deref().is_none_or(|m| m.trim().is_empty()) {
                    p.fault(Code::SchemaInvalid, format!("{at}/message"), None, format!("{}: say what is wrong (message)", r.id));
                }
                if let Some(f) = &r.field
                    && !shape.fields.contains_key(f)
                {
                    p.fault(Code::SchemaInvalid, format!("{at}/field"), None, format!("{} has no field {f}", r.entity));
                }
            }
        }
    }
    fields
}

/// The bounds of structured steps.
const MAX_BRANCHES: usize = 16;
pub const MAX_ITEMS: u32 = 1_000;
pub const MAX_CONCURRENCY: u32 = 20;
pub const MAX_ROUNDS: u32 = 100;
pub const MAX_APPROVALS: u32 = 9;
pub const MAX_DELEGATION: u32 = 3;
const MAX_CASE_TASKS: usize = 32;

/// What the blocks of one process are checked against.
struct Scope<'a> {
    caps: &'a BTreeMap<String, Capability>,
    asked: &'a BTreeSet<CapRef>,
    decisions: &'a BTreeMap<String, &'a decision::Table>,
    processes: &'a BTreeMap<String, &'a Process>,
    /// What each process already checked hands back from its completed ends.
    outputs: &'a BTreeMap<String, Ty>,
    /// Processes that call themselves, directly or through others.
    recursive: &'a BTreeSet<String>,
    decision_scopes: &'a mut BTreeMap<String, Env>,
    /// Each action of the process with what undoes it: its step, its capability, the compensating one.
    compensable: Vec<(String, String, Option<String>)>,
    /// Every step id of the process: ids are the process's, not a block's, so a step's output has one name.
    ids: BTreeSet<String>,
}

/// Where a block sits, which decides how it may end.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Process,
    Branch,
    Body,
    RepeatBody,
}

/// Every action anywhere in `nodes`, with its capability.
fn actions(nodes: &[Node], out: &mut Vec<(String, CapRef)>) {
    for n in nodes {
        if let Node::Action { id, capability, .. } = n {
            out.push((id.clone(), capability.clone()));
        }
        for b in n.blocks() {
            actions(&b.nodes, out);
        }
    }
}

/// The processes `nodes` call as subprocesses.
fn calls(nodes: &[Node], out: &mut BTreeSet<String>) {
    for n in nodes {
        if let Node::Subprocess { process, .. } = n {
            out.insert(process.clone());
        }
        for b in n.blocks() {
            calls(&b.nodes, out);
        }
    }
}

/// The most steps a run of `nodes` can take, every bound spent.
fn worst(nodes: &[Node], processes: &BTreeMap<String, &Process>, stack: &mut Vec<String>) -> u64 {
    nodes.iter().fold(0u64, |sum, n| {
        let inner = match n {
            Node::Parallel { branches, .. } => branches.iter().map(|b| worst(&b.nodes, processes, stack)).fold(0u64, u64::saturating_add),
            Node::ForEach { max_items, body, .. } => u64::from(*max_items).saturating_mul(worst(&body.nodes, processes, stack)),
            Node::Repeat { max_rounds, body, .. } => u64::from(*max_rounds).saturating_mul(worst(&body.nodes, processes, stack)),
            Node::Case { tasks, .. } => tasks.len() as u64,
            Node::Subprocess { process, .. } if !stack.contains(process) => match processes.get(process) {
                Some(called) => {
                    stack.push(process.clone());
                    let w = worst(&called.nodes, processes, stack);
                    stack.pop();
                    w
                }
                None => 0,
            },
            _ => 0,
        };
        sum.saturating_add(1).saturating_add(inner)
    })
}

fn wait_ok(p: &mut Pass, pointer: String, v: &Value, nid: Option<&str>, what: &str) {
    match crate::calendar::Wait::parse(v) {
        None => p.fault(
            Code::SchemaInvalid,
            pointer,
            nid,
            format!("{what} is seconds, an ISO 8601 duration (P2D, PT4H) or {{\"business_days\": n, \"calendar\": name}}, at most a year"),
        ),
        Some(w) => {
            if let Some(c) = w.calendar()
                && !p.resolver.calendar(c)
            {
                p.fault(Code::CapabilityUnavailable, pointer, nid, format!("{c} is not a calendar of this workspace"));
            }
        }
    }
}

fn form_ok(p: &mut Pass, at: &str, form: &[crate::solution::FormField], nid: Option<&str>) {
    let mut names = BTreeSet::new();
    for (fi, f) in form.iter().enumerate() {
        let fp = format!("{at}/form/{fi}");
        if !name_ok(&f.name) || TASK_FIELDS.contains(&f.name.as_str()) || !names.insert(f.name.as_str()) {
            p.fault(
                Code::SchemaInvalid,
                format!("{fp}/name"),
                nid,
                format!("'{}' cannot name a form field: lower_snake_case, once, and none of {}", f.name, TASK_FIELDS.join(", ")),
            );
        }
        match parameter_type(&f.ty) {
            None => p.fault(Code::SchemaInvalid, format!("{fp}/type"), nid, format!("{} has an unknown type '{}'", f.name, f.ty)),
            Some(t) if !f.options.is_empty() && t != Ty::Text => {
                p.fault(Code::SchemaInvalid, format!("{fp}/options"), nid, format!("{} offers choices, which are text", f.name))
            }
            _ => {}
        }
    }
}

/// Checks one process. Returns the type of what its completed ends hand back.
#[allow(clippy::too_many_arguments)]
fn process(
    p: &mut Pass,
    pi: usize,
    proc_: &Process,
    caps: &BTreeMap<String, Capability>,
    asked: &BTreeSet<CapRef>,
    decisions: &BTreeMap<String, &decision::Table>,
    processes: &BTreeMap<String, &Process>,
    outputs: &BTreeMap<String, Ty>,
    recursive: &BTreeSet<String>,
    params: &Ty,
    profile: RuntimeProfile,
    decision_scopes: &mut BTreeMap<String, Env>,
) -> Ty {
    let at = format!("/processes/{pi}");
    if profile == RuntimeProfile::Core {
        p.fault(
            Code::UnsupportedProfile,
            at,
            None,
            format!("{} is a durable process: it needs the Full Process profile, and this installation runs Core", proc_.id),
        );
        return Ty::Any;
    }
    let mut all = vec![];
    actions(&proc_.nodes, &mut all);
    let count = |nodes: &[Node]| -> usize {
        fn count(nodes: &[Node]) -> usize {
            nodes.iter().map(|n| 1 + n.blocks().iter().map(|b| count(&b.nodes)).sum::<usize>()).sum()
        }
        count(nodes)
    };
    if count(&proc_.nodes) > MAX_NODES {
        p.fault(Code::SchemaInvalid, format!("{at}/nodes"), None, format!("a process holds at most {MAX_NODES} steps, its blocks' included"));
        return Ty::Any;
    }
    let trigger_ty = match p.resolver.event(&proc_.trigger.event) {
        Some(t) => t,
        None => {
            p.fault(Code::CapabilityUnavailable, format!("{at}/trigger/event"), None, format!("{} is not an event of this workspace", proc_.trigger.event));
            Ty::Any
        }
    };
    let base = Env::default().with("trigger", trigger_ty).with("parameters", params.clone());
    if let Err(e) = expr::compile(&proc_.trigger.business_key, &base) {
        p.refusal(e, format!("{at}/trigger/business_key"), None);
    }
    if proc_.label.trim().is_empty() {
        p.fault(Code::SchemaInvalid, format!("{at}/label"), None, "a process has a label");
    }
    if proc_.max_steps == 0 || proc_.max_steps > MAX_STEPS {
        p.fault(Code::SchemaInvalid, format!("{at}/max_steps"), None, format!("max_steps is between 1 and {MAX_STEPS}"));
    } else if (proc_.max_steps as usize) < proc_.nodes.len() {
        p.fault(
            Code::SchemaInvalid,
            format!("{at}/max_steps"),
            None,
            format!("{} steps cannot all run within max_steps {}", proc_.nodes.len(), proc_.max_steps),
        );
    } else {
        let w = worst(&proc_.nodes, processes, &mut vec![proc_.id.clone()]);
        if w > u64::from(proc_.max_steps) {
            p.note(
                Code::SchemaInvalid,
                format!("{at}/max_steps"),
                None,
                format!("at worst {w} steps run; a run that reaches max_steps {} stops and waits for a person", proc_.max_steps),
            );
        }
    }
    let compensable = all
        .into_iter()
        .map(|(step, cap)| {
            let undo = caps.get(&cap.id).and_then(Capability::compensation);
            (step, cap.id, undo)
        })
        .collect();
    let mut s = Scope { caps, asked, decisions, processes, outputs, recursive, decision_scopes, compensable, ids: BTreeSet::new() };
    block(p, &at, &proc_.entry, &proc_.nodes, Kind::Process, &base, &mut s)
}

/// Checks a block's graph and its steps, in an order where each step's inputs are known. Returns the type of
/// what its completed ends hand back.
fn block(p: &mut Pass, at: &str, entry: &str, nodes: &[Node], kind: Kind, outer: &Env, s: &mut Scope) -> Ty {
    let mut index: BTreeMap<&str, usize> = BTreeMap::new();
    for (ni, n) in nodes.iter().enumerate() {
        let np = format!("{at}/nodes/{ni}");
        if index.insert(n.id(), ni).is_some() || !s.ids.insert(n.id().to_string()) {
            p.fault(Code::SchemaInvalid, format!("{np}/id"), Some(n.id()), format!("two steps are called {}", n.id()));
        }
        if !name_ok(n.id()) || SCOPE_NAMES.contains(&n.id()) {
            p.fault(
                Code::SchemaInvalid,
                format!("{np}/id"),
                Some(n.id()),
                format!("'{}' cannot name a step: lower_snake_case, and none of {}", n.id(), SCOPE_NAMES.join(", ")),
            );
        }
        if n.label().trim().is_empty() {
            p.fault(Code::SchemaInvalid, format!("{np}/label"), Some(n.id()), "every step has a label people can read");
        }
    }
    if !index.contains_key(entry) {
        p.fault(Code::SchemaInvalid, format!("{at}/entry"), None, format!("the entry {entry} is not a step of this block"));
        return Ty::Any;
    }
    let mut broken = false;
    for (ni, n) in nodes.iter().enumerate() {
        for (name, target) in n.routes() {
            if !index.contains_key(target) {
                p.fault(
                    Code::SchemaInvalid,
                    format!("{at}/nodes/{ni}/{name}"),
                    Some(n.id()),
                    format!("{} goes to {target}, which is not a step of its block", n.id()),
                );
                broken = true;
            }
        }
    }
    if broken {
        return Ty::Any;
    }

    // Reachability and cycles: within a block a graph is acyclic; repetition is the repeat step's. A recovery's
    // way back (`resume_to`) runs only when a person decides, so it is no loop, and is checked on its own below.
    let succ = |n: &Node| -> Vec<usize> { n.routes().iter().filter(|(name, _)| *name != "resume_to").filter_map(|(_, t)| index.get(t).copied()).collect() };
    let mut state = vec![0u8; nodes.len()]; // 0 unseen, 1 on the path, 2 done
    let mut cycle = None;
    let mut post = vec![];
    fn dfs(i: usize, nodes: &[Node], succ: &dyn Fn(&Node) -> Vec<usize>, state: &mut [u8], cycle: &mut Option<usize>, post: &mut Vec<usize>) {
        state[i] = 1;
        for j in succ(&nodes[i]) {
            match state[j] {
                1 => {
                    cycle.get_or_insert(j);
                }
                0 => dfs(j, nodes, succ, state, cycle, post),
                _ => {}
            }
        }
        state[i] = 2;
        post.push(i);
    }
    let entry = index[entry];
    dfs(entry, nodes, &succ, &mut state, &mut cycle, &mut post);
    for (ni, n) in nodes.iter().enumerate() {
        if state[ni] == 0 {
            p.fault(Code::SchemaInvalid, format!("{at}/nodes/{ni}"), Some(n.id()), format!("{} can never be reached", n.id()));
        }
    }
    if let Some(j) = cycle {
        p.fault(
            Code::SchemaInvalid,
            format!("{at}/nodes/{j}"),
            Some(nodes[j].id()),
            "a loop back to an earlier step: rework is a repeat step with a bound, not a cycle",
        );
        return Ty::Any;
    }

    // What each step may read: the outputs of steps that always ran before it (its dominators), and what the
    // enclosing blocks could read where this one starts.
    let n = nodes.len();
    let preds: Vec<Vec<usize>> = (0..n).map(|i| (0..n).filter(|&j| state[j] != 0 && succ(&nodes[j]).contains(&i)).collect()).collect();
    let mut dom: Vec<BTreeSet<usize>> = (0..n).map(|i| if i == entry { BTreeSet::from([i]) } else { (0..n).collect() }).collect();
    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..n {
            if i == entry || state[i] == 0 {
                continue;
            }
            let mut d = preds[i].iter().map(|&q| dom[q].clone()).reduce(|a, b| a.intersection(&b).copied().collect()).unwrap_or_default();
            d.insert(i);
            if d != dom[i] {
                dom[i] = d;
                changed = true;
            }
        }
    }
    // A recovery resumes where everything the step reads has run: what surely ran before the resumed step
    // surely ran before the suspension.
    for (si, node) in nodes.iter().enumerate() {
        if let Node::Suspend { resume_to: Some(target), .. } = node
            && let Some(&ti) = index.get(target.as_str())
            && state[si] != 0
            && let Some(missing) = dom[ti].iter().find(|&&d| d != ti && !dom[si].contains(&d))
        {
            p.fault(
                Code::SchemaInvalid,
                format!("{at}/nodes/{si}/resume_to"),
                Some(node.id()),
                format!("{target} may read {}, which may not have run before {}", nodes[*missing].id(), node.id()),
            );
        }
    }
    let mut out: BTreeMap<usize, Ty> = BTreeMap::new();
    let mut ends: BTreeMap<String, Ty> = BTreeMap::new();
    for &i in post.iter().rev() {
        let mut env = outer.clone();
        for &d in &dom[i] {
            if d != i {
                env = env.with(nodes[d].id(), Ty::Record(BTreeMap::from([("output".into(), out.get(&d).cloned().unwrap_or(Ty::Any))])));
            }
        }
        let t = step(p, &format!("{at}/nodes/{i}"), &nodes[i], &env, kind, &index, nodes, s);
        if let Node::End { outcome: EndOutcome::Completed, .. } = &nodes[i]
            && let Ty::Record(fields) = &t
        {
            ends.extend(fields.clone());
        }
        out.insert(i, t);
    }
    Ty::Record(ends)
}

/// Checks one step in its scope. Returns the type of its output (for an end, of what it hands back).
#[allow(clippy::too_many_arguments)]
fn step(p: &mut Pass, np: &str, node: &Node, env: &Env, kind: Kind, index: &BTreeMap<&str, usize>, nodes: &[Node], s: &mut Scope) -> Ty {
    let np = np.to_string();
    let nid = Some(node.id());
    let completed = |target: &str| matches!(index.get(target).map(|&k| &nodes[k]), Some(Node::End { outcome: EndOutcome::Completed, .. }));
    let not_success = |p: &mut Pass, route: &str, target: &str, why: &str| {
        if completed(target) {
            p.fault(Code::SchemaInvalid, format!("{np}/{route}"), nid, why.to_string());
        }
    };
    match node {
        Node::Query { capability, input, on_error, .. } | Node::Action { capability, input, on_error, .. } => {
            let query = matches!(node, Node::Query { .. });
            not_success(p, "on_error", on_error, &format!("{}'s failure goes to a completed end: a failure is not success", node.id()));
            if !s.asked.contains(capability) {
                p.fault(
                    Code::PermissionDenied,
                    format!("{np}/capability"),
                    nid,
                    format!("{} uses {} {}, which the solution does not ask for in requires", node.id(), capability.id, capability.version),
                );
                return Ty::Any;
            }
            let Some(cap) = s.caps.get(&capability.id) else { return Ty::Any };
            if !cap.profiles.contains(&Profile::Process) {
                p.fault(Code::UnsupportedProfile, format!("{np}/capability"), nid, format!("{} does not run in a process", cap.id));
            }
            if query && cap.effect.changes() {
                p.fault(
                    Code::SchemaInvalid,
                    format!("{np}/capability"),
                    nid,
                    format!("a query step cannot use {}, which changes something ({:?})", cap.id, cap.effect),
                );
            }
            if !query && !cap.effect.changes() {
                p.fault(Code::SchemaInvalid, format!("{np}/capability"), nid, format!("an action step uses {}, which only reads: make it a query", cap.id));
            }
            if cap.effect == Effect::HumanTask {
                p.fault(Code::SchemaInvalid, format!("{np}/capability"), nid, "a person's decision is a human_task step");
            }
            // The input: every required field given, nothing it does not take, each of the right type. A
            // dotted key (`values.description`) fills a field of an object the capability takes.
            for missing in missing_inputs(&cap.input, input) {
                p.fault(Code::SchemaInvalid, format!("{np}/input"), nid, format!("{} needs {missing}", cap.id));
            }
            for (k, e) in input {
                let ip = format!("{np}/input/{k}");
                if input.keys().any(|other| other.starts_with(&format!("{k}."))) {
                    p.fault(Code::SchemaInvalid, ip, nid, format!("{k} is given whole and field by field"));
                    continue;
                }
                let Some(sc) = k.split('.').try_fold(&cap.input, |sc, part| sc.get("properties")?.get(part)) else {
                    p.fault(Code::SchemaInvalid, ip, nid, format!("{} takes no {k}", cap.id));
                    continue;
                };
                match expr::compile(e, env) {
                    Ok(c) if expr::fits(&c.ty, &schema_type(sc)) => {}
                    Ok(c) => p.fault(Code::TypeMismatch, ip, nid, format!("{k} of {} is {}, and '{e}' is {}", cap.id, schema_type(sc), c.ty)),
                    Err(err) => p.refusal(err, ip, nid),
                }
            }
            if let Node::Action { effect_key, on_unknown, .. } = node {
                if effect_key != "stable_activation" {
                    p.fault(
                        Code::SchemaInvalid,
                        format!("{np}/effect_key"),
                        nid,
                        "an effect's key is the same for every attempt at one activation (stable_activation), never an attempt's own",
                    );
                }
                match on_unknown {
                    None if cap.unknown_outcome != UnknownOutcome::NotApplicable => {
                        p.fault(Code::SchemaInvalid, np.clone(), nid, format!("{} can end with its outcome unknown: say where that goes (on_unknown)", cap.id))
                    }
                    Some(u) => not_success(p, "on_unknown", u, "an unknown outcome goes to a completed end: unknown is not success"),
                    _ => {}
                }
            }
            schema_type(&cap.output)
        }
        Node::Decision { decision: name, routes, on_error, .. } => {
            not_success(p, "on_error", on_error, "an undecided decision goes to a completed end: unknown is not success");
            let Some(table) = s.decisions.get(name) else {
                p.fault(Code::SchemaInvalid, format!("{np}/decision"), nid, format!("{name} is not a decision of this solution"));
                return Ty::Any;
            };
            if matches!(table.hit_policy, decision::HitPolicy::Collect(_)) {
                p.fault(
                    Code::SchemaInvalid,
                    format!("{np}/decision"),
                    nid,
                    format!("{name} collects many outcomes; a step routes on one: use a unique or first table"),
                );
            }
            s.decision_scopes.entry(name.clone()).or_insert_with(|| env.clone());
            let mut outcomes: BTreeSet<String> = table.rows.iter().map(|r| r.outcome.to_string()).collect();
            if let Some(d) = &table.default {
                outcomes.insert(d.to_string());
            }
            let routed: BTreeSet<String> = routes.iter().map(|r| r.outcome.to_string()).collect();
            if routed != outcomes || routed.len() != routes.len() {
                p.fault(
                    Code::SchemaInvalid,
                    format!("{np}/routes"),
                    nid,
                    format!(
                        "{name}'s outcomes are {}; its routes say {}",
                        outcomes.into_iter().collect::<Vec<_>>().join(", "),
                        routed.into_iter().collect::<Vec<_>>().join(", ")
                    ),
                );
            }
            Ty::Record(BTreeMap::from([("outcome".into(), Ty::Any)]))
        }
        Node::HumanTask { role, exclude_requester, material, form, approvals, delegation, timeout, on_timeout, .. } => {
            if role.trim().is_empty() {
                p.fault(Code::SchemaInvalid, format!("{np}/role"), nid, "say which role decides");
            }
            if !exclude_requester {
                p.fault(
                    Code::PermissionDenied,
                    format!("{np}/exclude_requester"),
                    nid,
                    "the requester may not decide their own request: keep exclude_requester",
                );
            }
            if *approvals == 0 || *approvals > MAX_APPROVALS {
                p.fault(Code::SchemaInvalid, format!("{np}/approvals"), nid, format!("between 1 and {MAX_APPROVALS} people approve"));
            }
            if *delegation > MAX_DELEGATION {
                p.fault(Code::SchemaInvalid, format!("{np}/delegation"), nid, format!("a decision is handed on at most {MAX_DELEGATION} times"));
            }
            form_ok(p, &np, form, nid);
            if let Err(e) = expr::compile(material, env) {
                p.refusal(e, format!("{np}/material"), nid);
            }
            wait_ok(p, format!("{np}/timeout"), timeout, nid, "a task's timeout");
            not_success(p, "on_timeout", on_timeout, "nobody deciding is not success");
            task_output(form)
        }
        Node::WaitEvent { event, correlation, timeout, on_timeout, .. } => {
            let payload = p.resolver.event(event);
            if payload.is_none() {
                p.fault(Code::CapabilityUnavailable, format!("{np}/event"), nid, format!("{event} is not an event of this workspace"));
            }
            if let Err(e) = expr::compile(correlation, env) {
                p.refusal(e, format!("{np}/correlation"), nid);
            }
            wait_ok(p, format!("{np}/timeout"), timeout, nid, "a wait's timeout");
            not_success(p, "on_timeout", on_timeout, "an event that never came is not success");
            payload.unwrap_or(Ty::Any)
        }
        Node::Parallel { join: _, branches, on_error, .. } => {
            not_success(p, "on_error", on_error, "a refused branch is not success");
            if branches.len() < 2 || branches.len() > MAX_BRANCHES {
                p.fault(Code::SchemaInvalid, format!("{np}/branches"), nid, format!("a parallel step has between 2 and {MAX_BRANCHES} branches"));
            }
            let mut ids = BTreeSet::new();
            let mut out = BTreeMap::new();
            for (bi, b) in branches.iter().enumerate() {
                let bp = format!("{np}/branches/{bi}");
                if !name_ok(&b.id) || !ids.insert(b.id.as_str()) {
                    p.fault(Code::SchemaInvalid, format!("{bp}/id"), nid, format!("'{}' cannot name a branch: lower_snake_case, once", b.id));
                }
                let t = block(p, &bp, &b.entry, &b.nodes, Kind::Branch, env, s);
                out.insert(b.id.clone(), t);
            }
            Ty::Record(BTreeMap::from([("branches".into(), Ty::Record(out)), ("winner".into(), Ty::Text)]))
        }
        Node::ForEach { items, key, max_items, concurrency, body, on_error, .. } => {
            not_success(p, "on_error", on_error, "a refused item is not success");
            let item = match expr::compile(items, env) {
                Ok(c) => match c.ty {
                    Ty::List(t) => *t,
                    Ty::Any => Ty::Any,
                    other => {
                        p.fault(Code::TypeMismatch, format!("{np}/items"), nid, format!("'{items}' is {other}, not a list"));
                        Ty::Any
                    }
                },
                Err(e) => {
                    p.refusal(e, format!("{np}/items"), nid);
                    Ty::Any
                }
            };
            let inner = env.clone().with("item", item);
            match expr::compile(key, &inner) {
                Ok(c) if matches!(c.ty, Ty::Text | Ty::Int | Ty::Any) => {}
                Ok(c) => p.fault(Code::TypeMismatch, format!("{np}/key"), nid, format!("an item's key is text or a whole number, and '{key}' is {}", c.ty)),
                Err(e) => p.refusal(e, format!("{np}/key"), nid),
            }
            if *max_items == 0 || *max_items > MAX_ITEMS {
                p.fault(Code::SchemaInvalid, format!("{np}/max_items"), nid, format!("between 1 and {MAX_ITEMS} items"));
            }
            if *concurrency == 0 || *concurrency > MAX_CONCURRENCY || concurrency > max_items {
                p.fault(
                    Code::SchemaInvalid,
                    format!("{np}/concurrency"),
                    nid,
                    format!("between 1 and {MAX_CONCURRENCY} items at once, and no more than max_items"),
                );
            }
            if !name_ok(&body.id) {
                p.fault(Code::SchemaInvalid, format!("{np}/body/id"), nid, format!("'{}' cannot name a block: lower_snake_case", body.id));
            }
            let t = block(p, &format!("{np}/body"), &body.entry, &body.nodes, Kind::Body, &inner, s);
            Ty::List(Box::new(Ty::Record(BTreeMap::from([("key".into(), Ty::Text), ("outcome".into(), Ty::Text), ("output".into(), t)]))))
        }
        Node::Repeat { max_rounds, body, on_exhausted, .. } => {
            not_success(p, "on_exhausted", on_exhausted, "running out of rounds is not success");
            if *max_rounds == 0 || *max_rounds > MAX_ROUNDS {
                p.fault(Code::SchemaInvalid, format!("{np}/max_rounds"), nid, format!("between 1 and {MAX_ROUNDS} rounds"));
            }
            if !name_ok(&body.id) {
                p.fault(Code::SchemaInvalid, format!("{np}/body/id"), nid, format!("'{}' cannot name a block: lower_snake_case", body.id));
            }
            if !body.nodes.iter().any(|n| matches!(n, Node::End { outcome: EndOutcome::Again, .. })) {
                p.note(Code::SchemaInvalid, format!("{np}/body"), nid, "no step of the body ends `again`, so it runs once");
            }
            let t = block(p, &format!("{np}/body"), &body.entry, &body.nodes, Kind::RepeatBody, env, s);
            Ty::Record(BTreeMap::from([("rounds".into(), Ty::Int), ("output".into(), t)]))
        }
        Node::Subprocess { process, input, on_error, .. } => {
            not_success(p, "on_error", on_error, "a refused subprocess is not success");
            let Some(called) = s.processes.get(process) else {
                p.fault(Code::SchemaInvalid, format!("{np}/process"), nid, format!("{process} is not a process of this solution"));
                return Ty::Any;
            };
            if s.recursive.contains(process) {
                p.fault(
                    Code::SchemaInvalid,
                    format!("{np}/process"),
                    nid,
                    format!("{process} calls itself, directly or through others: a subprocess does not recurse"),
                );
                return Ty::Any;
            }
            let trigger = p.resolver.event(&called.trigger.event).unwrap_or(Ty::Any);
            for (k, e) in input {
                let ip = format!("{np}/input/{k}");
                let want = match &trigger {
                    Ty::Record(fields) => match fields.get(k) {
                        Some(t) => t.clone(),
                        None => {
                            p.fault(Code::SchemaInvalid, ip, nid, format!("{process}'s trigger has no {k}"));
                            continue;
                        }
                    },
                    _ => Ty::Any,
                };
                match expr::compile(e, env) {
                    Ok(c) if expr::fits(&c.ty, &want) => {}
                    Ok(c) => p.fault(Code::TypeMismatch, ip, nid, format!("{k} of {process} is {want}, and '{e}' is {}", c.ty)),
                    Err(err) => p.refusal(err, ip, nid),
                }
            }
            s.outputs.get(process).cloned().unwrap_or(Ty::Any)
        }
        Node::Case { owner, tasks, timeout, on_timeout, .. } => {
            not_success(p, "on_timeout", on_timeout, "a case nobody closed is not success");
            if owner.trim().is_empty() {
                p.fault(Code::SchemaInvalid, format!("{np}/owner"), nid, "say which role owns the case");
            }
            if tasks.is_empty() || tasks.len() > MAX_CASE_TASKS {
                p.fault(Code::SchemaInvalid, format!("{np}/tasks"), nid, format!("a case lists between 1 and {MAX_CASE_TASKS} tasks"));
            }
            let names: BTreeSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
            let mut seen = BTreeSet::new();
            let mut out = BTreeMap::new();
            for (ti, t) in tasks.iter().enumerate() {
                let tp = format!("{np}/tasks/{ti}");
                if !name_ok(&t.id) || !seen.insert(t.id.as_str()) {
                    p.fault(Code::SchemaInvalid, format!("{tp}/id"), nid, format!("'{}' cannot name a case task: lower_snake_case, once", t.id));
                }
                if t.role.trim().is_empty() {
                    p.fault(Code::SchemaInvalid, format!("{tp}/role"), nid, "say which role does the task");
                }
                if !t.exclude_requester {
                    p.fault(
                        Code::PermissionDenied,
                        format!("{tp}/exclude_requester"),
                        nid,
                        "the requester may not decide their own request: keep exclude_requester",
                    );
                }
                if t.label.trim().is_empty() {
                    p.fault(Code::SchemaInvalid, format!("{tp}/label"), nid, "every task has a label people can read");
                }
                form_ok(p, &tp, &t.form, nid);
                for r in &t.requires {
                    if r == &t.id || !names.contains(r.as_str()) {
                        p.fault(Code::SchemaInvalid, format!("{tp}/requires"), nid, format!("{} requires {r}, which is not another task of this case", t.id));
                    }
                }
                out.insert(t.id.clone(), task_output(&t.form));
            }
            // Dependencies that wait for each other could never be met.
            let mut done: BTreeSet<&str> = BTreeSet::new();
            while let Some(t) =
                tasks.iter().find(|t| !done.contains(t.id.as_str()) && t.requires.iter().all(|r| done.contains(r.as_str()) || !names.contains(r.as_str())))
            {
                done.insert(&t.id);
            }
            if done.len() < names.len() {
                p.fault(Code::SchemaInvalid, format!("{np}/tasks"), nid, "some of the case's tasks require each other: none of them could start");
            }
            wait_ok(p, format!("{np}/timeout"), timeout, nid, "a case's timeout");
            Ty::Record(out)
        }
        Node::Compensate { on_error, .. } => {
            not_success(p, "on_error", on_error, "a compensation that failed is not success");
            for (step_id, cap, undo) in s.compensable.clone() {
                let Some(undo) = undo else { continue };
                match s.asked.iter().find(|c| c.id == undo) {
                    None => p.fault(
                        Code::PermissionDenied,
                        np.clone(),
                        nid,
                        format!("{step_id} ({cap}) is undone by {undo}, which the solution does not ask for in requires"),
                    ),
                    Some(r) if p.resolver.capability(&r.id, &r.version).is_none_or(|c| !c.effect.changes()) => {
                        p.fault(Code::CapabilityUnavailable, np.clone(), nid, format!("{undo} {} cannot undo anything here", r.version))
                    }
                    _ => {}
                }
            }
            Ty::Record(BTreeMap::from([("compensated".into(), Ty::Int), ("failed".into(), Ty::Int)]))
        }
        Node::Delay { wait, .. } => {
            wait_ok(p, format!("{np}/wait"), wait, nid, "a delay");
            Ty::Record(BTreeMap::from([("at".into(), Ty::Instant)]))
        }
        Node::End { outcome, output, .. } => {
            if *outcome == EndOutcome::Again && kind != Kind::RepeatBody {
                p.fault(Code::SchemaInvalid, format!("{np}/outcome"), nid, "only a repeat step's body ends `again`");
            }
            let mut fields = BTreeMap::new();
            for (name, e) in output {
                if !name_ok(name) {
                    p.fault(Code::SchemaInvalid, format!("{np}/output/{name}"), nid, format!("'{name}' cannot name an output: lower_snake_case"));
                }
                match expr::compile(e, env) {
                    Ok(c) => {
                        fields.insert(name.clone(), c.ty);
                    }
                    Err(err) => p.refusal(err, format!("{np}/output/{name}"), nid),
                }
            }
            Ty::Record(fields)
        }
        Node::Suspend { reason, resume_to, .. } => {
            if reason.trim().is_empty() {
                p.fault(Code::SchemaInvalid, format!("{np}/reason"), nid, "say why it waits for a person");
            }
            if let Some(r) = resume_to {
                not_success(p, "resume_to", r, "a recovery that resumes at a completed end would make needing attention a success");
            }
            Ty::Any
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{Idempotency, Limits, Operation, object_schema};
    use serde_json::Map;

    /// A workspace with the package's purchase and condition capabilities, named as Ferrum names them
    /// (`{package}.{entity}.{operation}`, docs/logic/capabilities.md). Ferrum has no stock hold yet, so those two
    /// keep the package's example names (docs/logic/reuse-map.yaml records the gap).
    struct Fixture {
        caps: Vec<Capability>,
    }

    fn flat(fields: &[&str]) -> Value {
        object_schema(fields.iter().map(|k| (k.to_string(), json!({"type": "string"}))).collect(), fields.iter().map(|s| s.to_string()).collect())
    }

    /// W1's create: the record's fields under `values`.
    fn create_input(fields: &[&str], required: &[&str]) -> Value {
        let values =
            object_schema(fields.iter().map(|k| (k.to_string(), json!({"type": "string"}))).collect(), required.iter().map(|s| s.to_string()).collect());
        object_schema(Map::from_iter([("values".to_string(), values), ("submit".to_string(), json!({"type": "boolean"}))]), vec!["values".into()])
    }

    fn cap(id: &str, effect: Effect, input: Value, output: Value) -> Capability {
        Capability {
            id: id.into(),
            version: "1.0.0".into(),
            digest: String::new(),
            owner: id.split('.').next().unwrap().into(),
            entity: None,
            operation: Operation::Action(id.rsplit('.').next().unwrap().into()),
            label: id.into(),
            description: None,
            effect,
            profiles: vec![Profile::Process],
            permissions: vec![],
            input,
            output,
            idempotency: if effect.changes() { Idempotency::NativeReceipt } else { Idempotency::ReadOnly },
            retention_days: 365,
            unknown_outcome: if effect.changes() { UnknownOutcome::Reconcile } else { UnknownOutcome::NotApplicable },
            completion: "done".into(),
            compensate_with: None,
            limits: Limits::default(),
        }
        .sealed()
        .unwrap()
    }

    impl Fixture {
        fn new() -> Fixture {
            let request = json!({"type": "object", "properties": {
                "total": {"type": "object", "properties": {"amount": {"type": "string"}, "currency": {"type": "string"}}},
                "supplier_email": {"type": "string"}, "version": {"type": "integer"}, "category": {"type": "string"},
                "lines": {"type": "array", "items": {"type": "object", "properties": {"item_code": {"type": "string"}, "qty": {"type": ["string", "number"]}}}}}});
            let id_out = json!({"type": "object", "properties": {"id": {"type": "string"}}});
            let mut mail = cap("ferrum.mail", Effect::ExternalEffect, flat(&["to", "subject", "body"]), json!({"type": "object"}));
            mail.unknown_outcome = UnknownOutcome::Manual;
            Fixture {
                caps: vec![
                    cap("procurement.material_request.read", Effect::Query, flat(&["id"]), request),
                    cap(
                        "procurement.material_request.make_purchase_order",
                        Effect::TransactionalAction,
                        flat(&["id"]),
                        json!({"type": "object", "properties": {"order_id": {"type": "string"}}}),
                    ),
                    mail.sealed().unwrap(),
                    cap(
                        "assets_ops.maintenance_request.create",
                        Effect::TransactionalAction,
                        create_input(&["asset", "description"], &["asset"]),
                        id_out.clone(),
                    ),
                    cap(
                        "assets_ops.maintenance_request.update",
                        Effect::TransactionalAction,
                        json!({"type": "object", "properties": {"id": {"type": "string"}, "values": {"type": "object", "properties": {"status": {"type": "string"}}}}, "required": ["id"]}),
                        id_out.clone(),
                    ),
                    cap("field_service.service_request.create", Effect::TransactionalAction, create_input(&["subject", "description"], &["subject"]), id_out),
                    Capability {
                        owner: "example.inventory".into(),
                        compensate_with: Some("release_hold".into()),
                        ..cap(
                            "example.inventory.place_hold",
                            Effect::TransactionalAction,
                            flat(&["scope_ref", "case_ref"]),
                            json!({"type": "object", "properties": {"hold_id": {"type": "string"}}}),
                        )
                    }
                    .sealed()
                    .unwrap(),
                    cap("example.inventory.release_hold", Effect::TransactionalAction, flat(&["hold_ref", "disposition_ref"]), json!({"type": "object"})),
                ],
            }
        }
    }

    impl Resolver for Fixture {
        fn capability(&self, id: &str, version: &str) -> Option<Capability> {
            self.caps.iter().find(|c| c.id == id && c.version == version).cloned()
        }
        fn installed(&self, id: &str) -> Option<String> {
            self.caps.iter().find(|c| c.id == id).map(|c| c.version.clone())
        }
        fn calendar(&self, name: &str) -> bool {
            name == "india"
        }
        fn event(&self, id: &str) -> Option<Ty> {
            let rec = |fields: &[&str]| Some(Ty::Record(fields.iter().map(|f| (f.to_string(), Ty::Text)).collect()));
            match id {
                "procurement.material_request.submitted" => rec(&["id"]),
                "buying.purchase_order.updated" => rec(&["id", "status"]),
                // Declared by Edge's manifest when its episodes land (W5).
                "edge.condition_episode.opened" => rec(&["episode_id", "asset_id", "evidence_id"]),
                "stock_ops.quality_inspection.submitted" => rec(&["id", "reference_name", "status"]),
                _ => None,
            }
        }
        fn entity(&self, name: &str) -> Option<EntityShape> {
            if name == "Supplier" {
                return Some(EntityShape {
                    fields: BTreeMap::from([
                        ("credit_limit".into(), FieldShape { ty: Ty::Decimal, customer: false, restricted: false, link: None }),
                        ("bank_account".into(), FieldShape { ty: Ty::Text, customer: false, restricted: true, link: None }),
                    ]),
                });
            }
            (name == "MaterialRequest").then(|| EntityShape {
                fields: BTreeMap::from([
                    ("supplier".into(), FieldShape { ty: Ty::Text, customer: false, restricted: false, link: Some("Supplier".into()) }),
                    ("company".into(), FieldShape { ty: Ty::Text, customer: false, restricted: false, link: None }),
                    ("requested_by".into(), FieldShape { ty: Ty::Text, customer: false, restricted: false, link: None }),
                    ("x_department".into(), FieldShape { ty: Ty::Text, customer: true, restricted: false, link: None }),
                    ("x_risk".into(), FieldShape { ty: Ty::Text, customer: true, restricted: false, link: None }),
                    ("x_cost_centre_budget".into(), FieldShape { ty: Ty::Decimal, customer: true, restricted: true, link: None }),
                ]),
            })
        }
    }

    /// The package's purchase-exception example (examples/purchase.solution.json), with Ferrum's ids.
    fn purchase() -> Value {
        json!({
            "schema_version": "ferrum.solution/1", "kind": "Solution", "id": "acme.purchase_exception", "version": "1.0.0", "label": "Purchase exception",
            "requires": [
                {"id": "procurement.material_request.read", "version": "1.0.0"},
                {"id": "procurement.material_request.make_purchase_order", "version": "1.0.0"},
                {"id": "ferrum.mail", "version": "1.0.0"}
            ],
            "parameters": {"review_limit": {"type": "money:INR", "value": {"amount": "100000.00", "currency": "INR"}}},
            "rules": [
                {"id": "required_request_data", "entity": "MaterialRequest", "phase": "validate", "check": "has(doc.company) && has(doc.requested_by)", "message": "A request names its company and who asked"},
                {"id": "department_from_requester", "entity": "MaterialRequest", "phase": "derive", "writes": {"x_department": "coalesce(doc.x_department, \"general\")"}}
            ],
            "decisions": [{
                "id": "requires_review", "hit_policy": "unique",
                "inputs": [{"name": "total", "value": "snapshot.output.total"}],
                "rows": [{"id": "high_amount", "match": {"total": {"gt": {"amount": "100000.00", "currency": "INR"}}}, "outcome": "review"}],
                "default": "proceed"
            }],
            "processes": [{
                "id": "purchase_exception", "label": "Purchase exception",
                "trigger": {"event": "procurement.material_request.submitted", "business_key": "trigger.id"},
                "entry": "snapshot", "max_steps": 100,
                "nodes": [
                    {"type": "query", "id": "snapshot", "label": "Read the request", "capability": {"id": "procurement.material_request.read", "version": "1.0.0"}, "input": {"id": "trigger.id"}, "next": "decision", "on_error": "needs_attention"},
                    {"type": "decision", "id": "decision", "label": "Policy", "decision": "requires_review", "routes": [{"outcome": "review", "target": "review"}, {"outcome": "proceed", "target": "create_order"}], "on_error": "needs_attention"},
                    {"type": "human_task", "id": "review", "label": "Independent approval", "role": "Purchasing Reviewer", "material": "snapshot.output", "timeout": "P2D", "next": "create_order", "on_reject": "refused", "on_timeout": "needs_attention"},
                    {"type": "action", "id": "create_order", "label": "Create the order", "capability": {"id": "procurement.material_request.make_purchase_order", "version": "1.0.0"}, "input": {"id": "trigger.id"}, "next": "submit_supplier", "on_error": "needs_attention", "on_unknown": "needs_attention"},
                    {"type": "action", "id": "submit_supplier", "label": "Send to the supplier", "capability": {"id": "ferrum.mail", "version": "1.0.0"}, "input": {"to": "snapshot.output.supplier_email", "subject": "\"Purchase order\"", "body": "create_order.output.order_id"}, "next": "supplier_ack", "on_error": "needs_attention", "on_unknown": "needs_attention"},
                    {"type": "wait_event", "id": "supplier_ack", "label": "Acknowledged", "event": "buying.purchase_order.updated", "correlation": "create_order.output.order_id", "timeout": 86400, "next": "completed", "on_timeout": "needs_attention"},
                    {"type": "end", "id": "completed", "label": "Completed", "outcome": "completed"},
                    {"type": "end", "id": "refused", "label": "Refused", "outcome": "refused"},
                    {"type": "suspend", "id": "needs_attention", "label": "Needs attention", "reason": "Resolve original effects before continuing. This is not success."}
                ]
            }],
            "tests": [
                {"id": "at_the_limit", "decision": "requires_review", "given": {"snapshot": {"output": {"total": {"amount": "100000.00", "currency": "INR"}}}}, "expect": "proceed"},
                {"id": "over_the_limit", "decision": "requires_review", "given": {"snapshot": {"output": {"total": {"amount": "100000.01", "currency": "INR"}}}}, "expect": "review"},
                {"id": "no_currency", "decision": "requires_review", "given": {"snapshot": {"output": {"total": {"amount": "5"}}}}, "expect": "unknown"}
            ],
            "ui": {"layout": {"snapshot": {"x": 10, "y": 20}}, "notes": "needs_attention is not success"}
        })
    }

    /// The package's condition-response example (examples/condition.solution.json), with Ferrum's ids: one
    /// operational episode opens one case; people approve the hold and its release; nothing overrides quality.
    fn condition() -> Value {
        let act = |id: &str, label: &str, cap: &str, input: Value, next: &str| json!({"type": "action", "id": id, "label": label, "capability": {"id": cap, "version": "1.0.0"}, "input": input, "next": next, "on_error": "needs_attention", "on_unknown": "needs_attention"});
        json!({
            "schema_version": "ferrum.solution/1", "kind": "Solution", "id": "acme.condition_response", "version": "1.0.0", "label": "Condition response",
            "description": "One operational episode starts one business case; does not bypass native action or physical authority.",
            "requires": [
                {"id": "assets_ops.maintenance_request.create", "version": "1.0.0"},
                {"id": "example.inventory.place_hold", "version": "1.0.0"},
                {"id": "field_service.service_request.create", "version": "1.0.0"},
                {"id": "example.inventory.release_hold", "version": "1.0.0"},
                {"id": "assets_ops.maintenance_request.update", "version": "1.0.0"}
            ],
            "processes": [{
                "id": "condition_response", "label": "Condition response",
                "trigger": {"event": "edge.condition_episode.opened", "business_key": "trigger.episode_id"},
                "entry": "open_case", "max_steps": 100,
                "nodes": [
                    act("open_case", "Open the case with its evidence", "assets_ops.maintenance_request.create", json!({"values.asset": "trigger.asset_id", "values.description": "trigger.evidence_id"}), "review_hold"),
                    {"type": "human_task", "id": "review_hold", "label": "Approve the affected stock and the hold", "role": "Quality Manager", "material": "open_case.output",
                     "form": [{"name": "affected_stock", "label": "Affected stock", "type": "text", "required": true}],
                     "timeout": 3600, "next": "hold", "on_reject": "refused", "on_timeout": "needs_attention"},
                    act("hold", "Place the approved hold", "example.inventory.place_hold", json!({"scope_ref": "review_hold.output.affected_stock", "case_ref": "open_case.output.id"}), "request_work"),
                    act("request_work", "Ask for maintenance", "field_service.service_request.create", json!({"values.subject": "trigger.asset_id", "values.description": "open_case.output.id"}), "inspection"),
                    {"type": "wait_event", "id": "inspection", "label": "Wait for the completed inspection", "event": "stock_ops.quality_inspection.submitted", "correlation": "open_case.output.id", "timeout": 604800, "next": "disposition", "on_timeout": "needs_attention"},
                    {"type": "end", "id": "refused", "label": "Refused", "outcome": "refused"},
                    {"type": "suspend", "id": "needs_attention", "label": "Needs attention", "reason": "Resolve original effects, prerequisites and evidence before continuing. This is not success."},
                    {"type": "human_task", "id": "disposition", "label": "Approve the release on the inspection's evidence", "role": "Quality Release Authority", "material": "inspection.output",
                     "timeout": 86400, "next": "release_hold", "on_reject": "needs_attention", "on_timeout": "needs_attention"},
                    act("release_hold", "Release only the approved hold", "example.inventory.release_hold", json!({"hold_ref": "hold.output.hold_id", "disposition_ref": "disposition.output.decision_id"}), "close_case"),
                    act("close_case", "Close the case with its evidence", "assets_ops.maintenance_request.update", json!({"id": "open_case.output.id", "values.status": "\"Closed\""}), "completed"),
                    {"type": "end", "id": "completed", "label": "Resolved and recorded", "outcome": "completed"}
                ]
            }],
            "ui": {"notes": "Inspection and an independent disposition gate the hold's release. Rejection or unresolved effects suspend for reviewed recovery; no implicit quality override."}
        })
    }

    /// One process using every structured step: each line held (for-each), the supplier answering by mail or
    /// on the portal, whichever comes first (any), a two-person review with rework (repeat), an investigation
    /// case, a business-day delay, compensation and a recovery that resumes it.
    fn structured() -> Value {
        let mut v = purchase();
        v["requires"]
            .as_array_mut()
            .unwrap()
            .extend([json!({"id": "example.inventory.place_hold", "version": "1.0.0"}), json!({"id": "example.inventory.release_hold", "version": "1.0.0"})]);
        v["processes"][0]["nodes"] = json!([
            {"type": "query", "id": "snapshot", "label": "Read the request", "capability": {"id": "procurement.material_request.read", "version": "1.0.0"}, "input": {"id": "trigger.id"}, "next": "lines_each", "on_error": "refused"},
            {"type": "for_each", "id": "lines_each", "label": "Hold each line", "items": "snapshot.output.lines", "key": "item.item_code", "max_items": 50, "concurrency": 5,
             "body": {"id": "line", "entry": "hold_line", "nodes": [
                {"type": "action", "id": "hold_line", "label": "Hold the line's stock", "capability": {"id": "example.inventory.place_hold", "version": "1.0.0"}, "input": {"scope_ref": "item.item_code", "case_ref": "trigger.id"}, "next": "line_done", "on_error": "line_failed", "on_unknown": "line_failed"},
                {"type": "end", "id": "line_done", "label": "Held", "outcome": "completed", "output": {"held": "hold_line.output.hold_id"}},
                {"type": "end", "id": "line_failed", "label": "Not held", "outcome": "refused"}
             ]},
             "next": "quotes", "on_error": "undo"},
            {"type": "parallel", "id": "quotes", "label": "Hear from the supplier", "join": "any", "branches": [
                {"id": "email", "entry": "mail", "nodes": [
                    {"type": "action", "id": "mail", "label": "Mail the supplier", "capability": {"id": "ferrum.mail", "version": "1.0.0"}, "input": {"to": "snapshot.output.supplier_email", "subject": "\"Order\"", "body": "trigger.id"}, "next": "sent", "on_error": "mail_failed", "on_unknown": "mail_failed"},
                    {"type": "end", "id": "sent", "label": "Mailed", "outcome": "completed"},
                    {"type": "end", "id": "mail_failed", "label": "Not mailed", "outcome": "refused"}
                ]},
                {"id": "portal", "entry": "wait_portal", "nodes": [
                    {"type": "wait_event", "id": "wait_portal", "label": "Acknowledged on the portal", "event": "buying.purchase_order.updated", "correlation": "trigger.id", "timeout": "P1D", "next": "acknowledged", "on_timeout": "late"},
                    {"type": "end", "id": "acknowledged", "label": "Acknowledged", "outcome": "completed"},
                    {"type": "end", "id": "late", "label": "Too late", "outcome": "refused"}
                ]}
            ], "next": "review_loop", "on_error": "undo"},
            {"type": "repeat", "id": "review_loop", "label": "Review until right", "max_rounds": 3,
             "body": {"id": "round", "entry": "review", "nodes": [
                {"type": "human_task", "id": "review", "label": "Two reviewers approve", "role": "Purchasing Reviewer", "material": "snapshot.output", "approvals": 2, "delegation": 1,
                 "timeout": {"business_days": 2, "calendar": "india"}, "next": "approved", "on_reject": "rework", "on_timeout": "nobody"},
                {"type": "end", "id": "approved", "label": "Approved", "outcome": "completed"},
                {"type": "end", "id": "rework", "label": "Changes asked for", "outcome": "again"},
                {"type": "end", "id": "nobody", "label": "Nobody decided", "outcome": "refused"}
             ]},
             "next": "investigate", "on_exhausted": "undo"},
            {"type": "case", "id": "investigate", "label": "Investigate", "owner": "Quality Manager", "timeout": "P7D", "tasks": [
                {"id": "inspect", "label": "Inspect the goods", "role": "Inspector", "required": true, "form": [{"name": "passed", "label": "Passed", "type": "bool", "required": true}]},
                {"id": "report", "label": "Write the report", "role": "Quality Manager", "requires": ["inspect"]}
            ], "next": "settle", "on_timeout": "undo"},
            {"type": "delay", "id": "settle", "label": "Let it settle", "wait": {"business_days": 1, "calendar": "india"}, "next": "completed"},
            {"type": "compensate", "id": "undo", "label": "Release what was held", "next": "refused", "on_error": "attention"},
            {"type": "end", "id": "completed", "label": "Completed", "outcome": "completed", "output": {"lines": "lines_each.output", "rounds": "review_loop.output.rounds"}},
            {"type": "end", "id": "refused", "label": "Refused", "outcome": "refused"},
            {"type": "suspend", "id": "attention", "label": "Needs attention", "reason": "A compensation failed: settle it, then release again.", "resume_to": "undo"}
        ]);
        v["processes"][0]["entry"] = json!("snapshot");
        // Its review is a person's, not the purchase example's decision table.
        v["decisions"] = json!([]);
        v["tests"] = json!([]);
        v
    }

    fn compile_value(v: Value, profile: RuntimeProfile) -> Result<Release, Vec<Diagnostic>> {
        let s = Solution::from_value(v).map_err(|r| vec![Diagnostic { code: r.code, message: r.message, pointer: String::new(), node: None, fault: true }])?;
        compile(&s, &Fixture::new(), profile)
    }

    fn faults(v: Value) -> Vec<Diagnostic> {
        match compile_value(v, RuntimeProfile::FullProcess) {
            Ok(_) => vec![],
            Err(d) => d.into_iter().filter(|d| d.fault).collect(),
        }
    }

    fn refused_with(v: Value, code: Code, words: &str) {
        let f = faults(v);
        assert!(f.iter().any(|d| d.code == code && d.message.contains(words)), "expected {code:?} with '{words}', got {f:#?}");
    }

    fn node(v: &mut Value, id: &str) -> Value {
        v["processes"][0]["nodes"].as_array().unwrap().iter().find(|n| n["id"] == id).unwrap().clone()
    }

    fn set_node(v: &mut Value, id: &str, f: impl Fn(&mut Value)) {
        for n in v["processes"][0]["nodes"].as_array_mut().unwrap() {
            if n["id"] == id {
                f(n);
            }
        }
    }

    // The package's 34 checker cases (tests/test_contracts.py), as faults of this compiler. Cases 23-26
    // (pipelines) are Edge's, 27-28 (analysis intents) Intelligence's and 29-31 (bindings) the deployment's,
    // and are ported where those land; case 20's duplicate catalog entry is the kernel catalog's
    // (capability_it.rs); 34 is `scripts/registry.py check`. docs/logic/evidence/w2-compiler.md maps them.

    #[test]
    fn t01_the_purchase_example_compiles() {
        let r = compile_value(purchase(), RuntimeProfile::FullProcess).unwrap_or_else(|d| panic!("{d:#?}"));
        assert_eq!(r.lock.len(), 3);
        assert!(r.artifact_digest.starts_with("sha256:") && r.source_digest.starts_with("sha256:"));
    }

    #[test]
    fn t02_the_condition_example_compiles() {
        let r = compile_value(condition(), RuntimeProfile::FullProcess).unwrap_or_else(|d| panic!("{d:#?}"));
        assert_eq!(r.lock.len(), 5);
    }

    #[test]
    fn t10_a_rule_reaches_no_capability() {
        // A rule runs inside the record's transaction: it has no way to name an effect at all.
        let mut v = purchase();
        v["rules"][0]["capability"] = json!({"id": "ferrum.mail", "version": "1.0.0"});
        refused_with(v, Code::SchemaInvalid, "unknown field `capability`");
    }

    #[test]
    fn unknown_keys_are_refused_everywhere() {
        let mut v = purchase();
        v["processes"][0]["runtime"] = json!("volatile");
        refused_with(v, Code::SchemaInvalid, "unknown field `runtime`");
        let mut v = purchase();
        set_node(&mut v, "needs_attention", |n| n["resume_policy"] = json!("force_success"));
        refused_with(v, Code::SchemaInvalid, "resume_policy");
        let mut v = purchase();
        set_node(&mut v, "review", |n| n["timeout_seconds"] = json!(60));
        refused_with(v, Code::SchemaInvalid, "timeout_seconds");
    }

    #[test]
    fn a_task_form_is_typed_and_read_by_name() {
        let mut v = condition();
        set_node(&mut v, "hold", |n| n["input"]["scope_ref"] = json!("review_hold.output.affected_stok"));
        refused_with(v, Code::SchemaInvalid, "affected_stok");
        let mut v = condition();
        set_node(&mut v, "review_hold", |n| n["form"][0]["name"] = json!("decision"));
        refused_with(v, Code::SchemaInvalid, "cannot name a form field");
        let mut v = condition();
        set_node(&mut v, "review_hold", |n| n["form"][0]["type"] = json!("blob"));
        refused_with(v, Code::SchemaInvalid, "unknown type 'blob'");
        let mut v = condition();
        set_node(&mut v, "review_hold", |n| n["form"][0]["type"] = json!("money:INR"));
        refused_with(v, Code::TypeMismatch, "scope_ref");
    }

    #[test]
    fn nested_inputs_are_checked_field_by_field() {
        let mut v = condition();
        set_node(&mut v, "open_case", |n| {
            n["input"].as_object_mut().unwrap().remove("values.asset");
        });
        refused_with(v, Code::SchemaInvalid, "needs values.asset");
        let mut v = condition();
        set_node(&mut v, "open_case", |n| n["input"]["values.colour"] = json!("\"red\""));
        refused_with(v, Code::SchemaInvalid, "takes no values.colour");
        let mut v = condition();
        set_node(&mut v, "open_case", |n| n["input"]["values"] = json!("trigger"));
        refused_with(v, Code::SchemaInvalid, "given whole and field by field");
    }

    #[test]
    fn names_versions_and_bounds() {
        let mut v = purchase();
        v["processes"][0]["nodes"][0]["id"] = json!("read-request");
        v["processes"][0]["entry"] = json!("read-request");
        refused_with(v, Code::SchemaInvalid, "cannot name a step");
        let mut v = purchase();
        set_node(&mut v, "refused", |n| n["id"] = json!("trigger"));
        set_node(&mut v, "review", |n| n["on_reject"] = json!("trigger"));
        refused_with(v, Code::SchemaInvalid, "cannot name a step");
        let mut v = purchase();
        v["version"] = json!("1.0");
        refused_with(v, Code::SchemaInvalid, "not a version");
        let mut v = purchase();
        v["requires"][0]["version"] = json!("latest");
        refused_with(v, Code::SchemaInvalid, "not a version");
        let mut v = purchase();
        v["processes"][0]["max_steps"] = json!(100_001);
        refused_with(v, Code::SchemaInvalid, "max_steps is between");
        let mut v = purchase();
        set_node(&mut v, "review", |n| n["timeout"] = json!("P400D"));
        refused_with(v, Code::SchemaInvalid, "at most a year");
        let mut v = purchase();
        let again = v["processes"][0].clone();
        v["processes"].as_array_mut().unwrap().push(again);
        refused_with(v, Code::SchemaInvalid, "another process has it");
    }

    fn set_in(v: &mut Value, path: &[&str], f: impl Fn(&mut Value)) {
        let mut cur = &mut v["processes"][0]["nodes"];
        for id in path {
            let list = if cur.is_array() { cur } else { &mut cur["nodes"] };
            cur = list.as_array_mut().unwrap().iter_mut().find(|n| n["id"] == *id).unwrap_or_else(|| panic!("no {id}"));
        }
        f(cur);
    }

    #[test]
    fn structured_steps_compile() {
        let r = compile_value(structured(), RuntimeProfile::FullProcess).unwrap_or_else(|d| panic!("{d:#?}"));
        assert!(r.notes.iter().any(|n| n["message"].as_str().unwrap().contains("at worst")), "every bound spent is more than max_steps: {:?}", r.notes);
    }

    #[test]
    fn for_each_reads_a_bounded_list_by_stable_keys() {
        let mut v = structured();
        set_in(&mut v, &["lines_each"], |n| n["items"] = json!("snapshot.output.total"));
        refused_with(v, Code::TypeMismatch, "not a list");
        let mut v = structured();
        set_in(&mut v, &["lines_each"], |n| n["key"] = json!("item"));
        refused_with(v, Code::TypeMismatch, "an item's key");
        for (field, value, words) in [("max_items", json!(0), "items"), ("max_items", json!(5000), "items"), ("concurrency", json!(80), "at once")] {
            let mut v = structured();
            set_in(&mut v, &["lines_each"], |n| n[field] = value.clone());
            refused_with(v, Code::SchemaInvalid, words);
        }
        let mut v = structured();
        set_in(&mut v, &["quotes"], |n| n["branches"][0]["nodes"][0]["input"]["body"] = json!("item.item_code"));
        refused_with(v, Code::SchemaInvalid, "unknown name 'item'");
    }

    #[test]
    fn a_block_keeps_its_steps_to_itself() {
        let mut v = structured();
        set_in(&mut v, &["completed"], |n| n["output"]["held"] = json!("hold_line.output.hold_id"));
        refused_with(v, Code::SchemaInvalid, "unknown name 'hold_line'");
        let mut v = structured();
        set_in(&mut v, &["lines_each"], |n| n["body"]["nodes"][0]["next"] = json!("quotes"));
        refused_with(v, Code::SchemaInvalid, "not a step of its block");
        let mut v = structured();
        set_in(&mut v, &["lines_each"], |n| n["body"]["nodes"][1]["id"] = json!("snapshot"));
        refused_with(v, Code::SchemaInvalid, "two steps are called snapshot");
    }

    #[test]
    fn parallel_branches_are_named_and_more_than_one() {
        let mut v = structured();
        set_in(&mut v, &["quotes"], |n| {
            n["branches"].as_array_mut().unwrap().pop();
        });
        refused_with(v, Code::SchemaInvalid, "between 2 and");
        let mut v = structured();
        set_in(&mut v, &["quotes"], |n| n["branches"][1]["id"] = json!("email"));
        refused_with(v, Code::SchemaInvalid, "cannot name a branch");
        let mut v = structured();
        set_in(&mut v, &["quotes"], |n| n["on_error"] = json!("completed"));
        refused_with(v, Code::SchemaInvalid, "a refused branch is not success");
    }

    #[test]
    fn rework_is_bounded_and_running_out_is_not_success() {
        let mut v = structured();
        set_in(&mut v, &["review_loop"], |n| n["max_rounds"] = json!(0));
        refused_with(v, Code::SchemaInvalid, "rounds");
        let mut v = structured();
        set_in(&mut v, &["review_loop"], |n| n["on_exhausted"] = json!("completed"));
        refused_with(v, Code::SchemaInvalid, "running out of rounds is not success");
        let mut v = structured();
        set_in(&mut v, &["lines_each"], |n| n["body"]["nodes"][2]["outcome"] = json!("again"));
        refused_with(v, Code::SchemaInvalid, "only a repeat step's body ends `again`");
    }

    #[test]
    fn tasks_bound_their_approvers_delegation_and_calendar() {
        for (field, value, words) in [("approvals", json!(0), "approve"), ("approvals", json!(12), "approve"), ("delegation", json!(9), "handed on")] {
            let mut v = structured();
            set_in(&mut v, &["review_loop"], |n| n["body"]["nodes"][0][field] = value.clone());
            refused_with(v, Code::SchemaInvalid, words);
        }
        let mut v = structured();
        set_in(&mut v, &["settle"], |n| n["wait"] = json!({"business_days": 1, "calendar": "mars"}));
        refused_with(v, Code::CapabilityUnavailable, "mars is not a calendar");
    }

    #[test]
    fn a_case_offers_only_its_tasks_and_their_order() {
        let mut v = structured();
        set_in(&mut v, &["investigate"], |n| n["tasks"][0]["requires"] = json!(["report"]));
        refused_with(v, Code::SchemaInvalid, "require each other");
        let mut v = structured();
        set_in(&mut v, &["investigate"], |n| n["tasks"][1]["requires"] = json!(["approve_payment"]));
        refused_with(v, Code::SchemaInvalid, "not another task of this case");
        let mut v = structured();
        set_in(&mut v, &["investigate"], |n| n["tasks"][0]["exclude_requester"] = json!(false));
        refused_with(v, Code::PermissionDenied, "their own request");
    }

    #[test]
    fn compensation_is_asked_for_and_recovery_is_not_success() {
        let mut v = structured();
        v["requires"].as_array_mut().unwrap().retain(|c| c["id"] != "example.inventory.release_hold");
        refused_with(v, Code::PermissionDenied, "undone by example.inventory.release_hold");
        let mut v = structured();
        set_in(&mut v, &["attention"], |n| n["resume_to"] = json!("completed"));
        refused_with(v, Code::SchemaInvalid, "needing attention a success");
        let mut v = structured();
        set_in(&mut v, &["snapshot"], |n| n["on_error"] = json!("attention"));
        refused_with(v, Code::SchemaInvalid, "undo may read lines_each, which may not have run before attention");
    }

    #[test]
    fn subprocesses_are_typed_and_never_recurse() {
        let mut v = structured();
        let mut child = v["processes"][0].clone();
        child["id"] = json!("child");
        child["nodes"] = json!([{"type": "end", "id": "child_done", "label": "Done", "outcome": "completed", "output": {"echo": "trigger.id"}}]);
        child["entry"] = json!("child_done");
        v["processes"].as_array_mut().unwrap().push(child);
        set_in(
            &mut v,
            &["settle"],
            |n| *n = json!({"type": "subprocess", "id": "settle", "label": "Run the child", "process": "child", "input": {"id": "trigger.id"}, "next": "completed", "on_error": "undo"}),
        );
        compile_value(v.clone(), RuntimeProfile::FullProcess).unwrap_or_else(|d| panic!("{d:#?}"));
        let mut typed = v.clone();
        set_in(&mut typed, &["completed"], |n| n["output"]["echo"] = json!("settle.output.echo"));
        compile_value(typed, RuntimeProfile::FullProcess).unwrap_or_else(|d| panic!("a subprocess's output is typed: {d:#?}"));
        let mut wrong = v.clone();
        set_in(&mut wrong, &["settle"], |n| n["input"] = json!({"colour": "\"red\""}));
        refused_with(wrong, Code::SchemaInvalid, "trigger has no colour");
        let mut looped = v;
        looped["processes"][1]["nodes"] = json!([
            {"type": "subprocess", "id": "again_child", "label": "Itself", "process": "child", "input": {}, "next": "child_done", "on_error": "child_done"},
            {"type": "end", "id": "child_done", "label": "Done", "outcome": "refused"}
        ]);
        looped["processes"][1]["entry"] = json!("again_child");
        refused_with(looped, Code::SchemaInvalid, "a subprocess does not recurse");
    }

    #[test]
    fn a_step_routes_on_one_outcome() {
        let mut v = purchase();
        v["decisions"][0]["hit_policy"] = json!({"collect": "count"});
        refused_with(v, Code::SchemaInvalid, "a step routes on one");
    }

    #[test]
    fn t03_an_unknown_root_key_is_refused() {
        let mut v = purchase();
        v["admin"] = json!(true);
        refused_with(v, Code::SchemaInvalid, "admin");
    }

    #[test]
    fn t04_t05_t06_t07_graph_faults() {
        let mut v = purchase();
        let dup = node(&mut v, "snapshot");
        v["processes"][0]["nodes"].as_array_mut().unwrap().push(dup);
        refused_with(v, Code::SchemaInvalid, "two steps are called snapshot");
        let mut v = purchase();
        set_node(&mut v, "snapshot", |n| n["next"] = json!("missing"));
        refused_with(v, Code::SchemaInvalid, "not a step");
        let mut v = purchase();
        v["processes"][0]["nodes"].as_array_mut().unwrap().push(json!({"type": "end", "id": "orphan", "label": "Orphan", "outcome": "completed"}));
        refused_with(v, Code::SchemaInvalid, "never be reached");
        let mut v = purchase();
        set_node(&mut v, "snapshot", |n| n["next"] = json!("snapshot"));
        refused_with(v, Code::SchemaInvalid, "loop");
    }

    #[test]
    fn t08_t09_capabilities_must_exist_and_be_asked_for() {
        let mut v = purchase();
        set_node(&mut v, "create_order", |n| n["capability"]["id"] = json!("procurement.unknown.action"));
        refused_with(v, Code::PermissionDenied, "does not ask for");
        let mut v = purchase();
        v["requires"][0]["id"] = json!("procurement.unknown.action");
        refused_with(v, Code::CapabilityUnavailable, "not installed");
        let mut v = purchase();
        v["requires"].as_array_mut().unwrap().pop();
        refused_with(v, Code::PermissionDenied, "does not ask for");
        let mut v = purchase();
        v["requires"][0]["version"] = json!("0.9.0");
        refused_with(v, Code::CapabilityUnavailable, "installed at version 1.0.0");
    }

    #[test]
    fn t10_t11_t12_rules_stay_inside_their_record() {
        let mut v = purchase();
        v["rules"][1]["writes"] = json!({"tenant_id": "\"other\""});
        refused_with(v, Code::PermissionDenied, "platform's");
        let mut v = purchase();
        v["rules"][1]["writes"] = json!({"requested_by": "\"someone else\""});
        refused_with(v, Code::PermissionDenied, "customer's own fields");
        let mut v = purchase();
        v["rules"][0]["writes"] = json!({"x_risk": "\"high\""});
        refused_with(v, Code::SchemaInvalid, "writes nothing");
        let mut v = purchase();
        v["rules"][0]["check"] = json!("http_get(\"https://evil.test\") == \"ok\"");
        refused_with(v, Code::CapabilityUnavailable, "no function");
    }

    #[test]
    fn t13_a_query_cannot_write() {
        let mut v = purchase();
        set_node(&mut v, "snapshot", |n| n["capability"] = json!({"id": "procurement.material_request.make_purchase_order", "version": "1.0.0"}));
        refused_with(v, Code::SchemaInvalid, "a query step cannot use");
    }

    #[test]
    fn t14_t15_t16_effects_are_keyed_and_unknown_is_not_success() {
        let mut v = purchase();
        set_node(&mut v, "create_order", |n| n["effect_key"] = json!("attempt_id"));
        refused_with(v, Code::SchemaInvalid, "never an attempt's own");
        let mut v = purchase();
        set_node(&mut v, "submit_supplier", |n| {
            n.as_object_mut().unwrap().remove("on_unknown");
        });
        refused_with(v, Code::SchemaInvalid, "on_unknown");
        let mut v = purchase();
        set_node(&mut v, "submit_supplier", |n| n["on_unknown"] = json!("completed"));
        refused_with(v, Code::SchemaInvalid, "unknown is not success");
        let mut v = purchase();
        set_node(&mut v, "create_order", |n| n["on_error"] = json!("completed"));
        refused_with(v, Code::SchemaInvalid, "a failure is not success");
    }

    #[test]
    fn t17_nobody_approves_their_own_request() {
        let mut v = purchase();
        set_node(&mut v, "review", |n| n["exclude_requester"] = json!(false));
        refused_with(v, Code::PermissionDenied, "their own request");
    }

    #[test]
    fn t18_t19_decisions_and_their_routes() {
        let mut v = purchase();
        set_node(&mut v, "decision", |n| n["decision"] = json!("absent"));
        refused_with(v, Code::SchemaInvalid, "not a decision");
        let mut v = purchase();
        set_node(&mut v, "decision", |n| {
            n["routes"].as_array_mut().unwrap().pop();
        });
        refused_with(v, Code::SchemaInvalid, "routes say");
    }

    #[test]
    fn t20_a_capability_asked_for_twice() {
        let mut v = purchase();
        let first = v["requires"][0].clone();
        v["requires"].as_array_mut().unwrap().push(first);
        refused_with(v, Code::SchemaInvalid, "asked for twice");
    }

    #[test]
    fn t21_a_process_needs_its_profile() {
        let d = compile_value(purchase(), RuntimeProfile::Core).unwrap_err();
        assert!(d.iter().any(|x| x.code == Code::UnsupportedProfile), "{d:#?}");
    }

    #[test]
    fn t22_the_step_budget_covers_the_graph() {
        let mut v = purchase();
        v["processes"][0]["max_steps"] = json!(1);
        refused_with(v, Code::SchemaInvalid, "max_steps");
    }

    #[test]
    fn t32_t33_the_source_is_read_strictly() {
        assert!(Solution::parse(r#"{"schema_version": "ferrum.solution/1", "kind": "Solution", "kind": "Other"}"#).is_err(), "a key twice");
        assert!(Solution::parse("{\"a\": NaN}").is_err());
    }

    // Beyond the package's checker.

    #[test]
    fn types_follow_the_data() {
        let mut v = purchase();
        v["decisions"][0]["inputs"][0]["value"] = json!("snapshot.output.totl");
        refused_with(v, Code::SchemaInvalid, "no field 'totl'");
        let mut v = purchase();
        set_node(&mut v, "submit_supplier", |n| n["input"]["to"] = json!("snapshot.output.version"));
        refused_with(v, Code::TypeMismatch, "to of ferrum.mail");
        let mut v = purchase();
        v["parameters"]["review_limit"]["value"] = json!({"amount": "100000.00"});
        refused_with(v, Code::TypeMismatch, "review_limit");
    }

    #[test]
    fn a_step_reads_only_what_has_surely_run() {
        let mut v = purchase();
        // The decision may skip the review: create_order cannot read the review's output.
        set_node(&mut v, "create_order", |n| n["input"] = json!({"id": "review.output.decision"}));
        refused_with(v, Code::SchemaInvalid, "unknown name 'review'");
    }

    #[test]
    fn rules_read_nothing_restricted_and_write_once() {
        let mut v = purchase();
        v["rules"][0]["check"] = json!("doc.x_cost_centre_budget > dec(\"0\")");
        refused_with(v, Code::PermissionDenied, "restricted");
        let mut v = purchase();
        v["rules"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "another_department", "entity": "MaterialRequest", "phase": "derive", "writes": {"x_department": "\"ops\""}}));
        refused_with(v, Code::Conflict, "one writer per field");
    }

    #[test]
    fn a_release_locks_the_fields_its_rules_use() {
        let r = compile_value(purchase(), RuntimeProfile::FullProcess).unwrap();
        let locked: Vec<&str> = r.fields.keys().map(String::as_str).collect();
        assert_eq!(locked, vec!["MaterialRequest.company", "MaterialRequest.requested_by", "MaterialRequest.x_department"]);
    }

    #[test]
    fn linked_records_are_declared_and_unrestricted() {
        let mut v = purchase();
        v["rules"].as_array_mut().unwrap().push(json!({
            "id": "within_credit", "entity": "MaterialRequest", "phase": "validate", "message": "Over the supplier's limit",
            "lookups": [{"name": "supplier", "link": "supplier", "fields": ["credit_limit"]}],
            "check": "lookups.supplier.credit_limit > dec(\"0\")"
        }));
        let r = compile_value(v.clone(), RuntimeProfile::FullProcess).unwrap_or_else(|d| panic!("{d:#?}"));
        assert!(r.fields.contains_key("Supplier.credit_limit"));
        v["rules"][2]["lookups"][0]["fields"] = json!(["bank_account"]);
        refused_with(v.clone(), Code::PermissionDenied, "restricted");
        v["rules"][2]["lookups"][0]["link"] = json!("company");
        refused_with(v, Code::SchemaInvalid, "not a link");
    }

    #[test]
    fn derivations_may_not_wait_for_each_other() {
        let mut v = purchase();
        v["rules"][1]["writes"] = json!({"x_department": "doc.x_risk"});
        v["rules"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "risk", "entity": "MaterialRequest", "phase": "derive", "writes": {"x_risk": "doc.x_department"}}));
        refused_with(v, Code::Conflict, "each read what another fills");
    }

    #[test]
    fn the_solution_must_pass_its_own_tests() {
        let mut v = purchase();
        v["tests"][0]["expect"] = json!("review");
        refused_with(v, Code::SchemaInvalid, "test at_the_limit expected");
        let mut v = purchase();
        v["tests"][2]["expect"] = json!("proceed");
        refused_with(v, Code::SchemaInvalid, "no_currency");
    }

    #[test]
    fn layout_is_not_meaning_and_meaning_is() {
        let a = compile_value(purchase(), RuntimeProfile::FullProcess).unwrap();
        let mut moved = purchase();
        moved["ui"]["layout"]["snapshot"]["x"] = json!(400);
        let b = compile_value(moved, RuntimeProfile::FullProcess).unwrap();
        assert_eq!((a.source_digest.clone(), a.artifact_digest.clone()), (b.source_digest, b.artifact_digest), "moving a box changes nothing");
        let mut reworded = purchase();
        set_node(&mut reworded, "review", |n| n["label"] = json!("Approve if it looks fine"));
        let c = compile_value(reworded, RuntimeProfile::FullProcess).unwrap();
        assert_ne!(a.artifact_digest, c.artifact_digest, "what a person is told when deciding is part of the meaning");
        let mut limit = purchase();
        limit["parameters"]["review_limit"]["value"]["amount"] = json!("150000.00");
        limit["tests"][1]["given"]["snapshot"]["output"]["total"]["amount"] = json!("150000.01");
        limit["decisions"][0]["rows"][0]["match"]["total"]["gt"]["amount"] = json!("150000.00");
        limit["tests"][0]["given"]["snapshot"]["output"]["total"]["amount"] = json!("150000.00");
        let d = compile_value(limit, RuntimeProfile::FullProcess).unwrap_or_else(|d| panic!("{d:#?}"));
        assert_ne!(a.artifact_digest, d.artifact_digest, "a parameter is behaviour");
    }
}
