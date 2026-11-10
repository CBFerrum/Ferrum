//! Governance and Risk as a pack (docs/builder/evidence/grc-as-a-pack-2026-11-10.md), against a real
//! PostgreSQL (`FERRUM_STUDIO_TEST_DATABASE_URL`; the database is reset first).
//!
//! The app `ferrum-app-grc` (risks, controls and their tests, risk acceptances, policies and their
//! acknowledgements, findings: about 500 lines of Rust hooks and upkeep) is rebuilt here by configuration
//! alone, with the calls an administrator makes: one customization batch (entities, checks and six configured
//! actions), three roles, three scheduled automations, a solution with two rules, and a Studio space. It was
//! measured first without configured actions (10 November 2026) and again with them, `update_linked` and the
//! day in aggregate filters: the steps below say how it stands now. It is exported as a pack, which must be
//! exactly `packs/grc/grc.pack.json` (`FERRUM_WRITE_PACKS=1` writes it), and that committed file is installed
//! in a fresh workspace where every scenario of the app's `tests/grc_it.rs` is restated, as ordinary users
//! holding the pack's roles. Each step says whether the pack does it the **same** way, a **different** way
//! (and what a user notices), or **cannot** (and asserts what happens today, naming what is missing).
//!
//! Two things the harness does that a user does not: it ages records by SQL (in the tenant's transaction)
//! where the app's test moved its upkeep's clock, since computed fields and automation conditions read the
//! real day; and it runs the schedule scanner a day ahead (`day`), as pack_it.rs does.

use chrono::{Duration, Utc};
use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::pack::ExportOptions;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, automation};
use ferrum_meta::{Catalog, PermissionDef};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn message(e: &Error) -> String {
    match e {
        Error::Validation(v) => v.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

/// Refused as `kind` (invalid, forbidden, not found, conflict), saying `says`.
fn refused<V: std::fmt::Debug>(r: Result<V, Error>, kind: &str, says: &str) {
    let msg = match &r {
        Err(e) => message(e),
        Ok(v) => format!("{v:?}"),
    };
    let ok = matches!(
        (&r, kind),
        (Err(Error::Invalid(_) | Error::Validation(_)), "invalid")
            | (Err(Error::Forbidden(_)), "forbidden")
            | (Err(Error::NotFound(_)), "not found")
            | (Err(Error::Conflict(_)), "conflict")
    );
    assert!(ok && msg.contains(says), "expected {kind} saying {says:?}, got {msg}");
}

const REVIEWED_AT: &str = "2026-11-10T00:00:00+00:00";

fn pack_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs/grc/grc.pack.json")
}

// ───────────── the configuration: what an administrator enters ─────────────

const RESULTS: [&str; 3] = ["Effective", "Partially Effective", "Ineffective"];

/// A control's latest test by day, the last entered of that day (the app: `order by tested_on desc,
/// created_at desc`).
fn latest() -> String {
    let l = "doc.tests.filter(t, doc.tests.all(u, u.tested_on <= t.tested_on))";
    format!("{l}[size({l}) - 1]")
}

fn entities() -> Vec<Value> {
    let last = latest();
    let any = "has(doc.tests) && size(doc.tests) > 0";
    let score = |a: &str| format!("doc.{a}_likelihood * doc.{a}_impact");
    let scale = |name: &str| json!({"name": name, "type": "int", "required": true, "default": 3, "min": 1, "max": 5});
    vec![
        json!({"name": "XControlTest", "label": "Control Test", "kind": "child", "fields": [
            {"name": "tested_on", "type": "date", "required": true, "in_list": true},
            {"name": "tester", "type": "email", "read_only": true, "default_expr": "user.email", "in_list": true},
            {"name": "result", "type": "select", "options": RESULTS, "required": true, "in_list": true},
            {"name": "evidence", "type": "data", "in_list": true, "description": "What the evidence is"},
            {"name": "evidence_file", "type": "attach", "in_list": true},
            {"name": "notes", "type": "text"}]}),
        json!({"name": "XControl", "label": "Control", "label_plural": "Controls", "kind": "master", "title_field": "control_name",
            "description": "What is done to keep a risk down. It is effective while its last test passed and is still current.",
            "fields": [
            {"name": "control_name", "label": "Name", "type": "data", "required": true, "unique": true, "in_list": true},
            {"name": "category", "type": "select", "options": ["Policy", "Process", "Technical", "Physical"], "default": "Process", "in_list": true, "in_filter": true},
            {"name": "owner", "type": "email", "in_list": true},
            {"name": "status", "type": "select", "options": ["Planned", "In Place", "Retired"], "default": "Planned", "in_list": true, "in_filter": true},
            {"name": "references", "type": "data", "description": "The framework requirements it answers, by their identifiers"},
            {"name": "test_every_days", "label": "Test Every (Days)", "type": "int", "required": true, "default": 365, "min": 1,
             "description": "Its test is current this many days"},
            {"name": "tests", "type": "table", "of": "XControlTest"},
            {"name": "last_tested_on", "type": "date", "in_list": true, "computed": format!("{any} ? {last}.tested_on : null")},
            {"name": "last_result", "type": "select", "options": RESULTS, "in_list": true, "computed": format!("{any} ? {last}.result : null")},
            {"name": "effective", "type": "check", "in_list": true, "in_filter": true, "computed": format!(
                "{any} && {last}.result == 'Effective' && timestamp({last}.tested_on + 'T00:00:00Z') + duration(string(int(doc.test_every_days) * 24) + 'h') >= timestamp(today + 'T00:00:00Z')")},
            {"name": "description", "type": "text"}],
            "list": {"columns": ["control_name", "category", "status", "last_tested_on", "last_result", "effective"], "sort": "control_name asc"}}),
        json!({"name": "XRiskControl", "label": "Risk Control", "kind": "child", "fields": [
            {"name": "control", "type": "link", "to": "XControl", "required": true, "in_list": true}]}),
        json!({"name": "XRisk", "label": "Risk", "label_plural": "Risk Register", "kind": "master", "title_field": "title",
            "description": "Something that could go wrong, scored before controls (inherent), with them (current) and as targeted (residual), each likelihood times impact on a five-by-five matrix.",
            "numbering": {"template": "RSK-{YYYY}-{seq:4}"},
            "fields": [
            {"name": "title", "type": "data", "required": true, "in_list": true},
            {"name": "description", "type": "text"},
            {"name": "category", "type": "select", "options": ["Strategic", "Operational", "Financial", "Compliance", "Security", "Reputational"], "default": "Operational", "in_list": true, "in_filter": true},
            {"name": "owner", "type": "email", "required": true, "in_list": true, "in_filter": true},
            scale("inherent_likelihood"), scale("inherent_impact"), scale("current_likelihood"), scale("current_impact"), scale("residual_likelihood"), scale("residual_impact"),
            {"name": "inherent_score", "type": "int", "computed": score("inherent")},
            {"name": "current_score", "type": "int", "in_list": true, "computed": score("current")},
            {"name": "residual_score", "type": "int", "computed": score("residual")},
            {"name": "level", "type": "select", "options": ["Low", "Medium", "High", "Critical"], "in_list": true, "in_filter": true, "description": "The current score's",
             "computed": format!("{c} <= 4 ? 'Low' : ({c} <= 9 ? 'Medium' : ({c} <= 16 ? 'High' : 'Critical'))", c = score("current"))},
            {"name": "treatment", "type": "select", "options": ["Open", "Mitigate", "Accept", "Avoid", "Transfer"], "default": "Open", "in_list": true, "in_filter": true},
            {"name": "controls", "type": "table", "of": "XRiskControl"},
            {"name": "review_by", "type": "date", "in_list": true},
            {"name": "recheck", "type": "check", "hidden": true, "description": "Set by the daily recheck: an accepted risk whose acceptance ended is open again"}],
            "list": {"columns": ["number", "title", "owner", "current_score", "level", "treatment"], "sort": "created_at desc"}}),
        json!({"name": "XRiskAcceptance", "label": "Risk Acceptance", "label_plural": "Risk Acceptances", "kind": "master", "title_field": "risk",
            "description": "A risk above the tolerance taken on, said why, approved by a risk approver who does not own it, until it expires.",
            "numbering": {"template": "ACC-{YYYY}-{seq:4}"},
            "fields": [
            {"name": "risk", "type": "link", "to": "XRisk", "required": true, "in_list": true, "in_filter": true},
            {"name": "risk_owner", "type": "email", "read_only": true, "fetch_from": "risk.owner"},
            {"name": "justification", "type": "text", "required": true},
            {"name": "expires_on", "type": "date", "required": true, "in_list": true},
            {"name": "decision", "type": "select", "options": ["Accepted", "Rejected", "Revoked"], "read_only": true, "hidden": true, "no_copy": true,
             "description": "Set by Accept It, Reject It and Revoke It"},
            {"name": "status", "type": "select", "options": ["Draft", "Accepted", "Rejected", "Revoked", "Expired"], "no_copy": true, "in_list": true, "in_filter": true,
             "description": "Moved by its decisions, and Expired the day after its last day",
             "computed": "doc.decision == null || doc.decision == '' ? 'Draft' : (doc.decision == 'Accepted' && doc.expires_on < today ? 'Expired' : doc.decision)"},
            {"name": "approver", "type": "email", "read_only": true, "no_copy": true, "in_list": true},
            {"name": "decided_on", "type": "date", "read_only": true, "no_copy": true},
            {"name": "revoked_on", "type": "date", "read_only": true, "no_copy": true}],
            "list": {"columns": ["number", "risk", "expires_on", "status", "approver"], "sort": "created_at desc"}}),
        json!({"name": "XPolicy", "label": "Policy", "label_plural": "Policies", "kind": "master", "title_field": "title",
            "description": "What people are asked to follow. Published with new text, it is at its next revision, and everyone acknowledges it again.",
            "fields": [
            {"name": "title", "type": "data", "required": true, "unique": true, "in_list": true},
            {"name": "owner", "type": "email", "in_list": true},
            {"name": "text", "type": "markdown", "required": true},
            {"name": "status", "type": "select", "options": ["Draft", "Published", "Retired"], "default": "Draft", "read_only": true, "no_copy": true, "in_list": true, "in_filter": true},
            {"name": "revision", "type": "int", "read_only": true, "no_copy": true, "in_list": true},
            {"name": "published_text", "type": "markdown", "read_only": true, "no_copy": true, "description": "The text at its revision"},
            {"name": "published_on", "type": "date", "read_only": true, "no_copy": true, "in_list": true},
            {"name": "review_by", "type": "date"},
            {"name": "nothing_new", "type": "check", "hidden": true, "read_only": true, "no_copy": true}],
            "list": {"columns": ["title", "owner", "revision", "status", "published_on"], "sort": "title asc"}}),
        json!({"name": "XPolicyAcknowledgement", "label": "Policy Acknowledgement", "label_plural": "Policy Acknowledgements", "kind": "master", "title_field": "person",
            "unique": [["policy", "revision", "person"]],
            "fields": [
            {"name": "policy", "type": "link", "to": "XPolicy", "required": true, "in_list": true, "in_filter": true, "link_filters": {"status": "Published"}},
            {"name": "revision", "type": "int", "read_only": true, "in_list": true, "fetch_from": "policy.revision"},
            {"name": "person", "type": "email", "required": true, "read_only": true, "default_expr": "user.email", "in_list": true, "in_filter": true},
            {"name": "acknowledged_at", "type": "datetime", "read_only": true, "default_expr": "now", "in_list": true}],
            "list": {"columns": ["policy", "revision", "person", "acknowledged_at"], "sort": "acknowledged_at desc"}}),
        json!({"name": "XFinding", "label": "Finding", "label_plural": "Findings", "kind": "master", "title_field": "title",
            "description": "Something an audit or a test found wrong, owned and due. Closed, it says how it was fixed.",
            "numbering": {"template": "FND-{YYYY}-{seq:4}"},
            "fields": [
            {"name": "title", "type": "data", "required": true, "in_list": true},
            {"name": "severity", "type": "select", "options": ["Low", "Medium", "High", "Critical"], "default": "Medium", "in_list": true, "in_filter": true},
            {"name": "control", "type": "link", "to": "XControl", "in_list": true},
            {"name": "risk", "type": "link", "to": "XRisk"},
            {"name": "owner", "type": "email", "required": true, "in_list": true, "in_filter": true},
            {"name": "due_on", "type": "date", "required": true, "in_list": true},
            {"name": "status", "type": "select", "options": ["Open", "Remediating", "Closed"], "default": "Open", "in_list": true, "in_filter": true},
            {"name": "remediation", "type": "text"},
            {"name": "overdue", "type": "check", "no_copy": true, "in_list": true, "in_filter": true,
             "computed": "doc.status != 'Closed' && has(doc.due_on) && doc.due_on < today"}],
            "list": {"columns": ["number", "title", "severity", "owner", "due_on", "status", "overdue"], "sort": "due_on asc"}}),
    ]
}

/// The checks (`add_rule`), each with the app's words where it had some.
fn checks() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    vec![
        ("XControlTest", "today_or_before", "doc.tested_on <= today", "is today or before"),
        (
            "XControlTest",
            "effective_has_evidence",
            "doc.result != 'Effective' || (has(doc.evidence_file) && doc.evidence_file != null && doc.evidence_file != '')",
            "an effective result has its evidence",
        ),
        (
            "XControl",
            "tested_once_in_place",
            "!has(doc.tests) || doc.status == 'In Place' || size(doc.tests) <= ((old == null || !has(old.tests)) ? 0 : size(old.tests))",
            "is tested once it is in place",
        ),
        (
            "XRisk",
            "controls_no_worse",
            "doc.current_likelihood * doc.current_impact <= doc.inherent_likelihood * doc.inherent_impact",
            "controls do not make a risk worse than it is without them",
        ),
        (
            "XRisk",
            "target_no_worse",
            "doc.residual_likelihood * doc.residual_impact <= doc.current_likelihood * doc.current_impact",
            "the target is no worse than the risk now",
        ),
        (
            "XRisk",
            "mitigation_names_controls",
            "doc.treatment != 'Mitigate' || (has(doc.controls) && size(doc.controls) > 0)",
            "mitigating names the controls that do it",
        ),
        ("XRiskAcceptance", "after_today", "doc.status != 'Draft' || doc.expires_on > today", "is after today"),
        ("XPolicy", "nothing_new", "doc.nothing_new != true", "the text is as published: there is nothing new to publish"),
        (
            "XFinding",
            "closed_says_how",
            "doc.status != 'Closed' || (has(doc.remediation) && doc.remediation != null && doc.remediation != '')",
            "a closed finding says how it was fixed",
        ),
    ]
}

/// The app's six actions, as configured actions: buttons with their conditions, roles and refusals, whose
/// effects run as the system through the save pipeline (docs/api.md, "Configured actions").
fn actions() -> Vec<(&'static str, Value)> {
    let owner = json!({"when": "doc.risk_owner != null && doc.risk_owner == user.email", "message": "a risk's owner does not approve taking it on"});
    let decide = |name: &str, label: &str, decision: &str, risk: Option<Value>| {
        let mut effects = vec![json!({"type": "set_fields", "values": {"decision": format!("'{decision}'"), "approver": "user.email", "decided_on": "today"}})];
        effects.extend(risk.map(|v| json!({"type": "update_linked", "link": "risk", "values": v})));
        json!({"name": name, "label": label, "condition": "doc.status == 'Draft'", "roles": ["Risk Approver"], "refuse_if": [owner], "effects": effects})
    };
    // A policy's text is published at its next revision; with nothing new, the check `nothing_new` refuses it.
    let same = "doc.status == 'Published' && doc.text == doc.published_text";
    vec![
        ("XRiskAcceptance", decide("accept", "Accept It", "Accepted", Some(json!({"treatment": "'Accept'"})))),
        ("XRiskAcceptance", decide("reject", "Reject It", "Rejected", None)),
        (
            "XRiskAcceptance",
            json!({"name": "revoke", "label": "Revoke It", "condition": "doc.status == 'Accepted'", "roles": ["Risk Approver"], "confirm": "Revoke this acceptance? The risk is open again unless another acceptance holds it.",
                "effects": [{"type": "set_fields", "values": {"decision": "'Revoked'", "revoked_on": "today"}},
                            {"type": "update_linked", "link": "risk", "values": {"recheck": "true"}}]}),
        ),
        (
            "XPolicy",
            json!({"name": "publish", "label": "Publish It", "condition": "doc.status != 'Retired'",
                "effects": [{"type": "set_fields", "values": {"nothing_new": same, "status": "'Published'", "revision": "doc.revision + 1",
                    "published_text": "doc.text", "published_on": "today"}}]}),
        ),
        (
            "XPolicy",
            json!({"name": "retire", "label": "Retire It", "condition": "doc.status == 'Published'", "confirm": "Retire this policy? It is no longer offered to read.",
                "effects": [{"type": "set_fields", "values": {"status": "'Retired'"}}]}),
        ),
        (
            "XPolicy",
            json!({"name": "acknowledge", "label": "I Have Read It", "permission": "read", "condition": "doc.status == 'Published'",
                "effects": [{"type": "create_document", "entity": "XPolicyAcknowledgement", "values": {"policy": "doc.id", "person": "user.email"}}]}),
        ),
    ]
}

/// The rules a check cannot hold: they count other records, or fill fields from the ones they read.
fn solution() -> String {
    // Approved, and its last day today or later: an expired acceptance holds nothing, the day it expires.
    let held = json!([{"name": "held", "entity": "XRiskAcceptance", "link": "risk", "filter": [["status", "=", "Accepted"], ["expires_on", ">=", {"today": 0}]], "op": "exists"}]);
    json!({"schema_version": "ferrum.solution/1", "kind": "Solution", "id": "grc.rules", "version": "1.0.0", "label": "Governance and Risk rules",
        "parameters": {"tolerance": {"type": "int", "value": 6}},
        "rules": [
        {"id": "accepted_only_with_acceptance", "entity": "XRisk", "phase": "validate", "field": "treatment", "aggregates": held,
         "check": "doc.treatment != \"Accept\" || doc.current_likelihood * doc.current_impact <= parameters.tolerance || aggregates.held",
         "message": "a risk above the tolerance is accepted only with an approved acceptance"},
        // Revoking or expiring an acceptance asks its risk to look again: open, unless another acceptance holds it.
        {"id": "open_again", "entity": "XRisk", "phase": "derive", "when": "has(doc.recheck) && doc.recheck", "aggregates": held,
         "writes": {"recheck": "false", "treatment": "doc.treatment == \"Accept\" && !aggregates.held ? \"Open\" : doc.treatment"}}]})
    .to_string()
}

fn roles() -> Vec<(&'static str, Value)> {
    let rwc = |e: &str| json!({"entity": e, "read": true, "write": true, "create": true});
    let rwce = |e: &str| json!({"entity": e, "read": true, "write": true, "create": true, "export": true});
    vec![
        ("Risk Manager", rwce("XRisk")),
        ("Risk Manager", rwce("XControl")),
        ("Risk Manager", rwc("XRiskAcceptance")),
        ("Risk Manager", rwc("XPolicy")),
        ("Risk Manager", json!({"entity": "XPolicyAcknowledgement", "read": true, "export": true})),
        ("Risk Manager", rwce("XFinding")),
        ("Risk Approver", json!({"entity": "XRisk", "read": true})),
        ("Risk Approver", json!({"entity": "XRiskAcceptance", "read": true, "write": true})),
        ("Risk Approver", json!({"entity": "XControl", "read": true})),
        ("Policy Reader", json!({"entity": "XPolicy", "read": true, "where": {"status": ["Published"]}})),
        ("Policy Reader", json!({"entity": "XPolicyAcknowledgement", "read": true, "if_owner": true})),
    ]
}

/// The app's upkeep, as one automation a day per kind of record. A touch (a field set to itself) saves the
/// record again, so its computed fields are worked out on that day: an acceptance past its last day is
/// Expired, and its risk is asked to look again at once.
fn automations() -> Vec<Value> {
    let every = |at: &str, filter: Value| json!({"every": {"period": "day", "at": at, "filter": filter}});
    vec![
        json!({"name": "Acceptances expire", "entity": "XRiskAcceptance", "event": "scheduled", "schedule": every("00:00", json!([["status", "=", "Accepted"]])),
            "condition": "doc.expires_on < today", "actions": [{"type": "set_fields", "values": {"justification": "doc.justification"}},
                                                               {"type": "update_linked", "link": "risk", "values": {"recheck": "true"}}], "is_active": true}),
        json!({"name": "Controls' standing", "entity": "XControl", "event": "scheduled", "schedule": every("00:10", json!([["effective", "=", true]])),
            "actions": [{"type": "set_fields", "values": {"status": "doc.status"}}], "is_active": true}),
        json!({"name": "Findings overdue", "entity": "XFinding", "event": "scheduled", "schedule": every("00:15", json!([["status", "!=", "Closed"], ["overdue", "=", false]])),
            "actions": [{"type": "set_fields", "values": {"status": "doc.status"}}], "is_active": true}),
    ]
}

/// The app's own page at /grc/ (ui/src/Home.tsx): the register, controls to test, open findings, policies.
fn space() -> Value {
    json!({"label": "Governance and Risk", "icon": "clipboard-list", "sharing": "roles",
        "roles": [{"role": "Risk Manager"}, {"role": "Risk Approver"}, {"role": "Policy Reader"}],
        "blocks": [
            {"type": "heading", "text": "The register"},
            {"type": "shortcut", "label": "Risk Register", "entity": "XRisk", "show_count": true},
            {"type": "list", "label": "Risks", "entity": "XRisk", "fields": ["number", "title", "level", "current_score", "treatment"], "sort": "current_score desc", "limit": 50},
            {"type": "list", "label": "Controls to test", "entity": "XControl", "filters": [["status", "=", "In Place"], ["effective", "=", false]],
             "fields": ["control_name", "last_tested_on"], "sort": "control_name asc", "limit": 50},
            {"type": "list", "label": "Open findings", "entity": "XFinding", "filters": [["status", "!=", "Closed"]],
             "fields": ["number", "title", "due_on", "overdue"], "sort": "due_on asc", "limit": 50},
            {"type": "list", "label": "Policies", "entity": "XPolicy", "filters": [["status", "=", "Published"]],
             "fields": ["title", "revision"], "sort": "title asc", "limit": 50}]})
}

// ───────────── the harness ─────────────

struct W {
    k: Arc<Kernel>,
    tenant: Uuid,
    user: Uuid,
}

impl W {
    async fn admin(&self) -> Ctx {
        self.k.load_ctx(self.tenant, self.user, None, "it".into()).await.unwrap()
    }
    async fn user(&self, email: &str, roles: &[&str]) -> Ctx {
        let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
        let id = self.k.create_user(&self.admin().await, email, email, None, &roles, false).await.expect("user");
        self.k.load_ctx(self.tenant, id, None, "it".into()).await.unwrap()
    }
    async fn insert(&self, ctx: &Ctx, entity: &str, v: Value) -> Doc {
        self.k.insert(ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("{entity}: {}", message(&e)))
    }
    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.admin().await, entity, id, false).await.unwrap_or_else(|e| panic!("get {entity}: {}", message(&e)))
    }
    async fn set(&self, ctx: &Ctx, entity: &str, id: Uuid, v: Value) -> Result<Doc, Error> {
        self.k.update(ctx, entity, id, obj(v), None).await
    }
    /// A test recorded on a control: a line added to it, as its page does.
    async fn test(&self, ctx: &Ctx, control: Uuid, row: Value) -> Result<Doc, Error> {
        let mut rows = self.k.get(ctx, "XControl", control, false).await?["tests"].as_array().cloned().unwrap_or_default();
        rows.push(row);
        self.set(ctx, "XControl", control, json!({"tests": rows})).await
    }
    /// Moves a date of custom records (or lines) by `days`, as the passing of time would see it.
    async fn age(&self, ids: &[Uuid], field: &str, days: i64) {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        sqlx::query(
            "update custom_record set ext = jsonb_set(ext, array[$4], to_jsonb(((ext->>$4)::date + $3::int)::text)) where tenant_id = $1 and id = any($2)",
        )
        .bind(self.tenant)
        .bind(ids)
        .bind(days as i32)
        .bind(field)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
}

async fn workspace(k: &Arc<Kernel>, slug: &str) -> W {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: slug.into(),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into(), "studio".into()],
            settings: Some(json!({"timezone": "UTC"})),
        })
        .await
        .expect("workspace");
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    W { k: k.clone(), tenant, user: session.user }
}

/// A solution's release reviewed by a second administrator and activated.
async fn activate(w: &W, release: &Value, reviewer: &str) {
    let k = &w.k;
    let id: Uuid = release["id"].as_str().unwrap().parse().unwrap();
    let second = k.create_user(&w.admin().await, reviewer, "Second", None, &[], true).await.unwrap();
    let second = k.load_ctx(w.tenant, second, None, "it".into()).await.unwrap();
    k.review_release(&second, id, release["artifact_digest"].as_str().unwrap(), true, "Read the rules").await.unwrap();
    k.activate_release(&w.admin().await, id, None).await.unwrap();
}

/// The workspace built by configuration, exported (without the moment it was made).
async fn build(k: &Arc<Kernel>) -> Value {
    let src = workspace(k, "grc-source").await;
    let admin = src.admin().await;
    let mut ops: Vec<NewCustomization> = vec![];
    for e in entities() {
        let name = e["name"].as_str().unwrap().to_string();
        ops.push(serde_json::from_value(json!({"entity": name, "op": {"op": "define_entity", "entity": e}})).unwrap());
    }
    for (entity, name, check, says) in checks() {
        ops.push(serde_json::from_value(json!({"entity": entity, "op": {"op": "add_rule", "rule": {"name": name, "check": check, "message": says}}})).unwrap());
    }
    for (entity, action) in actions() {
        ops.push(serde_json::from_value(json!({"entity": entity, "op": {"op": "add_action", "action": action}})).unwrap());
    }
    k.batch_customizations(&admin, ops, false).await.unwrap_or_else(|(at, e)| panic!("customization {at:?}: {}", message(&e)));
    let admin = src.admin().await;
    for (role, rule) in roles() {
        let rule: PermissionDef = serde_json::from_value(rule).unwrap();
        k.set_role_permission(&admin, role, &rule).await.unwrap_or_else(|e| panic!("{role}: {}", message(&e)));
    }
    for a in automations() {
        automation::save(k, &admin, &obj(a)).await.unwrap_or_else(|e| panic!("automation: {}", message(&e)));
    }
    src.insert(&admin, "Space", space()).await;
    let saved = k.save_release(&admin, &solution()).await.unwrap_or_else(|e| panic!("solution: {}", message(&e)));
    activate(&src, &saved, "admin2@grc-source.test").await;
    let mut pack = k
        .export_pack(&src.admin().await, &ExportOptions { name: "grc".into(), label: "Governance and Risk".into(), version: 1, records: vec![] })
        .await
        .expect("export");
    pack.as_object_mut().unwrap().remove("exported_at");
    // A solution travels with the moment its source's second administrator reviewed it, which no rebuild
    // repeats: the reference pack says the day it was first made.
    for s in pack["solutions"].as_array_mut().unwrap() {
        for r in s["evidence"]["reviews"].as_array_mut().unwrap() {
            r["at"] = json!(REVIEWED_AT);
        }
    }
    pack
}

#[test]
fn governance_and_risk_as_a_pack() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_STUDIO_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_studio_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 12, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    let mut packages = vec![core];
    packages.extend(ferrum_studio::packages().expect("studio packages"));
    let mut hooks = HookRegistry::default();
    ferrum_studio::register(&mut hooks);
    let k: Arc<Kernel> = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");

    // ───────────── the committed pack is what the builder produces ─────────────
    let built = build(&k).await;
    let text = serde_json::to_string_pretty(&built).unwrap() + "\n";
    if std::env::var("FERRUM_WRITE_PACKS").is_ok_and(|v| v == "1") {
        std::fs::write(pack_path(), &text).expect("write the pack");
    }
    let committed = std::fs::read_to_string(pack_path()).expect("packs/grc/grc.pack.json");
    assert!(committed == text, "packs/grc/grc.pack.json is not what the builder in this test produces: run it with FERRUM_WRITE_PACKS=1 and commit the file");
    let pack: Value = serde_json::from_str(&committed).unwrap();

    // ───────────── installed in a fresh workspace, from the committed file ─────────────
    let t = workspace(&k, "secure").await;
    let admin = t.admin().await;
    let plan = k.plan_pack(&admin, &pack).await.expect("plan");
    assert_eq!(plan["can_install"], json!(true), "{plan}");
    let done = k.install_pack(&admin, &pack, plan["digest"].as_str().unwrap()).await.expect("install");
    assert_eq!(done["failed"], json!([]));
    let admin = t.admin().await;
    let drafts = k.logic_drafts(&admin).await.unwrap();
    assert_eq!(drafts.len(), 1, "the solution arrives as a draft");
    let release = k.release_logic_draft(&admin, drafts[0]["id"].as_str().unwrap().parse().unwrap(), 1).await.unwrap();
    activate(&t, &release, "admin2@secure.test").await;
    let k = &t.k;
    let rm = t.user("rm@secure.test", &["Risk Manager"]).await;
    let ra = t.user("ra@secure.test", &["Risk Approver"]).await;
    let owner_approver = t.user("cfo@secure.test", &["Risk Approver"]).await;
    let reader = t.user("pat@secure.test", &["Policy Reader"]).await;
    let reader2 = t.user("quinn@secure.test", &["Policy Reader"]).await;
    let today = Utc::now().date_naive();
    // The scanner's clock: each `day` runs every daily automation once more.
    let mut clock = Utc::now();
    macro_rules! day {
        () => {{
            clock += Duration::days(1);
            while k.scan_schedules_at(clock, ferrum_kernel::schedule::PAGE).await.unwrap() {}
            while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
        }};
    }
    // DIFFERENT: the app's Governance Settings (tolerance 6, a test current 90 days) has no record here. The
    // tolerance is the solution's parameter (changed by a new release a second administrator reviews), and how
    // long a test is current is a field of each control (default 365).

    // ───────────── controls and their tests ─────────────
    // SAME: two controls, one planned and one in place.
    let mfa = id_of(
        &t.insert(
            &rm,
            "XControl",
            json!({"control_name": "Multi-factor sign-in", "category": "Technical", "status": "Planned", "references": "A.8.5", "test_every_days": 90}),
        )
        .await,
    );
    let backups = id_of(
        &t.insert(&rm, "XControl", json!({"control_name": "Nightly backups", "category": "Technical", "status": "In Place", "test_every_days": 90})).await,
    );
    // DIFFERENT: a test is a line of its control (a test's own evidence cannot be lines of a line), with one
    // evidence file, recorded by saving the control; there is no Control Tests list of its own.
    let test = |on: String, result: &str, file: Option<&str>| json!({"tested_on": on, "result": result, "evidence": "Restore log", "evidence_file": file});
    let proof = Some("/files/restore-log");
    // SAME: refused while the control is planned, without its evidence, and on a day to come.
    refused(t.test(&rm, mfa, test(today.to_string(), "Effective", proof)).await, "invalid", "is tested once it is in place");
    refused(t.test(&rm, backups, test(today.to_string(), "Effective", None)).await, "invalid", "an effective result has its evidence");
    refused(t.test(&rm, backups, test((today + Duration::days(1)).to_string(), "Effective", proof)).await, "invalid", "is today or before");
    // SAME: the tester is whoever records it; the standing follows the latest test.
    let c = t.test(&rm, backups, test((today - Duration::days(30)).to_string(), "Ineffective", None)).await.unwrap();
    assert_eq!(c["tests"][0]["tester"], "rm@secure.test");
    assert_eq!((c["effective"].as_bool(), c["last_result"].as_str()), (Some(false), Some("Ineffective")));
    let c = t.test(&rm, backups, test(today.to_string(), "Effective", proof)).await.unwrap();
    assert_eq!((c["effective"].as_bool(), c["last_tested_on"].as_str()), (Some(true), Some(today.to_string().as_str())));
    // SAME: an older test entered later is not the last one.
    let c = t.test(&rm, backups, test((today - Duration::days(10)).to_string(), "Ineffective", None)).await.unwrap();
    assert_eq!(c["effective"], true, "the latest by day stands");
    // SAME: a test 91 days old is stale, found by the daily automation (the app's upkeep). The harness ages
    // the tests by 91 days, then the scanner runs a day.
    let rows: Vec<Uuid> = c["tests"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().parse().unwrap()).collect();
    t.age(&rows, "tested_on", -91).await;
    assert_eq!(t.get("XControl", backups).await["effective"], true, "kept until it is looked at again");
    day!();
    assert_eq!(t.get("XControl", backups).await["effective"], false, "a test 91 days old is stale");
    t.test(&rm, backups, test(today.to_string(), "Effective", proof)).await.unwrap();
    t.set(&rm, "XControl", mfa, json!({"status": "In Place"})).await.unwrap();

    // ───────────── risks ─────────────
    let risk = |title: &str, owner: &str, scores: [i64; 6], treatment: &str, controls: Value| {
        json!({"title": title, "owner": owner, "inherent_likelihood": scores[0], "inherent_impact": scores[1], "current_likelihood": scores[2], "current_impact": scores[3],
               "residual_likelihood": scores[4], "residual_impact": scores[5], "treatment": treatment, "controls": controls})
    };
    // SAME, in the platform's words: a likelihood above 5 is refused by the field's bounds ("must be ≤ 5",
    // where the app said "is from 1 to 5").
    refused(
        k.insert(&rm, "XRisk", obj(risk("Bad", "cfo@secure.test", [6, 3, 3, 3, 2, 2], "Open", json!([])))).await,
        "invalid",
        "inherent_likelihood: must be ≤ 5",
    );
    // SAME: controls never make it worse, nor does the target; mitigating names its controls.
    refused(k.insert(&rm, "XRisk", obj(risk("Worse", "cfo@secure.test", [2, 2, 3, 3, 2, 2], "Open", json!([])))).await, "invalid", "do not make a risk worse");
    refused(
        k.insert(&rm, "XRisk", obj(risk("Target", "cfo@secure.test", [4, 4, 3, 3, 4, 4], "Open", json!([])))).await,
        "invalid",
        "no worse than the risk now",
    );
    refused(k.insert(&rm, "XRisk", obj(risk("Bare", "cfo@secure.test", [4, 4, 3, 3, 2, 2], "Mitigate", json!([])))).await, "invalid", "names the controls");
    // SAME: three scores and the level, worked out on save.
    let ransom = t
        .insert(
            &rm,
            "XRisk",
            risk("Ransomware", "cfo@secure.test", [5, 5, 4, 4, 2, 3], "Mitigate", json!([{"control": backups.to_string()}, {"control": mfa.to_string()}])),
        )
        .await;
    assert_eq!(
        (ransom["inherent_score"].as_i64(), ransom["current_score"].as_i64(), ransom["residual_score"].as_i64(), ransom["level"].as_str()),
        (Some(25), Some(16), Some(6), Some("High"))
    );
    assert!(ransom["number"].as_str().unwrap().starts_with(&format!("RSK-{}-", today.format("%Y"))), "the app's series: {}", ransom["number"]);
    let ransom_id = id_of(&ransom);
    // SAME: at the tolerance, accepted without an acceptance.
    let small = id_of(&t.insert(&rm, "XRisk", risk("Printer jams", "cfo@secure.test", [2, 3, 2, 3, 1, 1], "Accept", json!([]))).await);
    assert_eq!(t.get("XRisk", small).await["level"], "Medium", "6, at the tolerance: accepted without an acceptance");
    // SAME: above it, not without one (the solution's rule counts the approved acceptances).
    refused(t.set(&rm, "XRisk", ransom_id, json!({"treatment": "Accept"})).await, "invalid", "only with an approved acceptance");

    // ───────────── accepting a risk ─────────────
    let acceptance = |risk: Uuid, until: String| json!({"risk": risk.to_string(), "justification": "Insurance covers it this year", "expires_on": until});
    let act = |ctx: &Ctx, entity: &'static str, id: Uuid, action: &'static str| {
        let ctx = ctx.clone();
        async move { k.run_action(&ctx, entity, id, action, json!({})).await }
    };
    // SAME: an acceptance ends after today.
    refused(k.insert(&rm, "XRiskAcceptance", obj(acceptance(ransom_id, today.to_string()))).await, "invalid", "is after today");
    let acc = id_of(&t.insert(&rm, "XRiskAcceptance", acceptance(ransom_id, (today + Duration::days(30)).to_string())).await);
    // SAME: the status is read-only, moved only by decisions and upkeep (a status sent is ignored).
    t.set(&rm, "XRiskAcceptance", acc, json!({"status": "Accepted"})).await.unwrap();
    assert_eq!(t.get("XRiskAcceptance", acc).await["status"], "Draft");
    // SAME, in the platform's words: deciding is a button (Accept It, Reject It, Revoke It) for a Risk Approver,
    // refused as forbidden to a manager, and to an approver who owns the risk.
    refused(act(&rm, "XRiskAcceptance", acc, "accept").await, "forbidden", "'Accept It' is for Risk Approver");
    refused(act(&owner_approver, "XRiskAcceptance", acc, "accept").await, "forbidden", "a risk's owner does not approve taking it on");
    refused(act(&ra, "XRiskAcceptance", acc, "revoke").await, "invalid", "'Revoke It' is not available");
    let a = act(&ra, "XRiskAcceptance", acc, "accept").await.expect("accepted");
    assert_eq!(a["status"], "Accepted", "the answer says the new status: {a}");
    let a = t.get("XRiskAcceptance", acc).await;
    assert_eq!((a["approver"].as_str(), a["decided_on"].as_str()), (Some("ra@secure.test"), Some(today.to_string().as_str())));
    // SAME: approving puts the risk's treatment to Accept (the action writes the risk it links to, under the
    // risk's own rule: the acceptance it is approving holds it).
    assert_eq!(t.get("XRisk", ransom_id).await["treatment"], "Accept", "accepted by the approval");
    // SAME: an accepted risk is kept.
    t.set(&rm, "XRisk", ransom_id, json!({"description": "Encrypted shares"})).await.expect("an accepted risk is kept");
    // SAME: past its day, the acceptance expires at the midnight upkeep, which opens the risk again at once
    // unless another acceptance holds it. The harness ages it past its day; one day of the scanner.
    t.age(&[acc], "expires_on", -31).await;
    day!();
    assert_eq!(t.get("XRiskAcceptance", acc).await["status"], "Expired");
    assert_eq!(t.get("XRisk", ransom_id).await["treatment"], "Open", "open again once its acceptance expired");
    let again = id_of(&t.insert(&rm, "XRiskAcceptance", acceptance(ransom_id, (today + Duration::days(60)).to_string())).await);
    act(&ra, "XRiskAcceptance", again, "accept").await.unwrap();
    assert_eq!(t.get("XRisk", ransom_id).await["treatment"], "Accept");
    // SAME: revoking is a Risk Approver's (forbidden to the manager).
    refused(act(&rm, "XRiskAcceptance", again, "revoke").await, "forbidden", "'Revoke It' is for Risk Approver");
    // SAME: past its day, before the midnight upkeep has run, an approved acceptance no longer holds the risk:
    // the rule counts approved acceptances whose last day is today or later (the day in the aggregate's filter).
    t.age(&[again], "expires_on", -61).await;
    t.set(&rm, "XRisk", ransom_id, json!({"treatment": "Open"})).await.unwrap();
    refused(t.set(&rm, "XRisk", ransom_id, json!({"treatment": "Accept"})).await, "invalid", "only with an approved acceptance");
    t.age(&[again], "expires_on", 61).await;
    t.set(&rm, "XRisk", ransom_id, json!({"treatment": "Accept"})).await.expect("held again once its day is ahead");
    // SAME: revoked, the risk is open again at once, and the day revoked is recorded.
    act(&ra, "XRiskAcceptance", again, "revoke").await.expect("revoked");
    let r = t.get("XRiskAcceptance", again).await;
    assert_eq!((r["status"].as_str(), r["revoked_on"].as_str()), (Some("Revoked"), Some(today.to_string().as_str())));
    assert_eq!(t.get("XRisk", ransom_id).await["treatment"], "Open", "open again once revoked");
    // SAME: a rejected acceptance leaves the risk open.
    let no = id_of(&t.insert(&rm, "XRiskAcceptance", acceptance(ransom_id, (today + Duration::days(60)).to_string())).await);
    act(&ra, "XRiskAcceptance", no, "reject").await.unwrap();
    assert_eq!(
        (t.get("XRiskAcceptance", no).await["status"].as_str(), t.get("XRisk", ransom_id).await["treatment"].as_str()),
        (Some("Rejected"), Some("Open"))
    );

    // ───────────── policies ─────────────
    let policy = id_of(&t.insert(&rm, "XPolicy", json!({"title": "Acceptable use", "owner": "rm@secure.test", "text": "Lock your screen."})).await);
    // SAME: a reader does not see a draft.
    refused(k.get(&reader, "XPolicy", policy, false).await, "not found", "");
    // SAME: publishing is a button (Publish It): the next revision, the text as published, the day.
    let p = act(&rm, "XPolicy", policy, "publish").await.expect("published");
    assert_eq!(p["status"], "Published", "{p}");
    let p = t.get("XPolicy", policy).await;
    assert_eq!((p["revision"].as_i64(), p["published_text"].as_str(), p["status"].as_str()), (Some(1), Some("Lock your screen."), Some("Published")));
    // SAME: nothing new to publish.
    refused(act(&rm, "XPolicy", policy, "publish").await, "invalid", "nothing new to publish");
    // SAME: "I Have Read It" records my acknowledgement of the current revision (made by the action as the
    // system: a reader creates none herself).
    let done = act(&reader, "XPolicy", policy, "acknowledge").await.expect("acknowledged");
    let first = t.get("XPolicyAcknowledgement", id_of(done["made"][0].as_object().unwrap())).await;
    assert_eq!((first["revision"].as_i64(), first["person"].as_str()), (Some(1), Some("pat@secure.test")));
    refused(k.insert(&reader, "XPolicyAcknowledgement", obj(json!({"policy": policy.to_string()}))).await, "forbidden", "");
    // DIFFERENT: the second time, the unique key refuses it as a conflict, in the platform's words.
    refused(act(&reader, "XPolicy", policy, "acknowledge").await, "conflict", "already has this Policy and Revision and Person");
    // SAME: what was published stays until published again, at its next revision.
    t.set(&rm, "XPolicy", policy, json!({"text": "Lock your screen. Report lost devices."})).await.unwrap();
    assert_eq!(t.get("XPolicy", policy).await["published_text"], "Lock your screen.", "what was published stays until published again");
    act(&rm, "XPolicy", policy, "publish").await.unwrap();
    assert_eq!(t.get("XPolicy", policy).await["revision"], 2);
    act(&reader, "XPolicy", policy, "acknowledge").await.unwrap();
    act(&reader2, "XPolicy", policy, "acknowledge").await.unwrap();
    let acks = k.list(&admin, "XPolicyAcknowledgement", &ListQuery::default()).await.unwrap().rows;
    let mut seen: Vec<(String, i64)> = acks.iter().map(|a| (a["person"].as_str().unwrap().to_string(), a["revision"].as_i64().unwrap())).collect();
    seen.sort();
    assert_eq!(seen, [("pat@secure.test".to_string(), 1), ("pat@secure.test".to_string(), 2), ("quinn@secure.test".to_string(), 2)]);
    // SAME: a reader sees her own acknowledgements.
    assert_eq!(k.list(&reader2, "XPolicyAcknowledgement", &ListQuery::default()).await.unwrap().rows.len(), 1, "a reader sees her own acknowledgements");
    // SAME: retiring is a button (Retire It); a retired policy is not found by a reader, so not acknowledged.
    act(&rm, "XPolicy", policy, "retire").await.unwrap();
    assert_eq!(t.get("XPolicy", policy).await["status"], "Retired");
    refused(act(&reader, "XPolicy", policy, "acknowledge").await, "not found", "");
    refused(k.get(&reader, "XPolicy", policy, false).await, "not found", "");

    // ───────────── findings ─────────────
    let finding = |title: &str, due: String| json!({"title": title, "severity": "High", "control": mfa.to_string(), "owner": "rm@secure.test", "due_on": due});
    let f1 = id_of(&t.insert(&rm, "XFinding", finding("MFA not on admin accounts", (today + Duration::days(7)).to_string())).await);
    let f2 = id_of(&t.insert(&rm, "XFinding", finding("Old VPN accounts", (today + Duration::days(30)).to_string())).await);
    // SAME: past its day, a finding is overdue, found by the daily automation (the harness ages the first ten
    // days, as the app's upkeep ran ten days on).
    t.age(&[f1, f2], "due_on", -10).await;
    day!();
    assert_eq!((t.get("XFinding", f1).await["overdue"].as_bool(), t.get("XFinding", f2).await["overdue"].as_bool()), (Some(true), Some(false)));
    // SAME: closed, it says how, and is not overdue.
    refused(t.set(&rm, "XFinding", f1, json!({"status": "Closed"})).await, "invalid", "says how it was fixed");
    t.set(&rm, "XFinding", f1, json!({"status": "Closed", "remediation": "Enforced for every admin"})).await.unwrap();
    assert_eq!(t.get("XFinding", f1).await["overdue"], false, "closed, it is not overdue");

    // ───────────── who may do what ─────────────
    // SAME: an approver creates no risk, a reader no policy, and an approver records no test (here: may not
    // change the control whose line it is).
    refused(k.insert(&ra, "XRisk", obj(risk("Mine", "ra@secure.test", [1, 1, 1, 1, 1, 1], "Open", json!([])))).await, "forbidden", "");
    refused(k.insert(&reader, "XPolicy", obj(json!({"title": "Mine", "text": "x"}))).await, "forbidden", "");
    refused(t.test(&ra, backups, test(today.to_string(), "Ineffective", None)).await, "forbidden", "");

    // ───────────── not in the app's tests: its page ─────────────
    // DIFFERENT: the app's own page at /grc/ is a Studio space here, with the same four lists; each person sees
    // the lists of what they may read.
    let space: Uuid = {
        let mut tx = k.db.tenant_tx(t.tenant).await.unwrap();
        let id: String = sqlx::query_scalar("select id::text from e_space where tenant_id = $1").bind(t.tenant).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        id.parse().unwrap()
    };
    let labels = |page: &Value| -> Vec<String> {
        page["blocks"].as_array().unwrap().iter().filter(|b| b["type"] == "list").map(|b| b["label"].as_str().unwrap().to_string()).collect()
    };
    let page = ferrum_studio::builder::space_data(k, &rm, space).await.expect("the space");
    assert_eq!(labels(&page), ["Risks", "Controls to test", "Open findings", "Policies"], "{page}");
    for b in page["blocks"].as_array().unwrap() {
        assert!(b.get("error").is_none(), "{b}");
    }
    let page = ferrum_studio::builder::space_data(k, &reader, space).await.expect("the space");
    assert_eq!(labels(&page), ["Policies"], "a reader sees what she may read: {page}");
}
