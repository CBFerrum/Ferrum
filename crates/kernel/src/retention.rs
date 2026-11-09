//! Two of a workspace's four separate operations on personal data, beside legal holds and erasure
//! (crate::privacy): each has its own reason, its own record, and does not stand in for another.
//!
//! **Restriction of processing** keeps someone's data and stops using it: while one covers a person,
//! no automated mail goes to them, under any address they have had, and apps ask
//! [`Kernel::may_contact`] before reaching out on other paths. It deletes nothing and blocks no one
//! from reading the records.
//!
//! **Retention rules** say how long records of an entity are kept: after so many days, records that
//! match are deleted or anonymized. A rule runs daily as a job, as the system, a bounded number of
//! records at a time. It never overrides what says otherwise: a legal hold stops it, what a package
//! retains for a stated reason cannot have a rule that deletes it, and a record that cannot be
//! deleted (a submitted document, one others link to) is counted, not forced. Immutable financial
//! history is out of reach by design: it is not an entity's record to delete.

use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::jobs::{Job, JobHandler, NewJob, enqueue};
use crate::outbox::PRIORITY_BULK;
use crate::privacy::{addresses, checked_email, held_people_records, record_held};
use crate::query::{Filter, ListQuery, Op, parse_filters};
use crate::{Kernel, audit};
use async_trait::async_trait;
use chrono::{Duration as Days, Utc};
use ferrum_meta::{EntityKind, FieldType};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use uuid::Uuid;

pub const RETENTION_KIND: &str = "platform.retention";
/// Records a page of a rule's scan reads.
const PAGE: usize = 500;
/// Failures a run lists.
const LISTED_FAILURES: usize = 100;

fn admin_only(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("this is for administrators".into())) }
}

fn checked_reason(reason: &str) -> Result<&str> {
    let reason = reason.trim();
    if reason.is_empty() || reason.chars().count() > 500 {
        return Err(Error::field("reason", "say why, in at most 500 characters"));
    }
    Ok(reason)
}

/// A rule as an administrator writes it.
#[derive(Debug, Clone, Deserialize)]
pub struct RetentionInput {
    pub entity: String,
    pub after_days: i32,
    #[serde(default = "default_date_field")]
    pub date_field: String,
    pub action: String,
    /// List filters the record must match as well.
    #[serde(default)]
    pub condition: Value,
    pub reason: String,
    #[serde(default = "yes")]
    pub is_active: bool,
}

fn default_date_field() -> String {
    "updated_at".into()
}

fn yes() -> bool {
    true
}

#[derive(Debug, sqlx::FromRow)]
struct Rule {
    id: Uuid,
    entity: String,
    after_days: i32,
    date_field: String,
    action: String,
    condition: Value,
    reason: String,
    /// Where its scan stands: the next run continues after the records the last one reached.
    scan_cursor: Option<String>,
}

const RULE: &str = "select id, entity, after_days, date_field, action, condition, reason, scan_cursor from retention_rule where tenant_id = $1";

pub(crate) struct RetentionJob;

#[async_trait]
impl JobHandler for RetentionJob {
    fn kind(&self) -> &'static str {
        RETENTION_KIND
    }

    fn title(&self) -> &'static str {
        "Retention"
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(4 * 3600)
    }

    async fn run(&self, k: &Kernel, job: &Job) -> Result<Value> {
        let mut tx = k.db.tenant_tx(job.tenant_id).await?;
        let rules: Vec<Rule> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!("{RULE} and is_active order by created_at"))).bind(job.tenant_id).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let sys = Ctx::system(job.tenant_id, k.registry.schema(job.tenant_id).await?);
        let mut results = Vec::new();
        for rule in rules {
            let (outcome, cursor) = match k.apply_rule(&sys, &rule).await {
                Ok(done) => done,
                // One rule that no longer fits (its entity's app was removed) leaves the others; its
                // scan starts again when it fits once more.
                Err(e) => (json!({"error": e.to_string()}), None),
            };
            let mut tx = k.db.tenant_tx(job.tenant_id).await?;
            sqlx::query("update retention_rule set last_run_at = now(), last_result = $3, scan_cursor = $4 where tenant_id = $1 and id = $2")
                .bind(job.tenant_id)
                .bind(rule.id)
                .bind(&outcome)
                .bind(cursor)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            results.push(json!({"rule": rule.id, "outcome": outcome}));
        }
        Ok(json!({"rules": results}))
    }
}

/// Why a message goes to someone (FEO-015): a transactional notice (a receipt, a reset, a reply to
/// them) or marketing, which needs their consent and stops when they withdraw it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    #[default]
    Transactional,
    Marketing,
}

impl Kernel {
    /// What a rule reaches, up to `limit`, continuing after `from` (null: from the beginning), and where
    /// the scan stands after them (null: it reached the end).
    async fn due_records(&self, sys: &Ctx, rule: &Rule, from: Option<String>, limit: usize) -> Result<(Vec<Uuid>, Option<String>)> {
        let cutoff = Utc::now() - Days::days(rule.after_days as i64);
        let is_date = self.entity(sys, &rule.entity)?.field(&rule.date_field).is_some_and(|f| f.ftype == FieldType::Date);
        let cutoff = if is_date { cutoff.date_naive().to_string() } else { cutoff.to_rfc3339() };
        let mut filters =
            if rule.condition.as_object().is_some_and(|o| o.is_empty()) || rule.condition.is_null() { vec![] } else { parse_filters(&rule.condition)? };
        filters.push(Filter { field: rule.date_field.clone(), op: Op::Lt, value: json!(cutoff) });
        let (mut ids, mut cursor) = (Vec::new(), from);
        while ids.len() < limit {
            let q = ListQuery {
                filters: filters.clone(),
                fields: Some(vec!["id".into()]),
                sort: Some("created_at".into()),
                // Never more than is left of the limit, so the cursor stands just after the last taken.
                limit: Some((limit - ids.len()).min(PAGE) as u32),
                cursor,
                ..Default::default()
            };
            let page = self.list(sys, &rule.entity, &q).await?;
            ids.extend(page.rows.iter().filter_map(|r| r.get("id")?.as_str()?.parse::<Uuid>().ok()));
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok((ids, cursor))
    }

    /// One run of a rule: up to the daily number of records, after where its last run stopped, so what
    /// it cannot remove (held, or failing) never keeps it from the records after them. What it reached,
    /// and where its scan now stands.
    async fn apply_rule(&self, sys: &Ctx, rule: &Rule) -> Result<(Value, Option<String>)> {
        let (mut done, mut held, mut failed, mut failures) = (0u64, 0u64, 0u64, Vec::new());
        let limit = self.settings.retention_daily_records.max(1);
        let started = rule.scan_cursor.is_none();
        let (ids, cursor) = match self.due_records(sys, rule, rule.scan_cursor.clone(), limit).await {
            // A cursor that no longer reads (the list's format changed) starts the scan again.
            Err(Error::Invalid(_)) if !started => self.due_records(sys, rule, None, limit).await?,
            other => other?,
        };
        // The records of people under a hold are kept as much as records placed under one.
        let mut tx = self.db.system_tenant_tx(sys.tenant).await?;
        let people: Vec<Uuid> = held_people_records(sys, &mut tx).await?.into_iter().filter(|(e, _)| *e == rule.entity).flat_map(|(_, ids)| ids).collect();
        tx.commit().await?;
        for id in &ids {
            let mut tx = self.db.system_tenant_tx(sys.tenant).await?;
            let on_hold = people.contains(id) || record_held(&mut tx, sys.tenant, &rule.entity, *id).await?;
            tx.commit().await?;
            if on_hold {
                held += 1;
                continue;
            }
            let outcome = match rule.action.as_str() {
                "delete" => self.delete(sys, &rule.entity, *id, None).await,
                _ => self.anonymize_record(sys, &rule.entity, *id, &rule.reason).await,
            };
            match outcome {
                Ok(()) => done += 1,
                Err(e) => {
                    failed += 1;
                    if failures.len() < LISTED_FAILURES {
                        failures.push(json!({"id": id, "error": e.to_string()}));
                    }
                }
            }
        }
        let outcome = json!({
            "reached": ids.len(), "done": done, "held": held, "failed": failed, "failures": failures,
            "scan": if started { "started" } else { "continued" }, "more": cursor.is_some(),
        });
        Ok((outcome, cursor))
    }

    fn check_rule(&self, ctx: &Ctx, r: &RetentionInput) -> Result<()> {
        checked_reason(&r.reason)?;
        let ent = self.entity(ctx, &r.entity)?;
        if matches!(ent.def.kind, EntityKind::Child | EntityKind::Ledger | EntityKind::Settings) {
            return Err(Error::field("entity", "records of this kind are not kept or removed one at a time"));
        }
        if !(1..=36500).contains(&r.after_days) {
            return Err(Error::field("after_days", "between 1 and 36500 days"));
        }
        if !matches!(r.date_field.as_str(), "created_at" | "updated_at")
            && !ent.field(&r.date_field).is_some_and(|f| matches!(f.ftype, FieldType::Date | FieldType::Datetime))
        {
            return Err(Error::field("date_field", "a date field of this entity"));
        }
        if !r.condition.is_null() && !r.condition.as_object().is_some_and(|o| o.is_empty()) {
            parse_filters(&r.condition)?;
        }
        let declared: Vec<_> = ctx.schema.personal_data.iter().filter(|d| d.entity == r.entity).collect();
        match r.action.as_str() {
            "delete" => {
                if let Some(why) = declared.iter().find_map(|d| d.retain.as_deref()) {
                    return Err(Error::field("action", format!("its package keeps these records: {why}")));
                }
            }
            "anonymize" => {
                if !declared.iter().any(|d| !d.erase.is_empty()) {
                    return Err(Error::field("action", "its package does not declare which fields are personal, so there is nothing to anonymize"));
                }
            }
            _ => return Err(Error::field("action", "delete or anonymize")),
        }
        Ok(())
    }

    pub async fn retention_rules(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar("select to_jsonb(t) - 'tenant_id' from retention_rule t where tenant_id = $1 order by created_at")
            .bind(ctx.tenant)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Adds or replaces a rule. It takes effect at the next daily run, and [`Kernel::retention_preview`]
    /// says what that would reach. A replaced rule's scan starts again from the beginning.
    pub async fn save_retention_rule(&self, ctx: &Ctx, id: Option<Uuid>, r: RetentionInput) -> Result<Uuid> {
        admin_only(ctx)?;
        self.check_rule(ctx, &r)?;
        let rule = id.unwrap_or_else(Uuid::now_v7);
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query(
            "insert into retention_rule (tenant_id, id, entity, after_days, date_field, action, condition, reason, is_active, created_by) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             on conflict (tenant_id, id) do update set entity = excluded.entity, after_days = excluded.after_days, date_field = excluded.date_field, action = excluded.action, \
             condition = excluded.condition, reason = excluded.reason, is_active = excluded.is_active, scan_cursor = null",
        )
        .bind(ctx.tenant)
        .bind(rule)
        .bind(&r.entity)
        .bind(r.after_days)
        .bind(&r.date_field)
        .bind(&r.action)
        .bind(if r.condition.is_null() { json!({}) } else { r.condition.clone() })
        .bind(r.reason.trim())
        .bind(r.is_active)
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .execute(&mut *tx)
        .await?;
        if n.rows_affected() == 0 {
            return Err(Error::NotFound("retention rule".into()));
        }
        audit::record(
            &mut tx,
            ctx,
            "RetentionRule",
            rule,
            "save",
            None,
            json!({"entity": r.entity, "after_days": r.after_days, "action": r.action, "reason": r.reason.trim()}),
        )
        .await?;
        tx.commit().await?;
        Ok(rule)
    }

    pub async fn delete_retention_rule(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("delete from retention_rule where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).execute(&mut *tx).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound("retention rule".into()));
        }
        audit::record(&mut tx, ctx, "RetentionRule", id, "delete", None, json!({})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// How many records a rule reaches now, from the beginning and up to a day's number, without
    /// touching any.
    pub async fn retention_preview(&self, ctx: &Ctx, id: Uuid) -> Result<Value> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rule: Option<Rule> = sqlx::query_as(sqlx::AssertSqlSafe(format!("{RULE} and id = $2"))).bind(ctx.tenant).bind(id).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        let rule = rule.ok_or_else(|| Error::NotFound("retention rule".into()))?;
        let sys = Ctx::system(ctx.tenant, ctx.schema.clone());
        let (ids, more) = self.due_records(&sys, &rule, None, self.settings.retention_daily_records.max(1)).await?;
        Ok(json!({"reaches": ids.len(), "more": more.is_some()}))
    }

    /// Queues today's run for every workspace with rules (`maintenance`, hourly: once a day each).
    pub(crate) async fn queue_retention(&self) -> Result<()> {
        let mut tx = self.db.system_tx().await?;
        let tenants: Vec<Uuid> = sqlx::query_scalar(
            "select distinct r.tenant_id from retention_rule r join tenant t on t.id = r.tenant_id where r.is_active and t.status = 'active'",
        )
        .fetch_all(&mut *tx)
        .await?;
        let today = Utc::now().date_naive();
        for tenant in tenants {
            let ran: bool =
                sqlx::query_scalar("select exists (select 1 from job where tenant_id = $1 and kind = $2 and created_at > now() - interval '20 hours')")
                    .bind(tenant)
                    .bind(RETENTION_KIND)
                    .fetch_one(&mut *tx)
                    .await?;
            if ran {
                continue;
            }
            let mut job = NewJob::new(RETENTION_KIND, json!({}));
            job.priority = PRIORITY_BULK;
            job.dedupe_key = Some(format!("retention-{today}"));
            enqueue(&mut tx, tenant, None, job).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Stops using someone's data, under every address they have had. Their records stay as they are.
    pub async fn restrict_processing(&self, ctx: &Ctx, email: &str, reason: &str) -> Result<Vec<String>> {
        admin_only(ctx)?;
        let email = checked_email(email)?;
        let reason = checked_reason(reason)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let emails = addresses(&mut tx, ctx.tenant, &email).await?;
        for address in &emails {
            sqlx::query(
                "insert into processing_restriction (tenant_id, id, email, reason, created_by) values ($1, $2, $3, $4, $5) \
                 on conflict (tenant_id, email) where lifted_at is null do nothing",
            )
            .bind(ctx.tenant)
            .bind(Uuid::now_v7())
            .bind(address)
            .bind(reason)
            .bind((!ctx.user.is_nil()).then_some(ctx.user))
            .execute(&mut *tx)
            .await?;
        }
        // By reference to its reason only: the audit trail outlives the restriction.
        audit::record(&mut tx, ctx, "PersonalData", Uuid::now_v7(), "restrict_processing", None, json!({"addresses": emails.len(), "reason": reason})).await?;
        tx.commit().await?;
        Ok(emails)
    }

    /// Lifts a restriction, under every address the person has had.
    pub async fn lift_restriction(&self, ctx: &Ctx, email: &str) -> Result<()> {
        admin_only(ctx)?;
        let email = checked_email(email)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let emails = addresses(&mut tx, ctx.tenant, &email).await?;
        let n = sqlx::query(
            "update processing_restriction set lifted_at = now(), lifted_by = $3 where tenant_id = $1 and lifted_at is null and lower(email::text) = any($2)",
        )
        .bind(ctx.tenant)
        .bind(&emails)
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("restriction".into()));
        }
        audit::record(&mut tx, ctx, "PersonalData", Uuid::now_v7(), "lift_restriction", None, json!({"addresses": n})).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn restrictions(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> =
            sqlx::query_scalar("select to_jsonb(t) - 'tenant_id' from processing_restriction t where tenant_id = $1 and lifted_at is null order by created_at")
                .bind(ctx.tenant)
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(rows)
    }

    /// Whether a transactional notice may be sent to this address: not while processing of their
    /// data is restricted. [`Kernel::may_contact_for`] for marketing.
    pub async fn may_contact(&self, tx: &mut crate::Tx, tenant: Uuid, email: &str) -> Result<bool> {
        self.may_contact_for(tx, tenant, email, Purpose::Transactional).await
    }

    /// The one decision every automated path that reaches people asks, at the moment a message goes
    /// out (FEO-015): kernel mail, apps' own delivery, automations and connectors. Nothing goes to a
    /// person whose processing is restricted; marketing does not go to one who withdrew their consent
    /// to it, under any address they have had. A transactional notice does not ask for consent.
    pub async fn may_contact_for(&self, tx: &mut crate::Tx, tenant: Uuid, email: &str, purpose: Purpose) -> Result<bool> {
        let Ok(email) = checked_email(email) else { return Ok(purpose == Purpose::Transactional) };
        let emails = addresses(tx, tenant, &email).await?;
        let (restricted, withdrawn): (bool, bool) = sqlx::query_as(
            "select exists (select 1 from processing_restriction where tenant_id = $1 and lifted_at is null and lower(email::text) = any($2)), \
                    exists (select 1 from contact_consent where tenant_id = $1 and purpose = 'marketing' and not granted and lower(email::text) = any($2))",
        )
        .bind(tenant)
        .bind(&emails)
        .fetch_one(&mut **tx)
        .await?;
        Ok(!(restricted || (purpose == Purpose::Marketing && withdrawn)))
    }

    /// Records what a person said about being contacted for `purpose`: an unsubscribe, or their own
    /// confirmed subscription, from any app. Every app's marketing asks it when it sends. Audited.
    pub async fn record_consent_tx(&self, tx: &mut crate::Tx, ctx: &Ctx, email: &str, purpose: Purpose, granted: bool, source: &str) -> Result<()> {
        let email = checked_email(email)?;
        let source = source.trim();
        if source.is_empty() || source.chars().count() > 200 {
            return Err(Error::field("source", "where it was said (a campaign's unsubscribe, a form), at most 200 characters"));
        }
        let Purpose::Marketing = purpose else { return Err(Error::field("purpose", "consent is recorded for marketing; transactional notices do not ask it")) };
        sqlx::query(
            "insert into contact_consent (tenant_id, email, purpose, granted, source, recorded_by) values ($1, $2, 'marketing', $3, $4, $5) \
             on conflict (tenant_id, email, purpose) do update set granted = excluded.granted, source = excluded.source, recorded_by = excluded.recorded_by, recorded_at = now()",
        )
        .bind(ctx.tenant)
        .bind(&email)
        .bind(granted)
        .bind(source)
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .execute(&mut **tx)
        .await?;
        audit::record(
            tx,
            ctx,
            "ContactConsent",
            Uuid::nil(),
            if granted { "consent_granted" } else { "consent_withdrawn" },
            None,
            json!({"email": email, "purpose": "marketing", "source": source}),
        )
        .await
    }

    /// [`Kernel::record_consent_tx`] in a transaction of its own.
    pub async fn record_consent(&self, ctx: &Ctx, email: &str, purpose: Purpose, granted: bool, source: &str) -> Result<()> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        self.record_consent_tx(&mut tx, ctx, email, purpose, granted, source).await?;
        tx.commit().await?;
        Ok(())
    }
}
