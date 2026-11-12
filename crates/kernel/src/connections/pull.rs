//! Pulling (BD-31): a schedule on a connection that calls one of its operations with a cursor kept between runs (an
//! opaque token from the last answer, or the moment the last run began), pages through what comes back up to a bound
//! per run, and maps each item as an inbound call is mapped. A page's records and the cursor past it commit together:
//! a run that stops half-way starts again at the page it did not finish, and mapping a page again changes nothing new.

use super::inbound::Mapping;
use super::{Outcome, Page, Pick, is_name};
use crate::Kernel;
use crate::ctx::Ctx;
use crate::db::Tx;
use crate::error::{Error, Result};
use crate::jobs::{Job, JobHandler, NewJob};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

pub const PULL_KIND: &str = "platform.connection.pull";
const DEFAULT_PAGES: u32 = 5;
const MAX_PAGES: u32 = 50;
const MIN_EVERY: u32 = 5;
const MAX_EVERY: u32 = 7 * 24 * 60;
/// Pulls queued in one look.
const SWEEP: i64 = 100;
/// Attempts of one run: the first, and as many more as a service answering "later" gets, with the jobs' backoff
/// (10 s, 20 s, 40 s), each only while it would start before the next scheduled run.
const ATTEMPTS: i32 = 4;

/// What the cursor is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cursor {
    /// The service's own, at `next_at` in each answer.
    #[default]
    Token,
    /// The moment the last run that finished began (RFC 3339).
    Time,
}

fn cursor_input() -> String {
    "cursor".into()
}

/// One schedule.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pull {
    pub name: String,
    pub operation: String,
    pub every_minutes: u32,
    /// Where the items are in each answer: a pointer to a list.
    pub items_at: String,
    #[serde(default)]
    pub cursor: Cursor,
    /// Where each answer gives the next cursor ([`Pick`]): a pointer, a header, or `{"link": "next"}`, whose address
    /// is then asked as it is for the next page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_at: Option<Pick>,
    /// The input the cursor is given as.
    #[serde(default = "cursor_input")]
    pub cursor_input: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pages: Option<u32>,
    /// What each item becomes; else the connection's inbound mapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map: Option<Mapping>,
}

impl Pull {
    pub(crate) fn problem(&self, desc: &super::Description) -> Option<String> {
        let at = |m: String| Some(format!("pull {}: {m}", self.name));
        if !is_name(&self.name) || !is_name(&self.cursor_input) {
            return at("its name and cursor_input are lower case letters, digits and _".into());
        }
        let Some(op) = desc.operations.get(&self.operation) else { return at(format!("there is no operation {}", self.operation)) };
        if op.changes() {
            return at(format!("{} changes something; a pull only reads", self.operation));
        }
        if desc.signs_in_with(&self.operation) {
            return at(format!("{} is how the connection signs in", self.operation));
        }
        if !op.inputs.is_empty() && !op.inputs.contains(&self.cursor_input) {
            return at(format!("{} takes no input {}", self.operation, self.cursor_input));
        }
        if !(MIN_EVERY..=MAX_EVERY).contains(&self.every_minutes) {
            return at(format!("every_minutes is {MIN_EVERY} to {MAX_EVERY}"));
        }
        if self.max_pages.is_some_and(|p| p == 0 || p > MAX_PAGES) {
            return at(format!("max_pages is 1 to {MAX_PAGES}"));
        }
        if let Some(p) = super::pointer_problem(&self.items_at).or_else(|| self.next_at.as_ref().and_then(|n| n.problem(true))) {
            return at(p);
        }
        if self.cursor == Cursor::Token && self.next_at.is_none() {
            return at("a token cursor says where the next one is (next_at)".into());
        }
        if self.map.is_none() && desc.inbound.as_ref().and_then(|i| i.map.as_ref()).is_none() {
            return at("it has a map, or the connection's inbound has one".into());
        }
        None
    }

    /// Whether the next page is the address an answer's `Link` names.
    fn by_link(&self) -> bool {
        self.next_at.as_ref().is_some_and(|n| n.spec(false).link.is_some())
    }

    fn map(&self, desc: &super::Description) -> Option<Mapping> {
        let mut m = self.map.clone().or_else(|| desc.inbound.as_ref().and_then(|i| i.map.clone()))?;
        m.items_at = Some(self.items_at.clone());
        Some(m)
    }
}

/// Keeps a connection's pull rows as its description says: one for each pull, due at once when new.
pub(crate) async fn keep(tx: &mut Tx, tenant: Uuid, id: Uuid, desc: &super::Description) -> Result<()> {
    let names: Vec<String> = desc.pulls.iter().map(|p| p.name.clone()).collect();
    sqlx::query("delete from conn_pull where tenant_id = $1 and conn_id = $2 and not (name = any($3))")
        .bind(tenant)
        .bind(id)
        .bind(&names)
        .execute(&mut **tx)
        .await?;
    sqlx::query("insert into conn_pull (tenant_id, conn_id, name) select $1, $2, unnest($3::text[]) on conflict do nothing")
        .bind(tenant)
        .bind(id)
        .bind(&names)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

impl Kernel {
    /// Queues the pulls that are due, on connections turned on (once a minute, from the schedule scanner).
    pub async fn sweep_pulls_at(&self, now: DateTime<Utc>) -> Result<usize> {
        let mut tx = self.db.system_tx().await?;
        let due: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
            "update conn_pull p set next_run = $1 + interval '5 minutes' from conn c \
             where c.tenant_id = p.tenant_id and c.id = p.conn_id and c.enabled and p.next_run <= $1 \
               and (p.tenant_id, p.conn_id, p.name) in (select tenant_id, conn_id, name from conn_pull where next_run <= $1 order by next_run limit $2) \
             returning p.tenant_id, p.conn_id, p.name",
        )
        .bind(now)
        .bind(SWEEP)
        .fetch_all(&mut *tx)
        .await?;
        for (tenant, conn, name) in &due {
            let mut job = NewJob::new(PULL_KIND, json!({"connection": conn, "pull": name}));
            job.dedupe_key = Some(format!("{conn}:{name}"));
            job.max_attempts = ATTEMPTS;
            crate::jobs::enqueue(&mut tx, *tenant, None, job).await?;
        }
        tx.commit().await?;
        Ok(due.len())
    }

    /// One run of a pull ([`PULL_KIND`]): pages up to its bound, each page's records and cursor committed together.
    async fn run_pull(&self, job: &Job) -> Result<Value> {
        let tenant = job.tenant_id;
        let conn_id: Uuid = job.payload["connection"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| Error::invalid("names its connection"))?;
        let name = job.payload["pull"].as_str().unwrap_or_default().to_string();
        let conn = self.load_connection(tenant, &conn_id.to_string()).await?;
        let Some(pull) = conn.desc.pulls.iter().find(|p| p.name == name).cloned().filter(|_| conn.enabled) else { return Ok(json!({"done": "gone"})) };
        let map = pull.map(&conn.desc).ok_or_else(|| Error::invalid("the pull has no mapping"))?;
        let ctx = Ctx::system(tenant, self.registry.schema(tenant).await?);
        let started = Utc::now();
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let mut cursor: Option<String> = sqlx::query_scalar("select cursor from conn_pull where tenant_id = $1 and conn_id = $2 and name = $3")
            .bind(tenant)
            .bind(conn_id)
            .bind(&name)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
        tx.commit().await?;
        let (mut items, mut pages, mut error, mut later, mut wait) = (0usize, 0u32, None, false, None);
        while pages < pull.max_pages.unwrap_or(DEFAULT_PAGES) {
            pages += 1;
            let mut inputs = serde_json::Map::new();
            if let Some(c) = &cursor {
                inputs.insert(pull.cursor_input.clone(), json!(c));
            }
            let given = json!(cursor);
            let page = Page { cursor: &given, address: cursor.as_deref().filter(|_| pull.by_link()) };
            let answer = match self.perform(&ctx, &conn, &pull.operation, &Value::Object(inputs), None, Some(page), None, None).await? {
                Outcome::Ok(_, answer) => answer,
                Outcome::Later(why, after) => {
                    (error, later, wait) = (Some(why), true, after);
                    break;
                }
                Outcome::No(why) => {
                    error = Some(why);
                    break;
                }
            };
            let next = match (pull.cursor, &pull.next_at) {
                (Cursor::Token, Some(n)) => {
                    let read = answer.read(&n.spec(false), &[("answer", &answer.body), ("connection", &json!({"name": conn.name, "id": conn.id}))]);
                    match read {
                        Ok(Value::String(s)) => Some(s),
                        Ok(Value::Null) => None,
                        Ok(other) => Some(other.to_string()),
                        Err(e) => {
                            error = Some(super::masked_text(&e.to_string(), &conn.mask()));
                            break;
                        }
                    }
                    .filter(|n| !n.is_empty() && Some(n) != cursor.as_ref())
                }
                _ => None,
            };
            let mut tx = self.db.system_tenant_tx(tenant).await?;
            match self.apply_mapping(&ctx, &mut tx, &map, &answer.body, &conn).await {
                Ok(n) => items += n,
                Err(e) => {
                    tx.rollback().await?;
                    error = Some(super::masked_text(&e.to_string(), &conn.mask()));
                    break;
                }
            }
            let moved = match (&pull.cursor, &next) {
                (Cursor::Token, Some(n)) => Some(n.clone()),
                _ => cursor.clone(),
            };
            sqlx::query("update conn_pull set cursor = $4 where tenant_id = $1 and conn_id = $2 and name = $3")
                .bind(tenant)
                .bind(conn_id)
                .bind(&name)
                .bind(&moved)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            cursor = moved;
            if next.is_none() {
                break;
            }
        }
        let next_run = started + chrono::Duration::minutes(pull.every_minutes as i64);
        // "Later": tried again from the page that failed (the cursor stays where the last page committed), while
        // attempts are left and the retry would start before the next scheduled run; the job's dedupe keeps the
        // schedule from queueing another meanwhile.
        // The service's own Retry-After (capped) in place of the backoff when it gave one.
        let delay = wait.map_or_else(|| crate::jobs::backoff(job.attempts), |s| s as f64);
        let retry = later && job.attempts < job.max_attempts && Utc::now() + chrono::Duration::milliseconds((delay * 1000.0) as i64) < next_run;
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let told: bool = sqlx::query_scalar("select told from conn_pull where tenant_id = $1 and conn_id = $2 and name = $3 for update")
            .bind(tenant)
            .bind(conn_id)
            .bind(&name)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(true);
        // Told once per streak of failed runs (a run retried is not over yet); a run that works ends the streak.
        let failed = error.is_some() && !retry;
        sqlx::query(
            "update conn_pull set last_run = $4, last_error = $5, next_run = $4 + make_interval(mins => $6), \
               cursor = case when $7 then $8 else cursor end, told = case when $5 is null then false when $9 then true else told end \
             where tenant_id = $1 and conn_id = $2 and name = $3",
        )
        .bind(tenant)
        .bind(conn_id)
        .bind(&name)
        .bind(started)
        .bind(&error)
        .bind(pull.every_minutes as i32)
        .bind(pull.cursor == Cursor::Time && error.is_none())
        .bind(started.to_rfc3339())
        .bind(failed)
        .execute(&mut *tx)
        .await?;
        if failed && !told {
            let admins: Vec<Uuid> = sqlx::query_scalar("select id from app_user where tenant_id = $1 and is_admin and is_active and kind = 'member'")
                .bind(tenant)
                .fetch_all(&mut *tx)
                .await?;
            let title = format!("Pulling {name} from {} failed", conn.name);
            for user in admins {
                crate::notify::send_kind(&mut tx, tenant, user, "general", "inapp", &title, error.as_deref(), Some("Connection"), Some(conn_id)).await?;
            }
        }
        tx.commit().await?;
        if retry {
            return Err(match wait {
                Some(secs) => Error::RetryAfter(error.unwrap_or_default(), secs),
                None => Error::Unavailable(error.unwrap_or_default()),
            });
        }
        Ok(json!({"pages": pages, "items": items, "error": error}))
    }

    /// Runs a pull now (an administrator's "pull now"): queued at once.
    pub async fn pull_now(&self, ctx: &Ctx, id: Uuid, name: &str) -> Result<Uuid> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("connections are configured by administrators".into()));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let here: bool = sqlx::query_scalar("select exists (select 1 from conn_pull where tenant_id = $1 and conn_id = $2 and name = $3)")
            .bind(ctx.tenant)
            .bind(id)
            .bind(name)
            .fetch_one(&mut *tx)
            .await?;
        if !here {
            return Err(Error::NotFound(format!("pull {name}")));
        }
        let mut job = NewJob::new(PULL_KIND, json!({"connection": id, "pull": name}));
        job.dedupe_key = Some(format!("{id}:{name}"));
        job.max_attempts = ATTEMPTS;
        let job = crate::jobs::enqueue(&mut tx, ctx.tenant, Some(ctx.user), job).await?;
        tx.commit().await?;
        Ok(job)
    }

    /// A connection's pulls: `{name, cursor, last_run, next_run, last_error}`.
    pub async fn connection_pulls(&self, ctx: &Ctx, id: Uuid) -> Result<Vec<Value>> {
        if !ctx.is_admin {
            return Err(Error::Forbidden("connections are configured by administrators".into()));
        }
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows = sqlx::query_scalar(
            "select jsonb_build_object('name', name, 'cursor', cursor, 'last_run', last_run, 'next_run', next_run, 'last_error', last_error) \
             from conn_pull where tenant_id = $1 and conn_id = $2 order by name",
        )
        .bind(ctx.tenant)
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }
}

struct Run;

#[async_trait]
impl JobHandler for Run {
    fn kind(&self) -> &'static str {
        PULL_KIND
    }

    fn title(&self) -> &'static str {
        "Pulling from a connection"
    }

    async fn run(&self, k: &Kernel, job: &Job) -> Result<Value> {
        k.run_pull(job).await
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(900)
    }
}

pub(crate) fn handler(kind: &str) -> Option<Arc<dyn JobHandler>> {
    (kind == PULL_KIND).then(|| Arc::new(Run) as Arc<dyn JobHandler>)
}
