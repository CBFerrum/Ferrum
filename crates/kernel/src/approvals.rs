//! Approvals on a workflow transition (docs/api.md, "Approvals"; docs/builder/decisions.md BD-19). A transition may
//! carry a policy: steps of people who must agree before it completes. Taking it makes a request (at most one pending
//! per record) and leaves the record where it is; each decision is a row and an entry in the record's history; when
//! the last step is satisfied the transition completes, once, in that decision's transaction, as the system on the
//! requester's behalf, with its effects and events.
//!
//! Every path (take, decide, recall, an edit, the minute loop) locks the record before its request, so two people
//! deciding at once queue on the record and the second sees the first's decision.

use crate::ctx::Ctx;
use crate::db::{Tx, with_retry};
use crate::error::{Error, FieldError, Result};
use crate::{Doc, Kernel};
use chrono::{DateTime, Utc};
use ferrum_logic::approval::{Decided, Plan};
use ferrum_meta::{ApprovalNeed, ApprovalPolicy, Approvers, DocumentAction, EffEntity, Expr, NeedWord, Schema, Scope};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use uuid::Uuid;

/// Requests the minute loop looks at in one pass.
pub const SWEEP_BATCH: i64 = 200;

/// One step of a request as it stands: `waiting`, `open`, `skipped`, `approved` or `rejected`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Step {
    state: String,
    /// Who may decide, worked out when it opened.
    #[serde(default)]
    approvers: Vec<Uuid>,
    /// Who an escalation added.
    #[serde(default)]
    escalated: Vec<Uuid>,
    /// How many different people must approve, worked out when it opened.
    #[serde(default)]
    need: usize,
    /// What an expression `need` gave when the request was made (BD-45): kept, so a later change of the record does not
    /// move it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    asked: Option<i64>,
    #[serde(default)]
    opened_at: Option<DateTime<Utc>>,
    #[serde(default)]
    remind_at: Option<DateTime<Utc>>,
    #[serde(default)]
    reminded_at: Option<DateTime<Utc>>,
    #[serde(default)]
    escalate_at: Option<DateTime<Utc>>,
    #[serde(default)]
    escalated_at: Option<DateTime<Utc>>,
}

impl Step {
    fn is(&self, state: &str) -> bool {
        self.state == state
    }

    fn may(&self, who: Uuid) -> bool {
        self.approvers.contains(&who) || self.escalated.contains(&who)
    }
}

#[derive(sqlx::FromRow)]
struct Request {
    id: Uuid,
    entity: String,
    record_id: Uuid,
    action: String,
    from_state: String,
    requester: Option<Uuid>,
    params: Value,
    policy: Value,
    steps: Value,
    status: String,
    created_at: DateTime<Utc>,
    /// On the document pipeline's submit or cancel (`action`), not a workflow transition.
    document: bool,
    failure: Option<String>,
    /// On running the entity's action `action` (BD-34), not a transition or the document pipeline.
    on_action: bool,
    /// Its secret parameters, each sealed with the server's key ([`seal_secrets`]).
    sealed: Option<Value>,
}

const COLUMNS: &str = "id, entity, record_id, action, from_state, requester, params, policy, steps, status, created_at, document, failure, on_action, sealed";

impl Request {
    fn parts(&self) -> Result<(ApprovalPolicy, Vec<Step>)> {
        let bad = |e: serde_json::Error| Error::internal(format!("approval request {}: {e}", self.id));
        Ok((serde_json::from_value(self.policy.clone()).map_err(bad)?, serde_json::from_value(self.steps.clone()).map_err(bad)?))
    }
}

#[derive(sqlx::FromRow, Serialize)]
struct Decision {
    step: i32,
    decider: Uuid,
    on_behalf_of: Option<Uuid>,
    outcome: String,
    comment: Option<String>,
    at: DateTime<Utc>,
    void: bool,
}

impl Decision {
    /// Whom it counts for: the person stood in for, else whoever decided.
    fn person(&self) -> Uuid {
        self.on_behalf_of.unwrap_or(self.decider)
    }
}

/// What a request is about, for the steps it opens.
struct On<'a> {
    ent: &'a EffEntity,
    record: Uuid,
    doc: &'a Doc,
    request: Uuid,
    requester: Uuid,
    action: &'a str,
    policy: &'a ApprovalPolicy,
}

/// The steps of `p`, stage by stage: a step marked `with_next` opens with the one after it.
fn stages(p: &ApprovalPolicy) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, s) in p.steps.iter().enumerate() {
        if !s.with_next || i + 1 == p.steps.len() {
            out.push(start..i + 1);
            start = i + 1;
        }
    }
    out
}

/// Whether enough different people approved `step` (decisions `of` it), by the rules a process's approvals keep
/// (`ferrum_logic::approval`): the same person twice is one, and a requester counts only where the policy allows.
fn satisfied(step: &Step, of: &[&Decision], requester: Uuid, allow_requester: bool) -> bool {
    let people: Vec<String> = of.iter().filter(|d| d.outcome == "approve" && !d.void).map(|d| d.person().to_string()).collect();
    let decided: Vec<Decided> = people.iter().map(|p| Decided { approver: p, digest: "", policy_revision: 0, expires_at_ms: i64::MAX }).collect();
    let asked_by = if allow_requester { String::new() } else { requester.to_string() };
    let plan = Plan { digest: "", requested_by: &asked_by, policy_revision: 0, expires_at_ms: i64::MAX, required: step.need as u32 };
    ferrum_logic::approval::satisfied(&plan, "", &decided, 0, 0).is_ok()
}

/// What binds a request's sealed parameter `name` to it.
pub(crate) fn sealed_label(tenant: Uuid, request: Uuid, name: &str) -> String {
    format!("{tenant}/approval/{request}/{name}")
}

/// `params` of action `def` as a request keeps them: each secret parameter given shown as `[redacted]`, and its value
/// sealed with the server's key apart (`{name: sealed}`), which only completing the request opens. Refused without
/// the server's key when a secret was given: it is never kept in the clear.
fn seal_secrets(k: &Kernel, tenant: Uuid, request: Uuid, def: &ferrum_meta::ActionDef, params: &Value) -> Result<(Value, Value)> {
    let mut sealed = serde_json::Map::new();
    for p in def.params.iter().filter(|p| p.secret) {
        let Some(v) = params.get(&p.name).filter(|v| !v.is_null()) else { continue };
        let key = k.settings.secret_key.as_ref().ok_or_else(|| {
            Error::Unavailable("an action with a secret parameter waits for approval only where the server has its secret key (FERRUM_SECRET_KEY)".into())
        })?;
        sealed.insert(p.name.clone(), json!(key.seal(&sealed_label(tenant, request, &p.name), &v.to_string())?));
    }
    Ok((def.redacted(params), Value::Object(sealed)))
}

/// The parameters request `r` was asked with, its secret ones opened again.
fn unsealed(k: &Kernel, tenant: Uuid, r: &Request) -> Result<Value> {
    let mut params = r.params.clone();
    if let (Some(Value::Object(sealed)), Some(out)) = (&r.sealed, params.as_object_mut()) {
        for (name, s) in sealed {
            let plain = k.open_sealed(&sealed_label(tenant, r.id, name), s.as_str().unwrap_or_default())?;
            out.insert(name.clone(), serde_json::from_str(&plain).map_err(|e| Error::internal(format!("sealed parameter {name}: {e}")))?);
        }
    }
    Ok(params)
}

/// The first reminder or escalation the open steps still owe.
fn next_due(steps: &[Step]) -> Option<DateTime<Utc>> {
    steps
        .iter()
        .filter(|s| s.is("open"))
        .flat_map(|s| [s.remind_at.filter(|_| s.reminded_at.is_none()), s.escalate_at.filter(|_| s.escalated_at.is_none())])
        .flatten()
        .min()
}

fn holds(ent: &EffEntity, ctx: &Ctx, doc: &Doc, src: Option<&str>) -> Result<bool> {
    let Some(src) = src else { return Ok(true) };
    let view = ent.cel_view(&Value::Object(doc.clone()), &ctx.schema);
    let e = Expr::compile(src).map_err(Error::invalid)?;
    e.eval_bool(&Scope { doc: &view, old: None, parent: None, user: &ctx.user_value() }).map_err(|m| Error::invalid(format!("approval '{src}': {m}")))
}

/// The people `who` names on `doc`: a role's holders, or the ids or e-mail addresses a field or an expression gives
/// (one, or a list), each an active member of the workspace.
async fn resolve(ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &Doc, who: &Approvers) -> Result<Vec<Uuid>> {
    if let Some(role) = &who.role {
        return Ok(sqlx::query_scalar(
            "select r.user_id from user_role r join app_user u on u.tenant_id = r.tenant_id and u.id = r.user_id \
             where r.tenant_id = $1 and r.role = $2 and (r.expires_at is null or r.expires_at > now()) and u.is_active and u.kind = 'member' order by 1",
        )
        .bind(ctx.tenant)
        .bind(role)
        .fetch_all(&mut **tx)
        .await?);
    }
    let given = match (&who.field, &who.expr) {
        (Some(f), _) => doc.get(f).cloned().unwrap_or(Value::Null),
        (_, Some(src)) => {
            let view = ent.cel_view(&Value::Object(doc.clone()), &ctx.schema);
            let e = Expr::compile(src).map_err(Error::invalid)?;
            e.eval(&Scope { doc: &view, old: None, parent: None, user: &ctx.user_value() }).map_err(|m| Error::invalid(format!("approvers '{src}': {m}")))?
        }
        _ => Value::Null,
    };
    let one = std::slice::from_ref(&given);
    let named: Vec<String> = given.as_array().map(Vec::as_slice).unwrap_or(one).iter().filter_map(Value::as_str).map(str::trim).map(str::to_string).collect();
    let ids: Vec<Uuid> = named.iter().filter_map(|s| s.parse().ok()).collect();
    let emails: Vec<String> = named.iter().filter(|s| s.contains('@')).map(|s| s.to_lowercase()).collect();
    if ids.is_empty() && emails.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_scalar(
        "select id from app_user where tenant_id = $1 and is_active and kind = 'member' and (id = any($2) or lower(email) = any($3)) order by 1",
    )
    .bind(ctx.tenant)
    .bind(&ids)
    .bind(&emails)
    .fetch_all(&mut **tx)
    .await?)
}

/// Who may not decide step `i` (`def`) of a request: its requester unless the policy allows, those who decided an
/// earlier step when it says so, and whoever took the transitions it names on this record.
async fn excluded(tx: &mut Tx, ctx: &Ctx, on: &On<'_>, i: usize, decided: &[Decision]) -> Result<Vec<Uuid>> {
    let def = &on.policy.steps[i];
    let mut out = Vec::new();
    if !on.policy.allow_requester {
        out.push(on.requester);
    }
    if def.not_earlier_deciders {
        out.extend(decided.iter().filter(|d| (d.step as usize) < i && !d.void).flat_map(|d| [d.decider, d.person()]));
    }
    if !def.not_who_took.is_empty() {
        let wf = on.ent.workflow.as_ref().map(|w| &w.def.transitions[..]).unwrap_or_default();
        let actions: Vec<String> = def.not_who_took.iter().map(|a| format!("workflow:{a}")).collect();
        // A transition that submits or cancels is in the history as that, its state moving to the transition's.
        let into: Vec<&str> = wf.iter().filter(|t| def.not_who_took.contains(&t.action)).map(|t| t.to.as_str()).collect();
        let took: Vec<Uuid> = sqlx::query_scalar(
            "select distinct actor from audit_event where tenant_id = $1 and entity = $2 and record_id = $3 and actor is not null \
             and (action = any($4) or (action in ('submit', 'cancel') and diff -> 'workflow_state' ->> 1 = any($5)))",
        )
        .bind(ctx.tenant)
        .bind(&on.ent.def.name)
        .bind(on.record)
        .bind(&actions)
        .bind(&into)
        .fetch_all(&mut **tx)
        .await?;
        out.extend(took);
    }
    Ok(out)
}

/// The people `user` stands in for today, while they are away (person_away, FEO-034).
async fn standing_for(tx: &mut Tx, ctx: &Ctx) -> Result<Vec<Uuid>> {
    // ponytail: the server's day, as process tasks take it; a stand-in may start or end a few hours off.
    Ok(sqlx::query_scalar("select user_id from person_away where tenant_id = $1 and substitute = $2 and current_date between starts_on and ends_on")
        .bind(ctx.tenant)
        .bind(ctx.user)
        .fetch_all(&mut **tx)
        .await?)
}

/// The policy of the request pending on record `id` of `entity` when an open step of it awaits `ctx`'s decision (as
/// one of its approvers, or for whom they stand in) and they have not decided it yet: what lets an approver read
/// that one record (BD-19). `None` once the step closes, they decide, or the request ends.
async fn awaiting(tx: &mut Tx, ctx: &Ctx, entity: &str, id: Uuid) -> Result<Option<ApprovalPolicy>> {
    let mut people: Vec<String> = standing_for(tx, ctx).await?.iter().map(Uuid::to_string).collect();
    people.push(ctx.user.to_string());
    let policy: Option<Value> = sqlx::query_scalar(
        "select policy from approval_request r where tenant_id = $1 and entity = $2 and record_id = $3 and status = 'pending' and exists ( \
           select 1 from jsonb_array_elements(r.steps) with ordinality s(v, n) where v ->> 'state' = 'open' \
             and ((v -> 'approvers') ?| $4 or (v -> 'escalated') ?| $4) \
             and not exists (select 1 from approval_decision d where d.tenant_id = r.tenant_id and d.request_id = r.id \
               and d.step = n - 1 and not d.void and d.decider = $5))",
    )
    .bind(ctx.tenant)
    .bind(entity)
    .bind(id)
    .bind(&people)
    .bind(ctx.user)
    .fetch_optional(&mut **tx)
    .await?;
    policy.map(|p| serde_json::from_value(p).map_err(|e| Error::internal(format!("approval policy: {e}")))).transpose()
}

/// Tells `users`, and whoever stands in for those of them away today, in the way each chose for approvals; with the
/// policy's `message`, by that template instead (`notice` says which notice this is).
#[allow(clippy::too_many_arguments)]
async fn tell(
    k: &Kernel,
    tx: &mut Tx,
    ctx: &Ctx,
    (message, notice): (Option<&str>, &str),
    users: &[Uuid],
    title: &str,
    entity: &str,
    record: Uuid,
) -> Result<()> {
    let away: Vec<Uuid> =
        sqlx::query_scalar("select substitute from person_away where tenant_id = $1 and user_id = any($2) and current_date between starts_on and ends_on")
            .bind(ctx.tenant)
            .bind(users)
            .fetch_all(&mut **tx)
            .await?;
    let all: BTreeSet<Uuid> = users.iter().chain(&away).copied().filter(|u| !u.is_nil()).collect();
    if let Some(template) = message {
        let values = serde_json::Map::from_iter([("notice".to_string(), json!(notice)), ("title".to_string(), json!(title))]);
        let to = all.into_iter().map(crate::messages::Recipient::user).collect();
        let m = crate::messages::Message { template: template.into(), entity: Some(entity.into()), record: Some(record), to, values, ..Default::default() };
        crate::messages::send(k, tx, ctx, &m).await?;
        return Ok(());
    }
    for u in all {
        crate::notify::send_kind(tx, ctx.tenant, u, "approval", "inapp", title, None, Some(entity), Some(record)).await?;
    }
    Ok(())
}

/// The moment `after` (seconds or an ISO 8601 duration) from `from`: working time on `calendar` when it names one.
async fn due(tx: &mut Tx, tenant: Uuid, calendar: Option<&str>, from: DateTime<Utc>, after: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    let Some(after) = after else { return Ok(None) };
    let secs = ferrum_logic::target::within(&json!(after)).map_err(Error::invalid)?.unwrap_or(0);
    match calendar {
        Some(name) => {
            let cal = crate::load_calendar(tx, tenant, name).await?.ok_or_else(|| Error::invalid(format!("the calendar {name} is not there")))?;
            Ok(Some(cal.add_working(from, secs, &[]).map_err(Error::invalid)?))
        }
        None => Ok(Some(from + chrono::Duration::seconds(secs as i64))),
    }
}

async fn decisions(tx: &mut Tx, tenant: Uuid, request: Uuid) -> Result<Vec<Decision>> {
    Ok(sqlx::query_as(
        "select step, decider, on_behalf_of, outcome, comment, at, void from approval_decision where tenant_id = $1 and request_id = $2 order by at, id",
    )
    .bind(tenant)
    .bind(request)
    .fetch_all(&mut **tx)
    .await?)
}

/// The pending request on a record, locked.
async fn pending(tx: &mut Tx, tenant: Uuid, entity: &str, record: Uuid) -> Result<Option<Request>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select {COLUMNS} from approval_request where tenant_id = $1 and entity = $2 and record_id = $3 and status = 'pending' for update"
    )))
    .bind(tenant)
    .bind(entity)
    .bind(record)
    .fetch_optional(&mut **tx)
    .await?)
}

async fn save_steps(tx: &mut Tx, tenant: Uuid, request: Uuid, steps: &[Step], status: &str) -> Result<()> {
    let ended = status != "pending";
    sqlx::query("update approval_request set steps = $3, status = $4, next_due = $5, ended_at = case when $6 then now() end where tenant_id = $1 and id = $2")
        .bind(tenant)
        .bind(request)
        .bind(serde_json::to_value(steps).map_err(|e| Error::internal(e.to_string()))?)
        .bind(status)
        .bind(if ended { None } else { next_due(steps) })
        .bind(ended)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// What a step's `when` makes of each step on `doc`: waiting, or skipped.
fn first_states(ent: &EffEntity, ctx: &Ctx, doc: &Doc, policy: &ApprovalPolicy) -> Result<Vec<Step>> {
    policy
        .steps
        .iter()
        .map(|s| {
            let asked = match &s.need {
                ApprovalNeed::Expr(src) => Some(count(ent, ctx, doc, src)?),
                _ => None,
            };
            Ok(Step { state: if holds(ent, ctx, doc, s.when.as_deref())? { "waiting" } else { "skipped" }.into(), asked, ..Default::default() })
        })
        .collect()
}

/// The number an expression `need` gives on `doc` (BD-45): a whole number, or a refusal naming the expression.
fn count(ent: &EffEntity, ctx: &Ctx, doc: &Doc, src: &str) -> Result<i64> {
    let view = ent.cel_view(&Value::Object(doc.clone()), &ctx.schema);
    let e = Expr::compile(src).map_err(Error::invalid)?;
    let v =
        e.eval(&Scope { doc: &view, old: None, parent: None, user: &ctx.user_value() }).map_err(|m| Error::invalid(format!("approval need '{src}': {m}")))?;
    v.as_i64()
        .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .ok_or_else(|| Error::invalid(format!("approval need '{src}' gave {v}, not a whole number")))
}

/// Opens the first stage whose steps are not all done, unless one of it is open already: its waiting steps get their
/// approvers (refused when a step has fewer than it needs), their due moments, and a notice to each. Whether every
/// step is done.
async fn open_next(k: &Kernel, ctx: &Ctx, tx: &mut Tx, on: &On<'_>, steps: &mut [Step]) -> Result<bool> {
    let now = k.now();
    for range in stages(on.policy) {
        if steps[range.clone()].iter().all(|s| s.is("skipped") || s.is("approved")) {
            continue;
        }
        if steps[range.clone()].iter().any(|s| s.is("open")) {
            return Ok(false);
        }
        let decided = decisions(tx, ctx.tenant, on.request).await?;
        for i in range {
            if !steps[i].is("waiting") {
                continue;
            }
            let def = &on.policy.steps[i];
            let mut who = resolve(ctx, tx, on.ent, on.doc, &def.approvers).await?;
            let out = excluded(tx, ctx, on, i, &decided).await?;
            who.retain(|u| !out.contains(u));
            let need = match &def.need {
                ApprovalNeed::Count(n) => *n as usize,
                ApprovalNeed::Word(NeedWord::All) => who.len(),
                ApprovalNeed::Word(NeedWord::Any) => 1,
                // As the request was made, between one and whoever may decide it.
                ApprovalNeed::Expr(_) => (steps[i].asked.unwrap_or(1).max(1) as usize).min(who.len().max(1)),
            };
            if need == 0 || who.len() < need {
                return Err(Error::Validation(vec![FieldError::new(
                    "approval",
                    format!("approval step {} ({}): {} may decide it, and it needs {}", i + 1, def.label, who.len(), need.max(1)),
                )]));
            }
            let s = &mut steps[i];
            s.remind_at = due(tx, ctx.tenant, def.calendar.as_deref(), now, def.remind_after.as_deref()).await?;
            s.escalate_at = due(tx, ctx.tenant, def.calendar.as_deref(), now, def.escalate_after.as_deref()).await?;
            (s.state, s.approvers, s.need, s.opened_at) = ("open".into(), who, need, Some(now));
            let title = format!("Waiting for your approval: {} {} ({})", on.action, crate::collab::record_title(ctx, &on.ent.def.name, on.doc), def.label);
            tell(k, tx, ctx, (on.policy.message.as_deref(), "waiting"), &s.approvers, &title, &on.ent.def.name, on.record).await?;
        }
        return Ok(false);
    }
    Ok(true)
}

impl Kernel {
    /// Asks for approval of `action` of record `id` (`doc`, locked) under `policy`, as `ctx` (who took it, its gates
    /// passed): a workflow transition from state `from`, with `document` the pipeline's submit or cancel, or with
    /// `on_action` the entity's action of that definition (its secret parameters sealed). A request with its first
    /// stage open, the record left where it is. `None` when every step's `when` is false: the action goes through at
    /// once.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn request_approval(
        &self,
        ctx: &Ctx,
        tx: &mut Tx,
        ent: &EffEntity,
        id: Uuid,
        doc: &Doc,
        (action, from, document): (&str, &str, bool),
        policy: &ApprovalPolicy,
        params: &Value,
        on_action: Option<&ferrum_meta::ActionDef>,
    ) -> Result<Option<Value>> {
        if let Some(r) = pending(tx, ctx.tenant, &ent.def.name, id).await? {
            return Err(Error::Conflict(format!(
                "this {} is awaiting approval of '{}': it is decided or recalled first",
                ent.def.label.to_lowercase(),
                r.action
            )));
        }
        let mut steps = first_states(ent, ctx, doc, policy)?;
        if steps.iter().all(|s| s.is("skipped")) {
            return Ok(None);
        }
        let request = Uuid::now_v7();
        let params = if params.is_null() { &json!({}) } else { params };
        let (params, sealed) = match on_action {
            Some(def) => {
                let (shown, sealed) = seal_secrets(self, ctx.tenant, request, def, params)?;
                (shown, Some(sealed))
            }
            None => (params.clone(), None),
        };
        let params = &params;
        let on = On { ent, record: id, doc, request, requester: ctx.user, action, policy };
        open_next(self, ctx, tx, &on, &mut steps).await?;
        let steps_json = serde_json::to_value(&steps).map_err(|e| Error::internal(e.to_string()))?;
        sqlx::query(
            "insert into approval_request (tenant_id, id, entity, record_id, action, from_state, requester, params, policy, steps, next_due, document, on_action, sealed) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(ctx.tenant)
        .bind(request)
        .bind(&ent.def.name)
        .bind(id)
        .bind(action)
        .bind(from)
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .bind(params)
        .bind(serde_json::to_value(policy).map_err(|e| Error::internal(e.to_string()))?)
        .bind(&steps_json)
        .bind(next_due(&steps))
        .bind(document)
        .bind(on_action.is_some())
        .bind(&sealed)
        .execute(&mut **tx)
        .await?;
        crate::audit::record(tx, ctx, &ent.def.name, id, "approval:requested", None, json!({"request": request, "action": action, "params": params})).await?;
        let r = by_id(tx, ctx.tenant, request).await?;
        Ok(Some(summary(tx, ctx.tenant, ent, &r).await?))
    }

    /// A decision on the pending request of record `id`: `approve`, or not (a reason is then needed in `comment`), by
    /// `ctx` as an approver of an open step, or for one they stand in for. The last approval completes the transition.
    /// The request as it now stands.
    pub async fn decide_approval(&self, ctx: &Ctx, entity: &str, id: Uuid, approve: bool, comment: Option<&str>) -> Result<Value> {
        with_retry(|| async {
            let mut tx = self.db.tenant_tx(ctx.tenant).await?;
            let out = self.decide_tx(ctx, &mut tx, entity, id, approve, comment).await?;
            tx.commit().await?;
            Ok(out)
        })
        .await
    }

    async fn decide_tx(&self, ctx: &Ctx, tx: &mut Tx, entity: &str, id: Uuid, approve: bool, comment: Option<&str>) -> Result<Value> {
        let ent = self.entity(ctx, entity)?;
        let held = crate::settings::with(self, ctx, tx, ctx.schema.settings_read(&ent)).await?;
        let ctx: &Ctx = &held;
        let comment = comment.map(str::trim).filter(|c| !c.is_empty());
        if !approve && comment.is_none() {
            return Err(Error::field("comment", "a reason is needed to reject"));
        }
        if comment.is_some_and(|c| c.chars().count() > 2000) {
            return Err(Error::field("comment", "at most 2,000 characters"));
        }
        // The record first, then its request: every path takes them in this order.
        let doc = self.get_tx(&crate::actions::system(ctx), tx, entity, id, true).await?;
        let req = pending(tx, ctx.tenant, entity, id).await?.ok_or_else(|| Error::NotFound("an approval awaiting a decision on this record".into()))?;
        let (policy, mut steps) = req.parts()?;
        let state = doc.get("workflow_state").and_then(Value::as_str).unwrap_or_default();
        // An action's request is not tied to where the record stands: the action's own conditions decide when it runs.
        if !req.on_action && state != req.from_state {
            return Err(Error::Conflict(format!("the record moved on from {} since approval was asked", req.from_state)));
        }
        let requester = req.requester.unwrap_or_default();
        let decided = decisions(tx, ctx.tenant, req.id).await?;
        let standing = standing_for(tx, ctx).await?;
        // The open step this decision is for, and whom it counts for: themselves, else someone they stand in for.
        let mine = |i: usize, person: Uuid| !decided.iter().any(|d| d.step as usize == i && !d.void && d.person() == person);
        let chosen = steps.iter().enumerate().filter(|(_, s)| s.is("open")).find_map(|(i, s)| {
            let me = (s.may(ctx.user) && mine(i, ctx.user)).then_some((i, ctx.user, None));
            me.or_else(|| standing.iter().find(|p| s.may(**p) && mine(i, **p)).map(|p| (i, *p, Some(*p))))
        });
        let Some((i, person, behalf)) = chosen else {
            let mine_once = steps.iter().any(|s| s.is("open") && (s.may(ctx.user) || standing.iter().any(|p| s.may(*p))));
            return Err(if mine_once {
                Error::Conflict("you have already decided this step".into())
            } else {
                Error::Forbidden("this approval is not yours to decide".into())
            });
        };
        // Independence and who may decide, worked out again now.
        let on = On { ent: &ent, record: id, doc: &doc, request: req.id, requester, action: &req.action, policy: &policy };
        let out = excluded(tx, ctx, &on, i, &decided).await?;
        if out.contains(&ctx.user) || out.contains(&person) {
            return Err(Error::Forbidden(if ctx.user == requester || person == requester {
                "someone else has to approve this: it was asked for by you, or by whom you stand in for".into()
            } else {
                "someone independent of this record decides this step".into()
            }));
        }
        let def = &policy.steps[i];
        let by_escalation = !steps[i].approvers.contains(&person);
        let spec = if by_escalation { def.escalate_to.as_ref() } else { Some(&def.approvers) };
        let still = match spec {
            Some(spec) => resolve(ctx, tx, &ent, &doc, spec).await?.contains(&person),
            None => false,
        };
        if !still {
            return Err(Error::Forbidden("you may no longer decide this step".into()));
        }
        sqlx::query(
            "insert into approval_decision (tenant_id, id, request_id, step, decider, on_behalf_of, outcome, comment) values ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(ctx.tenant)
        .bind(Uuid::now_v7())
        .bind(req.id)
        .bind(i as i32)
        .bind(ctx.user)
        .bind(behalf)
        .bind(if approve { "approve" } else { "reject" })
        .bind(comment)
        .execute(&mut **tx)
        .await?;
        let said = json!({"request": req.id, "action": req.action, "step": i + 1, "label": def.label, "comment": comment, "on_behalf_of": behalf});
        crate::audit::record(tx, ctx, entity, id, if approve { "approval:approve" } else { "approval:reject" }, None, said).await?;
        let record_title = crate::collab::record_title(ctx, entity, &doc);
        if !approve {
            steps[i].state = "rejected".into();
            save_steps(tx, ctx.tenant, req.id, &steps, "rejected").await?;
            if let Some(to) = &policy.on_reject {
                let system = self.on_behalf(ctx, req.requester).await;
                self.write_state(&system, tx, &ent, id, &doc, to, "approval:rejected").await?;
            }
            tell(self, tx, ctx, (policy.message.as_deref(), "rejected"), &[requester], &format!("Not approved: {} {record_title}", req.action), entity, id)
                .await?;
        } else {
            let decided = decisions(tx, ctx.tenant, req.id).await?;
            let of: Vec<&Decision> = decided.iter().filter(|d| d.step as usize == i).collect();
            if satisfied(&steps[i], &of, requester, policy.allow_requester) {
                steps[i].state = "approved".into();
            }
            if open_next(self, ctx, tx, &on, &mut steps).await? {
                save_steps(tx, ctx.tenant, req.id, &steps, "approved").await?;
                tell(self, tx, ctx, (policy.message.as_deref(), "approved"), &[requester], &format!("Approved: {} {record_title}", req.action), entity, id)
                    .await?;
                // Once, here: the request is no longer pending, so nothing completes it again.
                if req.document || req.on_action {
                    self.complete_document(ctx, tx, entity, id, &req, &record_title).await?;
                } else {
                    let system = self.on_behalf(ctx, req.requester).await;
                    Box::pin(self.take_tx(&system, tx, entity, id, &req.action, None, &req.params, Some(req.id))).await?;
                }
            } else {
                save_steps(tx, ctx.tenant, req.id, &steps, "pending").await?;
            }
        }
        let r = by_id(tx, ctx.tenant, req.id).await?;
        if matches!(r.status.as_str(), "approved" | "rejected" | "failed") {
            let decided = crate::hooks::ApprovalDecided {
                request: r.id,
                action: &r.action,
                outcome: &r.status,
                failure: r.failure.as_deref(),
                requester: r.requester,
                decided_by: ctx.user,
                on_behalf_of: behalf,
                comment,
            };
            self.tell_hooks(ctx, tx, &ent, id, &decided).await?;
        }
        summary(tx, ctx.tenant, &ent, &r).await
    }

    /// The app's hooks on the entity told of an approval's outcome (BD-66), each in a savepoint: one that fails is undone
    /// alone, logged and audited, and the decision stands. A failure worth trying again (a conflict between
    /// transactions) is passed on, so the whole decision is tried again and the hook runs once for the one that commits.
    async fn tell_hooks(&self, ctx: &Ctx, tx: &mut Tx, ent: &std::sync::Arc<EffEntity>, id: Uuid, decided: &crate::hooks::ApprovalDecided<'_>) -> Result<()> {
        let hooks = self.hooks.for_tenant(&ent.def.name, &ctx.schema.packages);
        if hooks.is_empty() {
            return Ok(());
        }
        let doc = self.get_tx(&crate::actions::system(ctx), tx, &ent.def.name, id, true).await?;
        for h in &hooks {
            sqlx::query("savepoint approval_hook").execute(&mut **tx).await?;
            let mut hc = crate::hooks::HookCtx { kernel: self, ctx, tx: &mut *tx, entity: ent };
            match h.approval_decided(&mut hc, &doc, decided).await {
                Ok(()) => {
                    sqlx::query("release savepoint approval_hook").execute(&mut **tx).await?;
                }
                Err(e) if e.is_retryable() => return Err(e),
                Err(e) => {
                    sqlx::query("rollback to savepoint approval_hook").execute(&mut **tx).await?;
                    tracing::warn!(entity = %ent.def.name, record = %id, hook = h.name(), "an app's hook on an approval's outcome failed: {e}");
                    let said = json!({"request": decided.request, "outcome": decided.outcome, "hook": h.name(), "error": e.to_string()});
                    crate::audit::record(tx, ctx, &ent.def.name, id, "approval:hook_failed", None, said).await?;
                }
            }
        }
        Ok(())
    }

    /// Completes an approved submit, cancel or action (`req`) of record `id` as its requester at this moment, not as the
    /// system (an action with the parameters it was asked with, its secret ones unsealed):
    /// their rights, the app's hooks and checks as if they pressed it now (the system, when the system asked). Where
    /// that is refused (their rights gone, a check of the app's), nothing of it stays: the request fails, saying why,
    /// and the requester is told. What may be tried again (a conflict between transactions) is passed on instead.
    async fn complete_document(&self, ctx: &Ctx, tx: &mut Tx, entity: &str, id: Uuid, req: &Request, title: &str) -> Result<()> {
        let who = match req.requester {
            Some(u) => self.load_ctx(ctx.tenant, u, None, ctx.request_id.clone()).await,
            None => Ok(crate::actions::system(&Ctx::system(ctx.tenant, ctx.schema.clone()))),
        };
        sqlx::query("savepoint approval_completes").execute(&mut **tx).await?;
        let done = match who {
            Ok(mut who) => {
                who.depth = ctx.depth;
                if req.on_action {
                    match unsealed(self, ctx.tenant, req) {
                        Ok(params) => {
                            let def = self.entity(&who, entity).ok().and_then(|e| e.def.actions.iter().find(|a| a.name == req.action).cloned());
                            let secrets = def.map(|a| a.secret_values(&params));
                            Box::pin(self.action_given(&who, tx, entity, id, &req.action, &params, true))
                                .await
                                .map(|_| ())
                                .map_err(|e| e.redact(&secrets.unwrap_or_default()))
                        }
                        Err(e) => Err(e),
                    }
                } else if req.action == DocumentAction::Cancel.as_str() {
                    Box::pin(self.cancel_inner(&who, tx, entity, id, None, None)).await.map(|_| ())
                } else {
                    Box::pin(self.submit_inner(&who, tx, entity, id, None, None)).await.map(|_| ())
                }
            }
            Err(e) => Err(e),
        };
        let why = match done {
            Ok(_) => {
                sqlx::query("release savepoint approval_completes").execute(&mut **tx).await?;
                return Ok(());
            }
            Err(e) if e.is_retryable() || matches!(e, Error::Unavailable(_)) => return Err(e),
            Err(e) => e.to_string(),
        };
        sqlx::query("rollback to savepoint approval_completes").execute(&mut **tx).await?;
        sqlx::query("update approval_request set status = 'failed', failure = $3 where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(req.id)
            .bind(&why)
            .execute(&mut **tx)
            .await?;
        crate::audit::record(tx, ctx, entity, id, "approval:failed", None, json!({"request": req.id, "action": req.action, "failure": why})).await?;
        let requester = req.requester.into_iter().collect::<Vec<_>>();
        let message = req.parts().ok().and_then(|(p, _)| p.message);
        tell(self, tx, ctx, (message.as_deref(), "not_done"), &requester, &format!("Approved, but not done: {} {title}: {why}", req.action), entity, id).await
    }

    /// The system on `requester`'s behalf: how a transition completes once approved, and a rejection moves its record.
    async fn on_behalf(&self, ctx: &Ctx, requester: Option<Uuid>) -> Ctx {
        let person = match requester {
            Some(u) => self.load_ctx(ctx.tenant, u, None, ctx.request_id.clone()).await.ok(),
            None => None,
        };
        let mut system = crate::actions::system(person.as_ref().unwrap_or(ctx));
        if person.is_none() {
            system.user = requester.unwrap_or_default();
        }
        system.depth = ctx.depth;
        system
    }

    /// The requester (or an administrator) takes back a pending request on record `id`.
    pub async fn recall_approval(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Result<Value> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let ent = self.entity(ctx, entity)?;
        self.get_tx(&crate::actions::system(ctx), &mut tx, entity, id, true).await?;
        let req = pending(&mut tx, ctx.tenant, entity, id).await?.ok_or_else(|| Error::NotFound("an approval awaiting a decision on this record".into()))?;
        if req.requester != Some(ctx.user) && !ctx.is_admin {
            return Err(Error::Forbidden("whoever asked for it recalls it".into()));
        }
        let (_, steps) = req.parts()?;
        save_steps(&mut tx, ctx.tenant, req.id, &steps, "recalled").await?;
        crate::audit::record(&mut tx, ctx, entity, id, "approval:recalled", None, json!({"request": req.id, "action": req.action})).await?;
        let r = by_id(&mut tx, ctx.tenant, req.id).await?;
        let out = summary(&mut tx, ctx.tenant, &ent, &r).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// A save of record `id` from `old` to `new` while approval is pending: a change to a field the policy names
    /// cancels the decisions so far and opens its steps again; other edits leave them, or, the policy locking the
    /// record, are refused.
    pub(crate) async fn approval_record_changed(&self, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, id: Uuid, old: &Doc, new: &Doc) -> Result<()> {
        let watched = |p: &ApprovalPolicy| !p.reset_on.is_empty() || p.lock;
        let on_transitions = ent.workflow.iter().flat_map(|w| &w.def.transitions).filter_map(|t| t.approval.as_ref()).any(watched);
        if !on_transitions && !ent.document_approvals.values().any(|a| watched(&a.policy)) {
            return Ok(());
        }
        let Some(req) = pending(tx, ctx.tenant, &ent.def.name, id).await? else { return Ok(()) };
        let (policy, _) = req.parts()?;
        if policy.lock {
            // What the record says, not what the server works out from it (a total of its lines, a formula).
            let derived = |f: &ferrum_meta::FieldDef| f.computed.is_some() || f.rollup.is_some() || f.fetch_from.is_some();
            let edited = ent.def.fields.iter().filter(|f| !derived(f) && !policy.reset_on.contains(&f.name)).find(|f| old.get(&f.name) != new.get(&f.name));
            if let Some(f) = edited {
                return Err(Error::Conflict(format!(
                    "this {} is awaiting approval of '{}' and stays as it is meanwhile ({} would change): recall it first",
                    ent.def.label.to_lowercase(),
                    req.action,
                    f.label.as_deref().unwrap_or(&f.name)
                )));
            }
        }
        let changed: Vec<&String> = policy.reset_on.iter().filter(|f| old.get(*f) != new.get(*f)).collect();
        if changed.is_empty() {
            return Ok(());
        }
        sqlx::query("update approval_decision set void = true where tenant_id = $1 and request_id = $2 and not void")
            .bind(ctx.tenant)
            .bind(req.id)
            .execute(&mut **tx)
            .await?;
        let mut steps = first_states(ent, ctx, new, &policy)?;
        let status = if steps.iter().all(|s| s.is("skipped")) {
            // Nothing is left to approve: the transition is taken again.
            "cancelled"
        } else {
            let on = On { ent, record: id, doc: new, request: req.id, requester: req.requester.unwrap_or_default(), action: &req.action, policy: &policy };
            open_next(self, ctx, tx, &on, &mut steps).await?;
            "pending"
        };
        save_steps(tx, ctx.tenant, req.id, &steps, status).await?;
        crate::audit::record(tx, ctx, &ent.def.name, id, "approval:reset", None, json!({"request": req.id, "changed": changed})).await
    }

    /// A transition of record `id` completed: a request still pending on it is over.
    pub(crate) async fn approvals_moved(&self, tx: &mut Tx, ctx: &Ctx, ent: &EffEntity, id: Uuid) -> Result<()> {
        if !ent.workflow.as_ref().is_some_and(|w| w.def.transitions.iter().any(|t| t.approval.is_some())) && ent.document_approvals.is_empty() {
            return Ok(());
        }
        sqlx::query(
            "update approval_request set status = 'cancelled', ended_at = now(), next_due = null \
             where tenant_id = $1 and entity = $2 and record_id = $3 and status = 'pending' and not on_action",
        )
        .bind(ctx.tenant)
        .bind(&ent.def.name)
        .bind(id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Record `id` as `ctx` reads it ([`Kernel::get`]), or, where that is refused, as an approver reads the one record
    /// awaiting their decision (BD-19): with its lines, its fields as their own field groups allow and those the
    /// policy's `show` lists. Only this read, the record's history and its files' downloads go through here; lists,
    /// searches, writes and other actions still refuse them.
    pub async fn read_record(&self, ctx: &Ctx, entity: &str, id: Uuid, with_titles: bool) -> Result<Doc> {
        let refused = match self.get(ctx, entity, id, with_titles).await {
            Err(e @ (Error::NotFound(_) | Error::Forbidden(_))) => e,
            other => return other,
        };
        // Approvers are the workspace's members; what goes on to an AI provider is read as the person reads.
        if ctx.outside || ctx.egress.is_some() || ctx.is_system {
            return Err(refused);
        }
        let ent = self.entity(ctx, entity)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let Some(policy) = awaiting(&mut tx, ctx, entity, id).await? else {
            return Err(refused);
        };
        let full = self.get_tx(&crate::actions::system(ctx), &mut tx, entity, id, false).await?;
        let mut doc = self.mask(ctx, &ent, full.clone());
        for f in &policy.show {
            if let Some(v) = full.get(f) {
                doc.insert(f.clone(), v.clone());
            }
        }
        if with_titles {
            self.add_titles(ctx, &mut tx, &ent, std::slice::from_mut(&mut doc)).await?;
        }
        tx.commit().await?;
        Ok(doc)
    }

    /// The latest approval request on record `id`, with its steps and decisions (null when there has been none), for
    /// whoever may read the record, its requester and its approvers.
    pub async fn approval_of(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Result<Value> {
        let ent = self.entity(ctx, entity)?;
        let reads = match self.get(ctx, entity, id, false).await {
            Ok(_) => true,
            Err(Error::NotFound(_) | Error::Forbidden(_)) => false,
            Err(e) => return Err(e),
        };
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let r: Option<Request> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select {COLUMNS} from approval_request where tenant_id = $1 and entity = $2 and record_id = $3 order by created_at desc limit 1"
        )))
        .bind(ctx.tenant)
        .bind(entity)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(r) = r else {
            tx.commit().await?;
            return if reads { Ok(Value::Null) } else { Err(Error::NotFound(ent.def.label.clone())) };
        };
        let standing = standing_for(&mut tx, ctx).await?;
        let (_, steps) = r.parts()?;
        let involved = r.requester == Some(ctx.user) || steps.iter().any(|s| s.may(ctx.user) || standing.iter().any(|p| s.may(*p)));
        if !reads && !involved {
            return Err(Error::NotFound(ent.def.label.clone()));
        }
        let out = summary(&mut tx, ctx.tenant, &ent, &r).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// `ctx`'s approvals: those open steps await from them or from whom they stand in for, not decided yet
    /// (`to_decide`), and their own requests still pending (`awaiting_others`), newest first, at most `limit` each.
    pub async fn my_approval_requests(&self, ctx: &Ctx, limit: i64) -> Result<Value> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let mut people: Vec<String> = standing_for(&mut tx, ctx).await?.iter().map(Uuid::to_string).collect();
        people.push(ctx.user.to_string());
        let to_decide: Vec<Request> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select {COLUMNS} from approval_request r where tenant_id = $1 and status = 'pending' and exists ( \
               select 1 from jsonb_array_elements(r.steps) with ordinality s(v, n) where v ->> 'state' = 'open' \
                 and ((v -> 'approvers') ?| $2 or (v -> 'escalated') ?| $2) \
                 and not exists (select 1 from approval_decision d where d.tenant_id = r.tenant_id and d.request_id = r.id \
                   and d.step = n - 1 and not d.void and d.decider = $3)) \
             order by created_at desc limit $4"
        )))
        .bind(ctx.tenant)
        .bind(&people)
        .bind(ctx.user)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        let awaiting: Vec<Request> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select {COLUMNS} from approval_request where tenant_id = $1 and status = 'pending' and requester = $2 order by created_at desc limit $3"
        )))
        .bind(ctx.tenant)
        .bind(ctx.user)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        let asked_by: Vec<Uuid> = to_decide.iter().chain(&awaiting).filter_map(|r| r.requester).collect();
        let names: std::collections::HashMap<Uuid, String> =
            sqlx::query_as::<_, (Uuid, String)>("select id, full_name from app_user where tenant_id = $1 and id = any($2)")
                .bind(ctx.tenant)
                .bind(&asked_by)
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .collect();
        let standing: Vec<Uuid> = people.iter().filter_map(|p| p.parse().ok()).collect();
        let mut lists = Vec::new();
        for (mine, rows) in [(true, to_decide), (false, awaiting)] {
            let mut out = Vec::new();
            for r in rows {
                let Some(ent) = ctx.schema.entity(&r.entity).cloned() else { continue };
                // Named as a notice names it: whoever decides learns what they decide on.
                let doc = self.get_tx(&crate::actions::system(ctx), &mut tx, &r.entity, r.record_id, false).await.unwrap_or_default();
                // The open step: the one awaiting the caller in their list, else the first still open.
                let (policy, steps) = r.parts()?;
                let open = steps.iter().enumerate().filter(|(_, s)| s.is("open"));
                let step = open
                    .clone()
                    .find(|(_, s)| !mine || standing.iter().any(|p| s.may(*p)))
                    .or(open.clone().next())
                    .map(|(i, s)| json!({"index": i, "label": policy.steps.get(i).map(|d| d.label.as_str()), "opened_at": s.opened_at}));
                out.push(json!({
                    "request": r.id, "entity": r.entity, "entity_label": ent.def.label, "record": r.record_id,
                    "title": crate::collab::record_title(ctx, &r.entity, &doc), "action": r.action, "from": r.from_state,
                    "requester": r.requester, "requester_name": r.requester.and_then(|u| names.get(&u)), "requested_at": r.created_at,
                    "step": step,
                }));
            }
            lists.push(Value::Array(out));
        }
        tx.commit().await?;
        let [to_decide, awaiting_others] = <[Value; 2]>::try_from(lists).map_err(|_| Error::internal("two lists"))?;
        Ok(json!({"to_decide": to_decide, "awaiting_others": awaiting_others}))
    }

    /// One pass of the minute loop's approval reminders and escalations at the clock `now`: each threshold passed is
    /// acted on once (the request is locked and its step marked in the same transaction). How many requests it moved.
    pub async fn sweep_approvals_at(&self, now: DateTime<Utc>) -> Result<usize> {
        if self.db.fenced() || self.holding().await? {
            return Ok(0);
        }
        let mut tx = self.db.system_tx().await?;
        let due: Vec<(Uuid, Uuid, String, Uuid)> = sqlx::query_as(
            "select tenant_id, id, entity, record_id from approval_request where status = 'pending' and next_due <= $1 order by next_due limit $2",
        )
        .bind(now)
        .bind(SWEEP_BATCH)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut moved = 0;
        for (tenant, request, entity, record) in due {
            match self.nudge(tenant, request, &entity, record, now).await {
                Ok(true) => moved += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(%tenant, %request, "approval reminder or escalation not done: {e}"),
            }
        }
        Ok(moved)
    }

    async fn nudge(&self, tenant: Uuid, request: Uuid, entity: &str, record: Uuid, now: DateTime<Utc>) -> Result<bool> {
        let schema = self.registry.schema(tenant).await?;
        let Some(ent) = schema.entity(entity).cloned() else { return Ok(false) };
        let system = Ctx::system(tenant, schema.clone());
        let mut tx = self.db.tenant_tx(tenant).await?;
        let ctx = crate::settings::with(self, &system, &mut tx, schema.settings_read(&ent)).await?.into_owned();
        let doc = match self.get_tx(&ctx, &mut tx, entity, record, true).await {
            Ok(d) => d,
            Err(Error::NotFound(_)) => return Ok(false),
            Err(e) => return Err(e),
        };
        let Some(req) = pending(&mut tx, tenant, entity, record).await?.filter(|r| r.id == request) else { return Ok(false) };
        let (policy, mut steps) = req.parts()?;
        let decided = decisions(&mut tx, tenant, req.id).await?;
        let on = On { ent: &ent, record, doc: &doc, request: req.id, requester: req.requester.unwrap_or_default(), action: &req.action, policy: &policy };
        let title = crate::collab::record_title(&ctx, entity, &doc);
        let mut did = false;
        for i in 0..steps.len() {
            if !steps[i].is("open") {
                continue;
            }
            let def = &policy.steps[i];
            if steps[i].remind_at.is_some_and(|t| t <= now) && steps[i].reminded_at.is_none() {
                steps[i].reminded_at = Some(now);
                let waiting: Vec<Uuid> = steps[i]
                    .approvers
                    .iter()
                    .chain(&steps[i].escalated)
                    .filter(|p| !decided.iter().any(|d| d.step as usize == i && !d.void && d.person() == **p))
                    .copied()
                    .collect();
                let said = format!("Reminder, waiting for your approval: {} {title} ({})", req.action, def.label);
                tell(self, &mut tx, &ctx, (policy.message.as_deref(), "reminder"), &waiting, &said, entity, record).await?;
                crate::audit::record(&mut tx, &ctx, entity, record, "approval:reminded", None, json!({"request": req.id, "step": i + 1})).await?;
                did = true;
            }
            if steps[i].escalate_at.is_some_and(|t| t <= now) && steps[i].escalated_at.is_none() {
                steps[i].escalated_at = Some(now);
                if let Some(to) = &def.escalate_to {
                    let out = excluded(&mut tx, &ctx, &on, i, &decided).await?;
                    let more: Vec<Uuid> = resolve(&ctx, &mut tx, &ent, &doc, to).await?.into_iter().filter(|u| !out.contains(u) && !steps[i].may(*u)).collect();
                    let said = format!("Escalated to you for approval: {} {title} ({})", req.action, def.label);
                    tell(self, &mut tx, &ctx, (policy.message.as_deref(), "escalated"), &more, &said, entity, record).await?;
                    steps[i].escalated.extend(more);
                }
                crate::audit::record(
                    &mut tx,
                    &ctx,
                    entity,
                    record,
                    "approval:escalated",
                    None,
                    json!({"request": req.id, "step": i + 1, "to": steps[i].escalated}),
                )
                .await?;
                did = true;
            }
        }
        save_steps(&mut tx, tenant, req.id, &steps, "pending").await?;
        tx.commit().await?;
        Ok(did)
    }
}

async fn by_id(tx: &mut Tx, tenant: Uuid, id: Uuid) -> Result<Request> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!("select {COLUMNS} from approval_request where tenant_id = $1 and id = $2")))
        .bind(tenant)
        .bind(id)
        .fetch_one(&mut **tx)
        .await?)
}

/// A request as the record shows it: whose, its steps with who may decide each, and every decision.
async fn summary(tx: &mut Tx, tenant: Uuid, ent: &EffEntity, r: &Request) -> Result<Value> {
    let (policy, steps) = r.parts()?;
    let decided = decisions(tx, tenant, r.id).await?;
    let to = ent
        .workflow
        .as_ref()
        .filter(|_| !r.document)
        .and_then(|w| w.def.transitions.iter().find(|t| t.action == r.action && t.from == r.from_state))
        .map(|t| t.to.clone());
    let steps: Vec<Value> = steps
        .iter()
        .zip(&policy.steps)
        .map(|(s, d)| {
            json!({"label": d.label, "state": s.state, "need": s.need, "approvers": s.approvers, "escalated": s.escalated,
                   "opened_at": s.opened_at, "remind_at": s.remind_at, "escalate_at": s.escalate_at,
                   "reminded_at": s.reminded_at, "escalated_at": s.escalated_at})
        })
        .collect();
    Ok(json!({
        "id": r.id, "entity": r.entity, "record": r.record_id, "action": r.action, "from": r.from_state, "to": to,
        "requester": r.requester, "requested_at": r.created_at, "status": r.status, "params": r.params,
        "steps": steps, "decisions": decided, "document": r.document, "on_action": r.on_action, "failure": r.failure,
    }))
}

/// Refuses customizations that leave an approval naming a role or a business calendar the workspace does not have,
/// that `was` did not name already, saying which transition and step.
pub(crate) async fn check_names(tx: &mut Tx, tenant: Uuid, was: &Schema, now: &Schema) -> Result<()> {
    // (entity, where, step, kind, name)
    let named = |s: &Schema| -> Vec<(String, String, usize, &'static str, String)> {
        let mut out = Vec::new();
        for e in s.entities.values() {
            let on_transitions =
                e.workflow.iter().flat_map(|w| &w.def.transitions).filter_map(|t| Some((format!("transition {}", t.action), t.approval.as_ref()?)));
            let on_documents = e.document_approvals.iter().map(|(on, a)| (on.key(), &a.policy));
            for (action, p) in on_transitions.chain(on_documents) {
                for (i, st) in p.steps.iter().enumerate() {
                    let roles = st.approvers.role.iter().chain(st.escalate_to.iter().flat_map(|x| x.role.iter()));
                    for r in roles {
                        out.push((e.def.name.clone(), action.clone(), i, "role", r.clone()));
                    }
                    if let Some(c) = &st.calendar {
                        out.push((e.def.name.clone(), action.clone(), i, "calendar", c.clone()));
                    }
                }
            }
        }
        out
    };
    let before: BTreeSet<(&'static str, String)> = named(was).into_iter().map(|n| (n.3, n.4)).collect();
    for (entity, action, i, kind, name) in named(now).into_iter().filter(|n| !before.contains(&(n.3, n.4.clone()))) {
        let sql = if kind == "role" {
            "select exists (select 1 from role where tenant_id = $1 and name = $2)"
        } else {
            "select exists (select 1 from logic_calendar where tenant_id = $1 and name = $2)"
        };
        let there: bool = sqlx::query_scalar(sql).bind(tenant).bind(&name).fetch_one(&mut **tx).await?;
        if !there {
            return Err(Error::invalid(format!("customization rejected: {entity}: {action}, approval step {}: there is no {kind} {name}", i + 1)));
        }
    }
    Ok(())
}
