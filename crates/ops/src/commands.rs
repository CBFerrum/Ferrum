//! The command ledger. A command is an intent written in the business transaction, so it exists
//! only if the document that wanted it commits, and it leaves through the site runtime's call, never
//! through a socket. What became of it is reported, not assumed: see `ferrum_ops_contract::command`.

use crate::{devices, emit_people};
use chrono::{DateTime, Utc};
use ferrum_domain::util::sys;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, HookCtx, Kernel, Result, Tx, audit, outbox};
use ferrum_ops_contract::command::{Report, payload_hash, step};
use ferrum_ops_contract::{Command, State, Step};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

/// The longest a command may wait. Work planned further ahead is issued nearer the time.
pub const MAX_LIFETIME_HOURS: i64 = 7 * 24;

/// Who wants this. Software other than a person's request goes through an envelope or a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Issuer {
    /// A person asked, through the ledger's own endpoints.
    #[default]
    Person,
    /// An app's engine, on behalf of what a person did there (releasing a wave).
    Engine,
    /// A rule or schedule.
    Automation,
    /// An AI agent. It proposes; a person or an approved envelope decides.
    Ai,
}

impl Issuer {
    fn as_str(self) -> &'static str {
        match self {
            Issuer::Person => "person",
            Issuer::Engine => "engine",
            Issuer::Automation => "automation",
            Issuer::Ai => "ai",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NewCommand {
    pub target: Uuid,
    pub operation: String,
    #[serde(default = "one")]
    pub schema_version: i32,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub expected_state: Value,
    pub expires_in_secs: i64,
    /// The same key names the same intent: issuing it again returns the first.
    #[serde(default)]
    pub dedupe_key: Option<String>,
    #[serde(default)]
    pub workflow_id: Option<String>,
    #[serde(default)]
    pub step_id: Option<String>,
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub issuer: Issuer,
}

fn one() -> i32 {
    1
}

#[derive(Debug, Clone, PartialEq)]
pub struct Issued {
    pub id: Uuid,
    pub state: State,
    /// It was issued before: nothing new was written.
    pub repeated: bool,
}

fn topic(state: State) -> String {
    format!("ops.command.{}", state.as_str().to_lowercase())
}

/// What subscribers hear about a state change. Apps follow their own work by `workflow_id`.
#[allow(clippy::too_many_arguments)]
async fn announce(tx: &mut Tx, tenant: Uuid, id: Uuid, state: State, row: &Row, outcome: &Value) -> Result<()> {
    let payload = json!({
        "command": id, "state": state.as_str(), "site": row.site_id, "domain": row.domain, "target": row.target_id, "operation": row.operation,
        "workflow_id": row.workflow_id, "step_id": row.step_id, "correlation_id": row.correlation_id, "outcome": outcome,
    });
    outbox::enqueue(tx, tenant, &topic(state), None, Some(id), payload).await?;
    if matches!(state, State::Proposed | State::OutcomeUnknown) {
        let role = if state == State::Proposed { "Ops Manager" } else { "Ops Recovery" };
        let title = if state == State::Proposed { format!("{} needs approval", row.operation) } else { format!("Nobody knows how {} ended", row.operation) };
        emit_people(tx, tenant, role, &title, row.target_id).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn record(
    tx: &mut Tx,
    tenant: Uuid,
    id: Uuid,
    report: Option<Uuid>,
    state: State,
    step: Step,
    at_source: Option<DateTime<Utc>>,
    actor: Option<Uuid>,
    epoch: Option<i64>,
    detail: &Value,
    evidence: Option<&Value>,
) -> Result<()> {
    let step = match step {
        Step::Move => "move",
        Step::Keep => "keep",
        Step::Conflict => "conflict",
        Step::Refuse => "refuse",
    };
    sqlx::query(
        "insert into op_command_event (tenant_id, command_id, report_id, state, step, at_source, actor, epoch, detail, evidence) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(tenant)
    .bind(id)
    .bind(report)
    .bind(state.as_str())
    .bind(step)
    .bind(at_source)
    .bind(actor)
    .bind(epoch)
    .bind(detail)
    .bind(evidence)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn person(ctx: &Ctx) -> Option<Uuid> {
    (!ctx.user.is_nil()).then_some(ctx.user)
}

/// One limit an envelope sets on a payload.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Limit {
    pub parameter: String,
    pub min_value: Option<f64>,
    pub max_value: Option<f64>,
    pub allowed: Option<String>,
}

impl Limit {
    /// Why `payload` is outside this limit, if it is. A parameter that is missing or cannot be read
    /// is outside: nobody approved what cannot be judged.
    pub fn outside(&self, payload: &Value) -> Option<String> {
        let name = &self.parameter;
        let Some(value) = name.split('.').try_fold(payload, |v, key| v.get(key)).filter(|v| !v.is_null()) else {
            return Some(format!("it does not say {name}"));
        };
        let text = value.as_str().map_or_else(|| value.to_string(), str::to_string);
        if let Some(list) = self.allowed.as_deref().filter(|l| !l.trim().is_empty())
            && !list.split(',').any(|v| v.trim() == text)
        {
            return Some(format!("{name} {text} is not one of those approved"));
        }
        if self.min_value.is_some() || self.max_value.is_some() {
            let Some(n) = value.as_f64().or_else(|| text.parse().ok()).filter(|n: &f64| n.is_finite()) else {
                return Some(format!("{name} {text} is not a number"));
            };
            if self.min_value.is_some_and(|min| n < min) || self.max_value.is_some_and(|max| n > max) {
                return Some(format!("{name} {text} is outside its limits"));
            }
        }
        None
    }
}

/// Whether software may do this on its own: the envelope's reference, or which limit it is outside
/// of, for the person who decides instead. The envelope is locked, so two commands at once cannot
/// both take the last place.
async fn envelope(tx: &mut Tx, tenant: Uuid, target: &devices::Target, cmd: &NewCommand) -> Result<std::result::Result<String, String>> {
    type Row = (Uuid, i64, Option<bool>, Option<i64>, Option<String>, Option<String>, Option<i64>);
    let row: Option<Row> = sqlx::query_as(
        "select id, max_per_hour, agents, max_in_progress, nullif(equipment_state, ''), nullif(evidence_metric, ''), evidence_max_age_secs \
         from e_automation_envelope where tenant_id = $1 and site = $2 and operation = $3 and enabled and valid_until > now() for update",
    )
    .bind(tenant)
    .bind(target.site)
    .bind(&cmd.operation)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((id, max_per_hour, agents, max_in_progress, equipment_state, evidence_metric, evidence_age)) = row else {
        return Ok(Err("no envelope in force covers this".into()));
    };
    if cmd.issuer == Issuer::Ai && agents != Some(true) {
        return Ok(Err("this envelope does not cover agents".into()));
    }
    let listed: Vec<Uuid> = sqlx::query_scalar("select device from e_automation_envelope__devices where tenant_id = $1 and parent_id = $2")
        .bind(tenant)
        .bind(id)
        .fetch_all(&mut **tx)
        .await?;
    if !listed.is_empty() && !listed.contains(&target.id) {
        return Ok(Err(format!("{} is not one of its devices", target.code)));
    }
    let limits: Vec<Limit> = sqlx::query_as(
        "select parameter, min_value::float8 as min_value, max_value::float8 as max_value, allowed from e_automation_envelope__limits where tenant_id = $1 and parent_id = $2",
    )
    .bind(tenant)
    .bind(id)
    .fetch_all(&mut **tx)
    .await?;
    if let Some(why) = limits.iter().find_map(|l| l.outside(&cmd.payload)) {
        return Ok(Err(why));
    }
    if let Some(wanted) = equipment_state {
        let last: Option<String> = sqlx::query_scalar("select state from op_equipment_event where tenant_id = $1 and device_id = $2 order by at desc limit 1")
            .bind(tenant)
            .bind(target.id)
            .fetch_optional(&mut **tx)
            .await?;
        if last.as_deref() != Some(wanted.as_str()) {
            return Ok(Err(format!("{} is not known to be {wanted}", target.code)));
        }
    }
    if let (Some(metric), Some(age)) = (evidence_metric, evidence_age) {
        let fresh: Option<i32> = sqlx::query_scalar(
            "select 1 from op_measurement where tenant_id = $1 and device_id = $2 and metric = $3 and source_time > now() - make_interval(secs => $4) limit 1",
        )
        .bind(tenant)
        .bind(target.id)
        .bind(&metric)
        .bind(age as f64)
        .fetch_optional(&mut **tx)
        .await?;
        if fresh.is_none() {
            return Ok(Err(format!("nothing recent says what {metric} is")));
        }
    }
    let reference = format!("envelope:{id}");
    let (this_hour, unfinished): (i64, i64) = sqlx::query_as(
        "select count(*) filter (where issued_at > now() - interval '1 hour'), count(*) filter (where state in ('Queued', 'Accepted', 'Executing', 'OutcomeUnknown')) \
         from op_command where tenant_id = $1 and approval_ref = $2",
    )
    .bind(tenant)
    .bind(&reference)
    .fetch_one(&mut **tx)
    .await?;
    if this_hour >= max_per_hour {
        return Ok(Err(format!("this envelope allows {max_per_hour} an hour")));
    }
    if max_in_progress.is_some_and(|max| unfinished >= max) {
        return Ok(Err(format!("{unfinished} under this envelope have not finished")));
    }
    Ok(Ok(reference))
}

/// Whoever turns an envelope on approved it, and what it allowed at that moment stays on record.
pub struct EnvelopeHook;

#[async_trait::async_trait]
impl DocHook for EnvelopeHook {
    fn name(&self) -> &'static str {
        "ops.envelope"
    }

    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, _old: Option<&Doc>) -> Result<()> {
        if doc.get("enabled").and_then(Value::as_bool) != Some(true) {
            return Ok(());
        }
        let id = doc.get("id").and_then(Value::as_str).and_then(|s| s.parse().ok()).ok_or_else(|| Error::internal("an envelope without an id"))?;
        let mut values = Doc::new();
        values.insert("approved_by".into(), json!(hc.ctx.email));
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "AutomationEnvelope", id, values).await?;
        audit::record(hc.tx, hc.ctx, "AutomationEnvelope", id, "ops.envelope_approved", None, Value::Object(doc.clone())).await
    }
}

/// A command already written for this intent or this physical thing.
async fn existing(tx: &mut Tx, tenant: Uuid, cmd: &NewCommand, exclusive: Option<&str>) -> Result<Option<Issued>> {
    if cmd.dedupe_key.is_none() && exclusive.is_none() {
        return Ok(None);
    }
    let found: Option<(Uuid, String)> = sqlx::query_as(
        "select id, state from op_command where tenant_id = $1 and (dedupe_key = $2 or (exclusive_key = $3 \
           and state in ('Proposed', 'Authorized', 'Queued', 'Accepted', 'Executing', 'OutcomeUnknown'))) \
         order by dedupe_key = $2 desc nulls last, issued_at desc limit 1",
    )
    .bind(tenant)
    .bind(&cmd.dedupe_key)
    .bind(exclusive)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((id, state)) = found else { return Ok(None) };
    let state = State::parse(&state).ok_or_else(|| Error::internal("a command in an unknown state"))?;
    Ok(Some(Issued { id, state, repeated: true }))
}

/// Writes a command in the caller's transaction. Engines call this for work a person released in
/// their own app; the ledger's endpoints check the caller's role first. Whatever the caller's rights,
/// a device receives only what it was commissioned for, and software proposes rather than decides.
pub async fn issue(tx: &mut Tx, ctx: &Ctx, cmd: NewCommand) -> Result<Issued> {
    write(tx, ctx, cmd, None).await
}

/// [`issue`], for what takes over one physical thing, named by `key`: a warehouse task, a production
/// run. While a command holding that name can still act, or nobody knows whether it did, issuing
/// another returns that one, so two intents cannot be alive for it. Only an engine has one of these
/// to give: no endpoint takes a name from its caller.
pub async fn issue_exclusive(tx: &mut Tx, ctx: &Ctx, cmd: NewCommand, key: &str) -> Result<Issued> {
    write(tx, ctx, cmd, Some(key)).await
}

async fn write(tx: &mut Tx, ctx: &Ctx, cmd: NewCommand, exclusive: Option<&str>) -> Result<Issued> {
    if cmd.operation.is_empty() || cmd.operation.len() > 100 {
        return Err(Error::field("operation", "is required, at most 100 characters"));
    }
    if cmd.expires_in_secs <= 0 || cmd.expires_in_secs > MAX_LIFETIME_HOURS * 3600 {
        return Err(Error::field("expires_in_secs", format!("must be between 1 second and {MAX_LIFETIME_HOURS} hours")));
    }
    let tenant = ctx.tenant;
    if let Some(found) = existing(tx, tenant, &cmd, exclusive).await? {
        return Ok(found);
    }
    let target = devices::target(tx, tenant, cmd.target).await?;
    if !target.lifecycle.takes_commands() {
        return Err(Error::invalid(format!("{} is {} and takes no work", target.code, target.lifecycle.as_str())));
    }
    let Some(allowed) = devices::operation(tx, tenant, cmd.target, &cmd.operation).await? else {
        return Err(Error::invalid(format!("{} was not commissioned for {}", target.code, cmd.operation)));
    };
    if allowed.engine_only && cmd.issuer != Issuer::Engine {
        return Err(Error::invalid(format!("{} on {} goes through the app that owns this work, past its own gates", cmd.operation, target.code)));
    }
    let epoch: Option<i64> = sqlx::query_scalar("select epoch from op_authority where tenant_id = $1 and domain = $2")
        .bind(tenant)
        .bind(&target.domain)
        .fetch_optional(&mut **tx)
        .await?;
    let Some(epoch) = epoch else {
        return Err(Error::invalid(format!("no gateway holds {}, so nothing can reach {}", target.domain, target.code)));
    };
    let (state, approval, outside) = match cmd.issuer {
        Issuer::Person | Issuer::Engine if !allowed.needs_approval => (State::Queued, None, None),
        Issuer::Person | Issuer::Engine => (State::Proposed, None, None),
        // An envelope is a decision made in advance, and this asks for one made each time.
        Issuer::Automation | Issuer::Ai if allowed.needs_approval => (State::Proposed, None, Some("each of these needs a second person".to_string())),
        Issuer::Automation | Issuer::Ai => match envelope(tx, tenant, &target, &cmd).await? {
            Ok(reference) => (State::Queued, Some(reference), None),
            Err(why) => (State::Proposed, None, Some(why)),
        },
    };
    let id = Uuid::now_v7();
    let hash = payload_hash(&cmd.payload);
    // Two at once get past the look above, and these keys are unique: the one that loses says so.
    let written = sqlx::query(
        "insert into op_command (tenant_id, id, site_id, domain, target_id, operation, schema_version, issued_by, issued_kind, approval_ref, epoch, \
         expected_state, expires_at, payload, payload_hash, workflow_id, step_id, correlation_id, config_version, dedupe_key, exclusive_key, state) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, now() + make_interval(secs => $13), $14, $15, $16, $17, $18, $19, $20, $21, $22) \
         on conflict do nothing",
    )
    .bind(tenant)
    .bind(id)
    .bind(target.site)
    .bind(&target.domain)
    .bind(cmd.target)
    .bind(&cmd.operation)
    .bind(cmd.schema_version)
    .bind(person(ctx))
    .bind(cmd.issuer.as_str())
    .bind(approval.as_deref())
    .bind(epoch)
    .bind(&cmd.expected_state)
    .bind(cmd.expires_in_secs as f64)
    .bind(&cmd.payload)
    .bind(&hash)
    .bind(&cmd.workflow_id)
    .bind(&cmd.step_id)
    .bind(&cmd.correlation_id)
    .bind(&target.config_version)
    .bind(&cmd.dedupe_key)
    .bind(exclusive)
    .bind(state.as_str())
    .execute(&mut **tx)
    .await?;
    if written.rows_affected() == 0 {
        return existing(tx, tenant, &cmd, exclusive).await?.ok_or_else(|| Error::Conflict("this was just issued, and has ended: try again".into()));
    }
    if state == State::Queued {
        record(
            tx,
            tenant,
            id,
            None,
            State::Authorized,
            Step::Move,
            None,
            person(ctx),
            Some(epoch),
            &json!({"by": cmd.issuer.as_str(), "approval": approval}),
            None,
        )
        .await?;
    }
    let detail = outside.map_or(Value::Null, |why| json!({"outside": why}));
    record(tx, tenant, id, None, state, Step::Move, None, person(ctx), Some(epoch), &detail, None).await?;
    let row = load(tx, tenant, id).await?;
    announce(tx, tenant, id, state, &row, &Value::Null).await?;
    Ok(Issued { id, state, repeated: false })
}

#[derive(sqlx::FromRow)]
struct Row {
    state: String,
    site_id: Uuid,
    domain: String,
    target_id: Uuid,
    operation: String,
    issued_by: Option<Uuid>,
    sent_at: Option<DateTime<Utc>>,
    conflict: bool,
    workflow_id: Option<String>,
    step_id: Option<String>,
    correlation_id: Option<String>,
}

const ROW: &str = "state, site_id, domain, target_id, operation, issued_by, sent_at, conflict, workflow_id, step_id, correlation_id";

impl Row {
    fn state(&self) -> Result<State> {
        State::parse(&self.state).ok_or_else(|| Error::internal("a command in an unknown state"))
    }
}

async fn load(tx: &mut Tx, tenant: Uuid, id: Uuid) -> Result<Row> {
    let row = sqlx::query_as::<_, Row>(sqlx::AssertSqlSafe(format!("select {ROW} from op_command where tenant_id = $1 and id = $2 for update")))
        .bind(tenant)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    row.ok_or_else(|| Error::NotFound("command".into()))
}

async fn set_state(tx: &mut Tx, tenant: Uuid, id: Uuid, state: State, outcome: Option<&Value>) -> Result<()> {
    sqlx::query("update op_command set state = $3, state_at = now(), outcome = coalesce($4, outcome) where tenant_id = $1 and id = $2")
        .bind(tenant)
        .bind(id)
        .bind(state.as_str())
        .bind(outcome)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// What became of a report from a site.
#[derive(Debug, PartialEq)]
pub enum Applied {
    /// Taken, whether it moved the command or only joined its history.
    Done(Step),
    /// Not taken, for this reason. Recorded where it names a command, so nothing said is lost.
    Refused(String),
}

/// Applies one report from the holder on `epoch`. Safe to repeat, and to deliver in any order.
pub async fn apply(tx: &mut Tx, tenant: Uuid, r: &Report, epoch: i64) -> Result<Applied> {
    let seen: Option<i32> = sqlx::query_scalar("select 1 from op_command_event where tenant_id = $1 and report_id = $2")
        .bind(tenant)
        .bind(r.report_id)
        .fetch_optional(&mut **tx)
        .await?;
    if seen.is_some() {
        return Ok(Applied::Done(Step::Keep));
    }
    let row = match load(tx, tenant, r.command_id).await {
        Ok(row) => row,
        Err(Error::NotFound(_)) => return Ok(Applied::Refused("no such command".into())),
        Err(e) => return Err(e),
    };
    let state = row.state()?;
    if r.epoch != epoch {
        record(
            tx,
            tenant,
            r.command_id,
            Some(r.report_id),
            r.state,
            Step::Refuse,
            Some(r.at_source),
            None,
            Some(r.epoch),
            &json!({"refused": "from an old authority", "detail": r.detail}),
            r.evidence.as_ref(),
        )
        .await?;
        return Ok(Applied::Refused("this authority was replaced".into()));
    }
    let reportable = matches!(
        r.state,
        State::Accepted | State::Rejected | State::Executing | State::Completed | State::Failed | State::Aborted | State::OutcomeUnknown | State::Expired
    );
    if !reportable || row.sent_at.is_none() {
        record(tx, tenant, r.command_id, Some(r.report_id), r.state, Step::Refuse, Some(r.at_source), None, Some(r.epoch), &r.detail, r.evidence.as_ref())
            .await?;
        return Ok(Applied::Refused(format!("{} cannot be reported for this command", r.state.as_str())));
    }
    let taken = step(state, r.state, r.evidence.is_some());
    record(tx, tenant, r.command_id, Some(r.report_id), r.state, taken, Some(r.at_source), None, Some(r.epoch), &r.detail, r.evidence.as_ref()).await?;
    match taken {
        Step::Move => {
            let outcome = json!({"detail": r.detail, "evidence": r.evidence});
            set_state(tx, tenant, r.command_id, r.state, Some(&outcome)).await?;
            announce(tx, tenant, r.command_id, r.state, &row, &outcome).await?;
        }
        Step::Conflict => {
            sqlx::query("update op_command set conflict = true where tenant_id = $1 and id = $2").bind(tenant).bind(r.command_id).execute(&mut **tx).await?;
            emit_people(tx, tenant, "Ops Recovery", &format!("{} has two endings", row.operation), row.target_id).await?;
        }
        Step::Keep | Step::Refuse => {}
    }
    Ok(Applied::Done(taken))
}

/// A person approves what was proposed. Nobody approves their own proposal.
pub async fn approve(tx: &mut Tx, ctx: &Ctx, id: Uuid) -> Result<State> {
    let row = load(tx, ctx.tenant, id).await?;
    if row.state()? != State::Proposed {
        return Err(Error::invalid(format!("this command is {}", row.state)));
    }
    if row.issued_by == person(ctx) && person(ctx).is_some() {
        return Err(Error::Forbidden("someone else has to approve this".into()));
    }
    let reference = format!("user:{}", ctx.user);
    sqlx::query("update op_command set approval_ref = $3 where tenant_id = $1 and id = $2")
        .bind(ctx.tenant)
        .bind(id)
        .bind(&reference)
        .execute(&mut **tx)
        .await?;
    record(tx, ctx.tenant, id, None, State::Authorized, Step::Move, None, person(ctx), None, &json!({"approved_by": ctx.email}), None).await?;
    record(tx, ctx.tenant, id, None, State::Queued, Step::Move, None, person(ctx), None, &Value::Null, None).await?;
    set_state(tx, ctx.tenant, id, State::Queued, None).await?;
    announce(tx, ctx.tenant, id, State::Queued, &row, &Value::Null).await?;
    Ok(State::Queued)
}

pub async fn refuse(tx: &mut Tx, ctx: &Ctx, id: Uuid, reason: &str) -> Result<()> {
    let row = load(tx, ctx.tenant, id).await?;
    if row.state()? != State::Proposed {
        return Err(Error::invalid(format!("this command is {}", row.state)));
    }
    let detail = json!({"reason": reason});
    record(tx, ctx.tenant, id, None, State::Refused, Step::Move, None, person(ctx), None, &detail, None).await?;
    set_state(tx, ctx.tenant, id, State::Refused, Some(&detail)).await?;
    announce(tx, ctx.tenant, id, State::Refused, &row, &detail).await
}

/// Asks for a command to stop. What never left stops here; anything the site may have is only
/// requested, and stays what it is until the site says otherwise.
pub async fn cancel(tx: &mut Tx, ctx: &Ctx, id: Uuid, reason: &str) -> Result<State> {
    let row = load(tx, ctx.tenant, id).await?;
    let state = row.state()?;
    if state.is_final() {
        return Err(Error::invalid(format!("this command is already {}", row.state)));
    }
    let detail = json!({"reason": reason, "by": ctx.email});
    if matches!(state, State::Proposed | State::Authorized) || (state == State::Queued && row.sent_at.is_none()) {
        record(tx, ctx.tenant, id, None, State::Aborted, Step::Move, None, person(ctx), None, &detail, None).await?;
        set_state(tx, ctx.tenant, id, State::Aborted, Some(&detail)).await?;
        announce(tx, ctx.tenant, id, State::Aborted, &row, &detail).await?;
        return Ok(State::Aborted);
    }
    sqlx::query("update op_command set cancel_requested_at = coalesce(cancel_requested_at, now()), cancel_requested_by = $3 where tenant_id = $1 and id = $2")
        .bind(ctx.tenant)
        .bind(id)
        .bind(person(ctx))
        .execute(&mut **tx)
        .await?;
    record(tx, ctx.tenant, id, None, state, Step::Keep, None, person(ctx), None, &json!({"cancel_requested": detail}), None).await?;
    Ok(state)
}

/// A person settles what nobody could prove, on what they found. The physical truth comes first: a
/// retry, if one is needed, is a new command with its own authorization.
pub async fn reconcile(tx: &mut Tx, ctx: &Ctx, id: Uuid, to: State, evidence: &str) -> Result<()> {
    if evidence.trim().is_empty() {
        return Err(Error::field("evidence", "say what you found"));
    }
    let row = load(tx, ctx.tenant, id).await?;
    let state = row.state()?;
    let evidence = json!({"found": evidence, "by": ctx.email});
    if row.conflict {
        // The first ending stands in the ledger; the finding is why that was accepted, or corrected
        // through the business records.
        sqlx::query("update op_command set conflict = false where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).execute(&mut **tx).await?;
        record(tx, ctx.tenant, id, None, state, Step::Keep, None, person(ctx), None, &json!({"conflict_settled": to.as_str()}), Some(&evidence)).await?;
    } else if state == State::OutcomeUnknown && step(state, to, true) == Step::Move {
        let outcome = json!({"evidence": evidence});
        record(tx, ctx.tenant, id, None, to, Step::Move, None, person(ctx), None, &Value::Null, Some(&evidence)).await?;
        set_state(tx, ctx.tenant, id, to, Some(&outcome)).await?;
        announce(tx, ctx.tenant, id, to, &row, &outcome).await?;
    } else {
        return Err(Error::invalid(format!("a command that is {} has nothing to settle as {}", row.state, to.as_str())));
    }
    audit::record(tx, ctx, "Device", row.target_id, "ops.reconcile", None, json!({"command": id, "to": to.as_str(), "evidence": evidence})).await
}

/// A command row with the names a person reads it by, as a query's first part.
macro_rules! named {
    ($drop:literal) => {
        concat!(
            "to_jsonb(c) - 'tenant_id'",
            $drop,
            " || jsonb_build_object('target_code', d.device_code, 'target_name', d.device_name, 'issued_by_email', u.email) from op_command c \
             left join e_device d on d.tenant_id = c.tenant_id and d.id = c.target_id left join app_user u on u.tenant_id = c.tenant_id and u.id = c.issued_by"
        )
    };
}

/// What a person sees of a command, with everything said about it.
pub async fn describe(k: &Kernel, ctx: &Ctx, id: Uuid) -> Result<Value> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let command: Option<Value> =
        sqlx::query_scalar(concat!("select ", named!(""), " where c.tenant_id = $1 and c.id = $2")).bind(ctx.tenant).bind(id).fetch_optional(&mut *tx).await?;
    let mut command = command.ok_or_else(|| Error::NotFound("command".into()))?;
    let history: Vec<Value> =
        sqlx::query_scalar("select to_jsonb(e) - 'tenant_id' - 'command_id' from op_command_event e where tenant_id = $1 and command_id = $2 order by seq")
            .bind(ctx.tenant)
            .bind(id)
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;
    command["history"] = json!(history);
    Ok(command)
}

/// Commands, newest first.
pub async fn list(k: &Kernel, ctx: &Ctx, sites: Option<&[Uuid]>, state: Option<&str>, needs_person: bool, limit: i64) -> Result<Vec<Value>> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let rows: Vec<Value> = sqlx::query_scalar(concat!(
        "select ",
        named!(" - 'payload'"),
        " where c.tenant_id = $1 and ($2::uuid[] is null or c.site_id = any($2)) and ($3::text is null or c.state = $3) \
             and (not $4 or c.state in ('Proposed', 'OutcomeUnknown') or c.conflict) order by c.issued_at desc limit $5"
    ))
    .bind(ctx.tenant)
    .bind(sites)
    .bind(state)
    .bind(needs_person)
    .bind(limit.clamp(1, 500))
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Of the commands a site is in doubt about, those that ended here: a person said what happened.
pub async fn settled(tx: &mut Tx, tenant: Uuid, domain: &str, ids: &[Uuid]) -> Result<Vec<(Uuid, State)>> {
    let rows: Vec<(Uuid, String)> = sqlx::query_as("select id, state from op_command where tenant_id = $1 and domain = $2 and id = any($3)")
        .bind(tenant)
        .bind(domain)
        .bind(ids)
        .fetch_all(&mut **tx)
        .await?;
    Ok(rows.into_iter().filter_map(|(id, s)| State::parse(&s).filter(|s| s.is_final()).map(|s| (id, s))).collect())
}

/// What the holder of `domains` is to receive: anything queued its journal has not confirmed.
pub async fn take_for_site(tx: &mut Tx, tenant: Uuid, domain: &str, epoch: i64, want: i64) -> Result<Vec<Command>> {
    #[derive(sqlx::FromRow)]
    struct C {
        id: Uuid,
        site_id: Uuid,
        domain: String,
        target_id: Uuid,
        operation: String,
        schema_version: i32,
        issued_by: Option<Uuid>,
        approval_ref: Option<String>,
        epoch: i64,
        expected_state: Value,
        issued_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
        payload: Value,
        payload_hash: String,
        workflow_id: Option<String>,
        step_id: Option<String>,
        correlation_id: Option<String>,
        config_version: Option<String>,
    }
    let rows: Vec<C> = sqlx::query_as(
        "update op_command set sent_at = coalesce(sent_at, now()) where (tenant_id, id) in ( \
           select tenant_id, id from op_command where tenant_id = $1 and domain = $2 and state = 'Queued' and epoch = $3 and held_at is null \
           and (sent_at is not null or expires_at > now()) order by issued_at limit $4 for update skip locked) \
         returning id, site_id, domain, target_id, operation, schema_version, issued_by, approval_ref, epoch, expected_state, issued_at, expires_at, payload, payload_hash, workflow_id, step_id, correlation_id, config_version",
    )
    .bind(tenant)
    .bind(domain)
    .bind(epoch)
    .bind(want.clamp(0, 500))
    .fetch_all(&mut **tx)
    .await?;
    let mut out: Vec<Command> = rows
        .into_iter()
        .map(|c| Command {
            id: c.id,
            tenant_id: tenant,
            site_id: c.site_id,
            domain: c.domain,
            target_id: c.target_id,
            operation: c.operation,
            schema_version: c.schema_version,
            issued_by: c.issued_by,
            approval_ref: c.approval_ref,
            epoch: c.epoch,
            expected_state: c.expected_state,
            issued_at: c.issued_at,
            expires_at: c.expires_at,
            payload: c.payload,
            payload_hash: c.payload_hash,
            workflow_id: c.workflow_id,
            step_id: c.step_id,
            correlation_id: c.correlation_id,
            config_version: c.config_version,
        })
        .collect();
    out.sort_by_key(|c| (c.issued_at, c.id));
    Ok(out)
}

pub async fn confirm_held(tx: &mut Tx, tenant: Uuid, domain: &str, ids: &[Uuid]) -> Result<()> {
    if !ids.is_empty() {
        sqlx::query("update op_command set held_at = coalesce(held_at, now()) where tenant_id = $1 and domain = $2 and id = any($3)")
            .bind(tenant)
            .bind(domain)
            .bind(ids)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// What a site is asked to stop, until it says how each ended.
pub async fn cancels(tx: &mut Tx, tenant: Uuid, domain: &str) -> Result<Vec<Uuid>> {
    Ok(sqlx::query_scalar(
        "select id from op_command where tenant_id = $1 and domain = $2 and cancel_requested_at is not null and sent_at is not null \
         and state in ('Queued', 'Accepted', 'Executing', 'OutcomeUnknown') order by id limit 500",
    )
    .bind(tenant)
    .bind(domain)
    .fetch_all(&mut **tx)
    .await?)
}

/// Commands no site ever received whose deadline passed. One a site may hold is left for the site
/// to expire, because only the site knows whether it started.
pub async fn expire_unsent(k: &Kernel) -> Result<u64> {
    let mut tx = k.db.system_tx().await?;
    let due: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "select tenant_id, id from op_command where state in ('Proposed', 'Authorized', 'Queued') and sent_at is null and expires_at <= now() limit 500 for update skip locked",
    )
    .fetch_all(&mut *tx)
    .await?;
    for (tenant, id) in &due {
        let row = load(&mut tx, *tenant, *id).await?;
        record(&mut tx, *tenant, *id, None, State::Expired, Step::Move, None, None, None, &Value::Null, None).await?;
        set_state(&mut tx, *tenant, *id, State::Expired, None).await?;
        announce(&mut tx, *tenant, *id, State::Expired, &row, &Value::Null).await?;
    }
    tx.commit().await?;
    Ok(due.len() as u64)
}

/// Commands an old holder may have started, when authority changes: nobody now can say what
/// happened to them.
pub async fn mark_in_doubt(tx: &mut Tx, tenant: Uuid, domain: &str, epoch: i64, why: &str) -> Result<u64> {
    let rows: Vec<Uuid> = sqlx::query_scalar(
        "select id from op_command where tenant_id = $1 and domain = $2 and (state in ('Accepted', 'Executing') or (state = 'Queued' and sent_at is not null)) for update",
    )
    .bind(tenant)
    .bind(domain)
    .fetch_all(&mut **tx)
    .await?;
    for id in &rows {
        let row = load(tx, tenant, *id).await?;
        let detail = json!({"reason": why});
        record(tx, tenant, *id, None, State::OutcomeUnknown, Step::Move, None, None, Some(epoch), &detail, None).await?;
        set_state(tx, tenant, *id, State::OutcomeUnknown, Some(&detail)).await?;
        announce(tx, tenant, *id, State::OutcomeUnknown, &row, &detail).await?;
    }
    sqlx::query("update op_command set epoch = $3 where tenant_id = $1 and domain = $2 and state in ('Proposed', 'Authorized', 'Queued') and sent_at is null")
        .bind(tenant)
        .bind(domain)
        .bind(epoch)
        .execute(&mut **tx)
        .await?;
    Ok(rows.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(parameter: &str, min: Option<f64>, max: Option<f64>, allowed: Option<&str>) -> Limit {
        Limit { parameter: parameter.into(), min_value: min, max_value: max, allowed: allowed.map(str::to_string) }
    }

    #[test]
    fn a_limit_judges_what_is_in_the_payload_and_refuses_what_is_not() {
        let payload = json!({"qty": "12.5", "job": 412, "task": {"station": "PACK-1"}, "empty": null});
        let max = limit("qty", None, Some(20.0), None);
        assert_eq!(max.outside(&payload), None, "a number may arrive as text");
        assert!(limit("qty", None, Some(10.0), None).outside(&payload).unwrap().contains("outside"));
        assert!(limit("qty", Some(13.0), None, None).outside(&payload).unwrap().contains("outside"));
        assert_eq!(limit("task.station", None, None, Some("PACK-1, PACK-2")).outside(&payload), None, "a path, and a list with spaces");
        assert!(limit("task.station", None, None, Some("PACK-2")).outside(&payload).unwrap().contains("not one of"));
        assert_eq!(limit("job", None, None, Some("412,413")).outside(&payload), None, "a number in a list");
        assert!(limit("recipe", None, None, Some("A")).outside(&payload).unwrap().contains("does not say"), "what is missing cannot be judged");
        assert!(limit("empty", Some(0.0), None, None).outside(&payload).unwrap().contains("does not say"));
        assert!(limit("task.station", Some(0.0), None, None).outside(&payload).unwrap().contains("not a number"));
        assert!(limit("qty", None, Some(1e9), None).outside(&json!({"qty": "NaN"})).unwrap().contains("not a number"));
    }
}
