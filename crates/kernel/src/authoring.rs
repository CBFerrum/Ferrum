//! Authoring solutions (docs/logic/studio.md): drafts people edit, checks and simulations that keep nothing,
//! and what a reviewer sees. Releases, reviews and activation are `logic.rs`'s; runs and tasks `process.rs`'s.
//!
//! * A draft is saved against the version its editor read: a stale save is refused with what is there now, so
//!   two editors never overwrite each other (FLC-UI-006). A draft is not a release, and nothing runs from one.
//! * A simulation runs in the state machine the runtime drives, on a virtual clock, against the workspace's
//!   calendars, and reaches nothing (FLC-UI-007, FLC-UI-010).
//! * A reviewer sees the release, what runs now, and what changed between them by meaning; the changing
//!   capabilities the release may use that what runs now cannot are consented to by name (FLC-UI-005).
//!
//! Administrators and holders of [`AUTHOR_ROLE`] author; reviewing and activating stay administrators', and
//! nobody reviews their own release.

use crate::Kernel;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use ferrum_logic::calendar::Calendar;
use ferrum_logic::compile::Release;
use ferrum_logic::diff;
use ferrum_logic::simulate::{self, Scenario};
use serde_json::{Value, json};
use uuid::Uuid;

/// Studio's role for people who build solutions without administering the workspace: they edit drafts,
/// check, simulate and keep releases; they neither review nor activate. Granted until a date, it ends then.
pub const AUTHOR_ROLE: &str = "Solution Author";
/// The largest source a draft holds.
pub const MAX_SOURCE: usize = 1_048_576;

pub(crate) fn require_author(ctx: &Ctx) -> Result<()> {
    if ctx.has_role(AUTHOR_ROLE) { Ok(()) } else { Err(Error::Forbidden(format!("solutions are edited by administrators and the {AUTHOR_ROLE} role"))) }
}

/// A draft's solution id and label, read loosely: a draft may not parse while it is being written.
fn identity(source: &str) -> (String, String) {
    let v: Value = serde_json::from_str(source).unwrap_or(Value::Null);
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(|s| s.chars().take(200).collect()).unwrap_or_default();
    (text("id"), text("label"))
}

fn sized(source: &str) -> Result<()> {
    if source.len() > MAX_SOURCE {
        return Err(Error::field("source", format!("a solution's source is at most {MAX_SOURCE} bytes")));
    }
    Ok(())
}

impl Kernel {
    /// The workspace's drafts, the latest first.
    pub async fn logic_drafts(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        require_author(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', id, 'solution', solution, 'label', label, 'version', version, 'created_by', created_by, \
               'updated_by', updated_by, 'updated_at', updated_at) from logic_draft where tenant_id = $1 order by updated_at desc limit 500",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    pub async fn create_logic_draft(&self, ctx: &Ctx, source: &str) -> Result<Value> {
        require_author(ctx)?;
        sized(source)?;
        let (solution, label) = identity(source);
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let id: Uuid = sqlx::query_scalar(
            "insert into logic_draft (tenant_id, solution, label, source, created_by, updated_by) values ($1, $2, $3, $4, $5, $5) returning id",
        )
        .bind(ctx.tenant)
        .bind(&solution)
        .bind(&label)
        .bind(source)
        .bind(ctx.user)
        .fetch_one(&mut *tx)
        .await?;
        crate::audit::record(&mut tx, ctx, "LogicDraft", id, "create", Some(1), json!({"solution": solution})).await?;
        tx.commit().await?;
        Ok(json!({"id": id, "version": 1, "solution": solution, "label": label}))
    }

    pub async fn logic_draft(&self, ctx: &Ctx, id: Uuid) -> Result<Value> {
        require_author(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<Value> = sqlx::query_scalar(
            "select jsonb_build_object('id', id, 'solution', solution, 'label', label, 'source', source, 'version', version, \
               'created_by', created_by, 'updated_by', updated_by, 'updated_at', updated_at) from logic_draft where tenant_id = $1 and id = $2",
        )
        .bind(ctx.tenant)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        row.ok_or_else(|| Error::NotFound("draft".into()))
    }

    /// Saves a draft against the version its editor read. Another save in between refuses this one, with the
    /// version and who saved it, so the editor compares and decides instead of losing either.
    pub async fn save_logic_draft(&self, ctx: &Ctx, id: Uuid, expected: i32, source: &str) -> Result<Value> {
        require_author(ctx)?;
        sized(source)?;
        let (solution, label) = identity(source);
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let saved: Option<i32> = sqlx::query_scalar(
            "update logic_draft set source = $4, solution = $5, label = $6, version = version + 1, updated_by = $7, updated_at = now() \
             where tenant_id = $1 and id = $2 and version = $3 returning version",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(expected)
        .bind(source)
        .bind(&solution)
        .bind(&label)
        .bind(ctx.user)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(version) = saved else {
            let now: Option<(i32, Option<Uuid>, chrono::DateTime<chrono::Utc>)> =
                sqlx::query_as("select version, updated_by, updated_at from logic_draft where tenant_id = $1 and id = $2")
                    .bind(ctx.tenant)
                    .bind(id)
                    .fetch_optional(&mut *tx)
                    .await?;
            tx.commit().await?;
            return Err(match now {
                None => Error::NotFound("draft".into()),
                Some((v, by, at)) => Error::Conflict(format!(
                    "this draft changed since you opened it: it is at version {v}, saved by {} at {}; open it again and compare before saving",
                    by.map(|u| u.to_string()).unwrap_or_else(|| "someone".into()),
                    at.to_rfc3339()
                )),
            });
        };
        crate::audit::record(&mut tx, ctx, "LogicDraft", id, "update", Some(version), json!({"solution": solution})).await?;
        tx.commit().await?;
        Ok(json!({"id": id, "version": version}))
    }

    pub async fn delete_logic_draft(&self, ctx: &Ctx, id: Uuid, expected: i32) -> Result<()> {
        require_author(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("delete from logic_draft where tenant_id = $1 and id = $2 and version = $3")
            .bind(ctx.tenant)
            .bind(id)
            .bind(expected)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::Conflict("this draft changed since you opened it, or is gone: open it again".into()));
        }
        crate::audit::record(&mut tx, ctx, "LogicDraft", id, "delete", Some(expected), json!({})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Keeps the draft as it was at `expected` as an immutable release.
    pub async fn release_logic_draft(&self, ctx: &Ctx, id: Uuid, expected: i32) -> Result<Value> {
        let draft = self.logic_draft(ctx, id).await?;
        if draft["version"].as_i64() != Some(i64::from(expected)) {
            return Err(Error::Conflict(format!("this draft is at version {} now, not {expected}: open it again", draft["version"])));
        }
        self.save_release(ctx, draft["source"].as_str().unwrap_or_default()).await
    }

    /// Runs a scenario against a source (compiled here, kept nowhere) or a kept release, on a virtual clock.
    pub async fn simulate_solution(&self, ctx: &Ctx, source: Option<&str>, release: Option<Uuid>, scenario: Value) -> Result<Value> {
        require_author(ctx)?;
        let release: Release = match (source, release) {
            (Some(src), _) => {
                sized(src)?;
                self.compile_solution(ctx, src).await?.map_err(crate::logic::diagnostics)?
            }
            (None, Some(id)) => {
                let mut tx = self.db.tenant_tx(ctx.tenant).await?;
                let stored: Option<Value> = sqlx::query_scalar("select release from logic_release where tenant_id = $1 and id = $2")
                    .bind(ctx.tenant)
                    .bind(id)
                    .fetch_optional(&mut *tx)
                    .await?;
                tx.commit().await?;
                serde_json::from_value(stored.ok_or_else(|| Error::NotFound("release".into()))?).map_err(Error::internal)?
            }
            (None, None) => return Err(Error::field("source", "give a source or a release")),
        };
        let mut scenario: Scenario = serde_json::from_value(scenario).map_err(|e| Error::field("scenario", e.to_string()))?;
        // The workspace's calendars, unless the scenario brings its own of the same name.
        for cal in self.logic_calendars(ctx).await? {
            if let Ok(c) = serde_json::from_value::<Calendar>(cal)
                && !scenario.calendars.iter().any(|x| x.name == c.name)
            {
                scenario.calendars.push(c);
            }
        }
        let sim = simulate::simulate(&release, &scenario).map_err(|e| Error::field("scenario", e))?;
        Ok(json!({"release": {"solution": release.solution, "version": release.version, "artifact_digest": release.artifact_digest}, "simulation": sim}))
    }

    /// The workspace's business calendars.
    pub async fn logic_calendars(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        require_author(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('name', name, 'zone', zone, 'weekends', weekends, 'holidays', holidays, 'revision', revision, \
               'updated_by', updated_by, 'updated_at', updated_at) from logic_calendar where tenant_id = $1 order by name",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// What a reviewer looks at: the release, what runs now for its solution, the decisions so far, and what
    /// changed between the two by meaning, with the new power to consent to.
    pub async fn logic_release_review(&self, ctx: &Ctx, release: Uuid) -> Result<Value> {
        require_author(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let row: Option<(String, String, String, String, Option<Uuid>, chrono::DateTime<chrono::Utc>, Value)> = sqlx::query_as(
            "select solution, version, source_digest, artifact_digest, created_by, created_at, release from logic_release where tenant_id = $1 and id = $2",
        )
        .bind(ctx.tenant)
        .bind(release)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((solution, version, source_digest, artifact_digest, created_by, created_at, stored)) = row else {
            return Err(Error::NotFound("release".into()));
        };
        let active: Option<(Uuid, String, i64, Value)> = sqlx::query_as(
            "select r.id, r.version, a.generation, r.release from logic_activation a join logic_release r on r.tenant_id = a.tenant_id and r.id = a.release_id \
             where a.tenant_id = $1 and a.solution = $2",
        )
        .bind(ctx.tenant)
        .bind(&solution)
        .fetch_optional(&mut *tx)
        .await?;
        let reviews: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('by', decided_by, 'decision', decision, 'reason', reason, 'digest', artifact_digest, 'at', decided_at) \
             from logic_review where tenant_id = $1 and release_id = $2 order by decided_at",
        )
        .bind(ctx.tenant)
        .bind(release)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let this: Release = serde_json::from_value(stored).map_err(Error::internal)?;
        let running: Option<Release> = active.as_ref().and_then(|(_, _, _, r)| serde_json::from_value(r.clone()).ok());
        let changes = diff::diff(running.as_ref().filter(|_| active.as_ref().is_some_and(|(id, ..)| *id != release)), &this);
        let processes: Vec<Value> = this.ir.processes.iter().map(|p| json!({"id": p.id, "label": p.label, "trigger": p.trigger.event})).collect();
        Ok(json!({
            "release": {
                "id": release, "solution": solution, "version": version, "source_digest": source_digest, "artifact_digest": artifact_digest,
                "created_by": created_by, "created_at": created_at, "profile": this.profile, "compiler": this.compiler, "lock": this.lock,
                "notes": this.notes, "rules": this.ir.rules.len(), "decisions": this.ir.decisions.len(), "processes": processes,
            },
            "active": active.map(|(id, version, generation, _)| json!({"release": id, "version": version, "generation": generation})),
            "reviews": reviews,
            "diff": changes,
        }))
    }
}

/// The changing capabilities `release` may use that what runs now for its solution cannot.
pub(crate) async fn privilege_growth(tx: &mut crate::db::Tx, tenant: Uuid, release: Uuid) -> Result<Vec<String>> {
    let rows: Vec<(Uuid, Value, bool)> = sqlx::query_as(
        "select r.id, r.release, r.id = $2 from logic_release r where r.tenant_id = $1 and (r.id = $2 or r.id = \
           (select a.release_id from logic_activation a join logic_release x on x.tenant_id = a.tenant_id and x.id = $2 \
            where a.tenant_id = $1 and a.solution = x.solution))",
    )
    .bind(tenant)
    .bind(release)
    .fetch_all(&mut **tx)
    .await?;
    let this = rows.iter().find(|(_, _, mine)| *mine).ok_or_else(|| Error::NotFound("release".into()))?;
    let this: Release = serde_json::from_value(this.1.clone()).map_err(Error::internal)?;
    let running: Option<Release> = rows.iter().find(|(id, _, _)| *id != release).and_then(|(_, r, _)| serde_json::from_value(r.clone()).ok());
    Ok(diff::diff(running.as_ref(), &this).privilege_growth)
}
