//! An operation of steps (BD-53): several requests in order, each another operation of the connection whose inputs
//! are expressions over the caller's `inputs` and the earlier steps' results (`steps.<name>`). Under a key, each step is
//! a call of its own under `<key>:<step>`, its answer kept with the call (masked as every answer is), so that the same
//! key again resumes after the last step that finished and never makes one twice.
//!
//! A step that changes something and whose answer was lost (no answer, a 5xx other than 503, an answer that could not
//! be read) may have been delivered: it is sent again only when its operation carries an idempotency header (the
//! service then takes it once), or after a read-back (`confirm`) shows it did not happen; otherwise the call stops,
//! saying so. One key is made by one worker at a time: another is told to come back.

use super::{Answer, Extra, Loaded, More, Operation, Outcome, is_name};
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
/// The most items one step goes through, and chunks it sends; how often an upload starts again.
const MAX_ITEMS: usize = 100;
const MAX_CHUNKS: u64 = 10_000;
const MAX_RESTARTS: u64 = 2;
/// What a step that needs settling says among the outcomes (never a service's words: they cannot hold a NUL).
const LOST: &str = "\u{0}lost";
const RESTART: &str = "\u{0}restart";

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
    /// Made for each item of this list (an expression), in order (BD-55): `item`, `index`, `previous` (the result
    /// before) and `results` (so far) are bound; the step's result is the list of theirs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_each: Option<String>,
    /// With `for_each`: the range of a stored file each item sends (an upload the service cut into parts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
    /// Made for each chunk of a stored file (BD-55): `chunk` is bound; the step's result is the last chunk's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks: Option<Chunks>,
}

/// A range of a stored file, as expressions: its id, the first byte, how many.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    pub file: String,
    pub offset: String,
    pub size: String,
}

impl Range {
    fn resolve(&self, vars: &Value) -> Result<(Uuid, u64, u64)> {
        let file = evaluate(&self.file, vars)?;
        let file = file.as_str().and_then(|s| s.trim().parse().ok()).ok_or_else(|| Error::invalid(format!("range: {file} is not a stored file")))?;
        let n = |e: &str| evaluate(e, vars).ok().and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()));
        match (n(&self.offset), n(&self.size)) {
            (Some(o), Some(s)) if s > 0 => Ok((file, o, s)),
            _ => Err(Error::invalid("range: offset and size are whole numbers, size above 0")),
        }
    }
}

/// A stored file sent `bytes` at a time. `chunk` is `{index, offset, end (its last byte), size, total, count, last}`.
/// A chunk whose answer was lost is settled by `offset_from` (a reading operation whose `offset` result says how many
/// bytes the service holds: a number or `bytes=0-<last>`; none and a 2xx: the upload is complete), else the upload starts
/// again from the step `restart_from` names (under new keys), else the call stops, saying so.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chunks {
    pub file: String,
    pub bytes: u64,
    /// `last`: what is left over goes in the last chunk (so every chunk is at least `bytes`); `own` (default): a smaller
    /// chunk of its own.
    #[serde(default, skip_serializing_if = "is_own")]
    pub remainder: Remainder,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_from: Option<Ask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_from: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Remainder {
    #[default]
    Own,
    Last,
}

fn is_own(r: &Remainder) -> bool {
    *r == Remainder::Own
}

/// A reading operation asked with inputs (expressions).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ask {
    pub operation: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, String>,
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
pub(crate) fn problem(desc: &super::Description, op: &Operation, more: &More) -> Option<String> {
    if more.steps.len() > MAX_STEPS {
        return Some(format!("at most {MAX_STEPS} steps"));
    }
    if !op.path.is_empty() || more.url.is_some() || !op.query.is_empty() || !op.headers.is_empty() || op.body.is_some() || !op.form.is_empty() {
        return Some("an operation of steps sends nothing itself: its steps do".into());
    }
    let mut names = std::collections::BTreeSet::new();
    for s in &more.steps {
        let at = |m: String| Some(format!("step {}: {m}", s.name));
        if !is_name(&s.name) {
            return at("a name is lower case letters, digits and _".into());
        }
        if !names.insert(&s.name) {
            return Some(format!("two steps are named {}", s.name));
        }
        let Some(inner) = desc.operations.get(&s.operation) else { return at(format!("there is no operation {}", s.operation)) };
        if !desc.more(&s.operation).steps.is_empty() {
            return at(format!("{} itself has steps", s.operation));
        }
        if desc.signs_in_with(&s.operation) {
            return at(format!("{} is how the connection signs in", s.operation));
        }
        if let Some(i) = s.inputs.keys().find(|i| !inner.inputs.is_empty() && !inner.inputs.contains(i)) {
            return at(format!("{} takes no input {i}", s.operation));
        }
        if s.for_each.is_some() && s.chunks.is_some() || (s.wait.is_some() && (s.for_each.is_some() || s.chunks.is_some())) {
            return at("a step goes for_each or chunks, or waits: one of them".into());
        }
        if s.range.is_some() && s.for_each.is_none() {
            return at("a range goes with for_each".into());
        }
        if let Some(c) = &s.chunks {
            if !(1..=super::MAX_FILE_BYTES).contains(&c.bytes) {
                return at(format!("chunks.bytes is 1 to {}", super::MAX_FILE_BYTES));
            }
            if let Some(r) = &c.restart_from
                && !more.steps.iter().take_while(|x| x.name != s.name).any(|x| &x.name == r)
                && r != &s.name
            {
                return at(format!("chunks.restart_from: {r} is not this step or one before it"));
            }
            if let Some(a) = &c.offset_from {
                match desc.operations.get(&a.operation) {
                    Some(_) if !desc.changes(&a.operation) && desc.more(&a.operation).steps.is_empty() => {}
                    _ => return at(format!("chunks.offset_from: {} is an operation that reads, in one request", a.operation)),
                }
            }
        }
        if let Some(w) = &s.wait {
            if desc.changes(&s.operation) {
                return at(format!("it waits only on an operation that reads, and {} changes something", s.operation));
            }
            if !(1..=MAX_EVERY).contains(&w.every()) || !(1..=MAX_WITHIN).contains(&w.within()) || !(1..=MAX_POLLS).contains(&w.attempts()) {
                return at(format!("wait: every_secs is 1 to {MAX_EVERY}, within_secs 1 to {MAX_WITHIN}, attempts 1 to {MAX_POLLS}"));
            }
        }
        if let Some(c) = &s.confirm {
            match desc.operations.get(&c.operation) {
                None => return at(format!("confirm: there is no operation {}", c.operation)),
                Some(_) if desc.changes(&c.operation) || !desc.more(&c.operation).steps.is_empty() => {
                    return at(format!("confirm: {} only reads, in one request", c.operation));
                }
                _ => {}
            }
        }
        let exprs = s
            .inputs
            .values()
            .chain(s.when.iter())
            .chain(s.confirm.iter().flat_map(|c| c.inputs.values().chain(std::iter::once(&c.proves))))
            .chain(s.wait.iter().flat_map(|w| std::iter::once(&w.until).chain(w.fail_if.iter())))
            .chain(s.for_each.iter())
            .chain(s.range.iter().flat_map(|r| [&r.file, &r.offset, &r.size]))
            .chain(s.chunks.iter().flat_map(|c| std::iter::once(&c.file).chain(c.offset_from.iter().flat_map(|a| a.inputs.values()))));
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
    /// read from `{<step>: <result>}` as its own `result` says (none: all of it). A chunk loop that must start its
    /// upload again ([`Chunks::restart_from`]) makes the steps from there again under new keys, at most twice.
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
        let digest = super::digest(inputs);
        // Which generation of keys the steps from `from` on are under: one more each time an upload starts again.
        let (mut round, mut from) = match key {
            Some(k) => {
                let p = self.key_attempt(conn, name, k, &digest).await?.progress;
                (p["gen"].as_u64().unwrap_or(0), p["from"].as_u64().unwrap_or(0) as usize)
            }
            None => (0, 0),
        };
        let mut done = Map::new();
        let mut i = 0;
        let steps = &conn.desc.more(name).steps;
        while let Some(step) = steps.get(i) {
            let (now, today) = super::clock_vars(clock);
            let vars = json!({"inputs": inputs, "steps": done, "key": key, "now": now, "today": today});
            if let Some(w) = &step.when
                && !truthy(&evaluate(w, &vars)?)
            {
                done.insert(step.name.clone(), Value::Null);
                i += 1;
                continue;
            }
            let skey = key.map(|k| if round > 0 && i >= from { format!("{k}:{}~{round}", step.name) } else { format!("{k}:{}", step.name) });
            match self.step(ctx, conn, step, &vars, skey.as_deref(), (clock, started), about).await? {
                Outcome::Ok(result, _) => {
                    done.insert(step.name.clone(), result);
                    i += 1;
                }
                Outcome::Later(why, _) if why == RESTART => {
                    let (Some(k), Some(back)) = (key, step.chunks.as_ref().and_then(|c| c.restart_from.as_deref())) else {
                        return Ok(Outcome::No(format!("{}: {}", step.name, lost())));
                    };
                    if round >= MAX_RESTARTS {
                        return Ok(Outcome::No(format!("{}: the upload was started again {MAX_RESTARTS} times; giving up", step.name)));
                    }
                    (round, from) = (round + 1, steps.iter().position(|s| s.name == back).unwrap_or(i));
                    self.progress(conn, name, k, &json!({"gen": round, "from": from})).await?;
                    for s in &steps[from..] {
                        done.remove(&s.name);
                    }
                    i = from;
                }
                Outcome::Later(why, after) => return Ok(Outcome::Later(format!("{}: {why}", step.name), after)),
                Outcome::No(why) => return Ok(Outcome::No(format!("{}: {why}", step.name))),
            }
        }
        let answer = Answer { body: Value::Object(done), ..empty_answer()? };
        let result = match self.mapped(ctx, conn, op, inputs, &answer, about).await {
            Ok(r) => super::masked(&r, &conn.mask()),
            Err(e) => return Ok(Outcome::No(super::masked_text(&e.to_string(), &conn.mask()))),
        };
        Ok(Outcome::Ok(result, Box::new(answer)))
    }

    /// One step: its earlier result under its key, or made: once, for each item, or for each chunk, or until ready.
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
        if let Some(skey) = skey
            && let Some((_, result)) = self.done_before(conn, &step.operation, skey).await?
        {
            return Ok(Outcome::Ok(result, Box::new(empty_answer()?)));
        }
        if let Some(list) = &step.for_each {
            return self.each(ctx, conn, step, list, vars, skey, clock, about).await;
        }
        if let Some(c) = &step.chunks {
            return self.chunked(ctx, conn, step, c, vars, skey, clock, about).await;
        }
        let given = inputs_of(step, vars)?;
        if let Some(w) = &step.wait {
            return self.wait(ctx, conn, step, w, (&given, vars), skey, (clock, started), about).await;
        }
        let extra = Extra { now: Some(clock), ..Default::default() };
        Ok(plain(self.once(ctx, conn, step, &given, vars, skey, extra, about).await?))
    }

    /// One request of a step under `skey`: its earlier result, or made once. A lost answer that neither the operation's
    /// idempotency header nor the step's `confirm` settles is `Later(LOST)`, for the caller to settle or refuse.
    #[allow(clippy::too_many_arguments)]
    async fn once(
        &self,
        ctx: &Ctx,
        conn: &Loaded,
        step: &Step,
        given: &Value,
        vars: &Value,
        skey: Option<&str>,
        extra: Extra<'_>,
        about: Option<(&str, Uuid)>,
    ) -> Result<Outcome> {
        let Some(skey) = skey else { return self.perform(ctx, conn, &step.operation, given, None, extra, about, None).await };
        if let Some((_, result)) = self.done_before(conn, &step.operation, skey).await? {
            return Ok(Outcome::Ok(result, Box::new(empty_answer()?)));
        }
        let inner = &conn.desc.operations[&step.operation];
        let digest = super::digest(given);
        let changes = conn.desc.changes(&step.operation);
        if changes && self.key_attempt(conn, &step.operation, skey, &digest).await?.sending && inner.idempotency_header.is_none() {
            // Its answer was lost: it may have happened.
            let Some(c) = &step.confirm else { return Ok(Outcome::Later(LOST.into(), None)) };
            let mut cv = vars.clone();
            cv["sent"] = given.clone();
            let mut ci = Map::new();
            for (n, e) in &c.inputs {
                ci.insert(n.clone(), evaluate(e, &cv)?);
            }
            let plain = Extra { range: None, ..extra };
            match self.perform(ctx, conn, &c.operation, &Value::Object(ci), None, plain, about, None).await? {
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
        if changes {
            self.sending(conn, &step.operation, skey, true).await?;
        }
        let (outcome, status, sent) = self.perform_full(ctx, conn, &step.operation, given, Some(skey), extra, about, Some(&digest)).await?;
        // Not delivered, or the service said it did nothing (it refused, or asked to come back): free to send again.
        let not_done = !sent || status.is_some_and(|s| (400..500).contains(&s) || s == 503);
        if changes && (not_done || matches!(outcome, Outcome::Ok(..))) {
            self.sending(conn, &step.operation, skey, false).await?;
        }
        Ok(outcome)
    }

    /// A step made for each item of a list, in order, each under `<key>:<index>`: their results, a list.
    #[allow(clippy::too_many_arguments)]
    async fn each(
        &self,
        ctx: &Ctx,
        conn: &Loaded,
        step: &Step,
        list: &str,
        vars: &Value,
        skey: Option<&str>,
        clock: DateTime<Utc>,
        about: Option<(&str, Uuid)>,
    ) -> Result<Outcome> {
        let items = match evaluate(list, vars)? {
            Value::Array(a) => a,
            Value::Null => vec![],
            _ => return Ok(Outcome::No("for_each gives no list".into())),
        };
        if items.len() > MAX_ITEMS {
            return Ok(Outcome::No(format!("{} items, more than the {MAX_ITEMS} a step makes", items.len())));
        }
        let mut results: Vec<Value> = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let mut v = vars.clone();
            v["item"] = item.clone();
            v["index"] = json!(i);
            v["previous"] = results.last().cloned().unwrap_or(Value::Null);
            v["results"] = json!(results);
            let given = inputs_of(step, &v)?;
            let range = match &step.range {
                Some(r) => Some(r.resolve(&v)?),
                None => None,
            };
            let ikey = skey.map(|k| format!("{k}:{i}"));
            let extra = Extra { now: Some(clock), range, ..Default::default() };
            match plain(self.once(ctx, conn, step, &given, &v, ikey.as_deref(), extra, about).await?) {
                Outcome::Ok(r, _) => results.push(r),
                Outcome::Later(why, after) => return Ok(Outcome::Later(format!("item {}: {why}", i + 1), after)),
                Outcome::No(why) => return Ok(Outcome::No(format!("item {}: {why}", i + 1))),
            }
        }
        let out = Value::Array(results);
        if let Some(k) = skey {
            self.remember(ctx, conn, &step.operation, k, &super::digest(&json!(items)), &out).await?;
        }
        Ok(Outcome::Ok(out, Box::new(empty_answer()?)))
    }

    /// A step made for each chunk of a stored file, in order, each under `<key>@<offset>` and sending only its range
    /// (read from storage a chunk at a time): the last chunk's result. Its place is kept, so a later call goes on from
    /// the first chunk not sent.
    #[allow(clippy::too_many_arguments)]
    async fn chunked(
        &self,
        ctx: &Ctx,
        conn: &Loaded,
        step: &Step,
        c: &Chunks,
        vars: &Value,
        skey: Option<&str>,
        clock: DateTime<Utc>,
        about: Option<(&str, Uuid)>,
    ) -> Result<Outcome> {
        let given = evaluate(&c.file, vars)?;
        let file: Uuid = given.as_str().and_then(|s| s.trim().parse().ok()).ok_or_else(|| Error::invalid(format!("{given} is not a stored file")))?;
        let info = self.file_info(ctx, file).await.map_err(|_| Error::invalid(format!("the file {file} is not one you may send")))?;
        let cap = self.settings.connection_upload_bytes;
        if info.size <= 0 {
            return Ok(Outcome::No(format!("{} is empty", info.name)));
        }
        let total = info.size as u64;
        if total > cap {
            return Ok(Outcome::No(format!("{} is larger than the {cap} bytes a chunked send may take", info.name)));
        }
        let count = if c.remainder == Remainder::Last { (total / c.bytes).max(1) } else { total.div_ceil(c.bytes) };
        if count > MAX_CHUNKS {
            return Ok(Outcome::No(format!("{count} chunks, more than the {MAX_CHUNKS} a step sends")));
        }
        let digest = super::digest(&json!({"file": file, "bytes": c.bytes}));
        let mut offset = match skey {
            Some(k) => self.key_attempt(conn, &step.operation, k, &digest).await?.progress["offset"].as_u64().unwrap_or(0),
            None => 0,
        };
        let mut last = Value::Null;
        while offset < total {
            let index = (offset / c.bytes).min(count - 1);
            let size = if index == count - 1 { total - offset } else { c.bytes.min(total - offset) };
            let mut v = vars.clone();
            v["chunk"] = json!({"index": index, "offset": offset, "end": offset + size - 1, "size": size, "total": total, "count": count,
                                "last": offset + size == total});
            let given = inputs_of(step, &v)?;
            let ckey = skey.map(|k| format!("{k}@{offset}"));
            let extra = Extra { now: Some(clock), range: Some((file, offset, size)), ..Default::default() };
            match self.once(ctx, conn, step, &given, &v, ckey.as_deref(), extra, about).await? {
                Outcome::Ok(r, _) => {
                    last = r;
                    offset += size;
                }
                Outcome::Later(why, _) if why == LOST => {
                    let ckey = ckey.unwrap_or_default();
                    let Some(of) = &c.offset_from else {
                        return Ok(if c.restart_from.is_some() {
                            Outcome::Later(RESTART.into(), None)
                        } else {
                            Outcome::No(format!("chunk {index}: {}", lost()))
                        });
                    };
                    // Asked how much it holds: carried on from there.
                    let mut ask = Map::new();
                    for (n, e) in &of.inputs {
                        ask.insert(n.clone(), evaluate(e, &v)?);
                    }
                    let extra = Extra { now: Some(clock), ..Default::default() };
                    let (outcome, status, _) = self.perform_full(ctx, conn, &of.operation, &Value::Object(ask), None, extra, about, None).await?;
                    let Outcome::Ok(r, _) = outcome else { return Ok(outcome) };
                    let held = match held_offset(&r["offset"]) {
                        // No offset and a plain success: the upload is complete, and this is its answer.
                        None if status.is_some_and(|s| (200..300).contains(&s)) => {
                            (last, offset) = (r, total);
                            self.sending(conn, &step.operation, &ckey, false).await?;
                            continue;
                        }
                        held => held.unwrap_or(0),
                    };
                    if held < offset || held > total {
                        return Ok(Outcome::No(format!("the service says it holds {held} bytes, not what was sent before ({offset})")));
                    }
                    self.sending(conn, &step.operation, &ckey, false).await?;
                    offset = held;
                }
                Outcome::Later(why, after) => return Ok(Outcome::Later(format!("chunk {index}: {why}"), after)),
                Outcome::No(why) => return Ok(Outcome::No(format!("chunk {index}: {why}"))),
            }
            if let Some(k) = skey {
                self.progress(conn, &step.operation, k, &json!({"offset": offset})).await?;
            }
        }
        if let Some(k) = skey {
            self.remember(ctx, conn, &step.operation, k, &digest, &last).await?;
        }
        Ok(Outcome::Ok(last, Box::new(empty_answer()?)))
    }
}

/// What a lost answer that nothing settles is said as.
fn lost() -> String {
    "it may have been sent already (its answer was lost) and is not sent again: the description gives it no idempotency_header or confirm; \
     check the service, then call again with a new key"
        .into()
}

/// An outcome as its caller sees it: a lost answer nothing settled, refused.
fn plain(o: Outcome) -> Outcome {
    match o {
        Outcome::Later(why, _) if why == LOST => Outcome::No(lost()),
        o => o,
    }
}

/// The step's inputs over `vars`.
fn inputs_of(step: &Step, vars: &Value) -> Result<Value> {
    let mut given = Map::new();
    for (n, e) in &step.inputs {
        given.insert(n.clone(), evaluate(e, vars)?);
    }
    Ok(Value::Object(given))
}

/// How many bytes a service says it holds: a number, or a `Range`-style `bytes=0-<last>` (or `0-<last>`).
fn held_offset(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => {
            let s = s.trim();
            match s.parse::<u64>() {
                Ok(n) => Some(n),
                Err(_) => s.trim_start_matches("bytes=").split('-').nth(1)?.trim().parse::<u64>().ok().map(|last| last + 1),
            }
        }
        _ => None,
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
