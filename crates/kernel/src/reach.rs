//! What decides who reaches a record (docs/guides/05; S9 §2.4): the fields of an entity whose values, changed or moved,
//! change who reaches its records. A merge that moves such a link is for administrators only (R19, `crate::merge`).

use crate::{Ctx, Result, Tx};
use ferrum_meta::via::{ViaStep, resolve};
use ferrum_meta::{ApprovalPolicy, EffEntity, FieldType};
use serde_json::Value;
use std::collections::BTreeSet;

/// The fields of `ent` that decide who reaches its records in `ctx`'s workspace: its owner fields (a line's names its
/// table); every field a rule's `where` names on it (rules on `*` too); every link or table field of it a rule's `via`
/// path crosses, and the field such a path ends at when it is `ent`'s; the links it declares in `reach_links` (its
/// app's code reaches records by them, as the system); its links to `Company` and to every entity people are scoped by
/// (`user_scope`); and the fields its approval steps route by (`approvers.field`, escalations' too). Read in `tx`.
pub async fn fields(ctx: &Ctx, tx: &mut Tx, ent: &EffEntity) -> Result<BTreeSet<String>> {
    let name = ent.def.name.as_str();
    let mut out: BTreeSet<String> = BTreeSet::new();
    out.extend(ent.def.owner_fields.iter().map(|o| o.split('.').next().unwrap_or(o).to_string()));
    out.extend(ent.def.reach_links.iter().cloned());
    let rules: Vec<(String, Option<Value>, Option<String>)> =
        sqlx::query_as("select entity, match_rule, via from role_permission where tenant_id = $1").bind(ctx.tenant).fetch_all(&mut **tx).await?;
    for (entity, rule, via) in rules {
        if entity == name || entity == "*" {
            for k in rule.as_ref().and_then(Value::as_object).into_iter().flat_map(|m| m.keys()) {
                if ent.field(k).is_some() {
                    out.insert(k.clone());
                }
            }
        }
        let Some(path) = via else { continue };
        let Ok(steps) = resolve(&entity, &path, |n| ctx.schema.entity(n).map(|e| &e.def)) else { continue };
        let mut at = entity.clone();
        for step in steps {
            let (field, next) = match step {
                ViaStep::Link { field, to } => (field, Some(to)),
                ViaStep::Table { field, of } => (field, Some(of)),
                ViaStep::Email { field } => (field, None),
            };
            if at == name {
                out.insert(field);
            }
            match next {
                Some(n) => at = n,
                None => break,
            }
        }
    }
    let scoped: Vec<String> = sqlx::query_scalar("select distinct entity from user_scope where tenant_id = $1").bind(ctx.tenant).fetch_all(&mut **tx).await?;
    for f in ent.value_fields().filter(|f| f.ftype == FieldType::Link) {
        if f.to.as_deref().is_some_and(|t| t == "Company" || scoped.iter().any(|s| s == t)) {
            out.insert(f.name.clone());
        }
    }
    let mut routed = |p: &ApprovalPolicy| {
        for step in &p.steps {
            for a in std::iter::once(&step.approvers).chain(step.escalate_to.as_ref()) {
                if let Some(f) = a.field.as_ref().filter(|f| ent.field(f).is_some()) {
                    out.insert(f.clone());
                }
            }
        }
    };
    for a in ent.document_approvals.values() {
        routed(&a.policy);
    }
    for t in ent.workflow.iter().flat_map(|w| &w.def.transitions) {
        if let Some(p) = &t.approval {
            routed(p);
        }
    }
    Ok(out)
}
