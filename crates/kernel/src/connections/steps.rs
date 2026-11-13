//! An operation of steps (BD-53): several requests in order, each another operation of the connection whose inputs
//! are expressions over the caller's `inputs` and the earlier steps' results (`steps.<name>`). Under a key, each step is
//! a call of its own under `<key>:<step>`, its answer kept with the call (masked as every answer is), so that the same
//! key again resumes after the last step that finished and never makes one twice.
//!
//! A step that changes something and whose answer was lost (no answer, a 5xx other than 503, an answer that could not
//! be read) may have been delivered: it is sent again only when its operation carries an idempotency header (the
//! service then takes it once), or after a read-back (`confirm`) shows it did not happen; otherwise the call stops,
//! saying so. One key is made by one worker at a time: another is told to come back.

use super::{Answer, Extra, Loaded, Operation, Outcome, is_name};
use crate::Kernel;
use crate::ctx::Ctx;
use crate::db::Tx;
use crate::error::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

/// The most steps an operation has.
const MAX_STEPS: usize = 12;

/// One step.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Its name: `steps.<name>` is its result, `<key>:<name>` its key.
    pub name: String,
    /// Another operation of the connection, without steps of its own.
    pub operation: String,
    /// Each input an expression (the templates' language, without braces) over `inputs`, `steps`, `key` and what the
    /// step adds (`item`, `chunk`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, String>,
    /// Made only when this expression is true (else its result is null).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// How to know whether it happened when its answer was lost: a reading operation, given inputs over the same names
    /// and `sent` (the step's own inputs), that proves it did when `proves` is true over its `result` and `answer`; its
    /// result is then the step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<Confirm>,
    /// Asked again on a delay until its answer says it is ready (BD-54).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<Wait>,
}

/// A step repeated until `until` is true over its `result`, `answer` and `headers`, `every_secs` apart, failing when
/// `fail_if` is, or after `attempts` asks, or `within_secs` after the key's first attempt. Past the call's patience
/// ([`PATIENCE`]) it answers "later", after `every_secs`, and the next call carries on from here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wait {
    pub until: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_if: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
}

/// How long one call waits in place before it answers "later" for the queue to carry it.
const PATIENCE: std::time::Duration = std::time::Duration::from_secs(20);
const MAX_EVERY: u64 = 3600;
const MAX_WITHIN: u64 = 86_400;
const MAX_POLLS: u32 = 1000;

impl Wait {
    fn every(&self) -> u64 {
        self.every_secs.unwrap_or(5)
    }
    fn within(&self) -> u64 {
        self.within_secs.unwrap_or(600)
    }
    fn attempts(&self) -> u32 {
        self.attempts.unwrap_or(MAX_POLLS)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Confirm {
    pub operation: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, String>,
    pub proves: String,
}

/// What is wrong with an expression, if anything.
pub(crate) fn expression_problem(e: &str) -> Option<String> {
    let env = super::environment();
    env.compile_expression(e).err().map(|m| format!("'{e}': {m}"))
}

/// `e` over `vars`, as JSON.
pub(crate) fn evaluate(e: &str, vars: &Value) -> Result<Value> {
    let env = super::environment();
    let expr = env.compile_expression(e).map_err(|m| Error::invalid(format!("'{e}': {m}")))?;
    let v = expr.eval(minijinja::Value::from_serialize(vars)).map_err(|m| Error::invalid(format!("'{e}': {m}")))?;
    if v.is_undefined() || v.is_none() {
        return Ok(Value::Null);
    }
    serde_json::to_value(&v).map_err(|m| Error::invalid(format!("'{e}': {m}")))
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// What is wrong with operation `name`'s steps in `desc`.
pub(crate) fn problem(desc: &super::Description, op: &Operation) -> Option<String> {
    if op.steps.len() > MAX_STEPS {
        return Some(format!("at most {MAX_STEPS} steps"));
    }
    if !op.path.is_empty() || op.url.is_some() || !op.query.is_empty() || !op.headers.is_empty() || op.body.is_some() || !op.form.is_empty() {
        return Some("an operation of steps sends nothing itself: its steps do".into());
    }
    let mut names = std::collections::BTreeSet::new();
    for s in &op.steps {
        let at = |m: String| Some(format!("step {}: {m}", s.name));
        if !is_name(&s.name) {
            return at("a name is lower case letters, digits and _".into());
        }
        if !names.insert(&s.name) {
            return Some(format!("two steps are named {}", s.name));
        }
        let Some(inner) = desc.operations.get(&s.operation) else { return at(format!("there is no operation {}", s.operation)) };
        if !inner.steps.is_empty() {
            return at(format!("{} itself has steps", s.operation));
        }
        if desc.signs_in_with(&s.operation) {
            return at(format!("{} is how the connection signs in", s.operation));
        }
        if let Some(i) = s.inputs.keys().find(|i| !inner.inputs.is_empty() && !inner.inputs.contains(i)) {
            return at(format!("{} takes no input {i}", s.operation));
        }
        if let Some(w) = &s.wait {
            if inner.changes() {
                return at(format!("it waits only on an operation that reads, and {} changes something", s.operation));
            }
            if !(1..=MAX_EVERY).contains(&w.every()) || !(1..=MAX_WITHIN).contains(&w.within()) || !(1..=MAX_POLLS).contains(&w.attempts()) {
                return at(format!("wait: every_secs is 1 to {MAX_EVERY}, within_secs 1 to {MAX_WITHIN}, attempts 1 to {MAX_POLLS}"));
            }
        }
        if let Some(c) = &s.confirm {
            match desc.operations.get(&c.operation) {
                None => return at(format!("confirm: there is no operation {}", c.operation)),
                Some(o) if o.changes() || !o.steps.is_empty() => return at(format!("confirm: {} only reads, in one request", c.operation)),
                _ => {}
            }
        }
        let exprs = s
            .inputs
            .values()
            .chain(s.when.iter())
            .chain(s.confirm.iter().flat_map(|c| c.inputs.values().chain(std::iter::once(&c.proves))))
            .chain(s.wait.iter().flat_map(|w| std::iter::once(&w.until).chain(w.fail_if.iter())));
        if let Some(p) = exprs.filter_map(|e| expression_problem(e)).next() {
            return at(p);
        }
    }
    None
}

/// The row a key's attempt keeps (`conn_attempt`).
pub(crate) struct Attempt {
    pub digest: String,
    pub clock: DateTime<Utc>,
    pub sending: bool,
    /// A wait's asks, a loop's place.
    pub progress: Value,
}

impl Kernel {
    /// The attempt at `op` under `key`, made the first time with `digest`: the clock fixed then, whether a request went
    /// out whose answer is not known.
    pub(crate) async fn key_attempt(&self, conn: &Loaded, op: &str, key: &str, digest: &str) -> Result<Attempt> {
        let mut tx = self.db.system_tenant_tx(conn.tenant).await?;
        sqlx::query("insert into conn_attempt (tenant_id, conn_id, operation, key, input_digest) values ($1, $2, $3, $4, $5) on conflict do nothing")
            .bind(conn.tenant)
            .bind(conn.id)
            .bind(op)
            .bind(key)
            .bind(digest)
            .execute(&mut *tx)
            .await?;
        let (digest, clock, sending, progress): (String, DateTime<Utc>, bool, Value) = sqlx::query_as(
            "select input_digest, clock, sending, progress from conn_attempt where tenant_id = $1 and conn_id = $2 and operation = $3 and key = $4",
        )
        .bind(conn.tenant)
        .bind(conn.id)
        .bind(op)
        .bind(key)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Attempt { digest, clock, sending, progress })
    }

    /// Keeps a wait's or a loop's place under `key`.
    pub(crate) async fn progress(&self, conn: &Loaded, op: &str, key: &str, progress: &Value) -> Result<()> {
        let mut tx = self.db.system_tenant_tx(conn.tenant).await?;
        sqlx::query("update conn_attempt set progress = $5, at = now() where tenant_id = $1 and conn_id = $2 and operation = $3 and key = $4")
            .bind(conn.tenant)
            .bind(conn.id)
            .bind(op)
            .bind(key)
            .bind(progress)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Marks whether a request of `op` under `key` is out with its answer unknown.
    pub(crate) async fn sending(&self, conn: &Loaded, op: &str, key: &str, sending: bool) -> Result<()> {
        let mut tx = self.db.system_tenant_tx(conn.tenant).await?;
        sqlx::query("update conn_attempt set sending = $5, at = now() where tenant_id = $1 and conn_id = $2 and operation = $3 and key = $4")
            .bind(conn.tenant)
            .bind(conn.id)
            .bind(op)
            .bind(key)
            .bind(sending)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Keeps `result` as what `op` under `key` gave (a step proved by reading back, an operation of steps finished).
    pub(crate) async fn remember(&self, ctx: &Ctx, conn: &Loaded, op: &str, key: &str, digest: &str, result: &Value) -> Result<()> {
        let mut tx = self.db.system_tenant_tx(conn.tenant).await?;
        sqlx::query(
            "insert into conn_call (tenant_id, id, conn_id, operation, outcome, key, input_digest, result, user_id) \
             values ($1, $2, $3, $4, 'ok', $5, $6, $7, $8) on conflict do nothing",
        )
        .bind(conn.tenant)
        .bind(Uuid::now_v7())
        .bind(conn.id)
        .bind(op)
        .bind(key)
        .bind(digest)
        .bind(super::masked(result, &conn.mask()))
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Holds `key` of `op` for this worker until the transaction ends: `None` when another holds it.
    pub(crate) async fn hold_key(&self, conn: &Loaded, op: &str, key: &str) -> Result<Option<Tx>> {
        let mut tx = self.db.system_tenant_tx(conn.tenant).await?;
        let held: bool = sqlx::query_scalar("select pg_try_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("conn_key:{}:{}:{op}:{key}", conn.tenant, conn.id))
            .fetch_one(&mut *tx)
            .await?;
        Ok(held.then_some(tx))
    }

    /// Runs operation `name`, made of steps, for `inputs` under `key` (none: nothing kept, nothing resumed): its result
    /// read from `{<step>: <result>}` as its own `result` says (none: all of it).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn perform_steps(
        &self,
        ctx: &Ctx,
        conn: &Loaded,
        name: &str,
        inputs: &Value,
        key: Option<&str>,
        clock: DateTime<Utc>,
        about: Option<(&str, Uuid)>,
    ) -> Result<Outcome> {
        let op = conn.desc.operations.get(name).ok_or_else(|| Error::invalid(format!("{} has no operation {name}", conn.name)))?;
        let started = std::time::Instant::now();
        let mut done = Map::new();
        for step in &op.steps {
            let vars = json!({"inputs": inputs, "steps": done, "key": key});
            if let Some(w) = &step.when
                && !truthy(&evaluate(w, &vars)?)
            {
                done.insert(step.name.clone(), Value::Null);
                continue;
            }
            let skey = key.map(|k| format!("{k}:{}", step.name));
            match self.step(ctx, conn, step, &vars, skey.as_deref(), (clock, started), about).await? {
                Outcome::Ok(result, _) => {
                    done.insert(step.name.clone(), result);
                }
                Outcome::Later(why, after) => return Ok(Outcome::Later(format!("{}: {why}", step.name), after)),
                Outcome::No(why) => return Ok(Outcome::No(format!("{}: {why}", step.name))),
            }
        }
        let answer = Answer {
            status: 200,
            body: Value::Object(done),
            headers: vec![],
            url: reqwest::Url::parse("https://steps.invalid/").map_err(Error::internal)?,
            raw: vec![],
        };
        let result = match self.mapped(ctx, conn, op, inputs, &answer, about).await {
            Ok(r) => super::masked(&r, &conn.mask()),
            Err(e) => return Ok(Outcome::No(super::masked_text(&e.to_string(), &conn.mask()))),
        };
        Ok(Outcome::Ok(result, Box::new(answer)))
    }

    /// One step: its inputs, its earlier answer under its key, or the request made (once).
    #[allow(clippy::too_many_arguments)]
    async fn step(
        &self,
        ctx: &Ctx,
        conn: &Loaded,
        step: &Step,
        vars: &Value,
        skey: Option<&str>,
        (clock, started): (DateTime<Utc>, std::time::Instant),
        about: Option<(&str, Uuid)>,
    ) -> Result<Outcome> {
        let mut given = Map::new();
        for (n, e) in &step.inputs {
            given.insert(n.clone(), evaluate(e, vars)?);
        }
        let given = Value::Object(given);
        let inner = &conn.desc.operations[&step.operation];
        let extra = Extra { now: Some(clock), ..Default::default() };
        let digest = super::digest(&given);
        if let Some(skey) = skey
            && let Some((_, result)) = self.done_before(conn, &step.operation, skey).await?
        {
            return Ok(Outcome::Ok(result, Box::new(empty_answer()?)));
        }
        if let Some(w) = &step.wait {
            return self.wait(ctx, conn, step, w, (&given, vars), skey, (clock, started), about).await;
        }
        let Some(skey) = skey else { return self.perform(ctx, conn, &step.operation, &given, None, extra, about, None).await };
        let changes = inner.changes();
        if changes && self.key_attempt(conn, &step.operation, skey, &digest).await?.sending {
            // Its answer was lost: it may have happened.
            if inner.idempotency_header.is_none() {
                let Some(c) = &step.confirm else {
                    return Ok(Outcome::No(
                        "it may have been sent already (its answer was lost) and is not sent again: the description gives it no idempotency_header or \
                         confirm; check the service, then call again with a new key"
                            .into(),
                    ));
                };
                let mut cv = vars.clone();
                cv["sent"] = given.clone();
                let mut ci = Map::new();
                for (n, e) in &c.inputs {
                    ci.insert(n.clone(), evaluate(e, &cv)?);
                }
                match self.perform(ctx, conn, &c.operation, &Value::Object(ci), None, extra, about, None).await? {
                    Outcome::Ok(result, answer) => {
                        cv["result"] = result.clone();
                        cv["answer"] = answer.body.clone();
                        if truthy(&evaluate(&c.proves, &cv)?) {
                            self.remember(ctx, conn, &step.operation, skey, &digest, &result).await?;
                            self.sending(conn, &step.operation, skey, false).await?;
                            return Ok(Outcome::Ok(result, answer));
                        }
                    }
                    other => return Ok(other),
                }
            }
        }
        if changes {
            self.sending(conn, &step.operation, skey, true).await?;
        }
        let (outcome, status, sent) = self.perform_full(ctx, conn, &step.operation, &given, Some(skey), extra, about, Some(&digest)).await?;
        // Not delivered, or the service said it did nothing (it refused, or asked to come back): free to send again.
        let not_done = !sent || status.is_some_and(|s| (400..500).contains(&s) || s == 503);
        if changes && (not_done || matches!(outcome, Outcome::Ok(..))) {
            self.sending(conn, &step.operation, skey, false).await?;
        }
        Ok(outcome)
    }
}

impl Kernel {
    /// A step asked until it is ready ([`Wait`]); its result, kept under its key once it is.
    #[allow(clippy::too_many_arguments)]
    async fn wait(
        &self,
        ctx: &Ctx,
        conn: &Loaded,
        step: &Step,
        w: &Wait,
        (given, vars): (&Value, &Value),
        skey: Option<&str>,
        (clock, started): (DateTime<Utc>, std::time::Instant),
        about: Option<(&str, Uuid)>,
    ) -> Result<Outcome> {
        let digest = super::digest(given);
        let mut polls = match skey {
            Some(k) => self.key_attempt(conn, &step.operation, k, &digest).await?.progress["polls"].as_u64().unwrap_or(0) as u32,
            None => 0,
        };
        let deadline = clock + chrono::Duration::seconds(w.within() as i64);
        let extra = Extra { now: Some(clock), ..Default::default() };
        loop {
            if Utc::now() > deadline {
                return Ok(Outcome::No(format!("not ready within {} s", w.within())));
            }
            match self.perform(ctx, conn, &step.operation, given, None, extra, about, None).await? {
                Outcome::Ok(result, answer) => {
                    let mut v = vars.clone();
                    v["result"] = result.clone();
                    v["answer"] = answer.body.clone();
                    v["headers"] = Value::Object(answer.headers.iter().map(|(h, x)| (h.clone(), Value::String(x.clone()))).collect());
                    if let Some(f) = &w.fail_if
                        && truthy(&evaluate(f, &v)?)
                    {
                        let said: String = result.to_string().chars().take(300).collect();
                        return Ok(Outcome::No(format!("the service says it failed: {said}")));
                    }
                    if truthy(&evaluate(&w.until, &v)?) {
                        if let Some(k) = skey {
                            self.remember(ctx, conn, &step.operation, k, &digest, &result).await?;
                        }
                        return Ok(Outcome::Ok(result, answer));
                    }
                }
                // "Later" from the service is "not ready yet".
                Outcome::Later(..) => {}
                no @ Outcome::No(_) => return Ok(no),
            }
            polls += 1;
            if let Some(k) = skey {
                self.progress(conn, &step.operation, k, &json!({"polls": polls})).await?;
            }
            if polls >= w.attempts() {
                return Ok(Outcome::No(format!("not ready after {polls} asks")));
            }
            let every = std::time::Duration::from_secs(w.every());
            if started.elapsed() + every > PATIENCE {
                return Ok(Outcome::Later("not ready yet".into(), Some(w.every())));
            }
            tokio::time::sleep(every).await;
        }
    }
}

pub(crate) fn empty_answer() -> Result<Answer> {
    Ok(Answer { status: 200, body: Value::Null, headers: vec![], url: reqwest::Url::parse("https://steps.invalid/").map_err(Error::internal)?, raw: vec![] })
}
