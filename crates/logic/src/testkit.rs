//! What the logic crate's tests share: a workspace with the package's capabilities, named as Ferrum names
//! them, and the package's examples written against it.

use crate::capability::{Capability, Effect, Idempotency, Limits, Operation, Profile, UnknownOutcome, object_schema};
use crate::compile::{Diagnostic, EntityShape, FieldShape, Release, Resolver, RuntimeProfile, compile};
use crate::expr::Ty;
use crate::solution::Solution;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// A workspace with the package's purchase and condition capabilities, named as Ferrum names them
/// (`{package}.{entity}.{operation}`, docs/logic/capabilities.md). Ferrum has no stock hold yet, so those two
/// keep the package's example names (docs/logic/reuse-map.yaml records the gap).
pub(crate) struct Fixture {
    pub(crate) caps: Vec<Capability>,
}

pub(crate) fn flat(fields: &[&str]) -> Value {
    object_schema(fields.iter().map(|k| (k.to_string(), json!({"type": "string"}))).collect(), fields.iter().map(|s| s.to_string()).collect())
}

/// W1's create: the record's fields under `values`.
pub(crate) fn create_input(fields: &[&str], required: &[&str]) -> Value {
    let values = object_schema(fields.iter().map(|k| (k.to_string(), json!({"type": "string"}))).collect(), required.iter().map(|s| s.to_string()).collect());
    object_schema(Map::from_iter([("values".to_string(), values), ("submit".to_string(), json!({"type": "boolean"}))]), vec!["values".into()])
}

pub(crate) fn cap(id: &str, effect: Effect, input: Value, output: Value) -> Capability {
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
        deprecated: None,
    }
    .sealed()
    .unwrap()
}

impl Fixture {
    pub(crate) fn new() -> Fixture {
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
                cap("assets_ops.maintenance_request.create", Effect::TransactionalAction, create_input(&["asset", "description"], &["asset"]), id_out.clone()),
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
        if name == "Charge" {
            let f = |ty: Ty, link: Option<&str>, restricted: bool| FieldShape { ty, customer: false, restricted, link: link.map(str::to_string) };
            return Some(EntityShape {
                fields: BTreeMap::from([
                    ("request".into(), f(Ty::Ref("MaterialRequest".into()), Some("MaterialRequest"), false)),
                    ("supplier".into(), f(Ty::Ref("Supplier".into()), Some("Supplier"), false)),
                    ("amount".into(), f(Ty::Decimal, None, false)),
                    ("hours".into(), f(Ty::Int, None, false)),
                    ("status".into(), f(Ty::Text, None, false)),
                    ("due".into(), f(Ty::Date, None, false)),
                    ("paid".into(), f(Ty::Bool, None, false)),
                    ("internal_note".into(), f(Ty::Text, None, true)),
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
pub(crate) fn purchase() -> Value {
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
pub(crate) fn condition() -> Value {
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
pub(crate) fn structured() -> Value {
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

pub(crate) fn compile_value(v: Value, profile: RuntimeProfile) -> Result<Release, Vec<Diagnostic>> {
    let s = Solution::from_value(v).map_err(|r| vec![Diagnostic { code: r.code, message: r.message, pointer: String::new(), node: None, fault: true }])?;
    compile(&s, &Fixture::new(), profile)
}
