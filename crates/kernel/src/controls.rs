//! Two access controls a workspace sets (docs/operations.md, "Asking again before sensitive work" and "Recording who
//! read what"):
//!
//! * **Step-up by rule** (`step_up`): the entities whose changes, deletions, exports or reads need the person to have
//!   proved who they are again recently. Zones add their own (`step_up: true` in `FERRUM_ZONES`). Checked in
//!   [`crate::Ctx::require`], the one place every record operation asks for its rights, after the rights themselves.
//! * **Who read it** (`record_reads`): the entities whose reads are recorded on the audit trail (category `access`), a
//!   single record opened (`open`), or lists, searches, reports and exports too (`all`). A read whose event cannot be
//!   written fails.
//!
//! Both are read with the workspace's schema (`Schema::controls`), so a read or a change of an entity nobody marked
//! asks the database nothing more. What cannot be read counts as marked.

use crate::Ctx;
use crate::db::Tx;
use crate::error::{Error, Result};
use crate::query::Filter;
use chrono::{DateTime, Utc};
use ferrum_meta::Controls;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

/// A workspace's step-up rules: an older build lets marked work through without asking. Dated by the merge of its pull
/// request.
pub const STEP_UP_FLOOR: crate::window::Floor = crate::window::Floor {
    feature: "step_up",
    since: "2026-11-14T08:54:21Z",
    what: "marked work asks the person to prove who they are again",
    temporary: false,
};
/// A workspace's recorded reads: an older build reads marked entities without recording it.
pub const READS_FLOOR: crate::window::Floor = crate::window::Floor {
    feature: "record_reads",
    since: "2026-11-14T08:54:21Z",
    what: "reads of marked entities are recorded, or refused",
    temporary: false,
};

/// The kinds of work a step-up rule names.
pub const WORK: [&str; 4] = ["write", "delete", "export", "read"];
/// How recent a step-up may be, in minutes: at least and at most.
pub const MINUTES: (i64, i64) = (1, 60);
/// The entities one setting names, at most.
const MAX_ENTITIES: usize = 200;

/// The kind of work an action is, as step-up rules name it.
pub fn work_of(action: crate::Action) -> &'static str {
    use crate::Action as A;
    match action {
        A::Read => "read",
        A::Export => "export",
        A::Delete => "delete",
        A::Write | A::Create | A::Submit | A::Cancel | A::Import => "write",
    }
}

/// What a context can say of its person's proof of who they are (`Ctx::proof`). Nothing a client sends sets it: the
/// server reads it from the browser session in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Proof {
    /// Nobody to ask: work with no request behind it, and the system's own.
    #[default]
    Inside,
    /// A client that cannot step up: an API key (an app password and a SCIM key included), an OAuth client, a service
    /// account. Held to a rule only where it says `keys: false`.
    Machine,
    /// A person. `at`: when their browser session last proved who they are (none: a credential with no browser
    /// session, which cannot step up). `staff`: zones ask only staff.
    Person { at: Option<DateTime<Utc>>, staff: bool },
}

impl Proof {
    /// How a request marks the work it queues (`ferrum.step_up`, the column `job.step_up`): nothing for a person who had
    /// stepped up (or nobody to ask), `machine`, or `no`.
    pub fn mark(self, secs: i64) -> &'static str {
        match self {
            Proof::Inside => "",
            Proof::Machine => "machine",
            Proof::Person { at, .. } if fresh(at, secs) => "",
            Proof::Person { .. } => "no",
        }
    }

    /// Back from a queued job's mark. `staff`: the person's kind.
    pub fn of_mark(mark: Option<&str>, staff: bool) -> Proof {
        match mark {
            None | Some("") => Proof::Inside,
            Some("machine") => Proof::Machine,
            // `no`, and anything else: not stepped up (fail closed).
            Some(_) => Proof::Person { at: None, staff },
        }
    }
}

/// The credentials that cannot step up: keys of every scope, app passwords and OAuth clients (MCP among them).
pub fn cannot_step_up(c: crate::zones::Credential) -> bool {
    use crate::zones::Credential as C;
    matches!(c, C::ApiKey | C::Scim | C::Dav | C::OAuth)
}

impl Ctx {
    /// This context, not asked to step up: for saying what one may do at all (an entity's description), never for
    /// doing it.
    pub fn unasked(&self) -> Ctx {
        Ctx { proof: Proof::Inside, ..self.clone() }
    }

    /// This request's context, its person's browser session having last proved who they are at `at` (read from the
    /// session in the database, never from the client). Work it queues carries whether it had (`job.step_up`).
    pub fn proved(mut self, at: Option<DateTime<Utc>>) -> Ctx {
        if let Proof::Person { staff, .. } = self.proof {
            self.proof = Proof::Person { at, staff };
            let secs = if self.schema.controls.step_up_secs > 0 { self.schema.controls.step_up_secs } else { crate::shell::STEP_UP_SECS };
            crate::db::Trace::mark_step_up(self.proof.mark(secs));
        }
        self
    }
}

fn fresh(at: Option<DateTime<Utc>>, secs: i64) -> bool {
    at.is_some_and(|t| Utc::now() - t < chrono::Duration::seconds(secs))
}

/// The answer when a step-up is needed: HTTP 403 as the other step-up answers, with the code `step_up` so that an
/// interface asks the person to confirm it is them, in one of their ways, and tries again. The server adds the ways to
/// the answer (`ways`), and says plainly when there is none.
pub fn needed() -> Error {
    Error::Refused { code: "step_up", status: 403, message: "confirm it is you to do this".into(), pointer: None }
}

impl Ctx {
    /// Whether this context may do `work` (`write`, `delete`, `export`, `read`) on `entity` now, as the workspace's
    /// step-up rules and the zone's say. Memory only.
    pub fn check_step_up(&self, entity: &str, work: &str) -> Result<()> {
        if self.proof == Proof::Inside || (self.is_system && matches!(work, "read" | "export")) {
            return Ok(());
        }
        let c = &self.schema.controls;
        let rule = if c.unreadable { Some((true, false)) } else { c.step_up.get(entity).map(|(w, keys)| (w.contains(work), *keys)) };
        let by_entity = rule.is_some_and(|r| r.0);
        // A zone asking for it: changes, and anything of an entity the workspace marked.
        let zone = self.zone.as_ref().and_then(|e| e.step_up().map(|report| (e.label().to_string(), report)));
        let by_zone = zone.as_ref().filter(|_| matches!(work, "write" | "delete") || rule.is_some());
        match self.proof {
            Proof::Inside => Ok(()),
            // Machines are held by their scopes and networks; a rule holds them only when it says so. Zones ask people.
            Proof::Machine if by_entity && rule.is_some_and(|r| !r.1) => {
                Err(Error::Forbidden(format!("{work} of {entity} needs a person who has just proved who they are; a key cannot")))
            }
            Proof::Machine => Ok(()),
            // People from outside (portal people and guests) sign in with an emailed code and have no password to give:
            // never asked. Held by their roles (portal roles reach only their own records), as keys are, and refused
            // outright where a rule says `keys: false`.
            Proof::Person { staff: false, .. } if by_entity && rule.is_some_and(|r| !r.1) => {
                Err(Error::Forbidden(format!("{work} of {entity} needs a staff member who has just proved who they are")))
            }
            Proof::Person { staff: false, .. } => Ok(()),
            Proof::Person { at, staff } => {
                if fresh(at, if c.step_up_secs > 0 { c.step_up_secs } else { crate::shell::STEP_UP_SECS }) {
                    return Ok(());
                }
                if by_entity {
                    return Err(needed());
                }
                match by_zone {
                    Some((name, report)) if staff => {
                        if *report {
                            crate::zones::count_hidden(name, "report", "step_up");
                            tracing::warn!(person = %self.user, workspace = %self.tenant, zone = %name, entity, work, "a zone would have asked for a step-up");
                            Ok(())
                        } else {
                            Err(needed())
                        }
                    }
                    _ => Ok(()),
                }
            }
        }
    }

    /// Whether reads of `entity` are recorded for this context: `Some(true)` at `all`, `Some(false)` at `open`. Only
    /// for a request's own context (a person's or a key's), never the system's or work with no request behind it.
    pub fn reads_recorded(&self, entity: &str) -> Option<bool> {
        let c = &self.schema.controls;
        if c.reads.is_empty() && !c.unreadable {
            return None;
        }
        let request = !self.is_system && self.zone.as_ref().is_some_and(|e| e.credential != crate::zones::Credential::Queued);
        match self.reads.via {
            Via::Never => return None,
            Via::Request if !request => return None,
            _ => {}
        }
        if c.unreadable { Some(true) } else { c.reads.get(entity).copied() }
    }
}

/// What one request has recorded already (`Ctx::reads`): each record and each entity's list once.
#[derive(Debug, Default)]
pub struct Seen {
    seen: Mutex<BTreeSet<(String, Option<Uuid>)>>,
    via: Via,
}

/// Whose reads a context's are.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Via {
    /// A request's (a person's or a key's): recorded; anything else is not.
    #[default]
    Request,
    /// Recorded although no request is behind it: a workspace export a job makes for whoever asked, a public link's
    /// page (no person). The words go into the event as `via`.
    Also(&'static str),
    /// A check that returns nothing of the record (whether a live update may be pushed): never recorded.
    Never,
}

impl Seen {
    /// True the first time.
    fn first(&self, entity: &str, record: Option<Uuid>) -> bool {
        // A poisoned lock: record again rather than not at all.
        self.seen.lock().map(|mut s| s.insert((entity.to_string(), record))).unwrap_or(true)
    }
}

impl Ctx {
    /// This context for a check that shows nothing of what it reads: its reads are not recorded.
    pub fn unrecorded(&self) -> Ctx {
        Ctx { reads: std::sync::Arc::new(Seen { via: Via::Never, ..Default::default() }), ..self.clone() }
    }

    /// This context, its reads recorded although no request is behind it (`via` says how they were made).
    pub fn recorded_via(&self, via: &'static str) -> Ctx {
        Ctx { reads: std::sync::Arc::new(Seen { via: Via::Also(via), ..Default::default() }), ..self.clone() }
    }
}

/// Statements the read log has sent since start: a test's proof that an unmarked read sends none.
static STATEMENTS: AtomicU64 = AtomicU64::new(0);

pub fn statements() -> u64 {
    STATEMENTS.load(Ordering::Relaxed)
}

/// What was read.
pub enum Read<'a> {
    /// One record's fields.
    One(Uuid),
    /// Rows of a list (`how`: `list`, `search`, `report`, `export`): its filters, whether a text search narrowed it,
    /// how many rows, and where an export went.
    Many { how: &'static str, filters: &'a [Filter], search: bool, rows: i64, to: Option<&'a str> },
}

/// A filter as an access event shows it: which fields, compared how, never with what.
pub fn summary(filters: &[Filter], search: bool) -> Value {
    let fields: Vec<Value> = filters.iter().map(|f| json!({"field": f.field, "op": format!("{:?}", f.op).to_lowercase()})).collect();
    json!({"filters": fields, "search": search})
}

/// Records `read` of `entity` for `ctx`'s request in `tx`, when the workspace marked it: once per record, and once per
/// entity's list, per request. Unmarked, or not a request's: nothing, and no statement. An error fails the read.
pub async fn record_read(tx: &mut Tx, ctx: &Ctx, entity: &str, read: Read<'_>) -> Result<()> {
    let Some(all) = ctx.reads_recorded(entity) else { return Ok(()) };
    let (action, record, mut details) = match read {
        Read::One(id) => ("open", Some(id), json!({"entity": entity, "record": id})),
        Read::Many { .. } if !all => return Ok(()),
        Read::Many { how, filters, search, rows, to } => {
            let mut d = json!({"entity": entity, "query": summary(filters, search), "rows": rows});
            if let Some(to) = to {
                d["to"] = json!(to);
            }
            (how, None, d)
        }
    };
    if !ctx.reads.first(entity, record) {
        return Ok(());
    }
    let entry = ctx.zone.as_deref();
    details["address"] = json!(entry.map(|e| e.address.as_str()).filter(|a| !a.is_empty()));
    details["zone"] = json!(entry.and_then(|e| e.name()));
    details["credential"] = json!(entry.map(|e| e.credential.as_str()));
    if let Via::Also(via) = ctx.reads.via {
        details["via"] = json!(via);
    }
    STATEMENTS.fetch_add(2, Ordering::Relaxed);
    crate::audit::record_access(tx, ctx, action, details).await
}

/// [`record_read`] in a transaction of its own, for reads made outside one (a report, an export, before its rows
/// leave): written and committed before the answer is given, or the answer is not given.
pub async fn record_read_alone(k: &crate::Kernel, ctx: &Ctx, entity: &str, read: Read<'_>) -> Result<()> {
    if ctx.reads_recorded(entity).is_none() {
        return Ok(());
    }
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    record_read(&mut tx, ctx, entity, read).await?;
    tx.commit().await?;
    Ok(())
}

/// Reads the workspace's settings into what its schema keeps. A setting that is there but cannot be read makes every
/// entity marked (said in the schema's warnings).
pub fn load(settings: &Value) -> (Controls, Option<String>) {
    let mut c = Controls::default();
    let mut wrong = None;
    match settings.get("step_up").filter(|v| !v.is_null()).map(check_step_up) {
        None => {}
        Some(Ok(v)) => {
            c.step_up_secs = v["minutes"].as_i64().unwrap_or(10) * 60;
            for (e, r) in v["entities"].as_object().into_iter().flatten() {
                let work = r["work"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
                c.step_up.insert(e.clone(), (work, r["keys"].as_bool().unwrap_or(true)));
            }
        }
        Some(Err(e)) => wrong = Some(format!("step_up: {e}")),
    }
    match settings.get("record_reads").filter(|v| !v.is_null()).map(check_reads) {
        None => {}
        Some(Ok(v)) => {
            for (e, level) in v.as_object().into_iter().flatten() {
                c.reads.insert(e.clone(), level == "all");
            }
        }
        Some(Err(e)) => wrong = Some(format!("record_reads: {e}")),
    }
    c.unreadable = wrong.is_some();
    (c, wrong)
}

fn entity_name(e: &str) -> bool {
    !e.is_empty() && e.len() <= 100 && e.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The `step_up` setting, checked and written as it is kept: `{"minutes": 10, "entities": {"Employee": {"work":
/// ["write", "read"], "keys": true}}}`.
pub fn check_step_up(v: &Value) -> std::result::Result<Value, String> {
    const SHAPE: &str =
        "must be {\"minutes\": 1 to 60, \"entities\": {\"Entity\": {\"work\": [\"write\", \"delete\", \"export\", \"read\"], \"keys\": true or false}}}";
    let o = v.as_object().ok_or(SHAPE)?;
    if o.keys().any(|k| k != "minutes" && k != "entities") {
        return Err(SHAPE.into());
    }
    let minutes = match o.get("minutes") {
        None => 10,
        Some(m) => m.as_i64().filter(|m| (MINUTES.0..=MINUTES.1).contains(m)).ok_or(SHAPE)?,
    };
    let empty = Map::new();
    let entities = match o.get("entities") {
        None => &empty,
        Some(e) => e.as_object().ok_or(SHAPE)?,
    };
    if entities.len() > MAX_ENTITIES {
        return Err(format!("may name at most {MAX_ENTITIES} entities"));
    }
    let mut out = BTreeMap::new();
    for (name, rule) in entities {
        if !entity_name(name) {
            return Err(format!("{name:?} is not an entity's name"));
        }
        let r = rule.as_object().filter(|r| r.keys().all(|k| k == "work" || k == "keys")).ok_or(SHAPE)?;
        let mut work = BTreeSet::new();
        for w in r.get("work").and_then(Value::as_array).ok_or(SHAPE)? {
            let w = w.as_str().filter(|w| WORK.contains(w)).ok_or_else(|| format!("{w} is not a kind of work: write, delete, export or read"))?;
            work.insert(w);
        }
        if work.is_empty() {
            return Err(format!("{name} names no work; leave it out instead"));
        }
        let keys = match r.get("keys") {
            None => true,
            Some(k) => k.as_bool().ok_or(SHAPE)?,
        };
        out.insert(name.clone(), json!({"work": work, "keys": keys}));
    }
    Ok(json!({"minutes": minutes, "entities": out}))
}

/// The `record_reads` setting, checked: `{"Employee": "open", "Payslip": "all"}`.
pub fn check_reads(v: &Value) -> std::result::Result<Value, String> {
    const SHAPE: &str = "must be {\"Entity\": \"open\" or \"all\"}";
    let o = v.as_object().ok_or(SHAPE)?;
    if o.len() > MAX_ENTITIES {
        return Err(format!("may name at most {MAX_ENTITIES} entities"));
    }
    let mut out = BTreeMap::new();
    for (name, level) in o {
        if !entity_name(name) {
            return Err(format!("{name:?} is not an entity's name"));
        }
        let level = level.as_str().filter(|l| *l == "open" || *l == "all").ok_or(SHAPE)?;
        out.insert(name.clone(), json!(level));
    }
    Ok(json!(out))
}

/// The entities a checked setting names.
pub fn named(kept: &Value, step_up: bool) -> Vec<String> {
    let map = if step_up { kept.get("entities") } else { Some(kept) };
    map.and_then(Value::as_object).map(|m| m.keys().cloned().collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_are_read_strictly() {
        let ok = check_step_up(&json!({"entities": {"Employee": {"work": ["read", "write", "read"]}}})).unwrap();
        assert_eq!(ok, json!({"minutes": 10, "entities": {"Employee": {"work": ["read", "write"], "keys": true}}}));
        for bad in [
            json!([]),
            json!({"minutes": 0}),
            json!({"minutes": 61}),
            json!({"other": 1}),
            json!({"entities": {"Employee": {"work": ["print"]}}}),
            json!({"entities": {"Employee": {"work": []}}}),
            json!({"entities": {"Employee": {"work": ["read"], "keys": "no"}}}),
            json!({"entities": {"Employee": {"work": ["read"], "colour": 1}}}),
            json!({"entities": {"Bad Name": {"work": ["read"]}}}),
        ] {
            assert!(check_step_up(&bad).is_err(), "{bad}");
        }
        assert_eq!(check_reads(&json!({"Employee": "all", "Lead": "open"})).unwrap(), json!({"Employee": "all", "Lead": "open"}));
        for bad in [json!({"Employee": "some"}), json!({"Employee": true}), json!(["Employee"])] {
            assert!(check_reads(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn what_cannot_be_read_marks_everything() {
        let (c, wrong) =
            load(&json!({"step_up": {"entities": {"Employee": {"work": ["write"], "keys": false}}, "minutes": 5}, "record_reads": {"Employee": "open"}}));
        assert!(wrong.is_none() && !c.unreadable);
        assert_eq!(c.step_up_secs, 300);
        assert_eq!(c.step_up["Employee"], (BTreeSet::from(["write".to_string()]), false));
        assert!(!c.reads["Employee"]);
        let (c, wrong) = load(&json!({"record_reads": {"Employee": "sometimes"}}));
        assert!(c.unreadable && wrong.is_some());
        assert_eq!(load(&json!({})).0, Controls::default(), "nothing set: nothing marked");
    }

    #[test]
    fn queued_marks_round_trip() {
        let now = Some(Utc::now());
        assert_eq!(Proof::Person { at: now, staff: true }.mark(600), "");
        assert_eq!(Proof::Person { at: None, staff: true }.mark(600), "no");
        assert_eq!(Proof::Machine.mark(600), "machine");
        assert_eq!(Proof::of_mark(Some("no"), true), Proof::Person { at: None, staff: true });
        assert_eq!(Proof::of_mark(Some("garbled"), false), Proof::Person { at: None, staff: false }, "fail closed");
        assert_eq!(Proof::of_mark(None, true), Proof::Inside);
    }
}
