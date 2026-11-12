//! No-code automations: `event → condition (CEL) → actions`, run after commit from the outbox.
//! Each (automation, event) runs at most once: the run record commits with the action's effects.
//! The event is a record's (created, updated, submitted, cancelled, deleted, transitioned) or, for an automation with a
//! schedule, a moment of a record coming due (`scheduled`, schedule.rs).
//! An automation runs as the administrator who saved it, with their rights as they are when it runs:
//! once they are no longer an active administrator it does not run, and its runs say why, until an
//! administrator saves it again. One saved by no one (an app's) runs as the system.

use crate::ctx::Ctx;
use crate::error::{Error, FieldError, Result};
use crate::outbox::Event;
use crate::{Doc, Kernel};
use ferrum_meta::{Expr, Scope};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub const MAX_DEPTH: u8 = 3;
/// The most items one `for_each` runs over, and lines one `copy_lines` copies (BD-18): more is refused, never cut.
pub const MAX_ITEMS: usize = 100;
/// The most effects one `for_each` runs per item, as an action holds at most 10.
pub const MAX_LOOP_EFFECTS: usize = 10;
/// The largest mail template an automation keeps, as large as a saved view or preferences may be.
pub const MAX_TEMPLATE: usize = 64 * 1024;

/// A mail template, compiled in the print formats' engine: refused when too large or when it does not compile.
fn mail_template(src: &str) -> Result<minijinja::Environment<'_>> {
    if src.len() > MAX_TEMPLATE {
        return Err(Error::field("template", "an e-mail template is larger than 64 KB"));
    }
    let mut env = crate::print::environment();
    env.add_template("mail", src).map_err(|e| Error::field("template", format!("e-mail template: {e}")))?;
    Ok(env)
}

/// One effect of a configured action or one action of an automation: what it does, and `when` it does it (BD-18).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    /// An expression over what its values see; it runs only when this holds. Always when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    #[serde(flatten)]
    pub action: AutomationAction,
}

/// Whether `copy_lines` adds to the lines there are or replaces them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyMode {
    #[default]
    Append,
    Replace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AutomationAction {
    /// Set fields on the triggering record (draft documents and masters only).
    SetFields { values: BTreeMap<String, String> },
    /// Create a new record; each value is a CEL expression over `doc` (the triggering record).
    CreateDocument { entity: String, values: BTreeMap<String, String> },
    /// Set fields of the record a link field of the triggering record names, saved through the pipeline with that
    /// record's checks and rules; each value is a CEL expression over `doc` and `linked` (the record written, as it
    /// was). An empty link does nothing; a link to a record that is not there is an error.
    UpdateLinked { link: String, values: BTreeMap<String, String> },
    /// In-app notification. `to`: `owner`, `role:<name>`, `user:<uuid>`, `field:<name>` — the
    /// user whose email is in that field of the record (an assignee, an employee) — or `links:<link>.<table>.<field>`,
    /// the users whose e-mail is in that field of each line of the record a link field names (a group's members).
    Notify {
        to: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        body: Option<String>,
        /// A message template (BD-20) told in its place: its in-app body, in each person's language.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// E-mail, queued with the outbox in the run's transaction: it goes out only if the run commits,
    /// once per event. `to`, `subject` and `body` are CEL expressions (`to` may instead be
    /// `links:<link>.<table>.<field>`: one mail to each address there, as `notify` reads it); what `body` gives is text, escaped, its line
    /// breaks kept. A `template` (MiniJinja, the print formats' engine) makes formatted mail in its place: its own
    /// markup stays, every value it prints is escaped, and it sees the record as `doc` and the runner as `user`. Marketing (`purpose`) is not sent to someone who
    /// withdrew their consent, asked again as it goes (FEO-015); a transactional notice does not ask.
    Email {
        to: String,
        #[serde(default)]
        subject: String,
        #[serde(default)]
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        template: Option<String>,
        #[serde(default)]
        purpose: crate::retention::Purpose,
        /// A message template (BD-20) sent in its place: its e-mail body, its category deciding what `purpose` does.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// A message from a template (BD-20) to each of `to` (`owner`, `user:<id>`, `role:<name>`, `field:<name>`,
    /// `links:<link>.<table>.<field>`, `expr:<expression>` giving an address, a number or `{email, phone, language, time_zone}`), on the first of
    /// `channels` (else the template's) each can be reached on, or every one with `all`. `values` are expressions the
    /// template reads as `values`. In the run's transaction: nothing goes unless it commits. Gives the sends.
    Message {
        template: String,
        #[serde(deserialize_with = "one_or_many")]
        to: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        channels: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        all: bool,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        values: BTreeMap<String, String>,
        /// Expressions each giving a file of this record to attach to its e-mail: an id, or what `generate` gave
        /// (`results[0]`). One that gives nothing (a step that did not run) attaches nothing.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<String>,
        /// Names of `values` that are secret: sent, but kept nowhere once handed over (`messages::Message::secret`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        secret: Vec<String>,
    },
    /// A document from a template (BD-21) for the record, rendered as the workspace holds it, its secret fields left
    /// out; stored on the record in the run's transaction, named by `name` (an expression; else the template's label
    /// and the record's number), in `language` (an expression; else the workspace's), the one the template made before
    /// dropped with `replace`. `values` are expressions the template reads as `values`. Gives `{file, url, name,
    /// content_type, size, sha256, template, entity, record}`: a later `message` attaches it, an `invoke` hands on its
    /// file and digest.
    Generate {
        template: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        replace: bool,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        values: BTreeMap<String, String>,
        /// An expression giving the record it is for, of the template's entity: the run's own (the default) or, in a
        /// `for_each`, the item's (`item.id`); any other is refused when it runs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        record: Option<String>,
    },
    /// Documents about a query that groups by a link (BD-21): one file per record the report's rows link to, kept on it
    /// and, with `message`, sent to it attached (each customer their statement). Read and made as the workspace, in
    /// jobs of [`crate::documents::MAX_GROUPS`] records each, the next queued after the last record done; the first is
    /// queued in the run's transaction. `params` (the report's `filters`, `from`, `to`) as written, each of
    /// `params_from` an expression over what other values see (an action's own `params`) put over them; `values`
    /// expressions the template reads as `values`. Gives `{job}`.
    ReportDocuments {
        template: String,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        params: Value,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        params_from: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        values: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<crate::documents::ReportMessage>,
    },
    /// Another app's step through the capability gate, in the run's transaction (BD-13): `capability` a catalog id,
    /// `input` its inputs as CEL expressions, `key` the effect key as an expression. Without `key`, the run's own:
    /// the same run on the same record version does it once. It gives the receipt's result.
    Invoke {
        capability: String,
        #[serde(default)]
        input: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    /// Adds one line at the end of `table`, a table field of the record, through the record's ordinary save; each
    /// value an expression. Every other line keeps its id and values.
    AppendLine { table: String, values: BTreeMap<String, String> },
    /// Changes the one line of `table` for which `where` holds (an expression over `row`, the line, besides what the
    /// values see), only the fields named; no line or more than one is refused. Through the record's ordinary save.
    SetLine {
        table: String,
        #[serde(rename = "where")]
        when: String,
        values: BTreeMap<String, String>,
    },
    /// The one record of `entity` whose fields `by` names hold the values given (expressions, none empty). With
    /// `create`, one is made when none matches (`by`'s values and `create`'s); with `update`, the one found is changed
    /// (its values see it as `linked`). Two that match refuse the run. Gives `{entity, id, number}`, or null when none
    /// was found and none made. Runs that may make the same record wait for one another (BD-18).
    Find {
        entity: String,
        by: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        create: Option<BTreeMap<String, String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        update: Option<BTreeMap<String, String>>,
    },
    /// Runs `effects` once per item, the item as `item` in their expressions: each line of `table` (a table field of
    /// the record), each line of a table of the record a link field names (`links`: `<link>.<table>`, read as the
    /// workspace as `links` is, BD-14), or each record of `entity` whose link field `link` names the record. `where`, over `item`, picks
    /// which. At most [`MAX_ITEMS`] items; no loop within a loop. Gives each item's results, in order.
    ForEach {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        table: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entity: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        link: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        links: Option<String>,
        #[serde(default, rename = "where", skip_serializing_if = "Option::is_none")]
        filter: Option<String>,
        effects: Vec<Step>,
    },
    /// Copies the lines of `from_table` of another record into `table`, a table field of the record, through the
    /// record's ordinary save: the record its link field `link` names, or the one an earlier effect gave (`result`, its
    /// place). Each value an expression over `item` (the line copied) besides what other values see. `mode`
    /// `append` (the default) or `replace`. Nothing to copy from (an empty link, a null result) copies nothing.
    CopyLines {
        table: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        link: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<usize>,
        from_table: String,
        values: BTreeMap<String, String>,
        #[serde(default)]
        mode: CopyMode,
    },
}

/// One string or a list of them.
pub(crate) fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Either::deserialize(d)? {
        Either::One(s) => vec![s],
        Either::Many(v) => v,
    })
}

/// Every action a list holds, a loop's own effects after it.
pub(crate) fn every(steps: &[Step]) -> impl Iterator<Item = &AutomationAction> {
    steps.iter().flat_map(|s| {
        let inner: &[Step] = match &s.action {
            AutomationAction::ForEach { effects, .. } => effects,
            _ => &[],
        };
        std::iter::once(&s.action).chain(inner.iter().map(|i| &i.action))
    })
}

/// The expressions a step holds, its `when` and a loop's own effects' included.
fn step_sources(s: &Step) -> Vec<&str> {
    s.when.iter().map(String::as_str).chain(sources(&s.action)).collect()
}

/// The expressions an action holds.
fn sources(a: &AutomationAction) -> Vec<&str> {
    let all: Vec<&String> = match a {
        AutomationAction::Find { by, create, update, .. } => by.values().chain(create.iter().chain(update.iter()).flat_map(|m| m.values())).collect(),
        AutomationAction::ForEach { filter, effects, .. } => {
            return filter.iter().map(String::as_str).chain(effects.iter().flat_map(step_sources)).collect();
        }
        AutomationAction::CopyLines { values, .. } => values.values().collect(),
        AutomationAction::SetFields { values }
        | AutomationAction::CreateDocument { values, .. }
        | AutomationAction::UpdateLinked { values, .. }
        | AutomationAction::AppendLine { values, .. } => values.values().collect(),
        AutomationAction::SetLine { when, values, .. } => std::iter::once(when).chain(values.values()).collect(),
        AutomationAction::Notify { message: Some(_), .. } => vec![],
        AutomationAction::Notify { title, body, .. } => std::iter::once(title).chain(body.iter()).collect(),
        AutomationAction::Email { to, message: Some(_), .. } => std::iter::once(to).filter(|t| !t.starts_with("links:")).collect(),
        AutomationAction::Email { to, subject, body, .. } => std::iter::once(to).filter(|t| !t.starts_with("links:")).chain([subject, body]).collect(),
        AutomationAction::Message { to, values, attachments, .. } => {
            return to
                .iter()
                .filter_map(|t| t.strip_prefix("expr:"))
                .chain(values.values().map(String::as_str))
                .chain(attachments.iter().map(String::as_str))
                .collect();
        }
        AutomationAction::Generate { name, language, values, record, .. } => {
            name.iter().chain(language.iter()).chain(values.values()).chain(record.iter()).collect()
        }
        AutomationAction::ReportDocuments { values, params_from, .. } => values.values().chain(params_from.values()).collect(),
        AutomationAction::Invoke { input, key, .. } => input.values().chain(key.iter()).collect(),
    };
    all.into_iter().map(String::as_str).collect()
}

/// The link fields an action's expressions read as `links.<field>` (BD-14).
pub(crate) fn links_used(a: &Step) -> Result<std::collections::BTreeSet<String>> {
    let exprs = step_sources(a).into_iter().map(compiled).collect::<Result<Vec<_>>>()?;
    Ok(crate::links::used(&exprs))
}

/// The settings entities an action's expressions read as `settings.<Entity>.<field>` (BD-17).
pub(crate) fn settings_used(a: &Step) -> Result<std::collections::BTreeSet<String>> {
    let exprs = step_sources(a).into_iter().map(compiled).collect::<Result<Vec<_>>>()?;
    Ok(crate::settings::used(&exprs))
}

/// What is wrong with the settings `src` reads, said by `schema` (BD-17).
fn settings_problem(schema: &ferrum_meta::Schema, src: &str) -> Result<Option<String>> {
    Ok(schema.settings_problem(&compiled(src)?))
}

/// [`check_action`] with its `when`, and a loop's own effects: 1 to [`MAX_LOOP_EFFECTS`] of them, none a loop.
pub(crate) fn check_step(s: &Step) -> Result<()> {
    if let Some(w) = &s.when {
        Expr::compile(w).map_err(|m| Error::invalid(format!("when '{w}': {m}")))?;
    }
    if let AutomationAction::ForEach { effects, .. } = &s.action {
        if effects.is_empty() || effects.len() > MAX_LOOP_EFFECTS {
            return Err(Error::invalid(format!("for_each runs 1 to {MAX_LOOP_EFFECTS} effects per item")));
        }
        for (j, e) in effects.iter().enumerate() {
            if matches!(e.action, AutomationAction::ForEach { .. }) {
                return Err(Error::invalid(format!("for_each, effect {}: a loop does not hold another loop", j + 1)));
            }
            check_step(e).map_err(|e| crate::pack::about(&format!("for_each, effect {}", j + 1), e))?;
        }
    }
    check_action(&s.action)
}

pub(crate) fn check_action(a: &AutomationAction) -> Result<()> {
    if let AutomationAction::Notify { to, .. } = a {
        let ok = to == "owner"
            || to.strip_prefix("role:").is_some_and(|r| !r.trim().is_empty())
            || to.strip_prefix("user:").is_some_and(|u| u.parse::<Uuid>().is_ok())
            || to.strip_prefix("field:").is_some_and(|f| !f.trim().is_empty())
            || to.strip_prefix("links:").is_some_and(|p| crate::messages::lines_path(p).is_some());
        if !ok {
            return Err(Error::invalid("notify.to must be owner, role:<name>, user:<id>, field:<name> or links:<link>.<table>.<field>"));
        }
    }
    if let AutomationAction::Email { template: Some(t), .. } = a {
        mail_template(t)?;
    }
    if let AutomationAction::Message { values, secret, .. } = a
        && let Some(n) = secret.iter().find(|n| !values.contains_key(*n))
    {
        return Err(Error::invalid(format!("message: secret names {n}, which is not one of its values")));
    }
    if let AutomationAction::Message { to, channels, .. } = a {
        let fits = |t: &String| {
            t == "owner"
                || ["user:", "role:", "field:", "expr:"].iter().any(|p| t.strip_prefix(p).is_some_and(|r| !r.trim().is_empty()))
                || t.strip_prefix("links:").is_some_and(|p| crate::messages::lines_path(p).is_some())
        };
        if to.is_empty() || !to.iter().all(fits) {
            return Err(Error::invalid("message.to is owner, user:<id>, role:<name>, field:<name>, links:<link>.<table>.<field> or expr:<expression>"));
        }
        if let Some(c) = channels.iter().find(|c| !ferrum_meta::MESSAGE_CHANNELS.contains(&c.as_str())) {
            return Err(Error::invalid(format!("message: {c} is not one of {}", ferrum_meta::MESSAGE_CHANNELS.join(", "))));
        }
    }
    if let AutomationAction::ReportDocuments { message: Some(m), .. } = a {
        let fits = |t: &String| {
            t == "owner"
                || ["user:", "role:", "field:"].iter().any(|p| t.strip_prefix(p).is_some_and(|r| !r.trim().is_empty()))
                || t.strip_prefix("links:").is_some_and(|p| crate::messages::lines_path(p).is_some())
        };
        if m.to.is_empty() || !m.to.iter().all(fits) {
            return Err(Error::invalid(
                "report_documents.message.to is owner, user:<id>, role:<name>, field:<name> or links:<link>.<table>.<field> of each record",
            ));
        }
    }
    for e in sources(a) {
        Expr::compile(e).map_err(|m| Error::invalid(format!("expression '{e}': {m}")))?;
    }
    Ok(())
}

/// The HTML a mail action sends: its template when it has one, otherwise its body as escaped text. The one
/// place it is made, so that a preview and a send never differ. A refusal is a field error on `template` or `body`.
/// `extra` are more variables, as for the mail's other expressions (a configured action's `params`).
pub fn render_mail(body: &str, template: Option<&str>, doc: &Value, user: &Value, extra: &[(&str, &Value)]) -> Result<String> {
    match template {
        Some(t) => {
            let mut vars: BTreeMap<&str, &Value> = extra.iter().copied().collect();
            vars.extend([("doc", doc), ("user", user)]);
            mail_template(t)?
                .get_template("mail")
                .and_then(|m| m.render(minijinja::Value::from_serialize(&vars)))
                .map_err(|e| Error::field("template", format!("e-mail template failed: {e}")))
        }
        None => Ok(crate::outbox::text_html(&as_text(eval_with(body, doc, user, extra).map_err(|e| Error::field("body", e.to_string()))?))),
    }
}

/// A mail as it would be sent, against one record of `entity`: the one named (read with the caller's rights), else
/// the one most recently changed that they may read, else an empty record. Sends and writes nothing.
pub async fn preview_mail(
    k: &Kernel,
    ctx: &Ctx,
    entity: &str,
    body: Option<&str>,
    template: Option<&str>,
    record: Option<Uuid>,
) -> Result<(String, Option<Uuid>)> {
    if !ctx.is_admin {
        return Err(Error::Forbidden("administrator access required".into()));
    }
    let ent = ctx.schema.entity(entity).ok_or_else(|| Error::field("entity", "unknown entity"))?;
    let found = match record {
        Some(id) => Some(k.get(ctx, entity, id, false).await?),
        None => {
            let q = crate::query::ListQuery { sort: Some("updated_at desc".into()), limit: Some(1), ..Default::default() };
            k.list(ctx, entity, &q).await?.rows.into_iter().next()
        }
    };
    let id = found.as_ref().and_then(|d| d.get("id")).and_then(Value::as_str).and_then(|s| s.parse().ok());
    let doc = ent.cel_view(&Value::Object(found.unwrap_or_default()), &ctx.schema);
    Ok((render_mail(body.unwrap_or("''"), template, &doc, &ctx.user_value(), &[])?, id))
}

pub fn validate_actions(actions: &Value) -> Result<Vec<Step>> {
    let list: Vec<Step> = serde_json::from_value(actions.clone()).map_err(|e| Error::invalid(format!("actions: {e}")))?;
    if list.is_empty() || list.len() > 20 {
        return Err(Error::invalid("an automation needs 1 to 20 actions"));
    }
    for (i, a) in list.iter().enumerate() {
        // A refusal is a field error on `actions`, in the row of the action it is about.
        check_step(a).map_err(|e| match e {
            Error::Validation(fs) => Error::Validation(fs.into_iter().map(|f| FieldError::new("actions", f.message).in_row("actions", i)).collect()),
            other => Error::Validation(vec![FieldError::new("actions", other.to_string()).in_row("actions", i)]),
        })?;
    }
    Ok(list)
}

/// Compiled expressions by their source: an automation's are compiled once, not at every run. Bounded:
/// emptied when it grows past what any workspace's automations need.
static COMPILED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, Expr>>> = std::sync::LazyLock::new(Default::default);
const COMPILED_MAX: usize = 4096;
/// How many expressions were compiled, for the test that they are not compiled again.
static COMPILES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn compiled(src: &str) -> Result<Expr> {
    if let Some(e) = COMPILED.lock().unwrap_or_else(|p| p.into_inner()).get(src) {
        return Ok(e.clone());
    }
    let expr = Expr::compile(src).map_err(Error::invalid)?;
    COMPILES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut cache = COMPILED.lock().unwrap_or_else(|p| p.into_inner());
    if cache.len() >= COMPILED_MAX {
        cache.clear();
    }
    cache.insert(src.to_string(), expr.clone());
    Ok(expr)
}

fn eval(src: &str, doc: &Value, old: Option<&Value>, user: &Value, extra: &[(&str, &Value)]) -> Result<Value> {
    compiled(src)?.eval_with(&Scope { doc, old, parent: None, user }, extra).map_err(|e| Error::invalid(format!("'{src}': {e}")))
}

/// [`eval`] with more variables: a configured action's `params`, the `linked` record an effect writes.
fn eval_with(src: &str, doc: &Value, user: &Value, extra: &[(&str, &Value)]) -> Result<Value> {
    let expr = compiled(src)?;
    expr.eval_with(&Scope { doc, old: None, parent: None, user }, extra).map_err(|e| Error::invalid(format!("'{src}': {e}")))
}

/// The steps of an effect's value that is only a path to a value — `doc.a`, `doc.a.b`, `linked.a`, `links.x.a`,
/// `row.a`, `params.a`, `results[0].a` — its root first, an index as its digits. Anything else (an operator, a
/// call, a literal, brackets around it: `doc.a + 0`, `(doc.a)`) is not one.
fn bare_path(src: &str) -> Option<Vec<&str>> {
    let ident = |t: &str| t.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let mut steps = Vec::new();
    for part in src.trim().split('.') {
        let (name, mut rest) = match part.split_once('[') {
            Some((n, r)) => (n, Some(r)),
            None => (part, None),
        };
        if !ident(name) {
            return None;
        }
        steps.push(name);
        while let Some(r) = rest {
            let (i, after) = r.split_once(']')?;
            if i.is_empty() || !i.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            steps.push(i);
            rest = if after.is_empty() { None } else { Some(after.strip_prefix('[')?) };
        }
    }
    (steps.len() > 1 && ["doc", "linked", "links", "row", "params", "results", "item"].contains(&steps[0])).then_some(steps)
}

/// Whether `steps` lead to nothing in `stored` (a value as it is stored): a null, or a key it does not have.
fn stored_empty(stored: &Value, steps: &[&str]) -> bool {
    let mut at = stored;
    for s in steps {
        at = match at {
            Value::Object(m) => match m.get(*s) {
                Some(v) => v,
                None => return true,
            },
            Value::Array(a) => match s.parse::<usize>().ok().and_then(|i| a.get(i)) {
                Some(v) => v,
                None => return false,
            },
            Value::Null => return true,
            _ => return false,
        };
    }
    at.is_null()
}

/// What an effect writes for `src`: its value as any expression gives it, except that a bare path ([`bare_path`])
/// to a value stored empty is empty, not the 0 or false an expression reads there. `stored` gives the roots as
/// stored (`doc`, `linked`, `row`); `links.x` is read as stored when it matters; `params` and `results` are as given.
#[allow(clippy::too_many_arguments)]
async fn effect_value(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut crate::db::Tx,
    entity: &str,
    src: &str,
    doc: &Value,
    user: &Value,
    vars: &[(&str, &Value)],
    stored: &[(&str, &Value)],
) -> Result<Value> {
    let v = eval_with(src, doc, user, vars)?;
    if v != Value::Bool(false) && v.as_f64() != Some(0.0) {
        return Ok(v);
    }
    let Some(steps) = bare_path(src) else { return Ok(v) };
    let empty = match (steps[0], stored.iter().find(|(n, _)| *n == steps[0])) {
        (_, Some((_, root))) => stored_empty(root, &steps[1..]),
        ("links", None) => {
            let to = ctx.schema.entity(entity).and_then(|e| linked_entity(e, steps[1])).map(str::to_string);
            let id = doc.get(steps[1]).and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok());
            let (Some(to), Some(id)) = (to, id) else { return Ok(v) };
            let linked = k.get_tx(&Ctx::system(ctx.tenant, ctx.schema.clone()), tx, &to, id, false).await?;
            stored_empty(&Value::Object(linked), &steps[2..])
        }
        _ => false,
    };
    Ok(if empty { Value::Null } else { v })
}

/// What an automation's (or a configured action's) actions write, checked against the entity they run on when
/// they are saved: `update_linked` names a link field of it, and every field written is a field of the record
/// written. A refusal names the action by its place.
pub(crate) fn check_targets(schema: &ferrum_meta::Schema, entity: &str, actions: &[Step]) -> Result<()> {
    let ent = schema.entity(entity).ok_or_else(|| Error::field("entity", "unknown entity"))?;
    for (i, step) in actions.iter().enumerate() {
        let a = step;
        let at = |m: String| Error::Validation(vec![FieldError::new("actions", m).in_row("actions", i)]);
        // What it reads through `links` are link fields of the record it runs on (BD-14).
        if let Some(n) = links_used(a)?.into_iter().find(|n| linked_entity(ent, n).is_none()) {
            return Err(at(format!("links.{n}: {n} is not a link field of {}", ent.def.label)));
        }
        // What it reads as `settings` is there (BD-17).
        for src in step_sources(a) {
            if let Some(m) = settings_problem(schema, src)? {
                return Err(at(m));
            }
        }
        let (target, values) = match &step.action {
            AutomationAction::Find { entity: to, by, create, update } => {
                let tent = schema.entity(to).cloned().ok_or_else(|| at(format!("find: unknown entity {to}")))?;
                if by.is_empty() {
                    return Err(at("find: by names the fields that pick the record".into()));
                }
                // `id` picks a record by its id (`item.id` in a loop).
                let fields = by.keys().filter(|f| *f != "id").chain(create.iter().chain(update.iter()).flat_map(|m| m.keys()));
                if let Some(f) = fields.clone().find(|f| tent.field(f).is_none_or(|d| d.ftype == ferrum_meta::FieldType::Table)) {
                    return Err(at(format!("find: {} has no field {f}", tent.def.label)));
                }
                continue;
            }
            AutomationAction::ForEach { table, entity: over, link, links, effects, .. } => {
                match (table, over) {
                    (None, None) if link.is_none() && links.is_some() => {
                        let path = links.as_deref().unwrap_or_default();
                        if path.split('.').count() != 2 {
                            return Err(at(format!("for_each: links is <link>.<table>, not {path}")));
                        }
                        if let Some(m) = crate::messages::lines_problem(schema, ent, path) {
                            return Err(at(format!("for_each: {m}")));
                        }
                    }
                    (Some(t), None) if link.is_none() && links.is_none() => {
                        if !ent.field(t).is_some_and(|f| f.ftype == ferrum_meta::FieldType::Table) {
                            return Err(at(format!("for_each: {t} is not a table of lines of {}", ent.def.label)));
                        }
                    }
                    (None, Some(o)) if links.is_none() => {
                        let oent = schema.entity(o).ok_or_else(|| at(format!("for_each: unknown entity {o}")))?;
                        let l = link.as_deref().ok_or_else(|| at(format!("for_each: link names the field of {o} that points here")))?;
                        if linked_entity(oent, l) != Some(entity) {
                            return Err(at(format!("for_each: {l} is not a link field of {} to {}", oent.def.label, ent.def.label)));
                        }
                    }
                    _ => {
                        return Err(at(
                            "for_each runs over a table (`table`), a linked record's table (`links`) or the records linking here (`entity` and `link`)".into(),
                        ));
                    }
                }
                check_targets(schema, entity, effects).map_err(|e| match e {
                    Error::Validation(fs) => {
                        at(fs.into_iter().map(|f| format!("for_each, effect {}: {}", f.row.map_or(0, |r| r.1) + 1, f.message)).collect::<Vec<_>>().join("; "))
                    }
                    other => other,
                })?;
                continue;
            }
            AutomationAction::CopyLines { table, link, result, from_table, values, .. } => {
                crate::actions::writable_as(ent, table, true).map_err(at)?;
                let source = match (link, result) {
                    (Some(l), None) => linked_entity(ent, l).ok_or_else(|| at(format!("copy_lines: {l} is not a link field of {}", ent.def.label)))?,
                    (None, Some(n)) => match actions.get(*n).filter(|_| *n < i).map(|s| &s.action) {
                        Some(AutomationAction::Find { entity, .. } | AutomationAction::CreateDocument { entity, .. }) => entity.as_str(),
                        _ => return Err(at(format!("copy_lines: result {n} is not an earlier find or create_document"))),
                    },
                    _ => return Err(at("copy_lines copies from the record a link field names (`link`) or an earlier effect gave (`result`)".into())),
                };
                let sent = schema.entity(source).ok_or_else(|| at(format!("copy_lines: {source} is not installed")))?;
                if !sent.field(from_table).is_some_and(|f| f.ftype == ferrum_meta::FieldType::Table) {
                    return Err(at(format!("copy_lines: {from_table} is not a table of lines of {}", sent.def.label)));
                }
                let of = ent.field(table).and_then(|f| f.of.as_deref()).and_then(|c| schema.entity(c)).cloned();
                let of = of.ok_or_else(|| at(format!("the lines of {table} are not installed")))?;
                if let Some(Err(m)) = values.keys().map(|f| crate::actions::writable(&of, f)).find(|r| r.is_err()) {
                    return Err(at(m));
                }
                continue;
            }
            AutomationAction::SetFields { values } => (ent.clone(), values),
            AutomationAction::CreateDocument { entity, values } => {
                (schema.entity(entity).cloned().ok_or_else(|| at(format!("unknown entity {entity}")))?, values)
            }
            AutomationAction::UpdateLinked { link, values } => {
                let to = linked_entity(ent, link).ok_or_else(|| at(format!("{link} is not a link field of {}", ent.def.label)))?;
                (schema.entity(to).cloned().ok_or_else(|| at(format!("{link} links to {to}, which is not installed")))?, values)
            }
            AutomationAction::AppendLine { table, values } | AutomationAction::SetLine { table, values, .. } => {
                crate::actions::writable_as(ent, table, true).map_err(at)?;
                let of = ent.field(table).and_then(|f| f.of.as_deref()).and_then(|c| schema.entity(c)).cloned();
                let of = of.ok_or_else(|| at(format!("the lines of {table} are not installed")))?;
                if let Some(Err(m)) = values.keys().map(|f| crate::actions::writable(&of, f)).find(|r| r.is_err()) {
                    return Err(at(m));
                }
                continue;
            }
            AutomationAction::Notify { to, .. } | AutomationAction::Email { to, .. } => {
                if let Some(m) = to.strip_prefix("links:").and_then(|p| crate::messages::lines_problem(schema, ent, p)) {
                    return Err(at(m));
                }
                continue;
            }
            AutomationAction::Message { to, .. } => {
                if let Some(m) = to.iter().filter_map(|t| t.strip_prefix("links:")).find_map(|p| crate::messages::lines_problem(schema, ent, p)) {
                    return Err(at(m));
                }
                if let Some(f) = to.iter().filter_map(|t| t.strip_prefix("field:")).find(|f| ent.field(f).is_none()) {
                    return Err(at(format!("message.to: {} has no field {f}", ent.def.label)));
                }
                continue;
            }
            AutomationAction::Generate { .. } | AutomationAction::Invoke { .. } | AutomationAction::ReportDocuments { .. } => continue,
        };
        // The platform's own keys are ignored when written, as they always were (a configured action refuses them).
        if let Some(f) = values.keys().find(|f| target.field(f).is_none() && !crate::store::SYSTEM_KEYS.contains(&f.as_str())) {
            return Err(at(format!("{} has no field {f}", target.def.label)));
        }
    }
    Ok(())
}

/// A refusal of a run's effect `i` (from 0), said with its place and kind: `effect 2 (set_fields): Title is required`;
/// what an effect said of itself first (`find: …`) is not said twice. A record not found stays a record not found.
pub(crate) fn at_effect(i: usize, a: &AutomationAction, e: Error) -> Error {
    let kind = serde_json::to_value(a).ok().and_then(|v| v["type"].as_str().map(str::to_string)).unwrap_or_default();
    let say = |m: String| format!("effect {} ({kind}): {}", i + 1, m.strip_prefix(&format!("{kind}: ")).unwrap_or(&m));
    match e {
        Error::Validation(fs) => Error::Validation(
            fs.into_iter()
                .map(|mut f| {
                    f.message = say(f.message);
                    f
                })
                .collect(),
        ),
        Error::Invalid(m) => Error::Invalid(say(m)),
        Error::Conflict(m) => Error::Conflict(say(m)),
        Error::Forbidden(m) => Error::Forbidden(say(m)),
        Error::Refused { code, status, message, pointer } => Error::Refused { code, status, message: say(message), pointer },
        other => other,
    }
}

/// The message and document templates there are in the workspace: its packages' and its own.
pub(crate) async fn known_templates(tx: &mut crate::db::Tx, tenant: Uuid, schema: &ferrum_meta::Schema) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let mut messages: BTreeSet<String> = schema.messages.iter().map(|m| m.key.clone()).collect();
    let mut documents: BTreeSet<String> = schema.documents.iter().map(|d| d.key.clone()).collect();
    messages.extend(sqlx::query_scalar::<_, String>("select key from message_template where tenant_id = $1").bind(tenant).fetch_all(&mut **tx).await?);
    documents.extend(sqlx::query_scalar::<_, String>("select key from document_template where tenant_id = $1").bind(tenant).fetch_all(&mut **tx).await?);
    Ok((messages, documents))
}

/// The first template `step` (a loop's own effects too) names that is not among `known` (messages, documents).
pub(crate) fn unknown_template(step: &Step, (messages, documents): &(BTreeSet<String>, BTreeSet<String>)) -> Option<String> {
    every(std::slice::from_ref(step)).find_map(|a| {
        let (message, document) = match a {
            AutomationAction::Message { template, .. } => (Some(template), None),
            AutomationAction::Notify { message, .. } | AutomationAction::Email { message, .. } => (message.as_ref(), None),
            AutomationAction::Generate { template, .. } => (None, Some(template)),
            AutomationAction::ReportDocuments { template, message, .. } => (message.as_ref().map(|m| &m.template), Some(template)),
            _ => (None, None),
        };
        match (message.filter(|m| !messages.contains(*m)), document.filter(|d| !documents.contains(*d))) {
            (Some(m), _) => Some(format!("there is no message template {m}")),
            (_, Some(d)) => Some(format!("there is no document template {d}")),
            _ => None,
        }
    })
}

/// A refusal that the capability is not in the catalog, said for whoever set the effect up.
fn gone(e: Error, id: &str) -> Error {
    match e.code() {
        "capability_unavailable" => ferrum_logic::refusal::Refusal::new(
            ferrum_logic::refusal::Code::CapabilityUnavailable,
            format!("{id} is not a capability here: is its app installed?"),
        )
        .into(),
        _ => e,
    }
}

/// An `invoke` checked against the catalog as `ctx` sees it: its capability is there, and its input names are the
/// capability's, the required ones given.
pub(crate) fn check_invoke(k: &Kernel, ctx: &Ctx, id: &str, input: &BTreeMap<String, String>) -> Result<()> {
    let cap = k.capability(ctx, id).map_err(|e| gone(e, id))?;
    // No effect runs another configured action (BD-11), so a button cannot loop through the gate.
    if let ferrum_logic::capability::Operation::Action(name) = &cap.operation
        && cap.entity.as_deref().and_then(|e| ctx.schema.entity(e)).is_some_and(|e| e.def.actions.iter().any(|a| &a.name == name && !a.effects.is_empty()))
    {
        return Err(Error::invalid(format!("{id} is a configured action: an effect does not run another one")));
    }
    let props = cap.input.get("properties").and_then(Value::as_object);
    if let Some(n) = input.keys().find(|n| props.is_none_or(|p| !p.contains_key(n.as_str()))) {
        return Err(Error::invalid(format!("{id} has no input {n}")));
    }
    let required = cap.input.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
    if let Some(r) = required.into_iter().find(|r| !input.contains_key(*r)) {
        return Err(Error::invalid(format!("{id} needs its input {r}")));
    }
    Ok(())
}

/// The entity a link field of `ent` points at.
fn linked_entity<'a>(ent: &'a ferrum_meta::EffEntity, link: &str) -> Option<&'a str> {
    ent.field(link).filter(|f| f.ftype == ferrum_meta::FieldType::Link).and_then(|f| f.to.as_deref())
}

fn as_text(v: Value) -> String {
    match v {
        Value::String(s) => s,
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[derive(sqlx::FromRow)]
struct Automation {
    id: Uuid,
    condition: Option<String>,
    actions: Value,
    created_by: Option<Uuid>,
    is_active: bool,
}

/// Whom an automation runs as: the administrator who saved it, read now; or why it may not run.
async fn runner(
    k: &Kernel,
    tenant: Uuid,
    schema: &std::sync::Arc<ferrum_meta::Schema>,
    made_by: Option<Uuid>,
    request_id: String,
) -> std::result::Result<Ctx, String> {
    let Some(user) = made_by else {
        let mut ctx = Ctx::system(tenant, schema.clone());
        ctx.request_id = request_id;
        return Ok(ctx);
    };
    match k.load_ctx(tenant, user, None, request_id).await {
        Ok(ctx) if ctx.is_admin => Ok(ctx),
        Ok(ctx) => Err(format!("{}, who saved it, is no longer an administrator: an administrator saves it again", ctx.email)),
        Err(_) => Err("whoever saved it can no longer sign in here: an administrator saves it again".into()),
    }
}

pub async fn run_for_event(k: &Kernel, ev: &Event) -> Result<()> {
    let Some((entity, event)) = ev.topic.split_once('.') else { return Ok(()) };
    let depth = ev.payload.get("depth").and_then(Value::as_u64).unwrap_or(0) as u8;
    if event == "scheduled" {
        return run_scheduled(k, ev, entity, depth).await;
    }
    let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
    let autos: Vec<Automation> = sqlx::query_as(
        // The active ones, and those off because the solution imported from each runs in its place: their events
        // are handed over to it (FLC-MIG-003).
        "select id, condition, actions, created_by, is_active from automation where tenant_id = $1 and entity = $2 and event = $3 \
           and (is_active or exists (select 1 from logic_activation a where a.tenant_id = automation.tenant_id \
             and a.solution = 'workspace.automation_' || replace(automation.id::text, '-', ''))) order by created_at",
    )
    .bind(ev.tenant_id)
    .bind(entity)
    .bind(event)
    .fetch_all(&mut *tx)
    .await?;
    if depth >= MAX_DEPTH && autos.iter().any(|a| a.is_active) {
        // Not run, and recorded so an administrator sees which automations answer one another's changes.
        for a in autos.iter().filter(|a| a.is_active) {
            sqlx::query(
                "insert into automation_run (tenant_id, automation_id, outbox_id, status, error) values ($1, $2, $3, 'failed', $4) on conflict do nothing",
            )
            .bind(ev.tenant_id)
            .bind(a.id)
            .bind(ev.id)
            .bind(format!(
                "not run: automations made this change after {depth} others in one chain, and stop at {MAX_DEPTH}; one of them answers its own changes"
            ))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        tracing::warn!(event = ev.id, topic = %ev.topic, "automations stopped at depth {depth}");
        return Ok(());
    }
    tx.commit().await?;
    if autos.is_empty() {
        return Ok(());
    }
    let schema = k.registry.schema(ev.tenant_id).await?;
    let Some(ent) = schema.entity(entity).cloned() else { return Ok(()) };
    let doc = ev.payload.get("doc").cloned().unwrap_or(Value::Null);
    let cel_doc = ent.cel_view(&doc, &schema);
    // The record before the change: the record now with what the change replaced (`was`) put back.
    let cel_old = ev.payload.get("was").and_then(Value::as_object).map(|was| {
        let mut old = doc.clone();
        for (k, v) in was {
            old[k] = v.clone();
        }
        ent.cel_view(&old, &schema)
    });
    let mut first_error = None;
    for a in autos {
        // Whom it would run as, worked out before its transaction holds anything.
        let who = runner(k, ev.tenant_id, &schema, a.created_by, format!("automation-{}-{}", a.id, ev.id)).await;
        let res = run_one(k, ev, entity, &a, &who, depth, ev.record_id, (&doc, &cel_doc, cel_old.as_ref()), true).await;
        if let Err(e) = res {
            tracing::warn!(automation = %a.id, event = ev.id, "automation failed: {e}");
            first_error.get_or_insert(e);
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// A moment of a record come due (schedule.rs): only the automation it names, with the record as it is now,
/// read with its saver's rights. Nothing happens when the automation is off or gone, when the record is gone
/// or no longer readable, or when the moment is no longer one of the record's (its date moved, it no longer
/// passes the filter); otherwise it runs as for a record's event, its changes one step further in a chain.
async fn run_scheduled(k: &Kernel, ev: &Event, entity: &str, depth: u8) -> Result<()> {
    let auto = ev.payload.get("automation").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok());
    let due = ev.payload.get("due_at").and_then(Value::as_str).and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
    let (Some(auto), Some(due), Some(record)) = (auto, due, ev.record_id) else { return Ok(()) };
    let due = due.to_utc();
    let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
    let found: Option<(Uuid, Option<String>, Value, Option<Uuid>, bool, Value)> = sqlx::query_as(
        "select id, condition, actions, created_by, is_active, schedule from automation \
         where tenant_id = $1 and id = $2 and entity = $3 and event = 'scheduled'",
    )
    .bind(ev.tenant_id)
    .bind(auto)
    .bind(entity)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let Some((id, condition, actions, created_by, is_active, schedule)) = found else { return Ok(()) };
    if !is_active {
        return Ok(());
    }
    let a = Automation { id, condition, actions, created_by, is_active };
    let schema = k.registry.schema(ev.tenant_id).await?;
    let Some(ent) = schema.entity(entity).cloned() else { return Ok(()) };
    let who = runner(k, ev.tenant_id, &schema, a.created_by, format!("automation-{}-{}", a.id, ev.id)).await;
    let (mut raw, mut cel_doc) = (Value::Null, Value::Null);
    if let Ok(ctx) = &who {
        let doc = match k.get(ctx, entity, record, false).await {
            Ok(doc) => doc,
            Err(Error::NotFound(_) | Error::Forbidden(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        let Ok(plan) = crate::schedule::plan(&schema, &ent, &schedule) else { return Ok(()) };
        if !crate::schedule::still_due(k, ev.tenant_id, &schema, &ent, &plan, record, due).await? {
            return Ok(());
        }
        raw = Value::Object(doc);
        cel_doc = ent.cel_view(&raw, &schema);
    }
    run_one(k, ev, entity, &a, &who, depth, Some(record), (&raw, &cel_doc, None), false).await.map(|_| ())
}

/// One automation's run for one event, in its own transaction: nothing if it is off (or the event was handed
/// over to the solution imported from it) or ran for this event already; otherwise its condition, then its
/// actions, recorded with their effects.
#[allow(clippy::too_many_arguments)]
async fn run_one(
    k: &Kernel,
    ev: &Event,
    entity: &str,
    a: &Automation,
    who: &std::result::Result<Ctx, String>,
    depth: u8,
    record: Option<Uuid>,
    (raw, cel_doc, cel_old): (&Value, &Value, Option<&Value>),
    hand_over: bool,
) -> Result<&'static str> {
    let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
    // Read again under its lock. An activation handing it over to the solution imported from it waits for
    // this to finish, or went first, and then the event is the solution's: claimed and started here unless
    // the solution's own subscriber claimed it already. Each event is run by one of the two (FLC-MIG-003).
    let active: Option<bool> = sqlx::query_scalar("select is_active from automation where tenant_id = $1 and id = $2 for share")
        .bind(ev.tenant_id)
        .bind(a.id)
        .fetch_optional(&mut *tx)
        .await?;
    if active != Some(true) {
        let handed = hand_over && k.hand_over_automation_event(&mut tx, ev, a.id).await?;
        tx.commit().await?;
        return Ok(if handed { "handed over" } else { "off" });
    }
    let done: Option<i32> = sqlx::query_scalar("select 1 from automation_run where tenant_id = $1 and automation_id = $2 and outbox_id = $3")
        .bind(ev.tenant_id)
        .bind(a.id)
        .bind(ev.id)
        .fetch_optional(&mut *tx)
        .await?;
    if done.is_some() {
        return Ok("already");
    }
    let mut ctx = match who {
        Ok(ctx) => ctx.clone(),
        Err(why) => {
            // Recorded, so it is not tried again for this event, and an administrator can see why.
            sqlx::query(
                "insert into automation_run (tenant_id, automation_id, outbox_id, status, error) values ($1, $2, $3, 'failed', $4) on conflict do nothing",
            )
            .bind(ev.tenant_id)
            .bind(a.id)
            .bind(ev.id)
            .bind(why)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            tracing::warn!(automation = %a.id, event = ev.id, "automation not run: {why}");
            return Ok("not run");
        }
    };
    ctx.depth = depth + 1;
    // The linked records and settings its condition and actions read, once for the run (BD-14, BD-17).
    let condition = a.condition.as_deref().filter(|c| !c.trim().is_empty());
    let cond = condition.map(compiled).transpose()?;
    let mut names = crate::links::used(cond.iter());
    let mut settings = crate::settings::used(cond.iter());
    for action in validate_actions(&a.actions).into_iter().flatten() {
        names.extend(links_used(&action)?);
        settings.extend(settings_used(&action)?);
    }
    let ctx = crate::settings::with(k, &ctx, &mut tx, settings).await?.into_owned();
    let user = ctx.user_value();
    let links = match ctx.schema.entity(entity).cloned() {
        Some(ent) => crate::links::load(k, &ctx, &mut tx, &ent, cel_doc, &names).await?,
        None => Value::Object(Map::new()),
    };
    // A transition's event says which, from and to, and by whom (BD-19); null for any other.
    let transition = ev.payload.get("transition").cloned().unwrap_or(Value::Null);
    let pass = match condition {
        Some(c) => matches!(eval(c, cel_doc, cel_old, &user, &[("links", &links), ("transition", &transition)])?, Value::Bool(true)),
        _ => true,
    };
    let status = if pass {
        // An `invoke` without a key of its own does its step once per automation, record version and place.
        let origin = format!("automation:{}:{}:v{}:", a.id, record.map(|r| r.to_string()).unwrap_or_default(), cel_doc.get("version").unwrap_or(&Value::Null));
        let mut results = Vec::new();
        for (i, action) in validate_actions(&a.actions)?.into_iter().enumerate() {
            let done = Value::Array(results.clone());
            let what = action.action.clone();
            results.push(
                run_step(
                    k,
                    &ctx,
                    &mut tx,
                    entity,
                    record,
                    (raw, cel_doc),
                    &user,
                    &[("results", &done), ("links", &links), ("transition", &transition)],
                    &format!("{origin}{i}"),
                    action,
                )
                .await
                .map_err(|e| at_effect(i, &what, e))?,
            );
        }
        "done"
    } else {
        "skipped"
    };
    sqlx::query("insert into automation_run (tenant_id, automation_id, outbox_id, status) values ($1, $2, $3, $4)")
        .bind(ev.tenant_id)
        .bind(a.id)
        .bind(ev.id)
        .bind(status)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(status)
}

/// Runs `step` as [`execute`] does its action, when its `when` holds (it sees what the values see); null otherwise.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_step(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut crate::db::Tx,
    entity: &str,
    record: Option<Uuid>,
    (raw, doc): (&Value, &Value),
    user: &Value,
    extra: &[(&str, &Value)],
    key: &str,
    step: Step,
) -> Result<Value> {
    if let Some(w) = &step.when
        && eval_with(w, doc, user, extra)? != Value::Bool(true)
    {
        return Ok(Value::Null);
    }
    execute(k, ctx, tx, entity, record, (raw, doc), user, extra, key, step.action).await
}

/// Runs one action as `ctx` in `tx`, its expressions over `doc` (the record as CEL sees it; `raw`, as stored), `user` and `extra`
/// (a configured action's `params`, earlier effects' `results`). Returns the record it made or wrote,
/// `{entity, id, number}`, an `invoke`'s result (its effect key `key` unless it names one), or null.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut crate::db::Tx,
    entity: &str,
    record: Option<Uuid>,
    (raw, doc): (&Value, &Value),
    user: &Value,
    extra: &[(&str, &Value)],
    key: &str,
    action: AutomationAction,
) -> Result<Value> {
    let eval = |src: &str| eval_with(src, doc, user, extra);
    let made = |entity: &str, d: &Doc| serde_json::json!({"entity": entity, "id": d.get("id"), "number": d.get("number")});
    match action {
        AutomationAction::SetFields { values } => {
            let id = record.ok_or_else(|| Error::invalid("set_fields needs a record"))?;
            let mut patch = Doc::new();
            for (field, expr) in values {
                patch.insert(field, effect_value(k, ctx, tx, entity, &expr, doc, user, extra, &[("doc", raw)]).await?);
            }
            k.update_tx(ctx, tx, entity, id, patch, None).await?;
        }
        AutomationAction::CreateDocument { entity: target, values } => {
            let mut input = Doc::new();
            for (field, expr) in values {
                input.insert(field, effect_value(k, ctx, tx, entity, &expr, doc, user, extra, &[("doc", raw)]).await?);
            }
            let d = k.insert_tx(ctx, tx, &target, input).await?;
            return Ok(made(&target, &d));
        }
        AutomationAction::UpdateLinked { link, values } => {
            let ent = ctx.schema.entity(entity).cloned().ok_or_else(|| Error::invalid(format!("unknown entity {entity}")))?;
            let target = linked_entity(&ent, &link).ok_or_else(|| Error::invalid(format!("update_linked: {link} is not a link field of {}", ent.def.label)))?;
            let tent = ctx.schema.entity(target).cloned().ok_or_else(|| Error::invalid(format!("update_linked: {target} is not installed")))?;
            // An empty link names nothing to write.
            let Some(id) = doc.get(&link).and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok()) else { return Ok(Value::Null) };
            // Locked for the rest of the transaction, as the save below locks it.
            let linked = match k.get_tx(ctx, tx, target, id, true).await {
                Ok(d) => d,
                Err(Error::NotFound(_)) => return Err(Error::invalid(format!("{link}: the {} it names does not exist", tent.def.label))),
                Err(e) => return Err(e),
            };
            let linked = Value::Object(linked);
            let seen = tent.cel_view(&linked, &ctx.schema);
            let mut vars = extra.to_vec();
            vars.push(("linked", &seen));
            let mut patch = Doc::new();
            for (field, expr) in values {
                patch.insert(field, effect_value(k, ctx, tx, entity, &expr, doc, user, &vars, &[("doc", raw), ("linked", &linked)]).await?);
            }
            let d = k.update_tx(ctx, tx, target, id, patch, None).await?;
            return Ok(made(target, &d));
        }
        AutomationAction::Notify { to, message: Some(template), .. } => {
            let people = Box::pin(crate::messages::recipients(k, tx, ctx, entity, doc, &to)).await?;
            let m = crate::messages::Message {
                template,
                entity: record.map(|_| entity.to_string()),
                record,
                to: people,
                channels: vec!["inapp".into()],
                ..Default::default()
            };
            return Ok(serde_json::json!(crate::messages::send(k, tx, ctx, &m).await?));
        }
        AutomationAction::Email { to, message: Some(template), .. } => {
            let to = match to.starts_with("links:") {
                true => Box::pin(crate::messages::recipients(k, tx, ctx, entity, doc, &to)).await?,
                false => vec![crate::messages::Recipient::address(&as_text(eval(&to)?))],
            };
            let m = crate::messages::Message {
                template,
                entity: record.map(|_| entity.to_string()),
                record,
                to,
                channels: vec!["email".into()],
                ..Default::default()
            };
            return Ok(serde_json::json!(crate::messages::send(k, tx, ctx, &m).await?));
        }
        AutomationAction::Message { template, to, channels, all, values, attachments, secret } => {
            let mut given = Map::new();
            for (name, expr) in values {
                given.insert(name, eval(&expr)?);
            }
            let given_files = attachments.iter().map(|src| eval(src).map(|v| (src.clone(), v))).collect::<Result<Vec<_>>>()?;
            let files = attached(tx, ctx, entity, record, given_files).await?;
            let mut people = Vec::new();
            for spec in &to {
                match spec.strip_prefix("expr:") {
                    Some(src) => people.push(crate::messages::Recipient::from_value(&eval(src)?)?),
                    None => people.extend(Box::pin(crate::messages::recipients(k, tx, ctx, entity, doc, spec)).await?),
                }
            }
            let m = crate::messages::Message {
                template,
                entity: record.map(|_| entity.to_string()),
                record,
                to: people,
                channels,
                all,
                values: given,
                attachments: files,
                secret,
            };
            return Ok(serde_json::json!(crate::messages::send(k, tx, ctx, &m).await?));
        }
        AutomationAction::Generate { template, name, language, replace, values, record: named } => {
            let id = match named {
                None => record.ok_or_else(|| Error::invalid("generate needs a record"))?,
                // The run's own record, or the item a loop is on: nothing else this run did not reach.
                Some(src) => {
                    let id = crate::doc::uuid_of(Some(&eval(&src)?)).ok_or_else(|| Error::invalid(format!("generate: {src} gives no record")))?;
                    let item = extra.iter().find(|(n, _)| *n == "item").and_then(|(_, v)| crate::doc::uuid_of(v.get("id")));
                    if Some(id) != record && Some(id) != item {
                        return Err(Error::invalid(format!("generate: {src} names neither this record nor the item of its loop")));
                    }
                    id
                }
            };
            let mut given = Map::new();
            for (n, expr) in values {
                given.insert(n, eval(&expr)?);
            }
            let text = |src: Option<String>| src.map(|s| eval(&s).map(as_text)).transpose();
            let g = crate::documents::Generate { template, record: id, name: text(name)?, language: text(language)?, replace, values: given };
            let made = Box::pin(crate::documents::generate(k, tx, ctx, &g)).await.map_err(|e| crate::pack::about("generate", e))?;
            return serde_json::to_value(made).map_err(Error::internal);
        }
        AutomationAction::ReportDocuments { template, mut params, params_from, values, message } => {
            let mut given = Map::new();
            for (n, expr) in values {
                given.insert(n, eval(&expr)?);
            }
            if !params_from.is_empty() && !params.is_object() {
                params = Value::Object(Map::new());
            }
            for (n, expr) in params_from {
                params[n] = eval(&expr)?;
            }
            let run = crate::documents::QueryRun { template, params, values: given, language: None };
            return Ok(serde_json::json!({"job": crate::documents::queue_groups(tx, ctx, run, message).await?}));
        }
        AutomationAction::Notify { to, title, body, message: None } => {
            let title = as_text(eval(&title)?);
            let body = body.map(|b| eval(&b).map(as_text)).transpose()?;
            let users: Vec<Uuid> = if to == "owner" {
                doc.get("created_by").and_then(Value::as_str).and_then(|s| s.parse().ok()).into_iter().collect()
            } else if let Some(role) = to.strip_prefix("role:") {
                sqlx::query_scalar("select user_id from user_role where tenant_id = $1 and role = $2 and (expires_at is null or expires_at > now())")
                    .bind(ctx.tenant)
                    .bind(role)
                    .fetch_all(&mut **tx)
                    .await?
            } else if let Some(u) = to.strip_prefix("user:") {
                vec![u.parse().map_err(|_| Error::invalid("notify: bad user id"))?]
            } else if to.starts_with("links:") {
                let people = Box::pin(crate::messages::recipients(k, tx, ctx, entity, doc, &to)).await?;
                crate::messages::users_of(tx, ctx.tenant, &people).await?
            } else if let Some(field) = to.strip_prefix("field:") {
                match doc.get(field).and_then(Value::as_str).map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()) {
                    Some(email) => {
                        sqlx::query_scalar("select id from app_user where tenant_id = $1 and lower(email) = $2 and is_active")
                            .bind(ctx.tenant)
                            .bind(email)
                            .fetch_all(&mut **tx)
                            .await?
                    }
                    None => vec![],
                }
            } else {
                return Err(Error::invalid("notify.to must be owner, role:<name>, user:<id>, field:<name> or links:<link>.<table>.<field>"));
            };
            for u in users {
                crate::notify::send_kind(tx, ctx.tenant, u, "automation", "inapp", &title, body.as_deref(), Some(entity), record).await?;
            }
        }
        AutomationAction::Email { to, subject, body, template, purpose, message: None } => {
            let to = match to.starts_with("links:") {
                true => {
                    let mut each: Vec<String> = Vec::new();
                    for r in Box::pin(crate::messages::recipients(k, tx, ctx, entity, doc, &to)).await? {
                        if let Some(e) = r.email.map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty() && !each.contains(e)) {
                            each.push(e);
                        }
                    }
                    each
                }
                false => vec![as_text(eval(&to)?)],
            };
            let subject = as_text(eval(&subject)?);
            // The body is text: nothing a record holds becomes markup in a mail sent in the workspace's name. A
            // template's own markup is the administrator's; what it prints from the record is escaped.
            let body = render_mail(&body, template.as_deref(), doc, user, extra)?;
            // Never sent from here: a later action could still roll the run back (FLC-HOOK-002).
            for to in to {
                k.queue_mail_for(tx, ctx.tenant, &to, &subject, &body, &[], purpose).await?;
            }
        }
        AutomationAction::Invoke { capability, input, key: named } => {
            // Checked again as it runs: the capability's app may have gone since, or its action been configured.
            check_invoke(k, ctx, &capability, &input)?;
            let mut values = Map::new();
            for (name, expr) in input {
                values.insert(name, eval(&expr)?);
            }
            let key = match named {
                Some(src) => as_text(eval(&src)?),
                None => key.to_string(),
            };
            let inv = crate::capability::Invocation::new(capability.as_str(), Value::Object(values)).key(key);
            // Boxed: an app's action reached through the gate is an action, which may run effects of its own.
            return match Box::pin(k.invoke_tx(ctx, tx, &inv)).await {
                Ok(r) => Ok(r.result),
                // Whoever pressed (a portal person too) is told in plain words; a key and a capability id are the
                // workspace's set-up, said to an administrator and the log (docs/api.md, "Once").
                Err(Error::Refused { code: "input_changed", status, message, pointer }) => {
                    tracing::info!(capability = %capability, "an invoke effect was refused: {message}");
                    let step = k.capability(ctx, &capability).map(|c| format!(" ({})", c.label)).unwrap_or_default();
                    let plain = format!("This step{step} was already done for this record with other details, so it was not done again.");
                    let message = if user.get("is_admin") == Some(&Value::Bool(true)) { format!("{plain} {message}") } else { plain };
                    Err(Error::Refused { code: "input_changed", status, message, pointer })
                }
                Err(e) => Err(e),
            };
        }
        AutomationAction::AppendLine { table, values } => {
            let id = record.ok_or_else(|| Error::invalid("append_line needs a record"))?;
            return write_line(k, ctx, tx, entity, id, (raw, doc), user, extra, &table, None, values).await;
        }
        AutomationAction::SetLine { table, when, values } => {
            let id = record.ok_or_else(|| Error::invalid("set_line needs a record"))?;
            return write_line(k, ctx, tx, entity, id, (raw, doc), user, extra, &table, Some(&when), values).await;
        }
        AutomationAction::Find { entity: target, by, create, update } => {
            return Box::pin(find(k, ctx, tx, entity, (raw, doc), user, extra, &target, by, create, update)).await.map_err(|e| crate::pack::about("find", e));
        }
        AutomationAction::ForEach { table, entity: over, link, links, filter, effects } => {
            let id = record.ok_or_else(|| Error::invalid("for_each needs a record"))?;
            let (what, items) = match (table, over) {
                (None, None) if links.is_some() => {
                    let path = links.unwrap_or_default();
                    let (l, t) = path.split_once('.').ok_or_else(|| Error::invalid(format!("for_each: links is <link>.<table>, not {path}")))?;
                    let ent = ctx.schema.entity(entity).cloned().ok_or_else(|| Error::invalid(format!("unknown entity {entity}")))?;
                    if let Some(m) = crate::messages::lines_problem(&ctx.schema, &ent, &path) {
                        return Err(Error::invalid(format!("for_each: {m}")));
                    }
                    let names = std::collections::BTreeSet::from([l.to_string()]);
                    let linked = crate::links::load(k, ctx, tx, &ent, doc, &names).await?;
                    (format!("lines of {path}"), linked[l][t].as_array().cloned().unwrap_or_default())
                }
                (Some(t), _) => {
                    let ent = ctx.schema.entity(entity).cloned().ok_or_else(|| Error::invalid(format!("unknown entity {entity}")))?;
                    let cent = ent.field(&t).and_then(|f| f.of.as_deref()).and_then(|c| ctx.schema.entity(c)).cloned();
                    let cent = cent.ok_or_else(|| Error::invalid(format!("for_each: {t} is not a table of lines of {}", ent.def.label)))?;
                    let stored = k.get_tx(ctx, tx, entity, id, false).await?;
                    let rows = stored.get(&t).and_then(Value::as_array).cloned().unwrap_or_default();
                    (format!("lines of {t}"), rows.iter().map(|r| cent.cel_view(r, &ctx.schema)).collect::<Vec<_>>())
                }
                (None, Some(o)) => {
                    let oent = ctx.schema.entity(&o).cloned().ok_or_else(|| Error::invalid(format!("for_each: {o} is not installed")))?;
                    let link = link.ok_or_else(|| Error::invalid("for_each: link names the field that points here"))?;
                    let filters = [crate::query::Filter::eq(&link, id.to_string())];
                    let found = k.find_tx(ctx, tx, &o, &filters, Some("created_at asc"), MAX_ITEMS as u32 + 1).await?;
                    (format!("{} records", oent.def.label), found.into_iter().map(|d| oent.cel_view(&Value::Object(d), &ctx.schema)).collect())
                }
                (None, None) => return Err(Error::invalid("for_each runs over a table or the records linking here")),
            };
            if items.len() > MAX_ITEMS {
                return Err(Error::invalid(format!("for_each: more than {MAX_ITEMS} {what}, the most one loop runs over; nothing was done")));
            }
            let mut out = Vec::new();
            for (n, item) in items.iter().enumerate() {
                let mut vars = extra.to_vec();
                vars.push(("item", item));
                if let Some(w) = &filter
                    && eval_with(w, doc, user, &vars)? != Value::Bool(true)
                {
                    continue;
                }
                let tag = item.get("id").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| n.to_string());
                let mut here = Vec::new();
                for (j, s) in effects.iter().cloned().enumerate() {
                    // Its own effect key per item, so an `invoke` in a loop runs once for each.
                    let r = Box::pin(run_step(k, ctx, tx, entity, record, (raw, doc), user, &vars, &format!("{key}.{tag}.{j}"), s)).await;
                    here.push(r.map_err(|e| crate::pack::about(&format!("for_each, item {}, effect {}", n + 1, j + 1), e))?);
                }
                out.push(Value::Array(here));
            }
            return Ok(Value::Array(out));
        }
        AutomationAction::CopyLines { table, link, result, from_table, values, mode } => {
            let id = record.ok_or_else(|| Error::invalid("copy_lines needs a record"))?;
            return copy_lines(k, ctx, tx, entity, id, (raw, doc), user, extra, (&table, link, result, &from_table), values, mode)
                .await
                .map_err(|e| crate::pack::about("copy_lines", e));
        }
    }
    Ok(Value::Null)
}

/// The files a `message` attaches: what each expression gave (an id, or `{file}` as `generate` gives it), each a file
/// of the run's record. Naming another's file is refused: an expression is no way to mail a file out.
async fn attached(tx: &mut crate::db::Tx, ctx: &Ctx, entity: &str, record: Option<Uuid>, given: Vec<(String, Value)>) -> Result<Vec<Uuid>> {
    let mut files = Vec::new();
    for (src, v) in given {
        let id = match v.get("file").unwrap_or(&v) {
            Value::Null => continue,
            Value::String(s) => {
                s.trim_start_matches("/files/").parse::<Uuid>().map_err(|_| Error::invalid(format!("message.attachments: {src} gives no file")))?
            }
            _ => return Err(Error::invalid(format!("message.attachments: {src} gives no file"))),
        };
        files.push(id);
    }
    if files.is_empty() {
        return Ok(files);
    }
    let theirs: i64 = sqlx::query_scalar("select count(distinct id) from file_object where tenant_id = $1 and id = any($2) and entity = $3 and record_id = $4")
        .bind(ctx.tenant)
        .bind(&files)
        .bind(entity)
        .bind(record)
        .fetch_one(&mut **tx)
        .await?;
    if theirs as usize != files.iter().collect::<std::collections::HashSet<_>>().len() {
        return Err(Error::invalid("message.attachments: a file to attach is not this record's"));
    }
    Ok(files)
}

/// What `find` does (see [`AutomationAction::Find`]).
#[allow(clippy::too_many_arguments)]
async fn find(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut crate::db::Tx,
    entity: &str,
    (raw, doc): (&Value, &Value),
    user: &Value,
    extra: &[(&str, &Value)],
    target: &str,
    by: BTreeMap<String, String>,
    create: Option<BTreeMap<String, String>>,
    update: Option<BTreeMap<String, String>>,
) -> Result<Value> {
    let tent = ctx.schema.entity(target).cloned().ok_or_else(|| Error::invalid(format!("{target} is not installed")))?;
    let made = |d: &Doc| serde_json::json!({"entity": target, "id": d.get("id"), "number": d.get("number")});
    let mut keys = Doc::new();
    for (f, src) in by {
        let v = effect_value(k, ctx, tx, entity, &src, doc, user, extra, &[("doc", raw)]).await?;
        if v.is_null() || v == Value::String(String::new()) {
            return Err(Error::invalid(format!("{f} is empty, so it picks no {}", tent.def.label)));
        }
        keys.insert(f, v);
    }
    let said = || keys.iter().map(|(f, v)| format!("{f} {}", as_text(v.clone()))).collect::<Vec<_>>().join(", ");
    if create.is_some() {
        // Runs that may make the same record take turns: the second finds what the first made (read committed), where
        // without this both would find none and make two. A unique key on `by` refuses a second all the same.
        let lock = format!("find:{}:{target}:{}", ctx.tenant, Value::Object(keys.clone()));
        sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))").bind(lock).execute(&mut **tx).await?;
    }
    let filters: Vec<crate::query::Filter> = keys.iter().map(|(f, v)| crate::query::Filter::eq(f, v.clone())).collect();
    match k.find_tx(ctx, tx, target, &filters, None, 2).await?.as_slice() {
        [] => {
            let Some(values) = create else { return Ok(Value::Null) };
            let mut input = keys.clone();
            for (f, src) in values {
                input.insert(f, effect_value(k, ctx, tx, entity, &src, doc, user, extra, &[("doc", raw)]).await?);
            }
            let d = k.insert_tx(ctx, tx, target, input).await?;
            Ok(made(&d))
        }
        [one] => {
            let Some(values) = update else { return Ok(made(one)) };
            let id = crate::store::doc_id(one)?;
            let linked = Value::Object(k.get_tx(ctx, tx, target, id, true).await?);
            let seen = tent.cel_view(&linked, &ctx.schema);
            let mut vars = extra.to_vec();
            vars.push(("linked", &seen));
            let mut patch = Doc::new();
            for (f, src) in values {
                patch.insert(f, effect_value(k, ctx, tx, entity, &src, doc, user, &vars, &[("doc", raw), ("linked", &linked)]).await?);
            }
            let d = k.update_tx(ctx, tx, target, id, patch, None).await?;
            Ok(made(&d))
        }
        _ => Err(Error::invalid(format!("more than one {} has {}: it picks one record, never a guess", tent.def.label, said()))),
    }
}

/// What `copy_lines` does (see [`AutomationAction::CopyLines`]): one save of the record.
#[allow(clippy::too_many_arguments)]
async fn copy_lines(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut crate::db::Tx,
    entity: &str,
    id: Uuid,
    (raw, doc): (&Value, &Value),
    user: &Value,
    extra: &[(&str, &Value)],
    (table, link, result, from_table): (&str, Option<String>, Option<usize>, &str),
    values: BTreeMap<String, String>,
    mode: CopyMode,
) -> Result<Value> {
    let ent = ctx.schema.entity(entity).cloned().ok_or_else(|| Error::invalid(format!("unknown entity {entity}")))?;
    let source = match (link, result) {
        (Some(l), _) => linked_entity(&ent, &l).map(str::to_string).zip(doc.get(&l).and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok())),
        (None, Some(n)) => {
            let given = extra.iter().find(|(name, _)| *name == "results").and_then(|(_, r)| r.get(n)).cloned().unwrap_or_default();
            given.get("entity").and_then(Value::as_str).map(str::to_string).zip(given.get("id").and_then(Value::as_str).and_then(|s| s.parse::<Uuid>().ok()))
        }
        (None, None) => None,
    };
    // Nothing to copy from.
    let Some((from, from_id)) = source else { return Ok(Value::Null) };
    let sent = ctx.schema.entity(&from).cloned().ok_or_else(|| Error::invalid(format!("{from} is not installed")))?;
    let scent = sent.field(from_table).and_then(|f| f.of.as_deref()).and_then(|c| ctx.schema.entity(c)).cloned();
    let scent = scent.ok_or_else(|| Error::invalid(format!("{from_table} is not a table of lines of {}", sent.def.label)))?;
    let rows = k.get_tx(ctx, tx, &from, from_id, false).await?.get(from_table).and_then(Value::as_array).cloned().unwrap_or_default();
    if rows.len() > MAX_ITEMS {
        return Err(Error::invalid(format!(
            "{} has {} lines of {from_table}, more than the {MAX_ITEMS} one copy takes; nothing was copied",
            sent.def.label,
            rows.len()
        )));
    }
    let stored = k.get_tx(ctx, tx, entity, id, false).await?;
    let mut lines: Vec<Value> = match mode {
        CopyMode::Append => stored.get(table).and_then(Value::as_array).into_iter().flatten().map(|r| serde_json::json!({"id": r.get("id")})).collect(),
        CopyMode::Replace => vec![],
    };
    for r in &rows {
        let item = scent.cel_view(r, &ctx.schema);
        let mut vars = extra.to_vec();
        vars.push(("item", &item));
        let mut line = Map::new();
        for (f, src) in &values {
            line.insert(f.clone(), effect_value(k, ctx, tx, entity, src, doc, user, &vars, &[("doc", raw), ("item", r)]).await?);
        }
        lines.push(Value::Object(line));
    }
    let d = k.update_tx(ctx, tx, entity, id, Doc::from_iter([(table.to_string(), Value::Array(lines))]), None).await?;
    Ok(serde_json::json!({"entity": entity, "id": d.get("id"), "number": d.get("number"), "copied": rows.len()}))
}

/// `append_line` (no `when`) or `set_line` on record `id`: through the record's ordinary save, with a patch that names
/// every stored line by its id alone, so each keeps what it holds, and gives only the one line its named fields.
#[allow(clippy::too_many_arguments)]
async fn write_line(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut crate::db::Tx,
    entity: &str,
    id: Uuid,
    (raw, doc): (&Value, &Value),
    user: &Value,
    extra: &[(&str, &Value)],
    table: &str,
    when: Option<&str>,
    values: BTreeMap<String, String>,
) -> Result<Value> {
    let ent = ctx.schema.entity(entity).cloned().ok_or_else(|| Error::invalid(format!("unknown entity {entity}")))?;
    let field = ent.field(table).filter(|f| f.ftype == ferrum_meta::FieldType::Table);
    let cent = field.and_then(|f| f.of.as_deref()).and_then(|c| ctx.schema.entity(c)).cloned();
    let cent = cent.ok_or_else(|| Error::invalid(format!("{table} is not a table field of {}", ent.def.label)))?;
    let stored = k.get_tx(ctx, tx, entity, id, false).await?;
    let rows = stored.get(table).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut lines: Vec<Value> = rows.iter().map(|r| serde_json::json!({"id": r.get("id")})).collect();
    let (at, row) = match when {
        None => (lines.len(), None),
        Some(w) => {
            let mut hits = Vec::new();
            for (i, r) in rows.iter().enumerate() {
                let row = cent.cel_view(r, &ctx.schema);
                let vars: Vec<(&str, &Value)> = extra.iter().copied().chain([("row", &row)]).collect();
                if eval_with(w, doc, user, &vars)? == Value::Bool(true) {
                    hits.push(i);
                }
            }
            match hits[..] {
                [i] => (i, Some(cent.cel_view(&rows[i], &ctx.schema))),
                [] => {
                    return Err(Error::invalid(format!("set_line: no line of {} has {w}", ent.field(table).and_then(|f| f.label.as_deref()).unwrap_or(table))));
                }
                _ => {
                    let which = hits.iter().map(|i| (i + 1).to_string()).collect::<Vec<_>>().join(", ");
                    return Err(Error::invalid(format!("set_line: lines {which} of {table} all have {w}; it changes one line")));
                }
            }
        }
    };
    let vars: Vec<(&str, &Value)> = extra.iter().copied().chain(row.as_ref().map(|r| ("row", r))).collect();
    let stored_row = rows.get(at).filter(|_| row.is_some()).unwrap_or(&Value::Null);
    let mut line = Map::new();
    for (f, src) in values {
        line.insert(f, effect_value(k, ctx, tx, entity, &src, doc, user, &vars, &[("doc", raw), ("row", stored_row)]).await?);
    }
    match lines.get_mut(at) {
        Some(Value::Object(kept)) => kept.extend(line),
        _ => lines.push(Value::Object(line)),
    }
    let d = k.update_tx(ctx, tx, entity, id, Doc::from_iter([(table.to_string(), Value::Array(lines))]), None).await?;
    Ok(serde_json::json!({"entity": entity, "id": d.get("id"), "number": d.get("number")}))
}

/// Validates and stores an automation definition.
pub async fn save(k: &Kernel, ctx: &Ctx, input: &Map<String, Value>) -> Result<Uuid> {
    // What an `invoke` names is checked against the catalog here, where the kernel is at hand.
    // ponytail: a pack's install (save_in) skips this; its invoke is refused when it runs instead.
    if ctx.is_admin
        && let Ok(list) = validate_actions(input.get("actions").unwrap_or(&Value::Null))
    {
        for (i, s) in list.iter().enumerate() {
            for a in every(std::slice::from_ref(s)) {
                if let AutomationAction::Invoke { capability, input, .. } = a {
                    check_invoke(k, ctx, capability, input)
                        .map_err(|e| Error::Validation(vec![FieldError::new("actions", e.to_string()).in_row("actions", i)]))?;
                }
            }
        }
    }
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let id = save_in(ctx, &mut tx, input).await?;
    tx.commit().await?;
    Ok(id)
}

/// [`save`] in the caller's transaction (a pack's install, pack.rs).
pub(crate) async fn save_in(ctx: &Ctx, tx: &mut crate::db::Tx, input: &Map<String, Value>) -> Result<Uuid> {
    // What it says runs with an administrator's rights (its saver's), so saying it takes one.
    if !ctx.is_admin {
        return Err(Error::Forbidden("administrator access required".into()));
    }
    let get = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_string);
    let name = get("name").filter(|n| !n.trim().is_empty()).ok_or_else(|| Error::field("name", "is required"))?;
    let entity = get("entity").ok_or_else(|| Error::field("entity", "is required"))?;
    if ctx.schema.entity(&entity).is_none() {
        return Err(Error::field("entity", "unknown entity"));
    }
    let event = get("event").ok_or_else(|| Error::field("event", "is required"))?;
    if !["created", "updated", "submitted", "cancelled", "deleted", "transitioned", "scheduled"].contains(&event.as_str()) {
        return Err(Error::field("event", "must be created, updated, submitted, cancelled, deleted, transitioned or scheduled"));
    }
    // A schedule in place of a record event, checked against the entity it is on (schedule.rs).
    let schedule = input.get("schedule").filter(|v| !v.is_null()).cloned();
    match (&schedule, event == "scheduled") {
        (Some(s), true) => {
            let ent = ctx.schema.entity(&entity).ok_or_else(|| Error::field("entity", "unknown entity"))?;
            crate::schedule::plan(&ctx.schema, ent, s)?;
        }
        (None, true) => return Err(Error::field("schedule", "a scheduled automation needs a schedule")),
        (Some(_), false) => return Err(Error::field("schedule", "only an automation whose event is scheduled has a schedule")),
        (None, false) => {}
    }
    let condition = get("condition").filter(|c| !c.trim().is_empty());
    if let Some(c) = &condition {
        let compiled = Expr::compile(c).map_err(|m| Error::field("condition", m))?;
        let ent = ctx.schema.entity(&entity).ok_or_else(|| Error::field("entity", "unknown entity"))?;
        if let Some(n) = compiled.links().iter().find(|n| linked_entity(ent, n).is_none()) {
            return Err(Error::field("condition", format!("links.{n}: {n} is not a link field of {}", ent.def.label)));
        }
        if let Some(m) = ctx.schema.settings_problem(&compiled) {
            return Err(Error::field("condition", m));
        }
    }
    let actions = input.get("actions").cloned().unwrap_or(Value::Null);
    let steps = validate_actions(&actions)?;
    check_targets(&ctx.schema, &entity, &steps)?;
    let known = known_templates(tx, ctx.tenant, &ctx.schema).await?;
    if let Some((i, m)) = steps.iter().enumerate().find_map(|(i, s)| unknown_template(s, &known).map(|m| (i, m))) {
        return Err(Error::Validation(vec![FieldError::new("actions", m).in_row("actions", i)]));
    }
    let id = input.get("id").and_then(Value::as_str).and_then(|s| s.parse().ok()).unwrap_or_else(Uuid::now_v7);
    let active = input.get("is_active").and_then(Value::as_bool).unwrap_or(true);
    if active {
        // Never both: while the solution imported from it runs in its place, it stays off. Its row is locked first,
        // as an activation handing it over locks it, so the two cannot cross (FLC-MIG-003).
        sqlx::query("select 1 from automation where tenant_id = $1 and id = $2 for update").bind(ctx.tenant).bind(id).execute(&mut **tx).await?;
        let solution = crate::authoring::solution_of(id);
        let running: Option<i32> = sqlx::query_scalar("select 1 from logic_activation where tenant_id = $1 and solution = $2")
            .bind(ctx.tenant)
            .bind(&solution)
            .fetch_optional(&mut **tx)
            .await?;
        if running.is_some() {
            return Err(Error::Conflict(format!(
                "{solution}, imported from this automation, runs in its place: deactivate it before turning the automation on again"
            )));
        }
    }
    // A schedule starts from now: when it is new, changed (or its entity), or switched on again, no moment
    // already past fires.
    sqlx::query(
        "insert into automation (tenant_id, id, name, entity, event, condition, actions, is_active, created_by, schedule, scanned_until) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, case when $10::jsonb is null then null else now() end) \
         on conflict (tenant_id, id) do update set name = excluded.name, \
         entity = excluded.entity, event = excluded.event, condition = excluded.condition, actions = excluded.actions, \
         is_active = excluded.is_active, created_by = excluded.created_by, updated_at = now(), schedule = excluded.schedule, \
         scanned_until = case when automation.schedule is not distinct from excluded.schedule and automation.entity = excluded.entity \
           and (automation.is_active or not excluded.is_active) then automation.scanned_until else excluded.scanned_until end, \
         scan_to = case when automation.schedule is not distinct from excluded.schedule and automation.entity = excluded.entity \
           and (automation.is_active or not excluded.is_active) then automation.scan_to end, \
         scan_cursor = case when automation.schedule is not distinct from excluded.schedule and automation.entity = excluded.entity \
           and (automation.is_active or not excluded.is_active) then automation.scan_cursor end",
    )
    .bind(ctx.tenant)
    .bind(id)
    .bind(name)
    .bind(entity)
    .bind(event)
    .bind(condition)
    .bind(actions)
    .bind(active)
    .bind((!ctx.user.is_nil()).then_some(ctx.user))
    .bind(schedule)
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

/// An automation row as a package's default is compared with it ([`ferrum_meta::PackageAutomation::definition`]).
const DEFINITION: &str = "jsonb_build_object('name', name, 'entity', entity, 'event', event, 'condition', condition, 'schedule', schedule, \
                          'actions', actions)";

/// Installs package `package`'s automation `a` in `ctx`'s workspace, as the system: new, it is added, turned on; one the
/// workspace has not edited takes the package's definition as it now is, staying on or off as the workspace left it;
/// an edited one is the workspace's and is kept (and flagged when the default changed, [`package_marks`]).
pub(crate) async fn install_default(ctx: &Ctx, tx: &mut crate::db::Tx, package: &str, a: &ferrum_meta::PackageAutomation) -> Result<()> {
    let default = a.definition();
    let found: Option<(Uuid, bool, Option<Value>, Value)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select id, is_active, base, {DEFINITION} from automation where tenant_id = $1 and package = $2 and package_key = $3 for update"
    )))
    .bind(ctx.tenant)
    .bind(package)
    .bind(&a.name)
    .fetch_optional(&mut **tx)
    .await?;
    let (id, active) = match found {
        // Edited here, or already as the package says.
        Some((_, _, base, now)) if base.as_ref() != Some(&now) || now == default => return Ok(()),
        Some((id, active, ..)) => (id, active),
        None => (Uuid::now_v7(), true),
    };
    let mut input = default.as_object().cloned().unwrap_or_default();
    input.insert("id".into(), Value::String(id.to_string()));
    input.insert("is_active".into(), Value::Bool(active));
    save_in(ctx, tx, &input).await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "update automation set package = $3, package_key = $4, base = {DEFINITION}, created_by = null where tenant_id = $1 and id = $2"
    )))
    .bind(ctx.tenant)
    .bind(id)
    .bind(package)
    .bind(&a.name)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Marks each of `rows` (automations as listed, `to_jsonb` of their rows) that a package ships: `edited` when the
/// workspace changed it, `default_changed` when a release changed the default since it was installed or edited.
pub fn package_marks(catalog: &ferrum_meta::Catalog, rows: &mut [Value]) {
    for r in rows.iter_mut() {
        let (Some(p), Some(key)) = (r["package"].as_str(), r["package_key"].as_str()) else { continue };
        let default = catalog.package(p).and_then(|p| p.manifest.automations.iter().find(|a| a.name == key)).map(|a| a.definition());
        let now = serde_json::json!({"name": r["name"], "entity": r["entity"], "event": r["event"], "condition": r["condition"],
            "schedule": r["schedule"], "actions": r["actions"]});
        r["edited"] = Value::Bool(r["base"] != now);
        r["default_changed"] = Value::Bool(default.is_some_and(|d| d != r["base"]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn a_bare_path_is_a_path_to_a_value_alone() {
        for (src, steps) in [
            ("doc.a", vec!["doc", "a"]),
            (" doc.a.b ", vec!["doc", "a", "b"]),
            ("linked.price", vec!["linked", "price"]),
            ("links.plan.price", vec!["links", "plan", "price"]),
            ("row.amount", vec!["row", "amount"]),
            ("params.x", vec!["params", "x"]),
            ("results[0].total", vec!["results", "0", "total"]),
        ] {
            assert_eq!(bare_path(src), Some(steps), "{src}");
        }
        for src in [
            "doc",
            "doc.a + 0",
            "doc.a == null ? null : doc.a",
            "(doc.a)",
            "doc.a()",
            "size(doc.a)",
            "user.email",
            "doc.a[x]",
            "doc.a[]",
            "doc..a",
            "0",
            "'doc.a'",
        ] {
            assert_eq!(bare_path(src), None, "{src}");
        }
    }

    #[test]
    fn stored_empty_is_a_null_or_a_missing_key() {
        let v = serde_json::json!({"a": null, "b": 0, "c": {"d": null}, "e": [false]});
        assert!(stored_empty(&v, &["a"]) && stored_empty(&v, &["z"]) && stored_empty(&v, &["c", "d"]) && stored_empty(&v, &["a", "x"]));
        assert!(!stored_empty(&v, &["b"]) && !stored_empty(&v, &["e", "0"]) && !stored_empty(&v, &["e", "5"]) && !stored_empty(&v, &["b", "x"]));
    }

    #[test]
    fn an_expression_is_compiled_once() {
        let src = "doc.title + ' (compiled once)'";
        let before = COMPILES.load(Ordering::Relaxed);
        for _ in 0..3 {
            assert_eq!(eval(src, &serde_json::json!({"title": "A"}), None, &Value::Null, &[]).unwrap(), "A (compiled once)");
        }
        assert_eq!(COMPILES.load(Ordering::Relaxed) - before, 1);
    }
}
