//! Customer logic in a workspace (docs/logic/rules.md).
//!
//! A solution's source is compiled against the workspace (`ferrum_logic::compile`) and kept as a release that
//! never changes. Someone other than its author reviews it, deciding on the artifact digest they were shown.
//! An administrator activates it: it is compiled again against the workspace as it is then, and must come
//! out the same, and its rules must run beside every other active release's (no field filled twice, no
//! derivations waiting for each other in a circle). One release per solution runs, with a generation
//! compared on every change.
//!
//! The rules of active releases run in every save of their records through [`TenantRules`], a document hook
//! for every entity, so a save through the API, `/api/v1`, an agent, an import, a bulk change, an automation,
//! a job or a plugin meets the same rules (FLC-HOOK-005). Derivations run after the app's own, before any
//! validation, so the app's guards see the final record (FLC-HOOK-001); a refused save rolls back, and the
//! evidence of it a rule asks for is kept in a transaction of its own (FLC-HOOK-007).

use crate::ctx::{Action, Ctx};
use crate::error::{Error, FieldError, Result};
use crate::hooks::{DocHook, HookCtx};
use crate::registry::Registry;
use crate::{Doc, Kernel};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use ferrum_logic::approval::{self, Decided, Plan};
use ferrum_logic::capability::Capability;
use ferrum_logic::compile::{self, Diagnostic, EntityShape, FieldShape, Release, Resolver, RuntimeProfile, schema_type};
use ferrum_logic::expr::Ty;
use ferrum_logic::refusal::{Code, Refusal};
use ferrum_logic::rules::{Active, Origin, Record, Refused, Stop};
use ferrum_logic::solution::{Phase, Solution};
use ferrum_meta::{EntityKind, FieldDef, FieldType, Schema};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};
use uuid::Uuid;

/// The review policy's revision: a change to who must approve, or how many, makes earlier approvals lapse.
const POLICY: i64 = 1;
/// How long a release waits for its approvals before it must be compiled again.
const REVIEW_DAYS: i64 = 30;

fn require_admin(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("customer logic is managed by administrators".into())) }
}

/// A field's type as a rule's expressions see it.
fn field_ty(f: &FieldDef, schema: &Schema, nested: bool) -> Ty {
    match f.ftype {
        FieldType::Int => Ty::Int,
        FieldType::Decimal | FieldType::Currency | FieldType::Percent => Ty::Decimal,
        FieldType::Check => Ty::Bool,
        FieldType::Date => Ty::Date,
        FieldType::Datetime => Ty::Instant,
        FieldType::Json => Ty::Any,
        // The id of a record of the kind the link names: one kind's never goes where another's is wanted.
        FieldType::Link if f.to.is_some() => Ty::Ref(f.to.clone().unwrap_or_default()),
        FieldType::Table => match f.of.as_deref().and_then(|c| schema.entity(c)) {
            Some(child) if !nested => Ty::List(Box::new(Ty::Record(
                child.def.fields.iter().filter(|cf| !restricted(cf)).map(|cf| (cf.name.clone(), field_ty(cf, schema, true))).collect(),
            ))),
            _ => Ty::List(Box::new(Ty::Any)),
        },
        _ => Ty::Text,
    }
}

/// In a permission group or a secret: no rule reads it, whoever saves.
fn restricted(f: &FieldDef) -> bool {
    f.perm_group.is_some() || f.ftype == FieldType::Secret
}

/// An entity as rules see it, or none for line items and ledgers, which rules are not on.
pub fn shape(schema: &Schema, name: &str) -> Option<EntityShape> {
    let ent = schema.entity(name)?;
    if matches!(ent.def.kind, EntityKind::Child | EntityKind::Ledger) {
        return None;
    }
    let mut fields: BTreeMap<String, FieldShape> = ent
        .def
        .fields
        .iter()
        .map(|f| {
            let link = (f.ftype == FieldType::Link).then(|| f.to.clone()).flatten();
            (f.name.clone(), FieldShape { ty: field_ty(f, schema, false), customer: ent.custom || f.name.starts_with("x_"), restricted: restricted(f), link })
        })
        .collect();
    fields.entry("id".into()).or_insert(FieldShape { ty: Ty::Ref(ent.def.name.clone()), customer: false, restricted: false, link: None });
    Some(EntityShape { fields })
}

/// What the compiler sees of a workspace: its capabilities at their versions, its events, its entities and its
/// calendars.
struct Workspace {
    caps: BTreeMap<String, Capability>,
    events: BTreeMap<String, Ty>,
    schema: Arc<Schema>,
    calendars: std::collections::BTreeSet<String>,
}

impl Resolver for Workspace {
    fn capability(&self, id: &str, version: &str) -> Option<Capability> {
        self.caps.get(id).filter(|c| c.version == version).cloned()
    }
    fn installed(&self, id: &str) -> Option<String> {
        self.caps.get(id).map(|c| c.version.clone())
    }
    fn event(&self, id: &str) -> Option<Ty> {
        self.events.get(id).cloned()
    }
    fn entity(&self, name: &str) -> Option<EntityShape> {
        shape(&self.schema, name)
    }
    fn calendar(&self, name: &str) -> bool {
        self.calendars.contains(name)
    }
}

pub(crate) fn diagnostics(d: Vec<Diagnostic>) -> Error {
    Error::Validation(d.into_iter().filter(|d| d.fault).map(|d| FieldError::new(d.pointer, format!("{}: {}", d.code.as_str(), d.message))).collect())
}

fn approval_error(r: approval::Refused) -> Error {
    match r {
        approval::Refused::Own => Error::Forbidden("someone other than its author reviews a release".into()),
        approval::Refused::AlreadyDecided => Error::Conflict("you have decided on this release already".into()),
        approval::Refused::Waiting { .. } => Error::Conflict(format!("not approved yet: {r}")),
        other => Refusal::new(Code::StaleApproval, other.to_string()).into(),
    }
}

/// A release's review state: who decided what, of which digest.
struct Reviewed {
    solution: String,
    version: String,
    digest: String,
    author: String,
    expires_ms: i64,
    decisions: Vec<(String, String, String)>,
}

impl Reviewed {
    fn plan(&self) -> Plan<'_> {
        Plan { digest: &self.digest, requested_by: &self.author, policy_revision: POLICY, expires_at_ms: self.expires_ms, required: 1 }
    }
    fn approvals(&self) -> Vec<Decided<'_>> {
        self.decisions
            .iter()
            .filter(|(_, _, d)| d == "approved")
            .map(|(by, digest, _)| Decided { approver: by, digest, policy_revision: POLICY, expires_at_ms: self.expires_ms })
            .collect()
    }
}

async fn reviewed(tx: &mut crate::db::Tx, tenant: Uuid, release: Uuid) -> Result<Reviewed> {
    let row: Option<(String, String, String, Option<Uuid>, DateTime<Utc>)> =
        sqlx::query_as("select solution, version, artifact_digest, created_by, created_at from logic_release where tenant_id = $1 and id = $2 for update")
            .bind(tenant)
            .bind(release)
            .fetch_optional(&mut **tx)
            .await?;
    let Some((solution, version, digest, author, created)) = row else { return Err(Error::NotFound("release".into())) };
    let decisions: Vec<(Uuid, String, String)> =
        sqlx::query_as("select decided_by, artifact_digest, decision from logic_review where tenant_id = $1 and release_id = $2 order by decided_at")
            .bind(tenant)
            .bind(release)
            .fetch_all(&mut **tx)
            .await?;
    Ok(Reviewed {
        solution,
        version,
        digest,
        author: author.map(|a| a.to_string()).unwrap_or_default(),
        expires_ms: (created + chrono::Duration::days(REVIEW_DAYS)).timestamp_millis(),
        decisions: decisions.into_iter().map(|(by, d, decision)| (by.to_string(), d, decision)).collect(),
    })
}

/// The active releases of a workspace, as the rules run them.
pub(crate) async fn active_releases(tx: &mut crate::db::Tx, tenant: Uuid, except: Option<&str>) -> Result<Vec<(Origin, Release)>> {
    let rows: Vec<(Uuid, String, String, String, Value)> = sqlx::query_as(
        "select r.id, r.solution, r.version, r.artifact_digest, r.release from logic_activation a \
         join logic_release r on r.tenant_id = a.tenant_id and r.id = a.release_id \
         where a.tenant_id = $1 and a.solution is distinct from $2 order by a.solution",
    )
    .bind(tenant)
    .bind(except)
    .fetch_all(&mut **tx)
    .await?;
    rows.into_iter()
        .map(|(id, solution, version, artifact_digest, release)| {
            let release: Release = serde_json::from_value(release).map_err(|e| Error::Internal(format!("release {solution} {version} cannot be read: {e}")))?;
            Ok((Origin { release: id.to_string(), solution, version, artifact_digest }, release))
        })
        .collect()
}

/// The account a solution's runs act as when no person's act started them (an event of the platform's, a site's or
/// a schedule's). Each solution has its own, holding exactly the rights of the capabilities its active release was
/// reviewed with, never its activator's: offboarding whoever activated it changes nothing, and nothing beyond the
/// reviewed closure is reachable (FLC-CAP-005).
pub(crate) fn solution_account(solution: &str) -> String {
    format!("solution.{solution}{}", crate::service::SERVICE_DOMAIN)
}

/// Gives a solution's own account the closure `release` was reviewed with, in the activation's transaction, so
/// what its runs may do changes only with what runs. `ctx` sees every capability installed (the activating
/// administrator, or the system for an account made later).
pub(crate) async fn grant_solution(k: &Kernel, tx: &mut crate::db::Tx, ctx: &Ctx, solution: &str, release: &Release) -> Result<Uuid> {
    let role = format!("solution:{solution}");
    let mut rights: BTreeMap<String, ferrum_meta::PermissionDef> = BTreeMap::new();
    let mut roles = vec![role.clone()];
    for locked in &release.lock {
        // One no longer installed grants nothing: its step waits for it (FLC-OPS-007).
        let Ok(cap) = k.capability(ctx, &locked.id) else { continue };
        for p in &cap.permissions {
            match p.split_once(':') {
                // A transition only that role may take: the account holds the role, as the review saw.
                Some(("role", r)) => roles.push(r.to_string()),
                Some((entity, action)) => {
                    let d = rights.entry(entity.to_string()).or_insert_with(|| ferrum_meta::PermissionDef { entity: entity.to_string(), ..Default::default() });
                    match action {
                        "read" => d.read = true,
                        "write" => d.write = true,
                        "create" => d.create = true,
                        "delete" => d.delete = true,
                        "submit" => d.submit = true,
                        "cancel" => d.cancel = true,
                        _ => {}
                    }
                }
                // An administrator's (mail): allowed at each step by this review, never through a role.
                None => {}
            }
        }
    }
    roles.sort();
    roles.dedup();
    for r in &roles {
        sqlx::query("insert into role (tenant_id, name) values ($1, $2) on conflict do nothing").bind(ctx.tenant).bind(r).execute(&mut **tx).await?;
    }
    let user: Uuid = sqlx::query_scalar(
        "insert into app_user (tenant_id, id, email, full_name, kind) values ($1, $2, $3, $4, 'service') \
         on conflict (tenant_id, email) do update set is_active = true, kind = 'service', full_name = excluded.full_name returning id",
    )
    .bind(ctx.tenant)
    .bind(Uuid::now_v7())
    .bind(solution_account(solution))
    .bind(format!("Solution: {solution}"))
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query("delete from user_role where tenant_id = $1 and user_id = $2 and role <> all($3)")
        .bind(ctx.tenant)
        .bind(user)
        .bind(&roles)
        .execute(&mut **tx)
        .await?;
    sqlx::query("insert into user_role (tenant_id, user_id, role) select $1, $2, unnest($3::text[]) on conflict do nothing")
        .bind(ctx.tenant)
        .bind(user)
        .bind(&roles)
        .execute(&mut **tx)
        .await?;
    sqlx::query("delete from role_permission where tenant_id = $1 and role = $2").bind(ctx.tenant).bind(&role).execute(&mut **tx).await?;
    for d in rights.values() {
        sqlx::query(
            "insert into role_permission (tenant_id, role, entity, can_read, can_write, can_create, can_delete, can_submit, can_cancel, \
             can_export, can_import, if_owner, read_groups, write_groups, match_rule, rule_key) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, false, false, false, $10, $11, null, $12)",
        )
        .bind(ctx.tenant)
        .bind(&role)
        .bind(&d.entity)
        .bind(d.read)
        .bind(d.write)
        .bind(d.create)
        .bind(d.delete)
        .bind(d.submit)
        .bind(d.cancel)
        .bind(&d.read_groups)
        .bind(&d.write_groups)
        .bind(d.condition_key())
        .execute(&mut **tx)
        .await?;
    }
    crate::auth::notify_users(tx, ctx.tenant, None).await?;
    Ok(user)
}

impl Kernel {
    /// What this installation runs releases under (docs/logic/decisions.md L-06): Full Process when a durable
    /// process runtime is configured, otherwise Core (rules and decisions; a process is refused).
    pub fn logic_profile(&self) -> RuntimeProfile {
        if self.settings.process_runtime { RuntimeProfile::FullProcess } else { RuntimeProfile::Core }
    }

    async fn workspace(&self, ctx: &Ctx) -> Result<Workspace> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let calendars: Vec<String> = sqlx::query_scalar("select name from logic_calendar where tenant_id = $1").bind(ctx.tenant).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok(self.workspace_with(ctx, calendars.into_iter().collect()))
    }

    fn workspace_with(&self, ctx: &Ctx, calendars: std::collections::BTreeSet<String>) -> Workspace {
        let caps = self.catalog(ctx).into_iter().map(|c| (c.id.clone(), c)).collect();
        let events = self
            .events(ctx)
            .into_iter()
            .filter_map(|e| {
                let id = e.get("id")?.as_str()?.to_string();
                // A declared event says its payload; a record's event carries the record.
                let ty = match (e.get("payload").filter(|p| !p.is_null()), e.get("entity").and_then(Value::as_str)) {
                    (Some(p), _) => schema_type(p),
                    (None, Some(entity)) => shape(&ctx.schema, entity).map(|s| ferrum_logic::rules::readable(&s)).unwrap_or(Ty::Any),
                    _ => Ty::Any,
                };
                Some((id, ty))
            })
            .collect();
        Workspace { caps, events, schema: ctx.schema.clone(), calendars }
    }

    /// Compiles a solution against this workspace, keeping nothing: the builder's check.
    pub async fn compile_solution(&self, ctx: &Ctx, source: &str) -> Result<std::result::Result<Release, Vec<Diagnostic>>> {
        let solution = match Solution::parse(source) {
            Ok(s) => s,
            Err(r) => {
                let pointer = r.pointer.clone().unwrap_or_default();
                return Ok(Err(vec![Diagnostic { code: r.code, message: r.message, pointer, node: None, fault: true }]));
            }
        };
        Ok(compile::compile(&solution, &self.workspace(ctx).await?, self.logic_profile()))
    }

    /// Compiles a solution and keeps the release, which never changes (administrators and solution authors;
    /// someone else reviews it).
    pub async fn save_release(&self, ctx: &Ctx, source: &str) -> Result<Value> {
        crate::authoring::require_author(ctx)?;
        let solution = Solution::parse(source)?;
        let release = compile::compile(&solution, &self.workspace(ctx).await?, self.logic_profile()).map_err(diagnostics)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let id: Option<Uuid> = sqlx::query_scalar(
            "insert into logic_release (tenant_id, solution, version, source_digest, artifact_digest, compiler, profile, source, release, created_by) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) on conflict (tenant_id, solution, version) do nothing returning id",
        )
        .bind(ctx.tenant)
        .bind(&release.solution)
        .bind(&release.version)
        .bind(&release.source_digest)
        .bind(&release.artifact_digest)
        .bind(&release.compiler)
        .bind(serde_json::to_value(release.profile).ok().and_then(|v| v.as_str().map(str::to_string)))
        .bind(serde_json::to_value(&solution).map_err(Error::internal)?)
        .bind(serde_json::to_value(&release).map_err(Error::internal)?)
        .bind(ctx.user)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = id else {
            return Err(Error::Conflict(format!(
                "{} {} is kept already, and a release does not change: give this one another version",
                release.solution, release.version
            )));
        };
        crate::audit::record(
            &mut tx,
            ctx,
            "LogicRelease",
            id,
            "create",
            None,
            json!({"solution": release.solution, "version": release.version, "artifact_digest": release.artifact_digest}),
        )
        .await?;
        tx.commit().await?;
        Ok(json!({
            "id": id, "solution": release.solution, "version": release.version, "source_digest": release.source_digest,
            "artifact_digest": release.artifact_digest, "notes": release.notes,
        }))
    }

    /// The workspace's releases, newest first, with their decisions and which runs (administrators).
    pub async fn logic_releases(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', r.id, 'solution', r.solution, 'version', r.version, 'artifact_digest', r.artifact_digest, \
               'source_digest', r.source_digest, 'created_by', r.created_by, 'created_at', r.created_at, \
               'active', a.release_id is not null, 'generation', a.generation, \
               'reviews', coalesce((select jsonb_agg(jsonb_build_object('by', v.decided_by, 'decision', v.decision, 'reason', v.reason, \
                  'digest', v.artifact_digest, 'at', v.decided_at) order by v.decided_at) from logic_review v \
                  where v.tenant_id = r.tenant_id and v.release_id = r.id), '[]'::jsonb)) \
             from logic_release r left join logic_activation a on a.tenant_id = r.tenant_id and a.release_id = r.id \
             where r.tenant_id = $1 order by r.created_at desc limit 500",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Records a decision on a release, by an administrator other than its author, of the digest they were
    /// shown: a release that is not the one they looked at cannot be approved by mistake. One that may change
    /// records with capabilities what runs now cannot is approved only by naming them (`review_release_consenting`).
    pub async fn review_release(&self, ctx: &Ctx, release: Uuid, shown: &str, approve: bool, reason: &str) -> Result<()> {
        self.review_release_consenting(ctx, release, shown, &[], approve, reason).await
    }

    /// As `review_release`, consenting to `privileges`: the changing capabilities the release may use that what
    /// runs now for its solution cannot, exactly as the reviewer was shown them (FLC-UI-005).
    pub async fn review_release_consenting(&self, ctx: &Ctx, release: Uuid, shown: &str, privileges: &[String], approve: bool, reason: &str) -> Result<()> {
        require_admin(ctx)?;
        if !approve && reason.trim().is_empty() {
            return Err(Error::field("reason", "say why it is rejected"));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let r = reviewed(&mut tx, ctx.tenant, release).await?;
        let earlier: Vec<Decided> =
            r.decisions.iter().map(|(by, digest, _)| Decided { approver: by, digest, policy_revision: POLICY, expires_at_ms: r.expires_ms }).collect();
        approval::may_decide(&r.plan(), shown, &ctx.user.to_string(), &earlier, POLICY, Utc::now().timestamp_millis()).map_err(approval_error)?;
        if approve {
            let growth = crate::authoring::privilege_growth(&mut tx, ctx.tenant, release).await?;
            let mut named = privileges.to_vec();
            named.sort();
            named.dedup();
            if named != growth {
                return Err(Error::field(
                    "privileges",
                    if growth.is_empty() {
                        "this release may do nothing more than what runs now: consent to nothing".to_string()
                    } else {
                        format!("this release may change records with {} that what runs now cannot: name exactly these to consent", growth.join(", "))
                    },
                ));
            }
        }
        let decision = if approve { "approved" } else { "rejected" };
        sqlx::query("insert into logic_review (tenant_id, release_id, artifact_digest, decided_by, decision, reason) values ($1, $2, $3, $4, $5, $6)")
            .bind(ctx.tenant)
            .bind(release)
            .bind(shown)
            .bind(ctx.user)
            .bind(decision)
            .bind(reason.trim())
            .execute(&mut *tx)
            .await?;
        crate::audit::record(
            &mut tx,
            ctx,
            "LogicRelease",
            release,
            decision,
            None,
            json!({"solution": r.solution, "version": r.version, "artifact_digest": shown, "reason": reason.trim()}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Makes a release the one that runs for its solution (administrators). It must be approved, by someone
    /// other than its author, of exactly its digest, and rejected by nobody; compiled again against the
    /// workspace as it is now, it must come out the same (a capability or field it uses has not changed:
    /// FLC-COMP-005); and its rules must run beside every other active release's (FLC-HOOK-003). `expected`
    /// is the generation the caller saw running, `None` when none was.
    pub async fn activate_release(&self, ctx: &Ctx, release: Uuid, expected: Option<i64>) -> Result<i64> {
        let done = self.activate_release_explained(ctx, release, expected).await?;
        done["generation"].as_i64().ok_or_else(|| Error::internal("an activation came back without its generation"))
    }

    /// [`Kernel::activate_release`], answering what changed with it: its generation, the release it replaced,
    /// the automation it took over from, and what stays as it was. Going back to an earlier release is this
    /// too, a new generation selecting it: runs started under another release go on under theirs, and nothing
    /// any run did is undone (FLC-MIG-006). A solution imported from an automation turns the automation off in
    /// the same step, so each of its events is run by one of the two (FLC-MIG-003).
    pub async fn activate_release_explained(&self, ctx: &Ctx, release: Uuid, expected: Option<i64>) -> Result<Value> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        // One activation at a time in a workspace: two at once could each miss the other's rules.
        sqlx::query("select pg_advisory_xact_lock(hashtext('ferrum.logic.activation'), hashtext($1))").bind(ctx.tenant.to_string()).execute(&mut *tx).await?;
        let r = reviewed(&mut tx, ctx.tenant, release).await?;
        // The automation it was imported from, locked before anything else is written, as the automation's runner
        // and its saving lock it.
        let automation = crate::authoring::automation_of(&r.solution);
        let replacing: Option<(bool, String)> = match automation {
            Some(a) => {
                sqlx::query_as("select is_active, name from automation where tenant_id = $1 and id = $2 for update")
                    .bind(ctx.tenant)
                    .bind(a)
                    .fetch_optional(&mut *tx)
                    .await?
            }
            None => None,
        };
        let replaced: Option<(Uuid, String)> = sqlx::query_as(
            "select r.id, r.version from logic_activation a join logic_release r on r.tenant_id = a.tenant_id and r.id = a.release_id \
             where a.tenant_id = $1 and a.solution = $2",
        )
        .bind(ctx.tenant)
        .bind(&r.solution)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((by, _, _)) = r.decisions.iter().find(|(_, _, d)| d == "rejected") {
            return Err(Error::Conflict(format!("{} {} was rejected (by {by}): compile a new version", r.solution, r.version)));
        }
        approval::satisfied(&r.plan(), &r.digest, &r.approvals(), POLICY, Utc::now().timestamp_millis()).map_err(approval_error)?;
        let (source, stored): (Value, Value) = sqlx::query_as("select source, release from logic_release where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(release)
            .fetch_one(&mut *tx)
            .await?;
        let stale = |why: String| -> Error {
            Refusal::new(Code::StaleApproval, format!("{} {} {why}: compile and review a new version", r.solution, r.version)).into()
        };
        let source: Solution = serde_json::from_value(source).map_err(|e| stale(format!("cannot be read again ({e})")))?;
        let now = compile::compile(&source, &self.workspace(ctx).await?, self.logic_profile())
            .map_err(|d| stale(format!("no longer compiles here ({})", d.iter().find(|d| d.fault).map(|d| d.message.as_str()).unwrap_or("?"))))?;
        if now.artifact_digest != r.digest {
            return Err(stale("was approved against capabilities or fields that have changed since".into()));
        }
        let stored: Release = serde_json::from_value(stored).map_err(Error::internal)?;
        // Beside every other active release: one writer per field, no circle, and nothing an app's own
        // hooks fill.
        let mut together = active_releases(&mut tx, ctx.tenant, Some(&r.solution)).await?;
        together.push((
            Origin { release: release.to_string(), solution: r.solution.clone(), version: r.version.clone(), artifact_digest: r.digest.clone() },
            stored.clone(),
        ));
        let schema = ctx.schema.clone();
        let mut problems = Active::prepare(&together, &|name| shape(&schema, name)).problems();
        for rule in stored.ir.rules.iter().filter(|r| r.phase == Phase::Derive) {
            for hook in self.hooks.for_tenant(&rule.entity, &ctx.schema.packages) {
                for field in rule.writes.keys().filter(|f| hook.fills().contains(&f.as_str())) {
                    problems.push(format!("{} fills {}.{field}, which {} fills too", rule.id, rule.entity, hook.name()));
                }
            }
        }
        if !problems.is_empty() {
            return Err(Error::Conflict(format!("{} {} cannot run beside what runs now: {}", r.solution, r.version, problems.join("; "))));
        }
        let generation: Option<i64> = match expected {
            None => {
                sqlx::query_scalar(
                    "insert into logic_activation (tenant_id, solution, release_id, activated_by) values ($1, $2, $3, $4) \
                 on conflict (tenant_id, solution) do nothing returning generation",
                )
                .bind(ctx.tenant)
                .bind(&r.solution)
                .bind(release)
                .bind(ctx.user)
                .fetch_optional(&mut *tx)
                .await?
            }
            Some(g) => {
                sqlx::query_scalar(
                    "update logic_activation set release_id = $3, generation = generation + 1, activated_by = $4, activated_at = now() \
                 where tenant_id = $1 and solution = $2 and generation = $5 returning generation",
                )
                .bind(ctx.tenant)
                .bind(&r.solution)
                .bind(release)
                .bind(ctx.user)
                .bind(g)
                .fetch_optional(&mut *tx)
                .await?
            }
        };
        let Some(generation) = generation else {
            return Err(Error::Conflict(format!("what runs for {} changed meanwhile: look again", r.solution)));
        };
        let handed_over = match (automation, &replacing) {
            (Some(a), Some((true, name))) => {
                sqlx::query("update automation set is_active = false, updated_at = now() where tenant_id = $1 and id = $2")
                    .bind(ctx.tenant)
                    .bind(a)
                    .execute(&mut *tx)
                    .await?;
                json!({"automation": a, "name": name})
            }
            _ => Value::Null,
        };
        let elsewhere: i64 =
            sqlx::query_scalar("select count(*) from logic_run where tenant_id = $1 and solution = $2 and release_id <> $3 and finished_at is null")
                .bind(ctx.tenant)
                .bind(&r.solution)
                .bind(release)
                .fetch_one(&mut *tx)
                .await?;
        let replaced = replaced.filter(|(id, _)| *id != release).map(|(id, version)| json!({"release": id, "version": version}));
        let account = grant_solution(self, &mut tx, ctx, &r.solution, &stored).await?;
        crate::audit::record(
            &mut tx,
            ctx,
            "LogicRelease",
            release,
            "activate",
            None,
            json!({"solution": r.solution, "version": r.version, "artifact_digest": r.digest, "generation": generation, "replaced": replaced,
                   "handed_over": handed_over, "account": account}),
        )
        .await?;
        // Every replica takes up the workspace's rules afresh: this one now, the others when told.
        Registry::bump_version(&mut tx, ctx.tenant).await?;
        tx.commit().await?;
        self.registry.invalidate(ctx.tenant);
        self.invalidate_tenant_users(ctx.tenant);
        let open = match elsewhere {
            0 => "No run started under another release is still open".to_string(),
            1 => "The one run still open under another release goes on under it".to_string(),
            n => format!("The {n} runs still open under other releases go on under theirs"),
        };
        let kept = format!(
            "New runs of {} start under {}. {open}, and nothing a run did is undone: an effect that happened stays, for a person to decide about.",
            r.solution, r.version
        );
        Ok(json!({"generation": generation, "replaced": replaced, "handed_over": handed_over, "open_elsewhere": elsewhere, "kept": kept}))
    }

    /// Stops a solution's release running (administrators); `expected` is the generation the caller saw.
    pub async fn deactivate_solution(&self, ctx: &Ctx, solution: &str, expected: i64) -> Result<()> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let release: Option<Uuid> =
            sqlx::query_scalar("delete from logic_activation where tenant_id = $1 and solution = $2 and generation = $3 returning release_id")
                .bind(ctx.tenant)
                .bind(solution)
                .bind(expected)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(release) = release else { return Err(Error::Conflict(format!("what runs for {solution} changed meanwhile: look again"))) };
        // Its own account stops with it; activated again, it is given that release's closure.
        sqlx::query("update app_user set is_active = false where tenant_id = $1 and email = $2")
            .bind(ctx.tenant)
            .bind(solution_account(solution))
            .execute(&mut *tx)
            .await?;
        crate::auth::notify_users(&mut tx, ctx.tenant, None).await?;
        crate::audit::record(&mut tx, ctx, "LogicRelease", release, "deactivate", None, json!({"solution": solution, "generation": expected})).await?;
        Registry::bump_version(&mut tx, ctx.tenant).await?;
        tx.commit().await?;
        self.registry.invalidate(ctx.tenant);
        self.invalidate_tenant_users(ctx.tenant);
        Ok(())
    }

    /// What runs on the capabilities of `packages` here (FLC-CAP-008): each active release whose reviewed lock names
    /// one of them, with the capabilities and versions it is bound to and how many of its runs are open.
    /// Administrators. Taking such an app out does not fail that work: its steps wait for the capability, telling
    /// administrators (FLC-OPS-007), and the ways on are bringing the app back, or moving the runs to a release that
    /// does without it (FLC-MIG-004).
    pub async fn logic_dependents(&self, ctx: &Ctx, packages: &[String]) -> Result<Vec<Value>> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<(String, Uuid, String, Value, i64)> = sqlx::query_as(
            "select a.solution, r.id, r.version, r.release, \
               (select count(*) from logic_run l where l.tenant_id = a.tenant_id and l.solution = a.solution and l.finished_at is null) \
             from logic_activation a join logic_release r on r.tenant_id = a.tenant_id and r.id = a.release_id where a.tenant_id = $1 order by a.solution",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut out = vec![];
        for (solution, release, version, stored, open) in rows {
            let Ok(stored) = serde_json::from_value::<Release>(stored) else { continue };
            let used: Vec<Value> = stored
                .lock
                .iter()
                .filter(|l| packages.iter().any(|p| l.id.starts_with(&format!("{p}."))))
                .map(|l| json!({"capability": l.id, "version": l.version}))
                .collect();
            if !used.is_empty() {
                out.push(json!({"solution": solution, "release": release, "version": version, "uses": used, "open_runs": open}));
            }
        }
        Ok(out)
    }

    /// Evidence of refused saves the rules asked to keep, newest first (administrators).
    pub async fn logic_refusals(&self, ctx: &Ctx, limit: i64) -> Result<Vec<Value>> {
        require_admin(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', f.id, 'release', f.release_id, 'solution', r.solution, 'version', r.version, 'rule', f.rule, \
               'entity', f.entity, 'record_id', f.record_id, 'actor', f.actor, 'outcome', f.outcome, 'correlation_id', f.correlation_id, \
               'created_at', f.created_at) \
             from logic_refusal f left join logic_release r on r.tenant_id = f.tenant_id and r.id = f.release_id \
             where f.tenant_id = $1 order by f.id desc limit $2",
        )
        .bind(ctx.tenant)
        .bind(limit.clamp(1, 500))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }
}

/// Active releases' rules by workspace, with the metadata version they were read at: an activation bumps
/// it, so every replica reads them again (registry.rs).
static ACTIVE: LazyLock<DashMap<Uuid, (i64, Arc<Active>)>> = LazyLock::new(DashMap::new);

async fn active(hc: &mut HookCtx<'_>) -> Result<Arc<Active>> {
    // The workspace as it is now, not as it was when this caller's session was loaded: an activation takes
    // effect for every save that starts after it.
    let tenant = hc.ctx.tenant;
    let schema = hc.kernel.registry.schema(tenant).await?;
    let version = schema.version;
    if let Some(found) = ACTIVE.get(&tenant)
        && found.0 == version
    {
        return Ok(found.1.clone());
    }
    let releases = active_releases(hc.tx, tenant, None).await?;
    let active = Arc::new(Active::prepare(&releases, &|name| shape(&schema, name)));
    if ACTIVE.get(&tenant).is_none_or(|e| e.0 <= version) {
        ACTIVE.insert(tenant, (version, active.clone()));
    }
    Ok(active)
}

/// The linked records this phase's rules declared, read with the saver's rights before any rule runs: a rule
/// reads nothing its saver could not (FLC-HOOK-004).
async fn linked(hc: &mut HookCtx<'_>, active: &Active, phase: Phase, doc: &Doc) -> Result<BTreeMap<String, Value>> {
    let entity = hc.entity.def.name.clone();
    let mut out = BTreeMap::new();
    for read in active.reads(&entity, phase).map_err(stopped)? {
        let Some(id) = doc.get(&read.link).and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok()) else {
            out.insert(read.link, Value::Null);
            continue;
        };
        if hc.ctx.require(&read.entity, Action::Read).is_err() {
            return Err(Error::Forbidden(format!("a rule on {entity} reads {}, which you may not read", read.entity)));
        }
        let record = match hc.kernel.get_tx(hc.ctx, hc.tx, &read.entity, id, false).await {
            Ok(d) => Value::Object(d.into_iter().filter(|(k, _)| read.fields.contains(k)).collect()),
            Err(Error::NotFound(_)) => Value::Null,
            Err(e) => return Err(e),
        };
        out.insert(read.link, record);
    }
    Ok(out)
}

fn stopped(s: Stop) -> Error {
    match s {
        Stop::Refused(r) => {
            // Where it came from, so that whoever looks into it finds the rule and its release (FLC-COMP-010).
            let message = format!("{} [{} {}, rule {}]", r.message, r.origin.solution, r.origin.version, r.rule);
            match r.field {
                Some(f) => Error::field(f, message),
                None => Error::invalid(message),
            }
        }
        Stop::Stale(why) => Refusal::new(Code::StaleApproval, why).into(),
    }
}

/// Keeps evidence that a rule refused a save, in a transaction of its own: the save's rolls back.
async fn evidence(k: &Kernel, ctx: &Ctx, entity: &str, record: Option<Uuid>, r: &Refused) {
    let kept: Result<()> = async {
        let release: Uuid = r.origin.release.parse().map_err(Error::internal)?;
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        sqlx::query("insert into logic_refusal (tenant_id, release_id, rule, entity, record_id, actor, outcome) values ($1, $2, $3, $4, $5, $6, $7)")
            .bind(ctx.tenant)
            .bind(release)
            .bind(&r.rule)
            .bind(entity)
            .bind(record)
            .bind((!ctx.is_system).then_some(ctx.user))
            .bind(r.outcome)
            .execute(&mut *tx)
            .await?;
        Ok(tx.commit().await?)
    }
    .await;
    if let Err(e) = kept {
        tracing::warn!(rule = %r.rule, solution = %r.origin.solution, "the evidence of a refused save was not kept: {e}");
    }
}

fn user_fact(ctx: &Ctx) -> Value {
    json!({"id": ctx.user.to_string(), "email": ctx.email, "roles": ctx.roles})
}

/// Active releases' rules, for every entity of every workspace (registered by [`Kernel::boot`]).
pub struct TenantRules;

#[async_trait]
impl DocHook for TenantRules {
    fn name(&self) -> &'static str {
        "ferrum.tenant_rules"
    }

    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let active = active(hc).await?;
        let entity = hc.entity.def.name.clone();
        if !active.has(&entity, Phase::Derive) {
            return Ok(());
        }
        let linked = linked(hc, &active, Phase::Derive, doc).await?;
        let (user, snapshot) = (user_fact(hc.ctx), Value::Object(doc.clone()));
        // The clock is read here, once, and handed to the rules as a fact (FLC-COMP-003).
        let rec = Record { doc: &snapshot, old: None, user: &user, now: Utc::now(), today: hc.ctx.today(), linked: &linked };
        active.derive(&entity, doc, &rec).map_err(stopped)
    }

    async fn validate_change(&self, hc: &mut HookCtx<'_>, old: Option<&Doc>, doc: &Doc) -> Result<()> {
        let active = active(hc).await?;
        let entity = hc.entity.def.name.clone();
        if !active.has(&entity, Phase::Validate) {
            return Ok(());
        }
        let linked = linked(hc, &active, Phase::Validate, doc).await?;
        let (user, now, old) = (user_fact(hc.ctx), Value::Object(doc.clone()), old.map(|o| Value::Object(o.clone())));
        let rec = Record { doc: &now, old: old.as_ref(), user: &user, now: Utc::now(), today: hc.ctx.today(), linked: &linked };
        match active.validate(&entity, &rec) {
            Ok(()) => Ok(()),
            Err(Stop::Refused(r)) => {
                if r.audit {
                    let record = doc.get("id").and_then(Value::as_str).and_then(|s| s.parse().ok());
                    evidence(hc.kernel, hc.ctx, &entity, record, &r).await;
                }
                Err(stopped(Stop::Refused(r)))
            }
            Err(stale) => Err(stopped(stale)),
        }
    }
}
