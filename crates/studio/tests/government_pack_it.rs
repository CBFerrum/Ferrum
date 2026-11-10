//! Government as a pack (docs/builder/evidence/government-as-a-pack-2026-11-11.md), against a real PostgreSQL
//! (`FERRUM_STUDIO_TEST_DATABASE_URL`; the database is reset first), with ERP's real packages.
//!
//! The app `ferrum-app-government` 0.3.0 (permit types with their documents and review stages, applications
//! reviewed stage by stage and paid through ERP, permits issued, revoked, expired and renewed, appeals, cases with
//! deadlines, a public register and a citizens' portal: about 1,000 lines of Rust) is rebuilt here by configuration
//! alone, with the calls an administrator makes: one customization batch (entities, checks, configured actions, a
//! menu marked for the portal, a published list), four roles (one for the portal), three scheduled automations
//! and a Studio space. It is exported as a pack, which must be exactly `packs/government/government.pack.json`
//! (`FERRUM_WRITE_PACKS=1` writes it), and that committed file is installed in a fresh workspace where every
//! scenario of the app's `tests/government_it.rs` is restated, as people holding the pack's roles and as portal
//! people. Each step says whether the pack does it the **same** way, a **different** way (and what a person
//! notices), or **cannot** (and asserts what happens today, naming what is missing).
//!
//! What the harness does that a person does not: it ages records by SQL (in the tenant's transaction) where the
//! app's test moved its upkeep's clock, since computed fields and automation conditions read the real day; it runs
//! the schedule scanner a day ahead (`day`), as grc_pack_it.rs does; and it writes a document's file as a stored
//! file's address, where the portal uploads one.

use chrono::{Duration, Utc};
use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::pack::ExportOptions;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, Site, SiteRequest, automation};
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

fn uid(v: &Value) -> Uuid {
    v.as_str().unwrap_or_else(|| panic!("not an id: {v}")).parse().unwrap()
}

fn dec(v: &Value) -> f64 {
    v.as_str().map(|s| s.parse().unwrap()).or_else(|| v.as_f64()).unwrap_or_else(|| panic!("not a number: {v}"))
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

fn pack_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs/government/government.pack.json")
}

/// The app's own Arabic and Hindi words (its `ui/src/locales/{ar,hi}.json` and `names/*.json`) for the texts this
/// pack defines, `{language: {english: words}}`: they travel as `InterfaceWord` records.
fn words() -> serde_json::Map<String, Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs/government/words.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// What the pack names that a page shows: entity labels and plurals, field labels, choices, buttons and their
/// confirmations, the menu's label and description (as grc_pack_it.rs counts them).
fn texts(pack: &Value) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for c in pack["customizations"].as_array().unwrap() {
        let op = &c["op"];
        match op["op"].as_str().unwrap() {
            "define_entity" | "redefine_entity" => {
                let e: ferrum_meta::EntityDef = serde_json::from_value(op["entity"].clone()).unwrap();
                out.extend([e.label.clone(), e.plural_label()]);
                for f in &e.fields {
                    out.insert(f.label());
                    out.extend(f.options.iter().cloned());
                }
            }
            "add_action" => {
                out.extend(["label", "confirm"].iter().filter_map(|k| op["action"][k].as_str().map(str::to_string)));
                for p in op["action"]["params"].as_array().into_iter().flatten() {
                    out.extend(p["label"].as_str().map(str::to_string));
                }
            }
            "define_module" => out.extend(["label", "description"].iter().filter_map(|k| op["module"][k].as_str().map(str::to_string))),
            _ => {}
        }
    }
    out
}

// ───────────── dates, in CEL: what an administrator writes where the app called chrono ─────────────

/// A date (an expression giving `YYYY-MM-DD`) as a timestamp at midnight UTC.
fn ts(d: &str) -> String {
    format!("timestamp({d} + 'T00:00:00Z')")
}

/// A timestamp as a date. CEL formats no dates: each part is taken and padded by hand.
fn day_of(t: &str) -> String {
    format!(
        "string({t}.getFullYear()) + '-' + ({t}.getMonth() < 9 ? '0' : '') + string({t}.getMonth() + 1) + '-' + ({t}.getDate() < 10 ? '0' : '') + string({t}.getDate())"
    )
}

/// `d` plus (or minus) `n` days.
fn days_from(d: &str, sign: char, n: &str) -> String {
    day_of(&format!("({} {sign} duration(string(int({n}) * 24) + 'h'))", ts(d)))
}

/// `d` plus `n` calendar months, the day kept unless the month is shorter (chrono's `checked_add_months`).
fn months_from(d: &str, n: &str) -> String {
    let t = ts(d);
    let m = format!("({t}.getMonth() + int({n}))");
    let y = format!("({t}.getFullYear() + {m} / 12)");
    let mo = format!("({m} % 12 + 1)");
    let last =
        format!("({mo} == 2 ? ({y} % 4 == 0 && ({y} % 100 != 0 || {y} % 400 == 0) ? 29 : 28) : ({mo} == 4 || {mo} == 6 || {mo} == 9 || {mo} == 11 ? 30 : 31))");
    format!(
        "string({y}) + '-' + ({mo} < 10 ? '0' : '') + string({mo}) + '-' + ({t}.getDate() < 10 ? '0' : '') + string({t}.getDate() < {last} ? {t}.getDate() : {last})"
    )
}

/// A configured action's parameter, or null when it was not sent.
fn p(name: &str) -> String {
    format!("(has(params.{name}) && params.{name} != '' ? params.{name} : null)")
}

/// A text field that holds something.
fn said(field: &str) -> String {
    format!("({field} != null && {field} != '')")
}

// ───────────── the review stages, by configuration ─────────────

const STAGES: &str = "links.permit_type.stages";

fn passed(stage: &str) -> String {
    format!("doc.reviews.exists(r, r.stage == {stage} && r.outcome == 'Passed')")
}

/// The stages an application waits for: those not passed whose step (a stage alongside the one before shares its
/// step) has no earlier step left to pass.
fn waiting() -> String {
    format!("{STAGES}.filter(s, !{} && {STAGES}.all(u, u.step >= s.step || {}))", passed("s.stage"), passed("u.stage"))
}

/// "Waiting For": the names of the stages it waits for. CEL joins no lists: up to three by name, then how many more.
fn waiting_for() -> String {
    let w = waiting();
    format!(
        "size({w}) == 0 ? null : {w}[0].stage + (size({w}) > 1 ? ', ' + {w}[1].stage : '') + (size({w}) > 2 ? ', ' + {w}[2].stage : '') + (size({w}) > 3 ? ' and ' + string(size({w}) - 3) + ' more' : '')"
    )
}

/// The stage Pass This Stage passes: the one named (`which`), else the first waiting that the caller's roles may
/// pass, else the first waiting.
fn chosen() -> String {
    let w = waiting();
    let mine = format!("{w}.filter(s, !has(s.role) || s.role == null || s.role == '' || s.role in user.roles)");
    format!("({} ? params.which : (size({mine}) > 0 ? {mine}[0].stage : {w}[0].stage))", "has(params.which) && params.which != null && params.which != ''")
}

/// A stage of the type named `c` whose step is `rel` to the step of `s` (the stage chosen) was passed by the caller.
fn passed_by_me(c: &str, rel: &str) -> String {
    format!(
        "{STAGES}.exists(s, s.stage == {c} && doc.reviews.exists(r, r.outcome == 'Passed' && r.reviewer == user.email && {STAGES}.exists(u, u.stage == r.stage && u.step {rel})))"
    )
}

// ───────────── the configuration: what an administrator enters ─────────────

const APPLICATION_STATUS: [&str; 8] = ["Draft", "Submitted", "Under Review", "Approved", "Rejected", "Appealed", "Issued", "Withdrawn"];
const PUBLISHED: [&str; 2] = ["Published", "Left Out"];

/// `permit_and_links`: the permit's links to applications, which can only be added once applications exist
/// (a link to an entity not yet defined is refused, so the two records that name each other are defined twice).
fn permit(permit_and_links: bool) -> Value {
    // Months from its last day to today, by calendar month, and whether today is past that day of the month.
    let (e, t) = (ts("doc.last_day"), ts("today"));
    let months_since = format!("(({t}.getFullYear() * 12 + {t}.getMonth()) - ({e}.getFullYear() * 12 + {e}.getMonth()))");
    let kept = format!(
        "(doc.type_keeps_for_good == true || {months_since} < doc.type_keep_months || ({months_since} == doc.type_keep_months && {t}.getDate() <= {e}.getDate()))"
    );
    let mut fields = vec![
        json!({"name": "permit_type", "type": "link", "to": "XPermitType", "required": true, "read_only": true, "in_list": true, "in_filter": true}),
        json!({"name": "holder_name", "type": "data", "required": true, "read_only": true, "in_list": true}),
        json!({"name": "holder_email", "type": "email", "read_only": true}),
        json!({"name": "issued_on", "type": "date", "read_only": true, "in_list": true}),
        json!({"name": "valid_months", "type": "int", "read_only": true, "hidden": true, "description": "Its type's months, when it was issued"}),
        // Computed fields are worked out in the order of their names: expires_on, last_day, on_register, renew_from,
        // status, each seeing those before it.
        json!({"name": "expires_on", "type": "date", "in_list": true, "computed": months_from("doc.issued_on", "doc.valid_months")}),
        json!({"name": "status", "type": "select", "options": ["Valid", "Expired", "Revoked"], "no_copy": true, "in_list": true, "in_filter": true,
               "description": "Revoked once revoked; Expired after its last day",
               "computed": "doc.revoked_on != null ? 'Revoked' : (doc.expires_on < today ? 'Expired' : 'Valid')"}),
        json!({"name": "revoked_reason", "type": "text", "read_only": true, "no_copy": true}),
        json!({"name": "revoked_on", "type": "date", "read_only": true, "no_copy": true}),
        json!({"name": "conditions", "type": "text", "read_only": true, "description": "What it was issued on condition of"}),
        json!({"name": "renewal_of", "type": "link", "to": "XPermit", "read_only": true, "description": "The permit it renews"}),
        json!({"name": "reminded_on", "type": "date", "hidden": true, "no_copy": true, "description": "When its holder was told it may be renewed"}),
        // What its type says, as of its issue and each night (a published list and a computed field read the
        // record's own fields only).
        json!({"name": "type_in_register", "type": "check", "hidden": true}),
        json!({"name": "type_shows_holder", "type": "check", "hidden": true}),
        json!({"name": "type_shows_conditions", "type": "check", "hidden": true}),
        json!({"name": "type_keep_months", "type": "int", "hidden": true}),
        json!({"name": "type_keeps_for_good", "type": "check", "hidden": true}),
        json!({"name": "type_renews", "type": "check", "hidden": true}),
        json!({"name": "type_renew_days", "type": "int", "hidden": true}),
        json!({"name": "last_day", "type": "date", "hidden": true, "computed": "doc.revoked_on != null ? doc.revoked_on : doc.expires_on"}),
        json!({"name": "renew_from", "type": "date", "description": "From when it may be renewed",
               "computed": format!("doc.type_renews != true ? null : {}", days_from("doc.expires_on", '-', "doc.type_renew_days"))}),
        json!({"name": "on_register", "type": "check", "hidden": true, "in_filter": true,
               "computed": format!("doc.type_in_register == true && ((doc.revoked_on == null && doc.expires_on >= today) || {kept})")}),
        json!({"name": "register_holder", "label": "Holder", "type": "data", "computed": "doc.type_shows_holder == true ? doc.holder_name : null"}),
        json!({"name": "register_conditions", "label": "Conditions", "type": "text", "computed": "doc.type_shows_conditions == true ? doc.conditions : null"}),
    ];
    if permit_and_links {
        fields.insert(1, json!({"name": "application", "type": "link", "to": "XPermitApplication", "read_only": true}));
        fields.push(json!({"name": "renewal", "type": "link", "to": "XPermitApplication", "read_only": true, "no_copy": true, "description": "Its renewal's application"}));
    }
    json!({"name": "XPermit", "label": "Permit", "label_plural": "Permits", "kind": "master", "title_field": "holder_name",
        "description": "A permit issued, valid until it expires or is revoked.", "numbering": {"template": "PRM-{YYYY}-{seq:5}"},
        "fields": fields, "list": {"columns": ["number", "permit_type", "holder_name", "expires_on", "status"], "sort": "issued_on desc"}})
}

fn entities() -> Vec<Value> {
    vec![
        json!({"name": "XPermitDocument", "label": "Document Needed", "kind": "child", "fields": [
            {"name": "document", "type": "data", "required": true, "in_list": true}]}),
        json!({"name": "XPermitStage", "label": "Review Stage", "kind": "child", "fields": [
            {"name": "stage", "type": "data", "required": true, "in_list": true},
            {"name": "role", "type": "data", "in_list": true, "description": "The role whose people pass it; any permit officer when empty"},
            {"name": "inspection", "label": "Site Inspection", "type": "check", "in_list": true, "description": "A visit: what was found is kept with it"},
            {"name": "alongside", "label": "Alongside The One Before", "type": "check", "in_list": true,
             "description": "Passed side by side with the stage before it, in any order, each by its own people"},
            // A line knows its place (`idx`) and its record's other lines: its step is how many groups start at or
            // before it.
            {"name": "step", "type": "int", "in_list": true, "description": "Stages of one step are passed side by side",
             "computed": "size(parent.stages.filter(t, t.idx <= doc.idx && (t.idx == 0 || !has(t.alongside) || t.alongside != true)))"}]}),
        json!({"name": "XPermitType", "label": "Permit Type", "label_plural": "Permit Types", "kind": "master", "title_field": "type_name",
            "description": "A permit people apply for, its fee, the documents it needs, the stages of its review, how long it lasts, whether it may be renewed, and whether its permits are on the public register.",
            "fields": [
            {"name": "type_name", "type": "data", "required": true, "unique": true, "in_list": true},
            {"name": "fee", "type": "currency", "default": 0, "in_list": true},
            {"name": "company", "type": "link", "to": "Company", "description": "Who invoices its fee"},
            {"name": "fee_item", "type": "link", "to": "Item", "description": "What its fee is invoiced as"},
            {"name": "valid_months", "type": "int", "required": true, "default": 12, "in_list": true},
            {"name": "documents", "label": "Documents Needed", "type": "table", "of": "XPermitDocument"},
            {"name": "stages", "label": "Review Stages", "type": "table", "of": "XPermitStage"},
            {"name": "renewable", "type": "check", "default": false, "description": "Its permits may be renewed, from Renew Within days before they expire"},
            {"name": "renew_within_days", "label": "Renew Within (Days)", "type": "int", "default": 60},
            {"name": "in_register", "label": "On The Public Register", "type": "check", "default": false},
            {"name": "register_holder", "label": "Holder on the Register", "type": "select", "options": PUBLISHED, "default": "Left Out"},
            {"name": "register_conditions", "label": "Conditions on the Register", "type": "select", "options": PUBLISHED, "default": "Left Out"},
            {"name": "register_keep_months", "label": "On the Register After It Ends (Months)", "type": "int", "default": 12, "min": 0},
            // An empty whole number reads as 0 in an expression, so "empty: for as long as it is kept" needs a box of its own.
            {"name": "register_keep_forever", "label": "On the Register for Good", "type": "check", "default": false,
             "description": "Ended permits stay on the register for as long as they are kept"},
            {"name": "appeal_within_days", "label": "Appeal Within (Days)", "type": "int", "default": 30, "min": 0},
            {"name": "description", "type": "text"},
            {"name": "active", "type": "check", "default": true, "in_list": true, "in_filter": true}],
            "list": {"columns": ["type_name", "fee", "valid_months", "active"], "sort": "type_name asc"}}),
        json!({"name": "XApplicationDocument", "label": "Document", "kind": "child", "fields": [
            {"name": "document", "type": "data", "required": true, "in_list": true},
            {"name": "file", "type": "attach", "in_list": true}]}),
        json!({"name": "XApplicationReview", "label": "Review", "kind": "child", "fields": [
            {"name": "stage", "type": "data", "read_only": true, "in_list": true},
            {"name": "outcome", "type": "select", "options": ["Passed", "Failed"], "read_only": true, "in_list": true},
            {"name": "reviewer", "type": "email", "read_only": true, "in_list": true},
            {"name": "reviewed_on", "type": "date", "read_only": true, "in_list": true},
            {"name": "note", "type": "text", "read_only": true},
            {"name": "findings", "type": "text", "read_only": true, "description": "What a site inspection found"}]}),
        permit(false),
        json!({"name": "XPermitApplication", "label": "Permit Application", "label_plural": "Permit Applications", "kind": "master", "title_field": "applicant_name",
            "description": "Someone's application for a permit, from the portal or the counter. Submitted with its documents and its fee invoiced; approved once the fee is paid; issued as a permit.",
            "numbering": {"template": "APP-{YYYY}-{seq:5}"},
            "fields": [
            {"name": "permit_type", "type": "link", "to": "XPermitType", "required": true, "in_list": true, "in_filter": true},
            {"name": "applicant_name", "type": "data", "required": true, "in_list": true},
            {"name": "applicant_email", "type": "email", "in_filter": true, "default_expr": "'Applicant' in user.roles ? user.email : null",
             "description": "Their portal sign-in, when they applied there"},
            {"name": "details", "type": "text"},
            {"name": "documents", "type": "table", "of": "XApplicationDocument"},
            {"name": "status", "type": "select", "options": APPLICATION_STATUS, "default": "Draft", "read_only": true, "no_copy": true, "in_list": true, "in_filter": true},
            {"name": "submitted_on", "type": "date", "read_only": true, "no_copy": true},
            {"name": "officer", "type": "email", "read_only": true, "no_copy": true, "in_list": true, "description": "Who took it up"},
            {"name": "customer", "type": "link", "to": "Customer", "read_only": true, "no_copy": true, "description": "Who the fee is invoiced to: made from the applicant"},
            {"name": "fee_invoice", "type": "link", "to": "SalesInvoice", "read_only": true, "no_copy": true},
            {"name": "waiting_for", "label": "Waiting For", "type": "data", "read_only": true, "no_copy": true, "in_list": true, "description": "The review stages it waits for"},
            {"name": "reviews", "type": "table", "of": "XApplicationReview", "read_only": true, "no_copy": true},
            {"name": "renewal_of", "type": "link", "to": "XPermit", "read_only": true, "description": "The permit it renews"},
            {"name": "decision_note", "type": "text", "read_only": true, "no_copy": true},
            {"name": "conditions", "type": "text", "read_only": true, "no_copy": true, "description": "What the permit is issued on condition of"},
            {"name": "decided_on", "type": "date", "read_only": true, "no_copy": true},
            {"name": "decided_by", "type": "email", "read_only": true, "no_copy": true, "description": "Who approved or rejected it"},
            {"name": "appeal_until", "type": "date", "read_only": true, "no_copy": true, "description": "The last day its rejection may be appealed"},
            {"name": "appealed_on", "type": "date", "read_only": true, "no_copy": true},
            {"name": "appeal_grounds", "type": "text", "read_only": true, "no_copy": true},
            {"name": "appeal_outcome", "type": "select", "options": ["Upheld", "Overturned"], "read_only": true, "no_copy": true, "in_filter": true},
            {"name": "appeal_decided_by", "type": "email", "read_only": true, "no_copy": true},
            {"name": "appeal_note", "type": "text", "read_only": true, "no_copy": true},
            {"name": "permit", "type": "link", "to": "XPermit", "read_only": true, "no_copy": true}],
            "list": {"columns": ["number", "permit_type", "applicant_name", "status", "officer"], "sort": "created_at desc"}}),
        permit(true),
        json!({"name": "XCaseCategory", "label": "Case Category", "label_plural": "Case Categories", "kind": "master", "title_field": "category_name",
            "fields": [
            {"name": "category_name", "type": "data", "required": true, "unique": true, "in_list": true},
            {"name": "days_to_resolve", "type": "int", "required": true, "default": 10, "in_list": true, "description": "A case is due this many days after it opens"}],
            "list": {"columns": ["category_name", "days_to_resolve"], "sort": "category_name asc"}}),
        json!({"name": "XCase", "label": "Case", "label_plural": "Cases", "kind": "master", "title_field": "subject",
            "description": "Something a citizen raised, due within its category's days.", "numbering": {"template": "CASE-{YYYY}-{seq:5}"},
            "fields": [
            {"name": "subject", "type": "data", "required": true, "in_list": true},
            {"name": "category", "type": "link", "to": "XCaseCategory", "required": true, "in_list": true, "in_filter": true},
            {"name": "citizen_name", "type": "data", "required": true, "in_list": true},
            {"name": "citizen_email", "type": "email", "in_filter": true, "default_expr": "'Applicant' in user.roles ? user.email : null"},
            {"name": "description", "type": "text"},
            {"name": "status", "type": "select", "options": ["Open", "In Progress", "Resolved", "Closed"], "default": "Open", "in_list": true, "in_filter": true},
            {"name": "officer", "type": "email", "in_list": true, "in_filter": true},
            {"name": "opened_on", "type": "date", "read_only": true, "no_copy": true, "in_list": true, "default_expr": "today"},
            {"name": "days_to_resolve", "type": "int", "read_only": true, "hidden": true, "fetch_from": "category.days_to_resolve"},
            // In the order of their names: due_on, overdue, resolved_on.
            {"name": "due_on", "type": "date", "no_copy": true, "in_list": true, "computed": days_from("doc.opened_on", '+', "doc.days_to_resolve")},
            {"name": "resolution", "type": "text"},
            // Keeps its first day: the stored value is what the record holds when it is worked out.
            {"name": "resolved_on", "type": "date", "no_copy": true,
             "computed": "doc.status == 'Resolved' || doc.status == 'Closed' ? (doc.resolved_on != null ? doc.resolved_on : today) : null"},
            {"name": "overdue", "type": "check", "no_copy": true, "in_list": true, "in_filter": true,
             "computed": "doc.status != 'Resolved' && doc.status != 'Closed' && doc.due_on < today"}],
            "list": {"columns": ["number", "subject", "category", "status", "due_on", "overdue"], "sort": "created_at desc"}}),
    ]
}

/// Moving from `from` (any, when empty) to `to`.
fn moving(from: &[&str], to: &str) -> String {
    let was = if from.is_empty() {
        "old != null".to_string()
    } else {
        format!("old != null && ({})", from.iter().map(|f| format!("old.status == '{f}'")).collect::<Vec<_>>().join(" || "))
    };
    format!("({was} && old.status != '{to}' && doc.status == '{to}')")
}

/// What a permit keeps of its type, written when it is issued and each night: a published list and a computed
/// field read the record's own fields only.
fn type_copies() -> Value {
    let l = "links.permit_type";
    json!({"type_in_register": format!("{l}.in_register == true"), "type_shows_holder": format!("{l}.register_holder == 'Published'"),
        "type_shows_conditions": format!("{l}.register_conditions == 'Published'"), "type_keep_months": format!("{l}.register_keep_months"),
        "type_keeps_for_good": format!("{l}.register_keep_forever == true"), "type_renews": format!("{l}.renewable == true"),
        "type_renew_days": format!("{l}.renew_within_days")})
}

/// The checks (`add_rule`), each with the app's words where it had some.
fn checks() -> Vec<(&'static str, &'static str, String, &'static str)> {
    let paid = "links.fee_invoice != null && links.fee_invoice.docstatus == 1 && links.fee_invoice.outstanding_amount != null && double(links.fee_invoice.outstanding_amount) <= 0.0";
    let applied = ["permit_type", "applicant_name", "applicant_email", "details", "documents"].map(|f| format!("doc.{f} == old.{f}")).join(" && ");
    vec![
        ("XPermitType", "stages_named_once", "doc.stages.all(s, size(doc.stages.filter(t, t.stage == s.stage)) == 1)".into(), "two stages have the same name"),
        (
            "XPermitApplication",
            "keeps_what_was_applied_for",
            format!("old == null || old.status == 'Draft' || ({applied})"),
            "a submitted application keeps what was applied for",
        ),
        (
            "XPermitApplication",
            "type_open",
            format!("!{} || links.permit_type.active == true", moving(&["Draft"], "Submitted")),
            "the permit type is not open for applications",
        ),
        (
            "XPermitApplication",
            "documents_given",
            format!(
                "!{} || links.permit_type.documents.all(d, doc.documents.exists(r, r.document == d.document && has(r.file) && {}))",
                moving(&["Draft"], "Submitted"),
                said("r.file")
            ),
            "still needed: every document its permit type lists, each with its file",
        ),
        (
            "XPermitApplication",
            "stage_waited_for",
            format!(
                "doc.reviews.all(r, r.outcome != 'Passed' || ({STAGES}.exists(s, s.stage == r.stage && {STAGES}.all(u, u.step >= s.step || {})) && size(doc.reviews.filter(q, q.stage == r.stage && q.outcome == 'Passed')) == 1))",
                passed("u.stage")
            ),
            "is not one it waits for",
        ),
        (
            "XPermitApplication",
            "inspection_findings",
            format!(
                "doc.reviews.all(r, r.outcome != 'Passed' || !{STAGES}.exists(s, s.stage == r.stage && has(s.inspection) && s.inspection == true) || (has(r.findings) && {}))",
                said("r.findings")
            ),
            "a site inspection says what it found",
        ),
        (
            "XPermitApplication",
            "stages_passed",
            format!("!{} || {STAGES}.all(s, {})", moving(&["Under Review"], "Approved"), passed("s.stage")),
            "every review stage is passed first",
        ),
        ("XPermitApplication", "fee_paid", format!("!{} || doc.fee_invoice == null || ({paid})", moving(&[], "Approved")), "the fee is not paid yet"),
        ("XPermitApplication", "rejection_says_why", format!("doc.status != 'Rejected' || {}", said("doc.decision_note")), "a rejection says why"),
        (
            "XPermitApplication",
            "appeal_says_why",
            format!("doc.status != 'Appealed' || {}", said("doc.appeal_grounds")),
            "an appeal says why the rejection should be looked at again",
        ),
        (
            "XPermitApplication",
            "appealable",
            format!("!{} || doc.appeal_until != null", moving(&["Rejected"], "Appealed")),
            "a rejection of this type may not be appealed",
        ),
        (
            "XPermitApplication",
            "appeal_in_time",
            format!("!{} || doc.appeal_until == null || doc.appeal_until >= today", moving(&["Rejected"], "Appealed")),
            "the days to appeal it have ended",
        ),
        (
            "XPermitApplication",
            "appeal_decided",
            format!(
                "!{} || (doc.appeal_outcome in ['Upheld', 'Overturned'] && {})",
                "(old != null && old.status == 'Appealed' && doc.status != 'Appealed')",
                said("doc.appeal_note")
            ),
            "an appeal's decision is Upheld or Overturned, saying why",
        ),
        ("XPermit", "revocation_says_why", format!("doc.revoked_on == null || {}", said("doc.revoked_reason")), "a revocation says why"),
        (
            "XPermit",
            "renewable",
            "old == null || doc.renewal == null || doc.renewal == old.renewal || links.permit_type.renewable == true".into(),
            "permits of this type are not renewed",
        ),
        (
            "XPermit",
            "renewal_open",
            format!(
                "old == null || doc.renewal == null || doc.renewal == old.renewal || today >= {}",
                days_from("doc.expires_on", '-', "links.permit_type.renew_within_days")
            ),
            "it may be renewed from its type's days before it ends",
        ),
        (
            "XCase",
            "resolution_says_how",
            format!("(doc.status != 'Resolved' && doc.status != 'Closed') || {}", said("doc.resolution")),
            "a resolved case says how",
        ),
    ]
}

/// A notification to the person whose email a field holds, when they have an account here (in-app).
fn tell(field: &str, title: String, body: Option<&str>) -> Value {
    let mut n = json!({"type": "notify", "to": format!("field:{field}"), "title": title});
    if let Some(b) = body {
        n["body"] = json!(b);
    }
    n
}

/// The app's eleven actions, as configured actions: buttons with their conditions, roles and refusals, whose
/// effects run as the system through the save pipeline.
fn actions() -> Vec<(&'static str, Value)> {
    let officer = json!(["Permit Officer"]);
    let submitted = json!({"type": "set_fields", "values": {"status": "'Submitted'", "submitted_on": "today"}});
    let told_submitted = tell("applicant_email", "'Your application ' + doc.number + ' is submitted'".into(), None);
    // The applicant's customer, made once for good per address (the effect key), and the fee invoiced as a draft.
    let customer = json!({"type": "invoke", "capability": "parties.customer.create",
        "key": "'government-customer:' + (doc.applicant_email != null && doc.applicant_email != '' ? doc.applicant_email : string(doc.id))",
        "input": {"values": "{'customer_name': doc.applicant_name, 'customer_type': 'Individual', 'email': doc.applicant_email}"}});
    let fee = json!({"type": "invoke", "capability": "selling.sales_invoice.create", "key": "'permit-fee:' + string(doc.id)",
        "input": {"values": "{'company': links.permit_type.company, 'customer': results[0].id, 'remarks': links.permit_type.type_name + ' application ' + doc.number, \
                   'items': [{'item': links.permit_type.fee_item, 'qty': '1', 'price_list_rate': string(links.permit_type.fee), 'discount_percent': '0', \
                   'description': links.permit_type.type_name + ' application ' + doc.number}]}", "submit": "false"}});
    let reject = |failing: bool| {
        let mut effects = vec![
            json!({"type": "set_fields", "values": {"status": "'Rejected'", "decision_note": p("note"), "decided_on": "today", "decided_by": "user.email",
            "appeal_until": format!("links.permit_type.appeal_within_days > 0 ? {} : null", days_from("today", '+', "links.permit_type.appeal_within_days"))}}),
        ];
        if failing {
            // The stages it waited for failed with it: one row naming them.
            effects.insert(
                0,
                json!({"type": "append_line", "table": "reviews", "values": {"stage": "doc.waiting_for", "outcome": "'Failed'", "reviewer": "user.email",
                "reviewed_on": "today", "note": p("note")}}),
            );
            effects.push(json!({"type": "set_fields", "values": {"waiting_for": "null"}}));
        }
        effects.push(tell("applicant_email", "'Your application ' + doc.number + ' is rejected'".into(), None));
        let waits = if failing { "doc.waiting_for != null && doc.waiting_for != ''" } else { "(doc.waiting_for == null || doc.waiting_for == '')" };
        json!({"name": if failing { "reject_at_stage" } else { "reject" }, "label": "Reject It", "condition": format!("doc.status == 'Under Review' && {waits}"),
            "roles": officer, "params": [{"name": "note", "label": "Why", "type": "text", "required": true}], "effects": effects})
    };
    let start = format!(
        "doc.renewal_of != null && links.renewal_of != null && links.renewal_of.expires_on >= today ? {} : today",
        days_from("links.renewal_of.expires_on", '+', "1")
    );
    let mut permit_values = type_copies();
    for (k, v) in [
        ("permit_type", "doc.permit_type".to_string()),
        ("application", "doc.id".into()),
        ("holder_name", "doc.applicant_name".into()),
        ("holder_email", "doc.applicant_email".into()),
        ("issued_on", start),
        ("valid_months", "links.permit_type.valid_months".into()),
        ("conditions", "doc.conditions".into()),
        ("renewal_of", "doc.renewal_of".into()),
    ] {
        permit_values[k] = json!(v);
    }
    vec![
        (
            "XPermitApplication",
            json!({"name": "submit_application", "label": "Submit It", "condition": "doc.status == 'Draft' && (links.permit_type == null || links.permit_type.fee <= 0.0)",
                "effects": [submitted, told_submitted]}),
        ),
        (
            "XPermitApplication",
            json!({"name": "submit_with_fee", "label": "Submit It", "condition": "doc.status == 'Draft' && links.permit_type != null && links.permit_type.fee > 0.0",
                "effects": [customer, fee, {"type": "set_fields", "values": {"status": "'Submitted'", "submitted_on": "today", "customer": "results[0].id", "fee_invoice": "results[1].id"}},
                    tell("applicant_email", "'Your application ' + doc.number + ' is submitted'".into(), Some("'Its fee is invoiced: it is reviewed once paid.'"))]}),
        ),
        (
            "XPermitApplication",
            json!({"name": "take_up", "label": "Take It Up", "condition": "doc.status == 'Submitted'", "roles": officer,
                "effects": [{"type": "set_fields", "values": {"status": "'Under Review'", "officer": "user.email", "waiting_for": waiting_for()}}]}),
        ),
        (
            "XPermitApplication",
            json!({"name": "pass_stage", "label": "Pass This Stage", "condition": "doc.status == 'Under Review' && doc.waiting_for != null && doc.waiting_for != ''", "roles": officer,
                "params": [{"name": "note", "label": "Note", "type": "text"}, {"name": "findings", "label": "Findings", "type": "text", "description": "What a site inspection found"},
                           {"name": "which", "label": "Stage", "type": "data", "description": "Which, when stages wait side by side and you may pass more than one: when empty, the first you may pass"}],
                "refuse_if": [
                    {"when": format!("{STAGES}.exists(s, s.stage == {} && has(s.role) && s.role != null && s.role != '' && !(s.role in user.roles))", chosen()),
                     "message": "the stage is passed by someone holding the role its row names"},
                    {"when": passed_by_me(&chosen(), "== s.step - 1"), "message": "someone other than whoever passed the stage before passes it"},
                    {"when": passed_by_me(&chosen(), "== s.step"), "message": "someone other than whoever passed a stage beside it passes it"}],
                "effects": [
                    {"type": "append_line", "table": "reviews", "values": {"stage": chosen(), "outcome": "'Passed'", "reviewer": "user.email", "reviewed_on": "today",
                        "note": p("note"), "findings": p("findings")}},
                    {"type": "set_fields", "values": {"waiting_for": waiting_for()}}]}),
        ),
        (
            "XPermitApplication",
            json!({"name": "approve", "label": "Approve It", "condition": "doc.status == 'Under Review'", "roles": officer,
                "params": [{"name": "note", "label": "Note", "type": "text"}, {"name": "conditions", "label": "Conditions", "type": "text", "description": "What the permit is issued on condition of"}],
                "effects": [{"type": "set_fields", "values": {"status": "'Approved'", "decision_note": p("note"), "decided_on": "today", "decided_by": "user.email", "conditions": p("conditions")}},
                    tell("applicant_email", "'Your application ' + doc.number + ' is approved'".into(), Some("doc.conditions"))]}),
        ),
        ("XPermitApplication", reject(false)),
        ("XPermitApplication", reject(true)),
        (
            "XPermitApplication",
            json!({"name": "appeal", "label": "Appeal the Rejection", "condition": "doc.status == 'Rejected' && doc.appealed_on == null",
                "params": [{"name": "grounds", "label": "Grounds", "type": "text", "required": true, "description": "Why the rejection should be looked at again"}],
                "effects": [{"type": "set_fields", "values": {"status": "'Appealed'", "appealed_on": "today", "appeal_grounds": p("grounds")}},
                    tell("officer", "'Application ' + doc.number + ' is appealed'".into(), Some("doc.appeal_grounds")),
                    tell("decided_by", "'Application ' + doc.number + ' is appealed'".into(), Some("doc.appeal_grounds"))]}),
        ),
        (
            "XPermitApplication",
            json!({"name": "decide_appeal", "label": "Decide the Appeal", "condition": "doc.status == 'Appealed'", "roles": officer,
                "params": [{"name": "outcome", "label": "Outcome", "type": "select", "options": ["Upheld", "Overturned"], "required": true},
                           {"name": "note", "label": "Why", "type": "text", "required": true},
                           {"name": "conditions", "label": "Conditions", "type": "text", "description": "Overturned: what the permit is issued on condition of"}],
                "refuse_if": [{"when": "doc.officer == user.email || doc.decided_by == user.email || doc.reviews.exists(r, r.reviewer == user.email)",
                               "message": "someone who took no part in the decision decides its appeal"}],
                "effects": [{"type": "set_fields", "values": {
                    "appeal_outcome": p("outcome"), "appeal_decided_by": "user.email", "appeal_note": p("note"),
                    "status": format!("{} == 'Overturned' ? 'Approved' : 'Rejected'", p("outcome")),
                    "decision_note": format!("{} == 'Overturned' ? {} : doc.decision_note", p("outcome"), p("note")),
                    "decided_on": format!("{} == 'Overturned' ? today : doc.decided_on", p("outcome")),
                    "decided_by": format!("{} == 'Overturned' ? user.email : doc.decided_by", p("outcome")),
                    "conditions": format!("{} == 'Overturned' ? {} : doc.conditions", p("outcome"), p("conditions"))}},
                    tell("applicant_email", "doc.status == 'Approved' ? 'Your application ' + doc.number + ' is approved on appeal' : 'The rejection of your application ' + doc.number + ' stands'".into(),
                         Some("doc.appeal_note"))]}),
        ),
        (
            "XPermitApplication",
            json!({"name": "issue", "label": "Issue the Permit", "condition": "doc.status == 'Approved'", "roles": officer,
                "effects": [{"type": "create_document", "entity": "XPermit", "values": permit_values},
                    {"type": "set_fields", "values": {"status": "'Issued'", "permit": "results[0].id"}},
                    tell("applicant_email", "'Your permit ' + results[0].number + ' is issued'".into(), Some("doc.conditions"))]}),
        ),
        (
            "XPermitApplication",
            json!({"name": "withdraw", "label": "Withdraw It", "condition": "doc.status == 'Draft' || doc.status == 'Submitted'",
                "effects": [{"type": "set_fields", "values": {"status": "'Withdrawn'"}}]}),
        ),
        (
            "XPermit",
            json!({"name": "revoke", "label": "Revoke It", "condition": "doc.status == 'Valid'", "roles": officer,
                "params": [{"name": "reason", "label": "Why", "type": "text", "required": true}],
                "effects": [{"type": "set_fields", "values": {"revoked_reason": p("reason"), "revoked_on": "today"}}]}),
        ),
        (
            "XPermit",
            json!({"name": "renew", "label": "Renew It",
                "condition": "(doc.status == 'Valid' || doc.status == 'Expired') && (doc.renewal == null || links.renewal == null || links.renewal.status == 'Rejected' || links.renewal.status == 'Withdrawn')",
                "effects": [{"type": "create_document", "entity": "XPermitApplication", "values": {"permit_type": "doc.permit_type", "applicant_name": "doc.holder_name",
                    "applicant_email": "doc.holder_email", "details": "'Renewal of ' + doc.number", "renewal_of": "doc.id"}},
                    {"type": "set_fields", "values": {"renewal": "results[0].id"}}]}),
        ),
    ]
}

/// The app's menu, tile, quick create, assistant notes and portal page, as a workspace's own menu shown in the
/// portal too. The instructions are the app's `shell.assistant` text, with the pack's entity names.
fn menu() -> Value {
    json!({"name": "x_government", "label": "Government", "description": "Permits, applications and citizens' cases", "icon": "landmark", "color": "#4338ca",
        "items": ["XPermitApplication", "XPermit", "XCase", "XPermitType", "XCaseCategory"], "create": ["XPermitApplication", "XCase"], "portal": true,
        "assistant": {
            "instructions": "A Permit Application (XPermitApplication) goes Draft, Submitted, Under Review (through its permit type's stages), then Approved, \
                Rejected, Appealed, Issued or Withdrawn; its fees are paid through ERP. An issued Permit (XPermit) is Valid until it Expires or is Revoked, and \
                is on the public register when its type says so. A Case (XCase: Open, In Progress, Resolved, Closed) is due within its category's days.",
            "suggestions": ["Which applications wait for review, oldest first?", "Which permits expire next month?", "Which cases are past their deadline?"]}})
}

/// The public register (the app's `/register`): permits of the types on it, with what each type publishes.
fn register() -> Value {
    json!({"name": "permits", "label": "Public register of permits", "description": "Permits issued, of the types published on the register",
        "fields": ["number", "permit_type", "register_holder", "issued_on", "expires_on", "status", "register_conditions"],
        "filter": [["on_register", "=", true]], "sort": ["issued_on", "desc"], "csv": true, "limit": 1000})
}

fn roles() -> Vec<(&'static str, Value)> {
    let rwce = |e: &str| json!({"entity": e, "read": true, "write": true, "create": true, "export": true});
    let rwc = |e: &str| json!({"entity": e, "read": true, "write": true, "create": true});
    let r = |e: &str| json!({"entity": e, "read": true});
    vec![
        ("Permit Officer", r("XPermitType")),
        ("Permit Officer", rwce("XPermitApplication")),
        ("Permit Officer", json!({"entity": "XPermit", "read": true, "write": true, "export": true})),
        ("Permit Officer", r("SalesInvoice")),
        ("Case Officer", r("XCaseCategory")),
        ("Case Officer", rwce("XCase")),
        ("Services Manager", rwc("XPermitType")),
        ("Services Manager", rwc("XCaseCategory")),
        ("Services Manager", r("XPermitApplication")),
        ("Services Manager", r("XPermit")),
        ("Services Manager", r("XCase")),
        ("Services Manager", r("Item")),
        ("Services Manager", r("Company")),
    ]
}

/// The portal's people: what may be applied for, and their own applications, permits and cases (by the address they
/// signed in with), with the buttons that are theirs.
fn portal_rules() -> Vec<Value> {
    vec![
        json!({"entity": "XPermitType", "read": true, "where": {"active": true}}),
        json!({"entity": "XCaseCategory", "read": true}),
        json!({"entity": "XPermitApplication", "read": true, "write": true, "create": true, "via": "applicant_email",
            "actions": ["submit_application", "submit_with_fee", "withdraw", "appeal"]}),
        json!({"entity": "XPermit", "read": true, "via": "holder_email", "actions": ["renew"]}),
        json!({"entity": "XCase", "read": true, "create": true, "via": "citizen_email"}),
    ]
}

/// The app's upkeep, as one automation a day per job. A touch (a field set to itself) saves a record again, so its
/// computed fields are worked out that day.
fn automations() -> Vec<Value> {
    let every = |at: &str, filter: Value| json!({"every": {"period": "day", "at": at, "filter": filter}});
    let l = "links.permit_type";
    vec![
        // Each permit's standing (Expired after its last day) and what its type says of the register and renewal.
        json!({"name": "Permits' standing", "entity": "XPermit", "event": "scheduled", "schedule": every("00:00", json!([["holder_name", "is", "set"]])),
            "actions": [{"type": "set_fields", "values": type_copies()}], "is_active": true}),
        json!({"name": "Renewal reminders", "entity": "XPermit", "event": "scheduled",
            "schedule": every("00:05", json!([["status", "=", "Valid"], ["reminded_on", "is", "not set"], ["renewal", "is", "not set"]])),
            "condition": format!("{l}.renewable == true && today >= {}", days_from("doc.expires_on", '-', "links.permit_type.renew_within_days")),
            "actions": [tell("holder_email", "'Your permit ' + doc.number + ' may be renewed: it ends on ' + doc.expires_on".into(), None),
                        {"type": "set_fields", "values": {"reminded_on": "today"}}], "is_active": true}),
        json!({"name": "Cases overdue", "entity": "XCase", "event": "scheduled",
            "schedule": every("00:15", json!([["status", "in", ["Open", "In Progress"]], ["overdue", "=", false]])),
            "actions": [{"type": "set_fields", "values": {"status": "doc.status"}}], "is_active": true}),
    ]
}

/// The app's own page at /government/ (ui/src/Home.tsx): the officers' queues.
fn space() -> Value {
    json!({"label": "Government", "icon": "landmark", "sharing": "roles",
        "roles": [{"role": "Permit Officer"}, {"role": "Case Officer"}, {"role": "Services Manager"}],
        "blocks": [
            {"type": "list", "label": "To take up", "entity": "XPermitApplication", "filters": [["status", "=", "Submitted"]],
             "fields": ["number", "applicant_name", "permit_type", "submitted_on"], "sort": "submitted_on asc", "limit": 50},
            {"type": "list", "label": "Under review", "entity": "XPermitApplication", "filters": [["status", "=", "Under Review"]],
             "fields": ["number", "applicant_name", "waiting_for"], "sort": "submitted_on asc", "limit": 50},
            {"type": "list", "label": "To issue", "entity": "XPermitApplication", "filters": [["status", "=", "Approved"]],
             "fields": ["number", "applicant_name", "permit_type"], "sort": "decided_on asc", "limit": 50},
            {"type": "list", "label": "Open cases", "entity": "XCase", "filters": [["status", "in", ["Open", "In Progress"]]],
             "fields": ["number", "subject", "due_on", "overdue"], "sort": "due_on asc", "limit": 50}]})
}

// ───────────── the harness ─────────────

struct W {
    k: Arc<Kernel>,
    tenant: Uuid,
    user: Uuid,
    slug: String,
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
    /// A portal person holding the pack's portal role.
    async fn citizen(&self, email: &str, name: &str) -> Ctx {
        let admin = self.admin().await;
        let id = self.k.create_user(&admin, email, name, None, &[], false).await.unwrap();
        self.k.set_user_kind(&admin, id, "portal").await.unwrap();
        self.k.set_user_roles_until(&admin, id, &["Applicant".into()], &Default::default()).await.unwrap();
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
    async fn act(&self, ctx: &Ctx, entity: &str, id: Uuid, action: &str, params: Value) -> Result<Value, Error> {
        self.k.run_action(ctx, entity, id, action, params).await
    }
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), limit: Some(50), ..Default::default() };
        let rows = self.k.list(&self.admin().await, entity, &q).await.unwrap().rows;
        assert_eq!(rows.len(), 1, "{entity} {filters}");
        rows[0].clone()
    }
    /// Moves a date of custom records by `days`, as the passing of time would see it.
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
    /// The titles of the notifications `email` was sent about `record`.
    async fn told(&self, email: &str, record: Uuid) -> Vec<String> {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let titles: Vec<String> = sqlx::query_scalar(
            "select n.title from notification n join app_user u on u.tenant_id = n.tenant_id and u.id = n.user_id \
             where n.tenant_id = $1 and n.record_id = $2 and u.email = $3 order by n.created_at, n.id",
        )
        .bind(self.tenant)
        .bind(record)
        .bind(email)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        titles
    }
    async fn listed(&self, ctx: &Ctx, entity: &str) -> Vec<Doc> {
        self.k.list(ctx, entity, &ListQuery { limit: Some(200), ..Default::default() }).await.unwrap().rows
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
            packages: vec!["selling".into(), "studio".into()],
            settings: Some(json!({"timezone": "UTC"})),
        })
        .await
        .expect("workspace");
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    W { k: k.clone(), tenant, user: session.user, slug: slug.into() }
}

/// The workspace built by configuration, exported (without the moment it was made).
async fn build(k: &Arc<Kernel>) -> Value {
    let src = workspace(k, "government-source").await;
    let admin = src.admin().await;
    let mut ops: Vec<NewCustomization> = vec![];
    let mut defined = std::collections::HashSet::new();
    for e in entities() {
        let name = e["name"].as_str().unwrap().to_string();
        let op = if defined.insert(name.clone()) { "define_entity" } else { "redefine_entity" };
        ops.push(serde_json::from_value(json!({"entity": name, "op": {"op": op, "entity": e}})).unwrap());
    }
    for (entity, name, check, says) in checks() {
        ops.push(serde_json::from_value(json!({"entity": entity, "op": {"op": "add_rule", "rule": {"name": name, "check": check, "message": says}}})).unwrap());
    }
    for (entity, action) in actions() {
        ops.push(serde_json::from_value(json!({"entity": entity, "op": {"op": "add_action", "action": action}})).unwrap());
    }
    ops.push(serde_json::from_value(json!({"entity": "x_government", "op": {"op": "define_module", "module": menu()}})).unwrap());
    ops.push(serde_json::from_value(json!({"entity": "XPermit", "op": {"op": "publish_list", "list": register()}})).unwrap());
    k.batch_customizations(&admin, ops, false).await.unwrap_or_else(|(at, e)| panic!("customization {at:?}: {}", message(&e)));
    let admin = src.admin().await;
    for (role, rule) in roles() {
        let rule: PermissionDef = serde_json::from_value(rule).unwrap();
        k.set_role_permission(&admin, role, &rule).await.unwrap_or_else(|e| panic!("{role}: {}", message(&e)));
    }
    for (i, rule) in portal_rules().into_iter().enumerate() {
        let rule: PermissionDef = serde_json::from_value(rule).unwrap();
        k.set_role_permission(&admin, "Applicant", &rule).await.unwrap_or_else(|e| panic!("Applicant: {}", message(&e)));
        if i == 0 {
            k.set_role_portal(&admin, "Applicant", true).await.expect("a portal role");
        }
    }
    for a in automations() {
        automation::save(k, &admin, &obj(a)).await.unwrap_or_else(|e| panic!("automation: {}", message(&e)));
    }
    src.insert(&admin, "Space", space()).await;
    for (language, words) in words() {
        for (english, said) in words.as_object().unwrap() {
            src.insert(&admin, "InterfaceWord", json!({"language": language, "english": english, "words": said})).await;
        }
    }
    let mut pack = k
        .export_pack(
            &src.admin().await,
            &ExportOptions { name: "government".into(), label: "Government".into(), version: 1, records: vec!["InterfaceWord".into()] },
        )
        .await
        .expect("export");
    pack.as_object_mut().unwrap().remove("exported_at");
    pack
}

#[test]
fn government_as_a_pack() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_STUDIO_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_studio_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 12, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("platform packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    packages.extend(ferrum_studio::packages().expect("studio packages"));
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    ferrum_studio::register(&mut hooks);
    let k: Arc<Kernel> = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("partitions");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");

    // ───────────── the committed pack is what the builder produces ─────────────
    let built = build(&k).await;
    let text = serde_json::to_string_pretty(&built).unwrap() + "\n";
    if std::env::var("FERRUM_WRITE_PACKS").is_ok_and(|v| v == "1") {
        std::fs::write(pack_path(), &text).expect("write the pack");
    }
    let committed = std::fs::read_to_string(pack_path()).expect("packs/government/government.pack.json");
    assert!(
        committed == text,
        "packs/government/government.pack.json is not what the builder in this test produces: run it with FERRUM_WRITE_PACKS=1 and commit the file"
    );
    let pack: Value = serde_json::from_str(&committed).unwrap();
    let defined = texts(&pack);
    for (language, words) in words() {
        let stray: Vec<&String> = words.as_object().unwrap().keys().filter(|e| !defined.contains(*e)).collect();
        assert!(stray.is_empty(), "{language}: words for texts the pack does not define: {stray:?}");
    }
    // The pack's size, as the findings give it.
    let longest = pack["customizations"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| {
            let mut found = vec![];
            fn walk(v: &Value, out: &mut Vec<usize>) {
                match v {
                    Value::String(s) => out.push(s.len()),
                    Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
                    Value::Object(o) => o.iter().filter(|(k, _)| k.as_str() != "instructions" && k.as_str() != "description").for_each(|(_, x)| walk(x, out)),
                    _ => {}
                }
            }
            walk(&c["op"], &mut found);
            found
        })
        .max()
        .unwrap();
    println!(
        "pack: {} customizations, {} roles, {} automations, {} studio items, longest expression {longest} characters, {} texts, {} KB",
        pack["customizations"].as_array().unwrap().len(),
        pack["roles"].as_array().unwrap().len(),
        pack["automations"].as_array().unwrap().len(),
        pack["studio"].as_array().unwrap().len(),
        defined.len(),
        committed.len() / 1024
    );

    // ───────────── installed in a fresh workspace, from the committed file ─────────────
    let t = workspace(&k, "town").await;
    let admin = t.admin().await;
    let plan = k.plan_pack(&admin, &pack).await.expect("plan");
    assert_eq!(plan["can_install"], json!(true), "{plan}");
    println!("plan warnings: {}", plan["warnings"]);
    let done = k.install_pack(&admin, &pack, plan["digest"].as_str().unwrap()).await.expect("install");
    assert_eq!(done["failed"], json!([]), "{done}");
    let k = &t.k;
    let a = t.admin().await;
    let today = Utc::now().date_naive();
    let mut clock = Utc::now();
    macro_rules! day {
        () => {{
            clock += Duration::days(1);
            while k.scan_schedules_at(clock, ferrum_kernel::schedule::PAGE).await.unwrap() {}
            while k.dispatch_outbox_once().await.unwrap_or(0) > 0 {}
        }};
    }

    // ───────────── the books, the permits and the categories ─────────────
    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let company = id_of(&t.insert(&a, "Company", json!({"company_name": "Riverton Town Hall", "abbr": "RT", "default_currency": inr["id"]})).await);
    let services = t.find_one("ItemGroup", json!({"group_name": "Services"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let fee_item = id_of(
        &t.insert(
            &a,
            "Item",
            json!({"item_code": "PERMIT-FEE", "item_name": "Permit fee", "item_group": services["id"], "stock_uom": nos["id"], "is_stock_item": false}),
        )
        .await,
    );
    let manager = t.user("manager@town.test", &["Services Manager"]).await;
    let officer = t.user("officer@town.test", &["Permit Officer"]).await;
    let caseworker = t.user("cases@town.test", &["Case Officer"]).await;
    // DIFFERENT (G8): there are no Government Settings: the company that invoices and the fee's item are each
    // permit type's (settings by configuration are missing).
    let books = json!({"company": company.to_string(), "fee_item": fee_item.to_string()});
    let with_books = |v: Value| {
        let mut v = v;
        v.as_object_mut().unwrap().extend(books.as_object().unwrap().clone());
        v
    };
    let building = id_of(
        &t.insert(
            &manager,
            "XPermitType",
            with_books(json!({"type_name": "Building permit", "fee": "2500", "valid_months": 24, "documents": [{"document": "Site plan"}, {"document": "Proof of ownership"}]})),
        )
        .await,
    );
    let party = id_of(&t.insert(&manager, "XPermitType", json!({"type_name": "Street party", "fee": "0", "valid_months": 1})).await);
    let stall =
        id_of(&t.insert(&manager, "XPermitType", with_books(json!({"type_name": "Market stall", "fee": "50", "valid_months": 3, "active": false}))).await);
    let signage = id_of(&t.insert(&manager, "XPermitType", with_books(json!({"type_name": "Signage", "fee": "100", "valid_months": 12}))).await);
    let potholes = id_of(&t.insert(&manager, "XCaseCategory", json!({"category_name": "Potholes", "days_to_resolve": 5})).await);
    let noise = id_of(&t.insert(&manager, "XCaseCategory", json!({"category_name": "Noise", "days_to_resolve": 3})).await);

    // ───────────── a citizen applies on the portal ─────────────
    let ria = t.citizen("ria@home.test", "Ria Sen").await;
    let sam = t.citizen("sam@home.test", "Sam Lee").await;
    // SAME: the closed market stall is not offered (the portal role reads active types only).
    let open: Vec<String> = t.listed(&ria, "XPermitType").await.iter().map(|x| x["type_name"].as_str().unwrap().to_string()).collect();
    assert_eq!(open, ["Building permit", "Signage", "Street party"], "the closed market stall is not offered");
    // DIFFERENT: she types her name; the portal's form does not fill it from her account (an expression knows the
    // person's email and roles, not their name). Her address is filled, read from her sign-in.
    let draft =
        t.insert(&ria, "XPermitApplication", json!({"permit_type": building.to_string(), "applicant_name": "Ria Sen", "details": "A rear extension"})).await;
    let app = id_of(&draft);
    assert_eq!((draft["status"].as_str(), draft["applicant_email"].as_str()), (Some("Draft"), Some("ria@home.test")));
    // DIFFERENT: refused, but not saying which documents are still needed (a check's message is fixed words).
    refused(t.act(&ria, "XPermitApplication", app, "submit_with_fee", json!({})).await, "invalid", "still needed");
    // NOT EXERCISED: the upload itself (base64, size). The portal's form uploads a file through the platform's
    // files; the harness writes the stored file's address into the row.
    t.set(&ria, "XPermitApplication", app, json!({"documents": [{"document": "Site plan", "file": "/files/plan"}]})).await.expect("a document");
    // SAME: Sam sees nothing of Ria's, and changes nothing.
    refused(t.set(&sam, "XPermitApplication", app, json!({"details": "not mine"})).await, "not found", "");
    refused(t.act(&sam, "XPermitApplication", app, "submit_with_fee", json!({})).await, "not found", "");
    refused(t.act(&ria, "XPermitApplication", app, "submit_with_fee", json!({})).await, "invalid", "still needed");
    // DIFFERENT: sent again, a document takes the place of the first only because she edits its row (the app's
    // upload replaced the row of the same name).
    let rows = t.get("XPermitApplication", app).await["documents"].clone();
    t.set(
        &ria,
        "XPermitApplication",
        app,
        json!({"documents": [{"id": rows[0]["id"], "document": "Site plan", "file": "/files/plan-revised"}, {"document": "Proof of ownership", "file": "/files/deed"}]}),
    )
    .await
    .unwrap();
    assert_eq!(t.get("XPermitApplication", app).await["documents"].as_array().unwrap().len(), 2);
    // SAME (G7, G8, G9): submitted; its fee invoiced in ERP as a draft through ERP's own step, to a customer made
    // from the applicant.
    let submitted = t.act(&ria, "XPermitApplication", app, "submit_with_fee", json!({})).await.expect("submitted");
    assert_eq!(submitted["status"], "Submitted", "{submitted}");
    let stored = t.get("XPermitApplication", app).await;
    assert_eq!(stored["submitted_on"], json!(today.to_string()));
    let fee = t.get("SalesInvoice", uid(&stored["fee_invoice"])).await;
    assert_eq!((dec(&fee["grand_total"]), fee["docstatus"].as_i64()), (2500.0, Some(0)), "{fee:?}");
    let customer = t.get("Customer", uid(&fee["customer"])).await;
    assert_eq!((customer["customer_name"].as_str(), customer["email"].as_str()), (Some("Ria Sen"), Some("ria@home.test")), "made from the applicant");
    // SAME (G4): what was applied for stays.
    refused(
        t.set(&ria, "XPermitApplication", app, json!({"documents": [{"document": "Site plan", "file": "/files/late"}]})).await,
        "invalid",
        "keeps what was applied for",
    );
    // DIFFERENT (G9): her next fee is invoiced to the same customer, because the customer is made once per address
    // (the effect's key); a customer ERP already had under her address is not found, and an applicant who gives
    // another name under the same address is refused (the key's input changed).
    let sign = id_of(&t.insert(&ria, "XPermitApplication", json!({"permit_type": signage.to_string(), "applicant_name": "Ria Sen"})).await);
    t.act(&ria, "XPermitApplication", sign, "submit_with_fee", json!({})).await.expect("the second");
    let sign_fee = t.get("XPermitApplication", sign).await["fee_invoice"].clone();
    assert_eq!(t.get("SalesInvoice", uid(&sign_fee)).await["customer"], fee["customer"], "one customer a citizen");
    // SAME (G5): a closed type takes no applications.
    let closed = id_of(&t.insert(&officer, "XPermitApplication", json!({"permit_type": stall.to_string(), "applicant_name": "Walk-in"})).await);
    refused(t.act(&officer, "XPermitApplication", closed, "submit_with_fee", json!({})).await, "invalid", "not open for applications");
    // SAME (G6): a document named without its file is still needed.
    let counter = id_of(
        &t.insert(
            &officer,
            "XPermitApplication",
            json!({"permit_type": building.to_string(), "applicant_name": "Counter Visitor",
                   "documents": [{"document": "Site plan"}, {"document": "Proof of ownership", "file": "/files/deed-on-paper"}]}),
        )
        .await,
    );
    refused(t.act(&officer, "XPermitApplication", counter, "submit_with_fee", json!({})).await, "invalid", "still needed");

    // ───────────── review, the fee, and the permit ─────────────
    // SAME: an officer cannot change what was applied for.
    refused(t.set(&officer, "XPermitApplication", app, json!({"details": "Two storeys"})).await, "invalid", "keeps what was applied for");
    // SAME (G11).
    t.act(&officer, "XPermitApplication", app, "take_up", json!({})).await.expect("taken up");
    assert_eq!(t.get("XPermitApplication", app).await["officer"], "officer@town.test");
    // SAME (G10): approval waits for the fee, issued and paid in ERP.
    refused(t.act(&officer, "XPermitApplication", app, "approve", json!({})).await, "invalid", "the fee is not paid yet");
    let fee_id = id_of(&fee);
    k.submit(&a, "SalesInvoice", fee_id, None).await.expect("the fee invoice submitted");
    refused(t.act(&officer, "XPermitApplication", app, "approve", json!({})).await, "invalid", "the fee is not paid yet");
    let payment = k.run_action(&a, "SalesInvoice", fee_id, "make_payment", Value::Null).await.expect("a payment");
    k.submit(&a, "PaymentEntry", uid(&payment["id"]), None).await.expect("paid");
    t.act(&officer, "XPermitApplication", app, "approve", json!({"note": "Meets the plan"})).await.expect("approved once paid");
    // SAME (G22, G23): issued as a permit valid for its type's months.
    t.act(&officer, "XPermitApplication", app, "issue", json!({})).await.expect("issued");
    let issued = t.get("XPermitApplication", app).await;
    assert_eq!(issued["status"], "Issued");
    let permit_id = uid(&issued["permit"]);
    let permit = t.get("XPermit", permit_id).await;
    let expires = today.checked_add_months(chrono::Months::new(24)).unwrap().to_string();
    assert_eq!(
        (permit["status"].as_str(), permit["expires_on"].as_str(), permit["holder_email"].as_str()),
        (Some("Valid"), Some(expires.as_str()), Some("ria@home.test"))
    );
    let year = today.format("%Y").to_string();
    let series = |n: &Value, prefix: &str| n.as_str().is_some_and(|n| n.len() == prefix.len() + 11 && n.starts_with(&format!("{prefix}-{year}-")));
    assert!(series(&permit["number"], "PRM") && series(&issued["number"], "APP"), "the app's series: {} {}", permit["number"], issued["number"]);
    // SAME (G3, G23): what the app keeps read-only stays as it is, whatever an officer sends.
    t.set(&officer, "XPermit", permit_id, json!({"holder_name": "Someone else", "expires_on": "2099-01-01"})).await.unwrap();
    t.set(&officer, "XPermitApplication", app, json!({"status": "Approved", "officer": "me@town.test"})).await.unwrap();
    let (pp, aa) = (t.get("XPermit", permit_id).await, t.get("XPermitApplication", app).await);
    assert_eq!((pp["holder_name"].as_str(), pp["expires_on"].as_str(), aa["status"].as_str()), (Some("Ria Sen"), Some(expires.as_str()), Some("Issued")));
    // SAME (G24): a revocation says why.
    refused(t.act(&officer, "XPermit", permit_id, "revoke", json!({})).await, "invalid", "says why");

    // ───────────── a rejection, an appeal, a permit that expires, a withdrawal ─────────────
    let apply = |who: &Ctx, kind: Uuid, name: &str| {
        let (who, name) = (who.clone(), name.to_string());
        let t = &t;
        async move { id_of(&t.insert(&who, "XPermitApplication", json!({"permit_type": kind.to_string(), "applicant_name": name})).await) }
    };
    let p1 = apply(&ria, party, "Ria Sen").await;
    t.act(&ria, "XPermitApplication", p1, "submit_application", json!({})).await.unwrap();
    assert!(t.get("XPermitApplication", p1).await["fee_invoice"].is_null(), "no fee, no invoice");
    t.act(&officer, "XPermitApplication", p1, "take_up", json!({})).await.unwrap();
    // SAME (G18).
    refused(t.act(&officer, "XPermitApplication", p1, "reject", json!({})).await, "invalid", "says why");
    t.act(&officer, "XPermitApplication", p1, "reject", json!({"note": "The road is a bus route"})).await.unwrap();
    // SAME (G19): appealed from the portal, once, within the type's thirty days.
    assert_eq!(t.get("XPermitApplication", p1).await["appeal_until"], json!((today + Duration::days(30)).to_string()), "thirty days");
    refused(t.act(&ria, "XPermitApplication", p1, "appeal", json!({})).await, "invalid", "says why");
    let appealed = t.act(&ria, "XPermitApplication", p1, "appeal", json!({"grounds": "Buses are diverted that day"})).await.unwrap();
    assert_eq!(appealed["status"], "Appealed");
    // SAME (G20): whoever took part in the decision does not decide the appeal.
    refused(
        t.act(&officer, "XPermitApplication", p1, "decide_appeal", json!({"outcome": "Overturned", "note": "Fine"})).await,
        "forbidden",
        "no part in the decision",
    );
    let appeals = t.user("appeals@town.test", &["Permit Officer"]).await;
    t.act(&appeals, "XPermitApplication", p1, "decide_appeal", json!({"outcome": "Upheld", "note": "The diversion is not confirmed"})).await.unwrap();
    let upheld = t.get("XPermitApplication", p1).await;
    assert_eq!(
        (upheld["status"].as_str(), upheld["appeal_outcome"].as_str(), upheld["appeal_decided_by"].as_str(), upheld["decided_by"].as_str()),
        (Some("Rejected"), Some("Upheld"), Some("appeals@town.test"), Some("officer@town.test"))
    );
    refused(t.act(&appeals, "XPermitApplication", p1, "appeal", json!({"grounds": "Again"})).await, "invalid", "not available");
    // SAME (G21): overturned on appeal, it is approved as an approval would be, then issued.
    let p4 = apply(&ria, party, "Ria Sen").await;
    t.act(&ria, "XPermitApplication", p4, "submit_application", json!({})).await.unwrap();
    t.act(&officer, "XPermitApplication", p4, "take_up", json!({})).await.unwrap();
    t.act(&officer, "XPermitApplication", p4, "reject", json!({"note": "Too late to plan"})).await.unwrap();
    t.act(&ria, "XPermitApplication", p4, "appeal", json!({"grounds": "It was filed a month ahead"})).await.unwrap();
    t.act(&appeals, "XPermitApplication", p4, "decide_appeal", json!({"outcome": "Overturned", "note": "It was in time", "conditions": "Ends by 20:00"}))
        .await
        .unwrap();
    let overturned = t.get("XPermitApplication", p4).await;
    assert_eq!(
        (overturned["status"].as_str(), overturned["conditions"].as_str(), overturned["decided_by"].as_str()),
        (Some("Approved"), Some("Ends by 20:00"), Some("appeals@town.test"))
    );
    t.act(&officer, "XPermitApplication", p4, "issue", json!({})).await.unwrap();
    // SAME: a type that takes no appeals says so.
    t.set(&manager, "XPermitType", party, json!({"appeal_within_days": 0})).await.unwrap();
    let p5 = apply(&ria, party, "Ria Sen").await;
    t.act(&ria, "XPermitApplication", p5, "submit_application", json!({})).await.unwrap();
    t.act(&officer, "XPermitApplication", p5, "take_up", json!({})).await.unwrap();
    t.act(&officer, "XPermitApplication", p5, "reject", json!({"note": "Not this street"})).await.unwrap();
    refused(t.act(&ria, "XPermitApplication", p5, "appeal", json!({"grounds": "Please"})).await, "invalid", "may not be appealed");
    t.set(&manager, "XPermitType", party, json!({"appeal_within_days": 30})).await.unwrap();
    let p2 = apply(&ria, party, "Ria Sen").await;
    t.act(&ria, "XPermitApplication", p2, "submit_application", json!({})).await.unwrap();
    t.act(&officer, "XPermitApplication", p2, "take_up", json!({})).await.unwrap();
    t.act(&officer, "XPermitApplication", p2, "approve", json!({})).await.unwrap();
    t.act(&officer, "XPermitApplication", p2, "issue", json!({})).await.unwrap();
    let short = uid(&t.get("XPermitApplication", p2).await["permit"]);
    // SAME: withdrawn from the portal.
    let p3 = apply(&ria, party, "Ria Sen").await;
    t.act(&ria, "XPermitApplication", p3, "withdraw", json!({})).await.unwrap();
    assert_eq!(t.get("XPermitApplication", p3).await["status"], "Withdrawn");
    // ───────────── beyond the app's test: the customer, the appeal's window, the fee on appeal, renewing early ─────────────
    let ana = t.citizen("ana@home.test", "Ana Roy").await;
    // DIFFERENT (G9): a customer ERP already has under her address is not found: the effect key makes one for the
    // pack's applicants, once per address.
    t.insert(&a, "Customer", json!({"customer_name": "Ana Roy", "customer_type": "Individual", "email": "ana@home.test"})).await;
    let a1 = apply(&ana, signage, "Ana Roy").await;
    t.act(&ana, "XPermitApplication", a1, "submit_with_fee", json!({})).await.unwrap();
    let hers = k
        .list(&a, "Customer", &ListQuery { filters: ferrum_kernel::query::parse_filters(&json!({"email": "ana@home.test"})).unwrap(), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(hers.rows.len(), 2, "a second customer under her address");
    // DIFFERENT (G9): another name under the same address is refused: the key was used with other input.
    let a2 = apply(&ana, signage, "Ana R. Roy").await;
    let e = t.act(&ana, "XPermitApplication", a2, "submit_with_fee", json!({})).await.unwrap_err();
    assert!(message(&e).contains("was used for parties.customer.create with other input"), "{e}");
    println!("another name under the same address: {} ({e})", e.code());
    // SAME (G21): overturned on appeal, the fee is checked as for an approval.
    t.act(&officer, "XPermitApplication", a1, "take_up", json!({})).await.unwrap();
    t.act(&officer, "XPermitApplication", a1, "reject", json!({"note": "Too large"})).await.unwrap();
    t.act(&ana, "XPermitApplication", a1, "appeal", json!({"grounds": "It is the usual size"})).await.unwrap();
    refused(
        t.act(&appeals, "XPermitApplication", a1, "decide_appeal", json!({"outcome": "Overturned", "note": "It is"})).await,
        "invalid",
        "the fee is not paid yet",
    );
    // SAME (G19): past the type's days, a rejection is not appealed; the record shows until when.
    let a3 = apply(&ana, party, "Ana Roy").await;
    t.act(&ana, "XPermitApplication", a3, "submit_application", json!({})).await.unwrap();
    t.act(&officer, "XPermitApplication", a3, "take_up", json!({})).await.unwrap();
    t.act(&officer, "XPermitApplication", a3, "reject", json!({"note": "Not this street"})).await.unwrap();
    t.age(&[a3], "appeal_until", -31).await;
    refused(t.act(&ana, "XPermitApplication", a3, "appeal", json!({"grounds": "Late"})).await, "invalid", "the days to appeal it have ended");
    // SAME (G27): a renewable permit is not renewed before its type's days before it ends; the permit shows from when.
    let cafe = id_of(
        &t.insert(&manager, "XPermitType", json!({"type_name": "Cafe licence", "fee": "0", "valid_months": 24, "renewable": true, "renew_within_days": 60}))
            .await,
    );
    let a4 = apply(&ana, cafe, "Ana Roy").await;
    t.act(&ana, "XPermitApplication", a4, "submit_application", json!({})).await.unwrap();
    t.act(&officer, "XPermitApplication", a4, "take_up", json!({})).await.unwrap();
    t.act(&officer, "XPermitApplication", a4, "approve", json!({})).await.unwrap();
    t.act(&officer, "XPermitApplication", a4, "issue", json!({})).await.unwrap();
    let licence = uid(&t.get("XPermitApplication", a4).await["permit"]);
    refused(t.act(&ana, "XPermit", licence, "renew", json!({})).await, "invalid", "it may be renewed from its type's days before it ends");
    let lp = t.get("XPermit", licence).await;
    let ends = chrono::NaiveDate::parse_from_str(lp["expires_on"].as_str().unwrap(), "%Y-%m-%d").unwrap();
    assert_eq!(lp["renew_from"], json!((ends - Duration::days(60)).to_string()));

    // SAME (G25): a month's permit, forty days on, is expired by the midnight run; the building permit runs two years.
    t.age(&[short], "issued_on", -40).await;
    day!();
    assert_eq!(t.get("XPermit", short).await["status"], "Expired", "a month's permit, forty days on");
    assert_eq!(t.get("XPermit", permit_id).await["status"], "Valid", "the building permit runs two years");
    t.act(&officer, "XPermit", permit_id, "revoke", json!({"reason": "Built beyond the plan"})).await.unwrap();
    let revoked = t.get("XPermit", permit_id).await;
    assert_eq!((revoked["status"].as_str(), revoked["revoked_on"].as_str()), (Some("Revoked"), Some(today.to_string().as_str())));

    // ───────────── reviewed in stages, issued on conditions, told, registered, renewed ─────────────
    // The stages' roles are the workspace's own.
    let rule = |p: Value| serde_json::from_value::<PermissionDef>(p).unwrap();
    for role in ["Planning Officer", "Inspector", "Fire Officer", "Health Officer"] {
        k.set_role_permission(&a, role, &rule(json!({"entity": "XPermitApplication", "read": true}))).await.expect("a role of the workspace's");
    }
    let planner = t.user("planner@town.test", &["Permit Officer", "Planning Officer", "Inspector"]).await;
    let inspector = t.user("inspector@town.test", &["Permit Officer", "Inspector"]).await;
    let kim = t.citizen("kim@home.test", "Kim Roy").await;
    let kiosk = id_of(
        &t.insert(
            &manager,
            "XPermitType",
            json!({"type_name": "Kiosk", "fee": "0", "valid_months": 1, "renewable": true, "renew_within_days": 60, "in_register": true,
                   "register_holder": "Published", "register_conditions": "Published",
                   "stages": [{"stage": "Planning review", "role": "Planning Officer"}, {"stage": "Site visit", "role": "Inspector", "inspection": true}]}),
        )
        .await,
    );
    let steps: Vec<i64> = t.get("XPermitType", kiosk).await["stages"].as_array().unwrap().iter().map(|s| s["step"].as_i64().unwrap()).collect();
    assert_eq!(steps, [1, 2], "each stage its own step");
    let k1 =
        id_of(&t.insert(&kim, "XPermitApplication", json!({"permit_type": kiosk.to_string(), "applicant_name": "Kim Roy", "details": "A coffee kiosk"})).await);
    t.act(&kim, "XPermitApplication", k1, "submit_application", json!({})).await.unwrap();
    t.act(&officer, "XPermitApplication", k1, "take_up", json!({})).await.unwrap();
    // SAME (G12): it waits for the first stage, and approval for every stage.
    assert_eq!(t.get("XPermitApplication", k1).await["waiting_for"], "Planning review");
    refused(t.act(&officer, "XPermitApplication", k1, "approve", json!({})).await, "invalid", "every review stage is passed first");
    // SAME (G13), in fixed words: each stage is passed by the role its row names (the app named the stage and role).
    refused(t.act(&inspector, "XPermitApplication", k1, "pass_stage", json!({})).await, "forbidden", "holding the role its row names");
    t.act(&planner, "XPermitApplication", k1, "pass_stage", json!({"note": "Fits the plan"})).await.expect("planning passed");
    assert_eq!(t.get("XPermitApplication", k1).await["waiting_for"], "Site visit");
    // SAME (G14): not by whoever passed the stage before.
    refused(t.act(&planner, "XPermitApplication", k1, "pass_stage", json!({"findings": "Fine"})).await, "forbidden", "someone other than");
    // SAME (G15): a site inspection says what it found.
    refused(t.act(&inspector, "XPermitApplication", k1, "pass_stage", json!({})).await, "invalid", "says what it found");
    t.act(&inspector, "XPermitApplication", k1, "pass_stage", json!({"findings": "Clear pavement, 2 m from the kerb"})).await.expect("visited");
    // SAME (G16): each pass appends a review row.
    let reviewed = t.get("XPermitApplication", k1).await;
    let reviews: Vec<(String, String, String)> = reviewed["reviews"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["stage"].as_str().unwrap().into(), r["outcome"].as_str().unwrap().into(), r["reviewer"].as_str().unwrap().into()))
        .collect();
    assert_eq!(
        reviews,
        [("Planning review", "Passed", "planner@town.test"), ("Site visit", "Passed", "inspector@town.test")]
            .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
            .to_vec()
    );
    assert_eq!(reviewed["reviews"][1]["findings"], "Clear pavement, 2 m from the kerb");
    assert!(reviewed["waiting_for"].is_null(), "every stage passed");
    // SAME (G17): approved on conditions, with who and when.
    t.act(&officer, "XPermitApplication", k1, "approve", json!({"conditions": "Closed by 22:00"})).await.expect("approved");
    let approved = t.get("XPermitApplication", k1).await;
    assert_eq!((approved["decided_by"].as_str(), approved["decided_on"].as_str()), (Some("officer@town.test"), Some(today.to_string().as_str())));
    t.act(&officer, "XPermitApplication", k1, "issue", json!({})).await.unwrap();
    let kiosk_permit = uid(&t.get("XPermitApplication", k1).await["permit"]);
    let kp = t.get("XPermit", kiosk_permit).await;
    assert_eq!(kp["conditions"], "Closed by 22:00");
    // DIFFERENT (G28): Kim is told at each step, in the app's words, but in the workspace only (a notification of
    // the automations' kind): not by e-mail, and not as a kind she may choose how to receive.
    let k1_titles = t.told("kim@home.test", k1).await;
    assert_eq!(k1_titles.len(), 3, "{k1_titles:?}");
    assert!(k1_titles[0].ends_with("is submitted") && k1_titles[1].ends_with("is approved") && k1_titles[2].ends_with("is issued"), "{k1_titles:?}");
    // SAME (G33), read without signing in: the register holds the kiosk's permit, with its holder and conditions;
    // the types not on it are not.
    let register = || async { k.published_list(&t.slug, "permits").await.unwrap().expect("the register") };
    let listed = register().await;
    assert_eq!(listed.rows.len(), 1, "{:?}", listed.json());
    let row = &listed.rows[0];
    assert_eq!(
        (row["register_holder"].as_str(), row["register_conditions"].as_str(), row["permit_type"].as_str()),
        (Some("Kim Roy"), Some("Closed by 22:00"), Some("Kiosk"))
    );
    assert!(!listed.rows.iter().any(|r| r["permit_type"] == "Building permit"), "a type not on the register");
    // DIFFERENT: the CSV is headed by the fields' labels (the app: their names), and a register for one type
    // (`?type=kiosk`) is not offered: one list per type would be needed.
    let csv = listed.csv();
    assert!(csv.starts_with("Number,Permit Type,Holder,Issued On,Expires On,Status,Conditions\r\n") && csv.contains("Kim Roy"), "{csv}");
    // DIFFERENT (G33): what the register publishes is the type's to say, but the permits follow the type at the
    // midnight run, not at once.
    t.set(&manager, "XPermitType", kiosk, json!({"register_holder": "Left Out"})).await.unwrap();
    assert_eq!(register().await.rows[0]["register_holder"].as_str(), Some("Kim Roy"), "until the midnight run");
    day!();
    let shown = register().await;
    assert_eq!((shown.rows[0]["register_holder"].clone(), shown.rows[0]["register_conditions"].as_str()), (Value::Null, Some("Closed by 22:00")));
    // SAME: ended, a permit stays its months on it, then goes, unless the type keeps it for good (at the next run).
    t.age(&[kiosk_permit], "issued_on", -400).await;
    day!();
    assert_eq!(t.get("XPermit", kiosk_permit).await["status"], "Expired");
    assert!(register().await.rows.is_empty(), "twelve months after it ended, it is off the register");
    // DIFFERENT: "for good" is a box of its own; the app's empty months cannot say it, since an empty whole number
    // reads as 0 in an expression (emptied, the months copied to the permit read 0: off at once).
    t.set(&manager, "XPermitType", kiosk, json!({"register_keep_months": null})).await.unwrap();
    day!();
    assert!(register().await.rows.is_empty(), "emptied months are 0 months");
    t.set(&manager, "XPermitType", kiosk, json!({"register_keep_forever": true})).await.unwrap();
    day!();
    assert_eq!(register().await.rows.len(), 1, "kept for good");
    t.age(&[kiosk_permit], "issued_on", 400).await;
    t.set(&manager, "XPermitType", kiosk, json!({"register_holder": "Published", "register_keep_months": 12, "register_keep_forever": false})).await.unwrap();
    day!();
    assert_eq!((t.get("XPermit", kiosk_permit).await["status"].as_str(), register().await.rows.len()), (Some("Valid"), 1));
    // CANNOT (not a behaviour of the paper's list): the app's backfill kept types made before the register's flags
    // publishing as before. A pack has no history to carry; nothing to restate.

    // SAME (G26): a month's permit renews from sixty days before its end: Kim is told once (the type's days, read
    // from the type), and sees from when.
    day!();
    day!();
    let reminders: Vec<String> = t.told("kim@home.test", kiosk_permit).await.into_iter().filter(|s| s.contains("may be renewed")).collect();
    assert_eq!(reminders.len(), 1, "told once that it may be renewed: {reminders:?}");
    let kp = t.get("XPermit", kiosk_permit).await;
    let ends = chrono::NaiveDate::parse_from_str(kp["expires_on"].as_str().unwrap(), "%Y-%m-%d").unwrap();
    assert_eq!(kp["renew_from"], json!((ends - Duration::days(60)).to_string()));
    // SAME (G27): renewed from the portal, only by its holder, once while the renewal is live.
    refused(t.act(&ria, "XPermit", kiosk_permit, "renew", json!({})).await, "not found", "");
    t.act(&kim, "XPermit", kiosk_permit, "renew", json!({})).await.expect("a renewal");
    let r1 = uid(&t.get("XPermit", kiosk_permit).await["renewal"]);
    let renewal = t.get("XPermitApplication", r1).await;
    assert_eq!((renewal["status"].as_str(), renewal["renewal_of"].as_str()), (Some("Draft"), Some(kiosk_permit.to_string().as_str())));
    // DIFFERENT words: "'Renew It' is not available" (the app: "its renewal is APP-…").
    refused(t.act(&kim, "XPermit", kiosk_permit, "renew", json!({})).await, "invalid", "not available");
    t.act(&kim, "XPermitApplication", r1, "submit_application", json!({})).await.unwrap();
    t.act(&officer, "XPermitApplication", r1, "take_up", json!({})).await.unwrap();
    t.act(&planner, "XPermitApplication", r1, "pass_stage", json!({})).await.unwrap();
    t.act(&inspector, "XPermitApplication", r1, "pass_stage", json!({"findings": "As before"})).await.unwrap();
    t.act(&officer, "XPermitApplication", r1, "approve", json!({"conditions": "Closed by 22:00"})).await.unwrap();
    t.act(&officer, "XPermitApplication", r1, "issue", json!({})).await.unwrap();
    let next = t.get("XPermit", uid(&t.get("XPermitApplication", r1).await["permit"])).await;
    let day_after = ends + Duration::days(1);
    assert_eq!(
        (next["issued_on"].as_str(), next["renewal_of"].as_str()),
        (Some(day_after.to_string().as_str()), Some(kiosk_permit.to_string().as_str())),
        "it runs on from the day after the old one ends"
    );
    // SAME: a type that does not renew refuses a renewal (the app's test does not try it).
    refused(t.act(&ria, "XPermit", short, "renew", json!({})).await, "invalid", "not renewed");
    // SAME: a stage that fails rejects the application, saying why.
    let k2 = id_of(&t.insert(&kim, "XPermitApplication", json!({"permit_type": kiosk.to_string(), "applicant_name": "Kim Roy"})).await);
    t.act(&kim, "XPermitApplication", k2, "submit_application", json!({})).await.unwrap();
    t.act(&officer, "XPermitApplication", k2, "take_up", json!({})).await.unwrap();
    // DIFFERENT (configuration): Reject It is two configured actions, one shown while a stage waits (it adds the
    // failed review) and one while none does; an effect has no condition of its own.
    refused(t.act(&planner, "XPermitApplication", k2, "reject", json!({"note": "A protected street"})).await, "invalid", "not available");
    t.act(&planner, "XPermitApplication", k2, "reject_at_stage", json!({"note": "A protected street"})).await.unwrap();
    let failed = t.get("XPermitApplication", k2).await;
    assert_eq!((failed["status"].as_str(), failed["reviews"][0]["outcome"].as_str()), (Some("Rejected"), Some("Failed")));
    // DIFFERENT (G2): two stages of the same name are refused, but only when spelt alike: "Check" and "check" pass
    // (an expression cannot fold case).
    refused(
        k.insert(
            &manager,
            "XPermitType",
            obj(json!({"type_name": "Twice", "valid_months": 1, "stages": [{"stage": "Check"}, {"stage": "Check", "alongside": true}]})),
        )
        .await,
        "invalid",
        "two stages have the same name",
    );
    let twice = k
        .insert(
            &manager,
            "XPermitType",
            obj(json!({"type_name": "Twice", "valid_months": 1, "stages": [{"stage": "Check"}, {"stage": "check", "alongside": true}]})),
        )
        .await;
    assert!(twice.is_ok(), "the app refuses \"Check\" and \"check\"; the pack takes them");
    // SAME (G12, G14): stages side by side: fire safety and health together, in any order, each by its own people
    // and no one passing both; then a final review.
    let market = id_of(
        &t.insert(
            &manager,
            "XPermitType",
            json!({"type_name": "Fair Stall", "fee": "0", "valid_months": 1,
                   "stages": [{"stage": "Fire safety", "role": "Fire Officer"}, {"stage": "Health check", "role": "Health Officer", "alongside": true},
                              {"stage": "Final review", "role": "Planning Officer"}]}),
        )
        .await,
    );
    let health = t.user("health@town.test", &["Permit Officer", "Health Officer"]).await;
    let both = t.user("both@town.test", &["Permit Officer", "Fire Officer", "Health Officer"]).await;
    let lee = t.citizen("lee@home.test", "Lee Park").await;
    let mut stalls = vec![];
    for _ in 0..2 {
        let m = apply(&lee, market, "Lee Park").await;
        t.act(&lee, "XPermitApplication", m, "submit_application", json!({})).await.unwrap();
        stalls.push(m);
    }
    let (m1, m2) = (stalls[0], stalls[1]);
    t.act(&officer, "XPermitApplication", m1, "take_up", json!({})).await.unwrap();
    assert_eq!(t.get("XPermitApplication", m1).await["waiting_for"], "Fire safety, Health check", "waiting for both at once");
    refused(t.act(&officer, "XPermitApplication", m1, "approve", json!({})).await, "invalid", "every review stage is passed first");
    t.act(&health, "XPermitApplication", m1, "pass_stage", json!({"note": "Clean"})).await.expect("health passed");
    assert_eq!(t.get("XPermitApplication", m1).await["waiting_for"], "Fire safety");
    refused(t.act(&health, "XPermitApplication", m1, "pass_stage", json!({})).await, "forbidden", "holding the role its row names");
    refused(t.act(&both, "XPermitApplication", m1, "pass_stage", json!({"which": "Health check"})).await, "invalid", "is not one it waits for");
    t.act(&both, "XPermitApplication", m1, "pass_stage", json!({})).await.expect("fire passed");
    assert_eq!(t.get("XPermitApplication", m1).await["waiting_for"], "Final review", "the group done, the next stage");
    t.act(&officer, "XPermitApplication", m2, "take_up", json!({})).await.unwrap();
    t.act(&both, "XPermitApplication", m2, "pass_stage", json!({"which": "Health check"})).await.expect("one of the two");
    refused(t.act(&both, "XPermitApplication", m2, "pass_stage", json!({})).await, "forbidden", "someone other than whoever passed a stage beside it");
    t.act(&officer, "XPermitApplication", m2, "reject_at_stage", json!({"note": "No stalls this season"})).await.unwrap();
    let outcomes: Vec<(String, String)> = t.get("XPermitApplication", m2).await["reviews"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["stage"].as_str().unwrap().into(), r["outcome"].as_str().unwrap().into()))
        .collect();
    assert_eq!(outcomes, [("Health check", "Passed"), ("Fire safety", "Failed")].map(|(a, b)| (a.to_string(), b.to_string())).to_vec());

    // ───────────── cases ─────────────
    // SAME (G29): due in the category's days; resolved, it says how and keeps the day. DIFFERENT (portal): the
    // citizen types her name.
    let raised = t.insert(&ria, "XCase", json!({"category": potholes.to_string(), "subject": "Pothole on Mill Lane", "citizen_name": "Ria Sen"})).await;
    assert_eq!((raised["due_on"].as_str(), raised["citizen_email"].as_str()), (Some((today + Duration::days(5)).to_string().as_str()), Some("ria@home.test")));
    let pothole = id_of(&raised);
    refused(t.set(&caseworker, "XCase", pothole, json!({"status": "Resolved"})).await, "invalid", "says how");
    t.set(&caseworker, "XCase", pothole, json!({"status": "Resolved", "resolution": "Filled on Tuesday"})).await.unwrap();
    assert_eq!(t.get("XCase", pothole).await["resolved_on"], json!(today.to_string()));
    t.set(&caseworker, "XCase", pothole, json!({"description": "Mill Lane, by the bridge"})).await.unwrap();
    assert_eq!(t.get("XCase", pothole).await["resolved_on"], json!(today.to_string()), "keeps its day");
    let loud = id_of(
        &t.insert(
            &caseworker,
            "XCase",
            json!({"category": noise.to_string(), "subject": "Late music", "citizen_name": "Sam Lee", "citizen_email": "sam@home.test"}),
        )
        .await,
    );
    assert_eq!(t.get("XCase", loud).await["due_on"], json!((today + Duration::days(3)).to_string()));
    // SAME (G30): open past its day, it is overdue at the midnight run (the harness ages it ten days).
    t.age(&[loud, pothole], "opened_on", -10).await;
    day!();
    assert_eq!(t.get("XCase", loud).await["overdue"], true, "open past its day");
    assert_eq!(t.get("XCase", pothole).await["overdue"], false, "resolved in time");
    let old_case = id_of(&t.insert(&caseworker, "XCase", json!({"category": noise.to_string(), "subject": "Old complaint", "citizen_name": "Sam Lee"})).await);
    t.age(&[old_case], "opened_on", -20).await;
    t.set(&caseworker, "XCase", old_case, json!({"status": "In Progress"})).await.unwrap();
    assert_eq!(t.get("XCase", old_case).await["overdue"], true, "open, twenty days on");
    t.set(&caseworker, "XCase", old_case, json!({"status": "Resolved", "resolution": "Spoke to the neighbours"})).await.unwrap();
    assert_eq!(t.get("XCase", old_case).await["overdue"], false, "resolved, it is not overdue");

    // ───────────── what a citizen sees ─────────────
    // SAME (G34's reach): her applications, permits and cases, and nothing of anyone else's.
    assert_eq!(t.listed(&ria, "XPermitApplication").await.len(), 7, "with the two appealed");
    assert_eq!(t.listed(&ria, "XPermit").await.len(), 3, "with the one approved on appeal");
    let cases: Vec<String> = t.listed(&ria, "XCase").await.iter().map(|c| c["subject"].as_str().unwrap().to_string()).collect();
    assert_eq!(cases, ["Pothole on Mill Lane"]);
    let rejected = t.listed(&ria, "XPermitApplication").await.into_iter().find(|x| x["id"] == json!(p1.to_string())).unwrap();
    assert_eq!((rejected["status"].as_str(), rejected["appeal_outcome"].as_str()), (Some("Rejected"), Some("Upheld")));
    assert_eq!(rejected["decision_note"], "The road is a bus route", "the reason reaches the citizen");
    assert!(t.listed(&sam, "XPermitApplication").await.is_empty() && t.listed(&sam, "XPermit").await.is_empty());
    assert_eq!(t.listed(&sam, "XCase").await.len(), 1, "the case raised for him at the counter");
    // DIFFERENT (G34): her page is Studio's records page of the menu marked for the portal, not "Your services".
    let req = SiteRequest {
        tenant: t.tenant,
        user: Some(ria.clone()),
        method: "GET".into(),
        path: "/portal/entities".into(),
        query: Default::default(),
        headers: Default::default(),
        body: vec![],
        client_ip: "203.0.113.9".into(),
    };
    let page: Value = serde_json::from_slice(&ferrum_studio::site::StudioSite.handle(k, req).await.expect("the portal page").body).unwrap();
    let menu = page["menus"].as_array().unwrap().iter().find(|m| m["name"] == "x_government").unwrap_or_else(|| panic!("the portal menu: {page}"));
    // Her own applications (with Create), permits and cases (with Create); the types and categories she picks from
    // are reference data, not listed.
    let items: Vec<(&str, bool)> = menu["items"].as_array().unwrap().iter().map(|i| (i["name"].as_str().unwrap(), i["create"] == true)).collect();
    assert_eq!(items, [("XPermitApplication", true), ("XPermit", false), ("XCase", true)], "{menu}");
    // A portal person runs only the buttons that are hers: an officer's is refused.
    let ria_app = apply(&ria, party, "Ria Sen").await;
    t.act(&ria, "XPermitApplication", ria_app, "submit_application", json!({})).await.unwrap();
    refused(t.act(&ria, "XPermitApplication", ria_app, "take_up", json!({})).await, "forbidden", "is for Permit Officer");

    // ───────────── who may do what ─────────────
    // SAME (G31).
    refused(t.act(&caseworker, "XPermitApplication", p2, "issue", json!({})).await, "forbidden", "");
    refused(k.insert(&officer, "XPermitType", obj(json!({"type_name": "Mine", "valid_months": 1}))).await, "forbidden", "");
    let p6 = apply(&ria, party, "Ria Sen").await;
    t.act(&ria, "XPermitApplication", p6, "submit_application", json!({})).await.unwrap();
    refused(t.act(&manager, "XPermitApplication", p6, "take_up", json!({})).await, "forbidden", "");

    // ───────────── a citizen's request ─────────────
    // CANNOT (G32): a pack declares no personal data. What a person asks for and erases is what the platform finds
    // without declarations; a draft she never put to the office is not erased while what she put to it is kept,
    // each saying why.
    let unsent =
        id_of(&t.insert(&ria, "XPermitApplication", json!({"permit_type": signage.to_string(), "applicant_name": "Ria Sen", "details": "A shop sign"})).await);
    let asked = k.personal_data(&a, "ria@home.test").await.unwrap();
    let records = asked["records"].as_array().cloned().unwrap_or_default();
    println!("personal data found for ria: {}", records.iter().map(|r| r["entity"].as_str().unwrap_or("?").to_string()).collect::<Vec<_>>().join(", "));
    assert!(!records.iter().any(|r| r["id"] == json!(unsent.to_string())), "an undeclared application is not found: {asked}");

    // ───────────── not in the app's tests: its page, tile, quick create, menu and assistant notes ─────────────
    // DIFFERENT (G36): the officers' queues are a Studio space with the same four lists.
    let space: Uuid = {
        let mut tx = k.db.tenant_tx(t.tenant).await.unwrap();
        let id: String = sqlx::query_scalar("select id::text from e_space where tenant_id = $1").bind(t.tenant).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        id.parse().unwrap()
    };
    let labels = |page: &Value| -> Vec<String> {
        page["blocks"].as_array().unwrap().iter().filter(|b| b["type"] == "list").map(|b| b["label"].as_str().unwrap().to_string()).collect()
    };
    let page = ferrum_studio::builder::space_data(k, &officer, space).await.expect("the space");
    assert_eq!(labels(&page), ["To take up", "Under review", "To issue"], "an officer sees the lists of what they may read: {page}");
    let page = ferrum_studio::builder::space_data(k, &caseworker, space).await.expect("the space");
    assert_eq!(labels(&page), ["Open cases"], "{page}");
    // DIFFERENT (G35): the menu "Government" with the app's icon and colour, quick create of an application and a
    // case, and the assistant's notes; the tile opens the records pages, not a page of its own.
    let admin = t.admin().await;
    let gov = admin.schema.modules.iter().find(|m| m.name == "x_government").expect("the menu");
    assert_eq!(
        (gov.package.as_str(), gov.label.as_str(), gov.icon.as_deref(), gov.color.as_deref(), gov.portal),
        ("tenant", "Government", Some("landmark"), Some("#4338ca"), true)
    );
    assert_eq!(gov.create, ["XPermitApplication", "XCase"]);
    assert_eq!(gov.assistant.as_ref().unwrap().suggestions.len(), 3);
    // DIFFERENT (G37): the app's words for the texts the pack defines, as the workspace's words.
    let ar = k.interface_words(t.tenant, "ar").await.unwrap();
    let hi = k.interface_words(t.tenant, "hi").await.unwrap();
    assert_eq!((ar.len(), hi.len()), (words()["ar"].as_object().unwrap().len(), words()["hi"].as_object().unwrap().len()));
    println!("words: ar {} hi {} of {} texts", ar.len(), hi.len(), defined.len());
}
