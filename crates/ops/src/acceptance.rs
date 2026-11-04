//! An app's industrial solution in Operations (FERRUM-IND-018, docs/industrial-solutions.md).
//! Installing the app makes its device profiles the workspace's Device Profiles and its acceptance
//! procedures what a device of one of them passes on site before it is commissioned.
//!
//! A run records each step as someone saw it: a person on site, or the site runtime through the
//! same calls. A read, or a command's check, passes only when what was seen meets the procedure's
//! expectation; a manual step passes when the person says it holds. A passed run says what was
//! checked, and certifies nothing beyond it.

use ferrum_apps::industrial::{AcceptancePackage, Access, DeviceProfile, Expect, Solution, StepKind};
use ferrum_domain::util::sys;
use ferrum_kernel::{Ctx, Doc, Error, Kernel, Result, Tx, audit};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap_or_default()
}

fn kind_label(k: StepKind) -> &'static str {
    match k {
        StepKind::Read => "Read",
        StepKind::Command => "Command",
        StepKind::Manual => "Manual",
    }
}

/// What a step expects, without what it leaves out.
fn expectation(e: Option<&Expect>) -> Value {
    let Some(e) = e else { return Value::Null };
    let mut out = serde_json::Map::new();
    if let Some(m) = e.min {
        out.insert("min".into(), json!(m));
    }
    if let Some(m) = e.max {
        out.insert("max".into(), json!(m));
    }
    if let Some(v) = e.equals.as_ref().filter(|v| !v.is_null()) {
        out.insert("equals".into(), v.clone());
    }
    if out.is_empty() { Value::Null } else { Value::Object(out) }
}

/// Every point, as the profile's Read Capabilities: `name: address (type, unit)`.
fn reads(p: &DeviceProfile) -> String {
    p.points
        .iter()
        .map(|pt| {
            let kind = serde_json::to_value(pt.kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
            let unit = pt.unit.as_ref().map(|u| format!(", {u}")).unwrap_or_default();
            format!("{}: {} ({kind}{unit})", pt.name, pt.address)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The points it takes and its commands, as the profile's Write and Command Capabilities.
fn writes(p: &DeviceProfile) -> String {
    let points = p.points.iter().filter(|pt| pt.access == Access::ReadWrite).map(|pt| format!("{}: {}", pt.name, pt.address));
    let commands = p.commands.iter().map(|c| match &c.point {
        Some(point) => format!("command {}: writes {point}", c.name),
        None => format!("command {}", c.name),
    });
    points.chain(commands).collect::<Vec<_>>().join("\n")
}

async fn existing(tx: &mut Tx, tenant: Uuid, table: &str, key: &str) -> Result<Option<Uuid>> {
    // `table` is one of this module's own.
    Ok(sqlx::query_scalar(AssertSqlSafe(format!("select id from {table} where tenant_id = $1 and solution = $2")))
        .bind(tenant)
        .bind(key)
        .fetch_optional(&mut **tx)
        .await?)
}

fn procedure_doc(app: &str, a: &AcceptancePackage) -> Doc {
    let steps: Vec<Value> = a
        .steps
        .iter()
        .map(|s| {
            json!({"step_id": s.id, "kind": kind_label(s.kind), "profile": s.profile, "point": s.point, "command": s.command,
                   "expect": expectation(s.expect.as_ref()), "instruction": s.instruction})
        })
        .collect();
    obj(json!({"procedure_name": a.title, "app": app, "description": a.description, "steps": steps}))
}

/// Makes an app's solution the workspace's: its profiles and procedures, added or brought up to date
/// (an upgrade). What people recorded of a profile (how far it was tried, its firmware) stays theirs.
pub async fn import(k: &Kernel, ctx: &Ctx, app: &str, s: &Solution) -> Result<()> {
    // The installation may have brought Operations in this same request: the caller's schema is
    // from before it, so the workspace's is read again.
    let mut system = Ctx::system(ctx.tenant, k.registry.schema(ctx.tenant).await?);
    system.request_id = ctx.request_id.clone();
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    for p in &s.profiles {
        let key = format!("{app}/{}", p.id);
        let mut doc = obj(json!({
            "profile_name": format!("{} {}", p.vendor, p.model),
            "vendor": p.vendor,
            "model": p.model,
            "protocol": p.scheme,
            "reads": reads(p),
            "writes": writes(p),
            "maintainer": app,
        }));
        match existing(&mut tx, ctx.tenant, "e_device_profile", &key).await? {
            Some(id) => {
                k.update_tx(&system, &mut tx, "DeviceProfile", id, doc, None).await?;
            }
            None => {
                doc.insert("solution".into(), json!(key));
                doc.insert("tested_on".into(), json!("Simulator"));
                k.insert_tx(&system, &mut tx, "DeviceProfile", doc).await?;
            }
        }
    }
    for a in &s.acceptance {
        let key = format!("{app}/{}", a.id);
        let mut doc = procedure_doc(app, a);
        match existing(&mut tx, ctx.tenant, "e_acceptance_procedure", &key).await? {
            Some(id) => {
                k.update_tx(&system, &mut tx, "AcceptanceProcedure", id, doc, None).await?;
            }
            None => {
                doc.insert("solution".into(), json!(key));
                k.insert_tx(&system, &mut tx, "AcceptanceProcedure", doc).await?;
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

/// A procedure applies to a device when one of its steps is for the device's profile.
const APPLIES: &str = "exists (select 1 from e_acceptance_procedure__steps s join e_device d on d.tenant_id = s.tenant_id and d.id = $2 \
     join e_device_profile dp on dp.tenant_id = d.tenant_id and dp.id = d.profile \
     where s.tenant_id = p.tenant_id and s.parent_id = p.id and p.app || '/' || s.profile = dp.solution)";

/// The procedures a device has yet to pass, by name: commissioning waits for them.
pub async fn waiting(tx: &mut Tx, tenant: Uuid, device: Uuid) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(AssertSqlSafe(format!(
        "select p.procedure_name from e_acceptance_procedure p where p.tenant_id = $1 and {APPLIES} \
         and not exists (select 1 from e_acceptance_run r where r.tenant_id = p.tenant_id and r.device = $2 and r.procedure = p.id and r.status = 'Passed') \
         order by p.procedure_name"
    )))
    .bind(tenant)
    .bind(device)
    .fetch_all(&mut **tx)
    .await?)
}

/// The procedures that apply to a device, whether it passed each, and its runs.
pub async fn for_device(k: &Kernel, ctx: &Ctx, device: Uuid) -> Result<Value> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let procedures: Vec<Value> = sqlx::query_scalar(AssertSqlSafe(format!(
        "select jsonb_build_object('id', p.id, 'name', p.procedure_name, 'solution', p.solution, \
           'passed', exists (select 1 from e_acceptance_run r where r.tenant_id = p.tenant_id and r.device = $2 and r.procedure = p.id and r.status = 'Passed')) \
         from e_acceptance_procedure p where p.tenant_id = $1 and {APPLIES} order by p.procedure_name"
    )))
    .bind(ctx.tenant)
    .bind(device)
    .fetch_all(&mut *tx)
    .await?;
    let runs: Vec<Value> = sqlx::query_scalar(
        "select jsonb_build_object('id', id, 'procedure', procedure, 'status', status, 'started_by', started_by, 'finished_at', finished_at, 'created_at', created_at) \
         from e_acceptance_run where tenant_id = $1 and device = $2 order by created_at desc limit 50",
    )
    .bind(ctx.tenant)
    .bind(device)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(json!({"procedures": procedures, "runs": runs}))
}

/// Starts a run of a procedure for a device: a result to fill in for each step.
pub async fn start(k: &Kernel, ctx: &Ctx, device: Uuid, procedure: Uuid) -> Result<Doc> {
    let system = sys(ctx);
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let applies: bool =
        sqlx::query_scalar(AssertSqlSafe(format!("select exists (select 1 from e_acceptance_procedure p where p.tenant_id = $1 and p.id = $3 and {APPLIES})")))
            .bind(ctx.tenant)
            .bind(device)
            .bind(procedure)
            .fetch_one(&mut *tx)
            .await?;
    if !applies {
        return Err(Error::invalid("this procedure is not for this device's profile"));
    }
    let steps: Vec<(String, String, Option<Value>, Option<String>)> =
        sqlx::query_as("select step_id, kind, expect, instruction from e_acceptance_procedure__steps where tenant_id = $1 and parent_id = $2 order by idx")
            .bind(ctx.tenant)
            .bind(procedure)
            .fetch_all(&mut *tx)
            .await?;
    let results: Vec<Value> = steps
        .into_iter()
        .map(|(step, kind, expect, instruction)| {
            let expected = match kind.as_str() {
                "Manual" => json!({"instruction": instruction}),
                _ => expect.unwrap_or(Value::Null),
            };
            json!({"step_id": step, "kind": kind, "expected": expected, "outcome": "Pending"})
        })
        .collect();
    let run = k
        .insert_tx(
            &system,
            &mut tx,
            "AcceptanceRun",
            obj(json!({"device": device, "procedure": procedure, "status": "Running", "started_by": ctx.email, "results": results})),
        )
        .await?;
    tx.commit().await?;
    Ok(run)
}

/// What someone saw at a step: the value read (a read, or a command's check), or whether it holds
/// (a manual step, or a command with nothing to check).
#[derive(Debug, Default, Deserialize)]
pub struct Seen {
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub passed: Option<bool>,
    #[serde(default)]
    pub note: Option<String>,
}

/// Whether a value meets an expectation: equal to `equals`, or a number within `min` and `max`.
pub fn meets(expected: &Value, value: &Value) -> bool {
    if let Some(eq) = expected.get("equals").filter(|v| !v.is_null()) {
        return eq == value || eq.as_f64().is_some_and(|e| value.as_f64() == Some(e));
    }
    let Some(n) = value.as_f64() else { return false };
    expected.get("min").and_then(Value::as_f64).is_none_or(|m| n >= m) && expected.get("max").and_then(Value::as_f64).is_none_or(|m| n <= m)
}

/// Records one step of a running run. The run fails at a failed step, and passes once every step has.
pub async fn record(k: &Kernel, ctx: &Ctx, run: Uuid, step: &str, seen: Seen) -> Result<Doc> {
    let system = sys(ctx);
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let status: Option<String> = sqlx::query_scalar("select status from e_acceptance_run where tenant_id = $1 and id = $2 for update")
        .bind(ctx.tenant)
        .bind(run)
        .fetch_optional(&mut *tx)
        .await?;
    match status.as_deref() {
        None => return Err(Error::NotFound("acceptance run".into())),
        Some("Running") => {}
        Some(other) => return Err(Error::invalid(format!("this run has {}: start another", other.to_lowercase()))),
    }
    let row: Option<(Uuid, String, Option<Value>, String)> =
        sqlx::query_as("select id, kind, expected, outcome from e_acceptance_run__results where tenant_id = $1 and parent_id = $2 and step_id = $3")
            .bind(ctx.tenant)
            .bind(run)
            .bind(step)
            .fetch_optional(&mut *tx)
            .await?;
    let (row, kind, expected, outcome) = row.ok_or_else(|| Error::NotFound(format!("step {step}")))?;
    if outcome != "Pending" {
        return Err(Error::invalid(format!("step {step} is recorded already")));
    }
    let checks = expected.as_ref().filter(|e| kind != "Manual" && !e.is_null());
    let passed = match checks {
        Some(e) => {
            let v = seen.value.as_ref().filter(|v| !v.is_null()).ok_or_else(|| Error::field("value", "what was seen is needed"))?;
            meets(e, v)
        }
        None => seen.passed.ok_or_else(|| Error::field("passed", "say whether it holds"))?,
    };
    let outcome = if passed { "Passed" } else { "Failed" };
    let now = chrono::Utc::now().to_rfc3339();
    k.set_child_values_tx(
        &system,
        &mut tx,
        "AcceptanceRun",
        "results",
        row,
        obj(json!({"observed": seen.value, "outcome": outcome, "recorded_by": ctx.email, "recorded_at": now, "note": seen.note})),
    )
    .await?;
    let (pending, failed): (i64, i64) = sqlx::query_as(
        "select count(*) filter (where outcome = 'Pending'), count(*) filter (where outcome = 'Failed') from e_acceptance_run__results where tenant_id = $1 and parent_id = $2",
    )
    .bind(ctx.tenant)
    .bind(run)
    .fetch_one(&mut *tx)
    .await?;
    let finished = if failed > 0 {
        Some("Failed")
    } else if pending == 0 {
        Some("Passed")
    } else {
        None
    };
    if let Some(status) = finished {
        k.set_values_tx(&system, &mut tx, "AcceptanceRun", run, obj(json!({"status": status, "finished_at": now}))).await?;
    }
    audit::record(
        &mut tx,
        ctx,
        "AcceptanceRun",
        run,
        "ops.acceptance_step",
        None,
        json!({"step": step, "observed": seen.value, "outcome": outcome, "note": seen.note}),
    )
    .await?;
    let doc = k.get_tx(&system, &mut tx, "AcceptanceRun", run, false).await?;
    tx.commit().await?;
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::meets;
    use serde_json::json;

    #[test]
    fn what_meets_an_expectation() {
        assert!(meets(&json!({"min": 207, "max": 253}), &json!(230.5)));
        assert!(!meets(&json!({"min": 207, "max": 253}), &json!(254)));
        assert!(!meets(&json!({"min": 207}), &json!(206.9)));
        assert!(!meets(&json!({"max": 1}), &json!("0")), "a number is a number, not text");
        assert!(meets(&json!({"equals": false}), &json!(false)));
        assert!(!meets(&json!({"equals": false}), &json!(0)));
        assert!(meets(&json!({"equals": 3}), &json!(3.0)));
        assert!(meets(&json!({"equals": "OK"}), &json!("OK")));
    }
}
