//! Messages to people (docs/builder/decisions.md BD-20): templates a workspace edits, sent on e-mail, text message,
//! chat app or in-app notice, recorded send by send.
//!
//! * **Templates.** A package ships a default for every message it sends (`messages:` in its YAML); a workspace
//!   edits a copy, sees the default, resets to it, and is told when a release changed the default it edited (never
//!   written over). Bodies are the print formats' engine over `doc` (the record as the workspace holds it, its
//!   secret fields left out), `settings`, `values` (what the sender passes) and `opt_out`.
//! * **Sending.** [`send`], or the `message` effect: for each recipient the channels are tried in order and the first
//!   one they can be reached on, and have not opted out of, is used (each one, with `all`). Every send is a row of
//!   `message_send`, skipped ones with why.
//! * **Delivery.** E-mail goes through the outbox as all mail does; in-app through notifications; text and chat-app
//!   messages through a job per send, to the workspace's channel connection: an HTTP request it describes
//!   ([`ChannelDesc`]), retried while the provider says "later", failed at once when it says "no". No provider is
//!   named here.
//! * **Choice.** Promotional messages respect an opt-out per address or number and channel (the consent records
//!   there are, `contact_consent`), and quiet hours; operational ones always go. Rates are limited per workspace and
//!   per recipient.

use crate::connections::at;
use crate::ctx::Ctx;
use crate::db::Tx;
use crate::error::{Error, Result};
use crate::jobs::{Job, JobHandler, NewJob};
use crate::notify::Audience::Outside;
use crate::{Kernel, audit};
use async_trait::async_trait;
use chrono::{DateTime, NaiveTime, Timelike, Utc};
use ferrum_meta::{FieldType, MessageBodies, MessageCategory, MessageDef};
use minijinja::{AutoEscape, UndefinedBehavior, Value as JValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use uuid::Uuid;

pub const DELIVER_KIND: &str = "platform.message.deliver";
/// Attempts at one text or chat-app message before it counts as failed.
const DELIVERY_ATTEMPTS: i32 = 8;
/// Text and chat-app messages a workspace sends an hour, and one recipient gets, unless its settings say otherwise.
const PER_HOUR: i64 = 500;
const PER_RECIPIENT_HOUR: i64 = 10;
/// One-time codes: how long one holds, how many tries it takes, how many a recipient is sent an hour.
const CODE_MINUTES: i64 = 10;
const CODE_TRIES: i32 = 5;
const CODES_PER_HOUR: i64 = 5;
/// Inbound calls refused because they named no channel there is.
static REFUSED_UNKNOWN: AtomicU64 = AtomicU64::new(0);

/// How many inbound calls named a channel that is not there.
pub fn refused_unknown() -> u64 {
    REFUSED_UNKNOWN.load(Ordering::Relaxed)
}

fn admin_only(ctx: &Ctx) -> Result<()> {
    if ctx.is_admin { Ok(()) } else { Err(Error::Forbidden("message templates and channels are for administrators".into())) }
}

// ── phone numbers and settings ──

/// The workspace's `messages` setting: `calling_code` completes a number typed without one (`+49`), `trunk_prefix`
/// is what such a number starts with nationally and drops (`0`), `quiet_hours` `{from, to}` (`"21:00"`, `"08:00"`) hold
/// promotional text and chat-app messages, `per_hour` and `per_recipient_hour` limit text and chat-app messages.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calling_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trunk_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_hours: Option<QuietHours>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_hour: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_recipient_hour: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuietHours {
    pub from: String,
    pub to: String,
}

/// What is wrong with a `messages` setting.
pub fn check_settings(v: &Value) -> Result<Settings> {
    let s: Settings = serde_json::from_value(v.clone()).map_err(|e| Error::field("messages", e.to_string()))?;
    if s.calling_code.as_deref().is_some_and(|c| !c.starts_with('+') || !(2..=4).contains(&c.len()) || !c[1..].bytes().all(|b| b.is_ascii_digit())) {
        return Err(Error::field("messages", "calling_code is + and one to three digits (+49)"));
    }
    if s.trunk_prefix.as_deref().is_some_and(|t| t.is_empty() || t.len() > 2 || !t.bytes().all(|b| b.is_ascii_digit())) {
        return Err(Error::field("messages", "trunk_prefix is one or two digits"));
    }
    if let Some(q) = &s.quiet_hours
        && (clock(&q.from).is_none() || clock(&q.to).is_none() || q.from == q.to)
    {
        return Err(Error::field("messages", "quiet_hours runs from one time (HH:MM) to another"));
    }
    if [s.per_hour, s.per_recipient_hour].iter().flatten().any(|n| *n < 0) {
        return Err(Error::field("messages", "a limit is a number of messages"));
    }
    Ok(s)
}

fn clock(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M").ok()
}

/// A number in international form (`+4915123456789`): as written when it starts with `+`, otherwise completed with
/// the workspace's calling code (its trunk prefix dropped). Without a calling code such a number is refused, never
/// guessed.
pub fn international(number: &str, s: &Settings) -> std::result::Result<String, String> {
    let t = number.trim();
    let bad = || format!("{t} is not a phone number");
    if t.is_empty() || !t.chars().enumerate().all(|(i, c)| c.is_ascii_digit() || " -.()/".contains(c) || (c == '+' && i == 0)) {
        return Err(bad());
    }
    let digits: String = t.chars().filter(char::is_ascii_digit).collect();
    let full = if t.starts_with('+') {
        digits
    } else {
        let code = s.calling_code.as_deref().ok_or_else(|| format!("{t} has no country calling code, and the workspace sets none"))?;
        let national = s.trunk_prefix.as_deref().and_then(|p| digits.strip_prefix(p)).unwrap_or(&digits);
        format!("{}{national}", &code[1..])
    };
    if !(7..=15).contains(&full.len()) || full.starts_with('0') {
        return Err(bad());
    }
    Ok(format!("+{full}"))
}

/// The workspace's `messages` setting and language.
pub(crate) async fn workspace(tx: &mut Tx, tenant: Uuid) -> Result<(Settings, Option<String>)> {
    let (s, language): (Option<Value>, Option<String>) =
        sqlx::query_as("select settings->'messages', settings->>'language' from tenant where id = $1").bind(tenant).fetch_one(&mut **tx).await?;
    Ok((s.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default(), language))
}

/// Seconds until quiet hours end, when `now` is in them where the recipient is (`zone`, else the workspace's).
fn quiet_for(q: Option<&QuietHours>, zone: &str, now: DateTime<Utc>) -> Option<u64> {
    let q = q?;
    let (from, to) = (clock(&q.from)?, clock(&q.to)?);
    let tz: chrono_tz::Tz = zone.parse().unwrap_or(chrono_tz::UTC);
    let local = now.with_timezone(&tz);
    let t = local.time();
    let inside = if from < to { t >= from && t < to } else { t >= from || t < to };
    if !inside {
        return None;
    }
    let secs = |x: NaiveTime| x.num_seconds_from_midnight() as i64;
    let left = (secs(to) - secs(t)).rem_euclid(86_400);
    Some(left.max(1) as u64)
}

// ── templates ──

fn digest(def: &MessageDef) -> String {
    hex::encode(Sha256::digest(serde_json::to_vec(def).unwrap_or_default()))
}

/// The environment bodies render in: HTML-escaped for an e-mail's body, as written for anything else; `json` writes a
/// value as JSON (for a channel's request).
fn environment(html: bool, strict: bool) -> minijinja::Environment<'static> {
    let mut env = crate::print::environment();
    if !html {
        env.set_auto_escape_callback(|_| AutoEscape::None);
    }
    if strict {
        env.set_undefined_behavior(UndefinedBehavior::Strict);
    }
    env.add_filter("json", |v: JValue| serde_json::to_string(&v).unwrap_or_default());
    env
}

fn render(src: &str, html: bool, vars: &Value, strict: bool) -> Result<String> {
    if src.len() > crate::automation::MAX_TEMPLATE {
        return Err(Error::field("template", "a body is larger than 64 KB"));
    }
    environment(html, strict).render_str(src, JValue::from_serialize(vars)).map_err(|e| Error::field("template", e.to_string()))
}

/// One channel's message, rendered: `{subject, html}`, `{text}`, `{template_name, values, text}` or `{title, text}`.
fn render_channel(b: &MessageBodies, channel: &str, vars: &Value, strict: bool) -> Result<Value> {
    let plain = |s: &str| render(s, false, vars, strict);
    let missing = || Error::field("channel", format!("the template has no {channel} body in this language"));
    Ok(match channel {
        "email" => {
            let m = b.email.as_ref().ok_or_else(missing)?;
            let subject = plain(&m.subject)?;
            json!({"subject": subject.split_whitespace().collect::<Vec<_>>().join(" "), "html": render(&m.body, true, vars, strict)?})
        }
        "text" => json!({"text": plain(&b.text.as_ref().ok_or_else(missing)?.text)?}),
        "chat" => {
            let c = b.chat.as_ref().ok_or_else(missing)?;
            let values = c.values.iter().map(|v| plain(v)).collect::<Result<Vec<_>>>()?;
            json!({"template_name": c.template, "values": values, "text": c.text.as_deref().map(plain).transpose()?})
        }
        "inapp" => {
            let n = b.inapp.as_ref().ok_or_else(missing)?;
            json!({"title": plain(&n.title)?, "text": n.text.as_deref().map(plain).transpose()?})
        }
        other => return Err(Error::field("channel", format!("{other} is not a channel"))),
    })
}

/// The settings entities a template's bodies read (`settings.Entity`).
fn settings_named(def: &MessageDef) -> std::collections::BTreeSet<String> {
    let text = serde_json::to_string(&def.languages).unwrap_or_default();
    let mut out = std::collections::BTreeSet::new();
    for (i, _) in text.match_indices("settings.") {
        let name: String = text[i + 9..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if name.starts_with(|c: char| c.is_ascii_uppercase()) {
            out.insert(name);
        }
    }
    out
}

/// A record as a message shows it: as the workspace holds it, its secret fields (and its lines') left out. A message
/// goes to people outside, so it is not read with the rights of whoever caused it (an automation's runner, a guest),
/// and what is sealed never leaves.
async fn record_view(k: &Kernel, tx: &mut Tx, ctx: &Ctx, entity: Option<&str>, record: Option<Uuid>) -> Result<Value> {
    let (Some(entity), Some(id)) = (entity, record) else { return Ok(Value::Null) };
    let sys = Ctx::system(ctx.tenant, ctx.schema.clone());
    let ent = k.entity(&sys, entity)?;
    let mut doc = k.get_tx(&sys, tx, entity, id, false).await?;
    without_secrets(&ctx.schema, &ent, &mut doc);
    Ok(Value::Object(doc))
}

/// `doc` with its secret fields, and its lines', left out.
pub(crate) fn without_secrets(schema: &ferrum_meta::Schema, ent: &ferrum_meta::EffEntity, doc: &mut crate::Doc) {
    for s in ent.secret_fields() {
        doc.remove(&s.name);
    }
    for t in ent.table_fields() {
        let (Some(cent), Some(Value::Array(rows))) = (t.of.as_deref().and_then(|c| schema.entity(c)), doc.get_mut(&t.name)) else { continue };
        for r in rows.iter_mut().filter_map(Value::as_object_mut) {
            for s in cent.secret_fields() {
                r.remove(&s.name);
            }
        }
    }
}

/// The platform's keys a record has besides its fields.
const SAMPLE_KEYS: [&str; 8] = ["id", "number", "created_at", "updated_at", "created_by", "version", "docstatus", "status"];

/// One channel's bodies of a language, as written.
fn channel_bodies(b: &MessageBodies, ch: &str) -> Value {
    match ch {
        "email" => json!(b.email),
        "text" => json!(b.text),
        "chat" => json!(b.chat),
        _ => json!(b.inapp),
    }
}

/// A record of `entity` with every field there, for checking a template: an unknown field is refused.
fn sample_doc(ctx: &Ctx, entity: Option<&str>) -> Value {
    let Some(ent) = entity.and_then(|e| ctx.schema.entity(e)) else { return json!({}) };
    let mut doc: Map<String, Value> = SAMPLE_KEYS.into_iter().map(|f| (f.to_string(), json!(""))).collect();
    for f in ent.def.fields.iter().filter(|f| f.ftype != FieldType::Secret) {
        let v = match f.ftype {
            FieldType::Table => json!([]),
            FieldType::Check => json!(false),
            t if t.is_numeric() => json!(0),
            _ => json!(""),
        };
        doc.insert(f.name.clone(), v);
    }
    Value::Object(doc)
}

/// The template `key` as the workspace has it, and the package's default when there is one.
async fn template_of(tx: &mut Tx, ctx: &Ctx, key: &str) -> Result<(Option<MessageDef>, Option<(MessageDef, Option<String>)>)> {
    let default = ctx.schema.messages.iter().find(|m| m.key == key).cloned();
    let own: Option<(Value, Option<String>)> =
        sqlx::query_as("select def, base from message_template where tenant_id = $1 and key = $2").bind(ctx.tenant).bind(key).fetch_optional(&mut **tx).await?;
    let own = own.map(|(d, b)| serde_json::from_value(d).map(|d| (d, b)).map_err(Error::internal)).transpose()?;
    Ok((default, own))
}

/// The template a send uses: the workspace's copy, else the package's default.
async fn effective(tx: &mut Tx, ctx: &Ctx, key: &str) -> Result<MessageDef> {
    match template_of(tx, ctx, key).await? {
        (_, Some((own, _))) => Ok(own),
        (Some(default), None) => Ok(default),
        (None, None) => Err(Error::invalid(format!("there is no message template {key}"))),
    }
}

/// Template `key`'s in-app wording (`title`, `text`) rendered for `values` in `language` (else the workspace's, else
/// English), with no record: what a public page of the platform says (BD-23). Plain text, for the caller to escape.
pub(crate) async fn notice(tx: &mut Tx, ctx: &Ctx, key: &str, language: &str, values: &Value) -> Result<Option<(String, Option<String>)>> {
    let def = effective(tx, ctx, key).await?;
    let (_, b) = bodies(&def, Some(language), None);
    if b.inapp.is_none() {
        return Ok(None);
    }
    let said = render_channel(b, "inapp", &variables(Value::Null, json!({}), values.clone(), ""), false)?;
    Ok(Some((said["title"].as_str().unwrap_or_default().to_string(), said["text"].as_str().map(str::to_string).filter(|t| !t.is_empty()))))
}

/// What a template's variables are: `doc`, `settings`, `values`, `opt_out`.
fn variables(doc: Value, settings: Value, values: Value, opt_out: &str) -> Value {
    json!({"doc": doc, "settings": settings, "values": values, "opt_out": opt_out})
}

/// What is wrong with a template, said as a field error: its shape, its entity, its key (one a package owns must be
/// that package's message), and each body rendered against a record with every field, refusing an unknown one.
async fn check_template(k: &Kernel, tx: &mut Tx, ctx: &Ctx, def: &MessageDef) -> Result<()> {
    if let Some(p) = def.problem() {
        return Err(Error::field("template", p));
    }
    if let Some(e) = &def.entity
        && ctx.schema.entity(e).is_none()
    {
        return Err(Error::field("entity", format!("{e} is not an entity here")));
    }
    let owner = ctx.schema.packages.iter().find(|p| def.key.strip_prefix(p.as_str()).is_some_and(|r| r.starts_with('.')));
    if let Some(p) = owner
        && !ctx.schema.messages.iter().any(|m| m.key == def.key)
    {
        return Err(Error::field("key", format!("{p} sends no message {}; a workspace's own template has a key no package's name starts", def.key)));
    }
    let settings = crate::settings::load(k, ctx, tx, &settings_named(def)).await?;
    let values: Map<String, Value> = def.values.iter().map(|v| (v.clone(), json!(""))).collect();
    let vars = variables(sample_doc(ctx, def.entity.as_deref()), settings, Value::Object(values), "");
    let ent = def.entity.as_deref().and_then(|e| ctx.schema.entity(e));
    let has = |f: &str| match SAMPLE_KEYS.contains(&f) {
        true => Some(false),
        false => ent.and_then(|e| e.def.fields.iter().find(|d| d.name == f)).map(|d| d.ftype == FieldType::Secret),
    };
    for (lang, b) in &def.languages {
        for ch in ferrum_meta::MESSAGE_CHANNELS.iter().filter(|c| b.has(c)) {
            render_channel(b, ch, &vars, true).map_err(|e| {
                let text = serde_json::to_string(&channel_bodies(b, ch)).unwrap_or_default();
                Error::field(
                    "template",
                    format!(
                        "{lang}, {ch}: {}",
                        undefined_said(plain(&e), &text, &def.values, ent.map(|e| (e.def.label.as_str(), &has as &dyn Fn(&str) -> Option<bool>)))
                    ),
                )
            })?;
        }
    }
    Ok(())
}

/// What a template's check said when a body reads what is not there, in plain words where the body names it: a value
/// the template does not declare (`values.x`), or a field the record does not have (`doc.x`). `label` is the record's
/// entity's, `has` whether a name is one of its keys (and a secret field); anything else keeps what the engine said. The line the engine
/// named is kept (`(line 2)`).
pub(crate) fn undefined_said(said: String, text: &str, values: &[String], entity: Option<(&str, &dyn Fn(&str) -> Option<bool>)>) -> String {
    if !said.contains("undefined value") {
        return said;
    }
    let line = said.rsplit_once("<string>:").and_then(|(_, r)| r.split(')').next()).filter(|n| n.chars().all(|c| c.is_ascii_digit()));
    let at = line.map(|n| format!(" (line {n})")).unwrap_or_default();
    let names = |prefix: &str| -> Vec<String> {
        text.match_indices(prefix)
            .filter(|(i, _)| *i == 0 || !text[..*i].ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_' || c == '.'))
            .map(|(i, _)| text[i + prefix.len()..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect::<String>())
            .filter(|n| !n.is_empty())
            .collect()
    };
    if let Some(v) = names("values.").into_iter().find(|v| !values.contains(v)) {
        return format!("the body reads values.{v}, which is not among the values this template says the sender passes: add {v} to its values{at}");
    }
    if let Some((label, has)) = entity {
        for f in names("doc.") {
            match has(&f) {
                None => return format!("doc.{f} is not a field of {label}{at}"),
                Some(true) => return format!("doc.{f} is a secret field of {label}: no template prints one{at}"),
                Some(false) => {}
            }
        }
    }
    said
}

fn plain(e: &Error) -> String {
    match e {
        Error::Validation(fs) => fs.iter().map(|f| f.message.clone()).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

/// [`Kernel::save_message_template`] in the caller's transaction: `created`, `changed` or `unchanged`.
pub(crate) async fn save_template(k: &Kernel, tx: &mut Tx, ctx: &Ctx, def: &MessageDef) -> Result<&'static str> {
    check_template(k, tx, ctx, def).await?;
    let was: Option<Value> = sqlx::query_scalar("select def from message_template where tenant_id = $1 and key = $2")
        .bind(ctx.tenant)
        .bind(&def.key)
        .fetch_optional(&mut **tx)
        .await?;
    let now = serde_json::to_value(def).map_err(Error::internal)?;
    if was.as_ref() == Some(&now) {
        return Ok("unchanged");
    }
    let base = ctx.schema.messages.iter().find(|m| m.key == def.key).map(digest);
    sqlx::query(
        "insert into message_template (tenant_id, key, def, base, updated_by) values ($1, $2, $3, $4, $5) \
         on conflict (tenant_id, key) do update set def = excluded.def, base = excluded.base, updated_by = excluded.updated_by, updated_at = now()",
    )
    .bind(ctx.tenant)
    .bind(&def.key)
    .bind(now)
    .bind(&base)
    .bind((!ctx.user.is_nil()).then_some(ctx.user))
    .execute(&mut **tx)
    .await?;
    audit::record_admin(tx, ctx, "MessageTemplate", Uuid::nil(), "save", None, json!({"key": def.key})).await?;
    Ok(if was.is_some() { "changed" } else { "created" })
}

/// The workspace's own and edited templates, and its channel connections' descriptions (never a secret), for a pack.
pub(crate) async fn for_pack(tx: &mut Tx, tenant: Uuid) -> Result<(Vec<MessageDef>, Vec<PackChannel>)> {
    let defs: Vec<Value> = sqlx::query_scalar("select def from message_template where tenant_id = $1 order by key").bind(tenant).fetch_all(&mut **tx).await?;
    let channels: Vec<(String, String, Value)> = sqlx::query_as("select kind, name, description from message_channel where tenant_id = $1 order by kind, name")
        .bind(tenant)
        .fetch_all(&mut **tx)
        .await?;
    Ok((
        defs.into_iter().filter_map(|d| serde_json::from_value(d).ok()).collect(),
        channels.into_iter().filter_map(|(kind, name, d)| Some(PackChannel { kind, name, description: serde_json::from_value(d).ok()? })).collect(),
    ))
}

/// A channel connection as a pack carries it: no secret, and installed turned off, for an administrator to give it its
/// secrets and turn it on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackChannel {
    pub kind: String,
    pub name: String,
    pub description: ChannelDesc,
}

/// Installs a pack's channel, by name: one of the name here is left as it is.
pub(crate) async fn install_channel(k: &Kernel, tx: &mut Tx, ctx: &Ctx, c: &PackChannel) -> Result<&'static str> {
    let here: bool = sqlx::query_scalar("select exists (select 1 from message_channel where tenant_id = $1 and name = $2)")
        .bind(ctx.tenant)
        .bind(&c.name)
        .fetch_one(&mut **tx)
        .await?;
    if here {
        return Ok("unchanged");
    }
    if !matches!(c.kind.as_str(), "text" | "chat") {
        return Err(Error::field("kind", "text or chat"));
    }
    if let Some(p) = c.description.problem(k.settings.webhooks_allow_private) {
        return Err(Error::field("description", p));
    }
    sqlx::query("insert into message_channel (tenant_id, id, kind, name, description, enabled) values ($1, $2, $3, $4, $5, false)")
        .bind(ctx.tenant)
        .bind(Uuid::now_v7())
        .bind(&c.kind)
        .bind(c.name.trim())
        .bind(serde_json::to_value(&c.description).map_err(Error::internal)?)
        .execute(&mut **tx)
        .await?;
    Ok("created")
}

/// The bodies for `language` (or its base, `pt` for `pt-BR`), else the workspace's, else English, else the first.
fn bodies<'a>(def: &'a MessageDef, language: Option<&str>, workspace: Option<&str>) -> (&'a str, &'a MessageBodies) {
    let find = |l: &str| {
        let l = l.to_ascii_lowercase();
        let base = l.split('-').next().unwrap_or_default().to_string();
        def.languages.iter().find(|(k, _)| k.to_ascii_lowercase() == l).or_else(|| def.languages.iter().find(|(k, _)| k.to_ascii_lowercase() == base))
    };
    let (k, b) = language
        .and_then(find)
        .or_else(|| workspace.and_then(find))
        .or_else(|| find("en"))
        .or_else(|| def.languages.iter().next())
        .expect("a template has a language");
    (k.as_str(), b)
}

impl Kernel {
    /// Every template the workspace has: the packages' defaults and its own, each `{key, label, entity, category,
    /// channels, source}`, `source` `package`, `workspace` or `edited`; an edited one says `default_changed` when a release
    /// has changed the default since it was edited.
    pub async fn message_templates(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let own: Vec<(String, Value, Option<String>)> =
            sqlx::query_as("select key, def, base from message_template where tenant_id = $1").bind(ctx.tenant).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let mut out: BTreeMap<String, Value> = BTreeMap::new();
        let row = |d: &MessageDef, source: &str, changed: bool| {
            json!({"key": d.key, "label": d.label, "entity": d.entity, "category": d.category, "channels": d.order(), "languages": d.languages.keys().collect::<Vec<_>>(),
                   "source": source, "default_changed": changed})
        };
        for d in &ctx.schema.messages {
            out.insert(d.key.clone(), row(d, "package", false));
        }
        for (key, def, base) in own {
            let Ok(d) = serde_json::from_value::<MessageDef>(def) else { continue };
            let default = ctx.schema.messages.iter().find(|m| m.key == key);
            let changed = default.is_some_and(|m| base.as_deref() != Some(digest(m).as_str()));
            out.insert(key, row(&d, if default.is_some() { "edited" } else { "workspace" }, changed));
        }
        Ok(out.into_values().collect())
    }

    /// `{template, default, edited, default_changed}`: the template as sends use it, and the package's default.
    pub async fn message_template(&self, ctx: &Ctx, key: &str) -> Result<Value> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let (default, own) = template_of(&mut tx, ctx, key).await?;
        tx.commit().await?;
        let changed = matches!((&default, &own), (Some(d), Some((_, base))) if base.as_deref() != Some(digest(d).as_str()));
        let template = own.as_ref().map(|o| &o.0).or(default.as_ref()).ok_or_else(|| Error::NotFound(format!("message template {key}")))?;
        Ok(json!({"template": template, "default": default, "edited": own.is_some(), "default_changed": changed}))
    }

    /// Saves a workspace's template: its own, or its copy of a package's default (which then says which default it
    /// started from). Refused when [`check_template`] finds anything wrong. Audited.
    pub async fn save_message_template(&self, ctx: &Ctx, def: &MessageDef) -> Result<()> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        save_template(self, &mut tx, ctx, def).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Back to the package's default (a workspace's own template is removed). Audited.
    pub async fn reset_message_template(&self, ctx: &Ctx, key: &str) -> Result<()> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("delete from message_template where tenant_id = $1 and key = $2").bind(ctx.tenant).bind(key).execute(&mut *tx).await?;
        if n.rows_affected() == 0 {
            return Err(Error::NotFound(format!("an edited message template {key}")));
        }
        audit::record_admin(&mut tx, ctx, "MessageTemplate", Uuid::nil(), "reset", None, json!({"key": key})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// One channel's message as it would go, against a record: the one named (read with the caller's rights), else the
    /// latest the caller may read, else none. `template` is a key, or a template not saved yet (checked as a save
    /// checks it). Sends and writes nothing.
    pub async fn preview_message(&self, ctx: &Ctx, template: &Value, channel: &str, language: Option<&str>, record: Option<Uuid>) -> Result<Value> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let def = match template {
            Value::String(key) => effective(&mut tx, ctx, key).await?,
            v => {
                let def: MessageDef = serde_json::from_value(v.clone()).map_err(|e| Error::field("template", e.to_string()))?;
                check_template(self, &mut tx, ctx, &def).await?;
                def
            }
        };
        let found = match (&def.entity, record) {
            (Some(e), Some(id)) => Some(crate::store::doc_id(&self.get(ctx, e, id, false).await?)?),
            (Some(e), None) => {
                let q = crate::query::ListQuery { sort: Some("updated_at desc".into()), limit: Some(1), ..Default::default() };
                self.list(ctx, e, &q).await?.rows.first().map(crate::store::doc_id).transpose()?
            }
            (None, _) => None,
        };
        let doc = record_view(self, &mut tx, ctx, def.entity.as_deref(), found).await?;
        let settings = crate::settings::load(self, ctx, &mut tx, &settings_named(&def)).await?;
        let (_, workspace_language) = workspace(&mut tx, ctx.tenant).await?;
        tx.commit().await?;
        let (lang, b) = bodies(&def, language, workspace_language.as_deref());
        let values: Map<String, Value> = def.values.iter().map(|v| (v.clone(), json!(format!("[{v}]")))).collect();
        let rendered = render_channel(b, channel, &variables(doc, settings, Value::Object(values), ""), false)?;
        Ok(json!({"language": lang, "message": rendered, "record_id": found}))
    }
}

impl Kernel {
    /// Sends a template on one channel to the caller themselves (their account, or `phone` for a text or chat-app
    /// message), against a record of its entity when named: for trying it before people get it.
    pub async fn send_test_message(&self, ctx: &Ctx, template: &str, channel: &str, phone: Option<&str>, record: Option<Uuid>) -> Result<Vec<Sent>> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let def = effective(&mut tx, ctx, template).await?;
        if let (Some(e), Some(id)) = (&def.entity, record) {
            self.get_tx(ctx, &mut tx, e, id, false).await?;
        }
        let to = vec![Recipient { user: Some(ctx.user), phone: phone.map(str::to_string), ..Default::default() }];
        let m = Message { template: template.into(), entity: record.and(def.entity.clone()), record, to, channels: vec![channel.into()], ..Default::default() };
        let sent = send(self, &mut tx, ctx, &m).await?;
        tx.commit().await?;
        Ok(sent)
    }
}

// ── sending ──

/// Who a message goes to. A user is reached in the app, and on their account's address; anyone else on what is given.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipient {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    /// Their language, when known; otherwise the workspace's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Their time zone, when known (for quiet hours); otherwise the workspace's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
}

impl Recipient {
    pub fn user(id: Uuid) -> Recipient {
        Recipient { user: Some(id), ..Default::default() }
    }

    /// An address (`a@b`) or a number, as written.
    pub fn address(a: &str) -> Recipient {
        if a.contains('@') {
            Recipient { email: Some(a.trim().to_lowercase()), ..Default::default() }
        } else {
            Recipient { phone: Some(a.trim().into()), ..Default::default() }
        }
    }

    /// What an expression gave: an address, a number, or `{email, phone, language, time_zone, user}`.
    pub fn from_value(v: &Value) -> Result<Recipient> {
        match v {
            Value::String(s) if !s.trim().is_empty() => Ok(Recipient::address(s)),
            Value::Object(_) => serde_json::from_value(v.clone()).map_err(|e| Error::invalid(format!("a recipient: {e}"))),
            Value::Null => Ok(Recipient::default()),
            other => Err(Error::invalid(format!("a recipient is an address, a number or an object, not {other}"))),
        }
    }
}

/// A message to send ([`send`]).
#[derive(Debug, Clone, Default)]
pub struct Message {
    /// A template's key.
    pub template: String,
    /// The record it is about, when it is about one.
    pub entity: Option<String>,
    pub record: Option<Uuid>,
    pub to: Vec<Recipient>,
    /// The channels tried, in order; none: the template's.
    pub channels: Vec<String>,
    /// Every channel a recipient can be reached on, not the first.
    pub all: bool,
    /// What the template reads as `values`.
    pub values: Map<String, Value>,
    /// Stored files an e-mail attaches.
    pub attachments: Vec<Uuid>,
    /// Names of `values` that are secret (a gift card's code, a one-time code): rendered into what goes out, sealed
    /// while it waits, and a placeholder in what is kept once it has been handed to the transport. Never left in the app.
    pub secret: Vec<String>,
}

/// One send, as recorded.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Sent {
    pub id: Uuid,
    pub channel: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Sends `m` in the caller's transaction: nothing goes, and nothing is recorded, unless it commits. For each recipient
/// the channels are tried in order; the first they can be reached on is used (each one with `all`), and when none is,
/// each is recorded as skipped with why. An unknown template, or a record that is not the template's entity, is an
/// error; a body that fails to render is too (it was checked when saved).
pub async fn send(k: &Kernel, tx: &mut Tx, ctx: &Ctx, m: &Message) -> Result<Vec<Sent>> {
    // Boxed: callers deep in an action's or an approval's future need not carry its size.
    Box::pin(send_with(k, tx, ctx, m)).await
}

/// A message rendered for one recipient as [`send`] would send it, for an app that sends it itself (BD-45): its mail's
/// `subject` and `html`, its `text` body when the template has one, and, for a promotional message, the opt-out
/// address and the `List-Unsubscribe` and `List-Unsubscribe-Post` header values to send with it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Rendered {
    pub channel: String,
    pub address: String,
    pub language: String,
    pub subject: Option<String>,
    pub html: Option<String>,
    pub text: Option<String>,
    pub opt_out: Option<String>,
    pub list_unsubscribe: Option<(String, String)>,
}

/// Renders `m` for its one recipient on `channel` (`email` or `text`/`chat`) exactly as [`send`] would, and sends
/// nothing (BD-45). Refused, saying so, when the address opted out of a promotional message on that channel (an erased
/// person's opt-out kept as a digest included) or may not be contacted, when it has no such address, or when a
/// promotional message's opt-out link cannot be made (the server's public address or key not configured).
pub async fn render_for(k: &Kernel, tx: &mut Tx, ctx: &Ctx, m: &Message, channel: &str) -> Result<Rendered> {
    let [r] = m.to.as_slice() else { return Err(Error::invalid("a message is rendered for one recipient")) };
    let def = effective(tx, ctx, &m.template).await?;
    let promotional = def.category == MessageCategory::Promotional;
    let r = complete(tx, ctx.tenant, r).await?;
    let (cfg, workspace_language) = workspace(tx, ctx.tenant).await?;
    let address = match channel {
        "email" => r.email.clone().ok_or_else(|| Error::field("to", "no e-mail address"))?,
        "text" | "chat" => international(r.phone.as_deref().ok_or_else(|| Error::field("to", "no phone number"))?, &cfg).map_err(|m| Error::field("to", m))?,
        other => return Err(Error::invalid(format!("{other} is not a channel an app sends on itself"))),
    };
    if channel == "email" {
        let purpose = if promotional { crate::retention::Purpose::Marketing } else { crate::retention::Purpose::Transactional };
        if !k.may_contact_for(tx, ctx.tenant, &address, purpose).await? {
            return Err(Error::Forbidden(if promotional { "opted out" } else { "processing of their data is restricted" }.into()));
        }
    } else if promotional && k.opted_out(tx, ctx.tenant, channel, &address).await? {
        return Err(Error::Forbidden("opted out".into()));
    }
    let opt_out = match (promotional, k.settings.secret_key.as_ref()) {
        (false, _) => None,
        (true, Some(key)) => {
            k.person_link(ctx.tenant, Outside, &format!("/api/messages/opt-out?token={}", opt_out_token(key, ctx.tenant, channel, &address))).await
        }
        (true, None) => None,
    };
    if promotional && opt_out.is_none() {
        return Err(Error::Unavailable("a promotional message needs the server's public address and key for its opt-out link".into()));
    }
    let doc = record_view(k, tx, ctx, m.entity.as_deref(), m.record).await?;
    let settings = crate::settings::load(k, ctx, tx, &settings_named(&def)).await?;
    let (language, b) = bodies(&def, r.language.as_deref(), workspace_language.as_deref());
    let vars = variables(doc, settings, Value::Object(m.values.clone()), opt_out.as_deref().unwrap_or_default());
    let (subject, html) = match (channel, b.email.is_some()) {
        ("email", true) => {
            let mail = render_channel(b, "email", &vars, false)?;
            (mail["subject"].as_str().map(str::to_string), mail["html"].as_str().map(str::to_string))
        }
        ("email", false) => return Err(Error::field("channel", "the template has no e-mail body in this language")),
        _ => (None, None),
    };
    let text = if b.has("text") { render_channel(b, "text", &vars, false)?["text"].as_str().map(str::to_string) } else { None };
    Ok(Rendered {
        channel: channel.into(),
        address,
        language: language.into(),
        subject,
        html,
        text,
        list_unsubscribe: opt_out.as_ref().map(|u| (format!("<{u}>"), "List-Unsubscribe=One-Click".into())),
        opt_out,
    })
}

/// One recipient, completed: a user's address, language and zone from their account.
async fn complete(tx: &mut Tx, tenant: Uuid, r: &Recipient) -> Result<Recipient> {
    let mut r = r.clone();
    if let Some(u) = r.user {
        let (email, language, zone): (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
            "select u.email::text, (select value->>'language' from ui_pref where tenant_id = $1 and user_id = $2 and scope = 'locale'), \
                    (select value->>'zone' from ui_pref where tenant_id = $1 and user_id = $2 and scope = 'timezone') \
             from app_user u where u.tenant_id = $1 and u.id = $2 and u.is_active",
        )
        .bind(tenant)
        .bind(u)
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or((None, None, None));
        if email.is_none() {
            r.user = None;
        }
        r.email = r.email.or(email);
        r.language = r.language.or(language);
        r.time_zone = r.time_zone.or(zone);
    }
    r.email = r.email.map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty());
    r.phone = r.phone.filter(|p| !p.trim().is_empty());
    // Someone named by an address that is a person's here is that person, in the app too.
    if let (None, Some(e)) = (r.user, &r.email) {
        let found: Option<Uuid> = sqlx::query_scalar("select id from app_user where tenant_id = $1 and lower(email::text) = $2 and is_active")
            .bind(tenant)
            .bind(e)
            .fetch_optional(&mut **tx)
            .await?;
        if let Some(u) = found {
            return Box::pin(complete(tx, tenant, &Recipient { user: Some(u), ..r })).await;
        }
    }
    Ok(r)
}

/// Whether `address` opted out of promotional messages on `channel`, before an erasure (`erased`: its marks) or since.
async fn opted_out(tx: &mut Tx, tenant: Uuid, address: &str, channel: &str, erased: &[String]) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "select exists (select 1 from contact_consent where tenant_id = $1 and purpose = 'marketing' and channel = $3 and not granted \
           and (lower(email::text) = lower($2) or email::text = any($4)))",
    )
    .bind(tenant)
    .bind(address)
    .bind(channel)
    .bind(erased)
    .fetch_one(&mut **tx)
    .await?)
}

/// What an erased opt-out keeps in place of its address: [`erased_mark`].
pub(crate) const ERASED: &str = "erased:";

/// A keyed digest of the workspace, channel and address (normalised: an e-mail address in lower case, a number in
/// international form), so an erased opt-out still stops a promotional message to that address on that channel
/// while nothing shows the address, and trying addresses without the server's key finds nothing (BD-20).
fn erased_mark(key: &crate::secret::SecretKey, tenant: Uuid, channel: &str, address: &str) -> String {
    format!("{ERASED}{}", key.fingerprint("erased opt-out", &format!("{tenant}|{channel}|{}", address.trim().to_lowercase())))
}

impl Kernel {
    /// Whether a promotional message may not go to `address` on `channel` (BD-45): the address opted out (an erased
    /// person's opt-out, kept as a digest, included), or, for e-mail, its processing is restricted. What
    /// [`render_for`] and [`send`] ask; an app sending through another transport asks it too.
    pub async fn opted_out(&self, tx: &mut Tx, tenant: Uuid, channel: &str, address: &str) -> Result<bool> {
        if channel == "email" {
            return Ok(!self.may_contact_for(tx, tenant, address, crate::retention::Purpose::Marketing).await?);
        }
        opted_out(tx, tenant, address, channel, &self.erased_marks(tenant, channel, address)).await
    }

    /// The marks an erased opt-out of `address` on `channel` may carry: under the server's key and those being rotated out.
    pub(crate) fn erased_marks(&self, tenant: Uuid, channel: &str, address: &str) -> Vec<String> {
        self.secret_keys().map(|k| erased_mark(k, tenant, channel, address)).collect()
    }
}

/// The enabled channel connection for `kind`.
async fn connection(tx: &mut Tx, tenant: Uuid, kind: &str) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar("select id from message_channel where tenant_id = $1 and kind = $2 and enabled")
        .bind(tenant)
        .bind(kind)
        .fetch_optional(&mut **tx)
        .await?)
}

#[allow(clippy::too_many_arguments)]
async fn record(
    tx: &mut Tx,
    ctx: &Ctx,
    m: &Message,
    channel: &str,
    to: (Option<&str>, Option<Uuid>),
    language: &str,
    state: &str,
    reason: Option<&str>,
    pending: Option<Value>,
    channel_id: Option<Uuid>,
) -> Result<Sent> {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into message_send (tenant_id, id, template, channel, channel_id, recipient, user_id, entity, record_id, language, state, reason, pending) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
    )
    .bind(ctx.tenant)
    .bind(id)
    .bind(&m.template)
    .bind(channel)
    .bind(channel_id)
    .bind(to.0)
    .bind(to.1)
    .bind(&m.entity)
    .bind(m.record)
    .bind(language)
    .bind(state)
    .bind(reason)
    .bind(pending)
    .execute(&mut **tx)
    .await?;
    Ok(Sent { id, channel: channel.into(), state: state.into(), reason: reason.map(str::to_string) })
}

/// What the outbox and the delivery log hold of a message with secret values: sealed under this label while it waits.
fn sealed_message(tenant: Uuid) -> String {
    format!("{tenant}/message")
}

/// The text of `m`'s secret values, and the server's key that seals them while they wait.
fn secret_values<'a>(k: &'a Kernel, m: &Message) -> Result<(Vec<String>, Option<&'a crate::secret::SecretKey>)> {
    let masks: Vec<String> = m
        .secret
        .iter()
        .filter_map(|n| m.values.get(n))
        .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
        .filter(|v| !v.is_empty() && v != "null")
        .collect();
    if masks.is_empty() {
        return Ok((masks, None));
    }
    let masks = encodings(&masks);
    let key = k.settings.secret_key.as_ref().ok_or_else(|| Error::Unavailable("a message with a secret value needs the server's secret key".into()))?;
    Ok((masks, Some(key)))
}

/// Every form each secret value can be printed in (BD-44): as it is, HTML-escaped (an attribute's or a link's
/// address in an e-mail), percent-encoded (an address a value was put into elsewhere, a channel's request), as JSON,
/// and what the `qr` and `barcode` helpers draw of it (as their defaults draw it), in both the e-mail's escaping
/// environment and the plain one; longest first, so a drawing goes before the text inside it. Masking all of these
/// wherever rendered text is kept or logged leaves none of them readable.
pub(crate) fn encodings(secrets: &[String]) -> Vec<String> {
    const FORMS: [&str; 4] = ["{{ v }}", "{{ v|json }}", "{{ qr(v) }}", "{{ barcode(v) }}"];
    let percent = |v: &str, upper: bool| -> String {
        v.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                _ if upper => format!("%{b:02X}"),
                _ => format!("%{b:02x}"),
            })
            .collect()
    };
    let mut out: Vec<String> = secrets.to_vec();
    for v in secrets {
        out.extend([percent(v, true), percent(v, false), percent(v, true).replace("%20", "+")]);
    }
    for html in [true, false] {
        let env = environment(html, false);
        for v in secrets {
            for form in FORMS {
                if let Ok(r) = env.render_str(form, minijinja::context! { v => v }) {
                    out.push(r.trim_matches('"').to_string());
                    out.push(r);
                }
            }
        }
    }
    out.retain(|m| m.chars().count() >= 4 || secrets.contains(m));
    out.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    out.dedup();
    out
}

async fn send_with(k: &Kernel, tx: &mut Tx, ctx: &Ctx, m: &Message) -> Result<Vec<Sent>> {
    let (masks, key) = secret_values(k, m)?;
    let def = effective(tx, ctx, &m.template).await?;
    if let Some(e) = &def.entity
        && m.entity.as_ref().is_some_and(|me| me != e)
    {
        return Err(Error::invalid(format!("message {} is about {e}, not {}", def.key, m.entity.as_deref().unwrap_or_default())));
    }
    if let Some(c) = m.channels.iter().find(|c| !ferrum_meta::MESSAGE_CHANNELS.contains(&c.as_str())) {
        return Err(Error::invalid(format!("{c} is not a channel")));
    }
    let promotional = def.category == MessageCategory::Promotional;
    let doc = record_view(k, tx, ctx, m.entity.as_deref(), m.record).await?;
    let settings = crate::settings::load(k, ctx, tx, &settings_named(&def)).await?;
    let (cfg, workspace_language) = workspace(tx, ctx.tenant).await?;
    let mut out = Vec::new();
    let mut seen = Vec::new();
    for r in &m.to {
        let r = complete(tx, ctx.tenant, r).await?;
        if r == Recipient::default() || seen.contains(&r) {
            continue;
        }
        seen.push(r.clone());
        let (language, b) = bodies(&def, r.language.as_deref(), workspace_language.as_deref());
        let order = if m.channels.is_empty() { def.order() } else { m.channels.clone() };
        let (mut used, mut skipped) = (false, Vec::new());
        for ch in order.iter().filter(|c| b.has(c)) {
            // Where it goes on this channel, or why it cannot.
            let reach: std::result::Result<(Option<String>, Option<Uuid>), String> = match ch.as_str() {
                "inapp" if !masks.is_empty() => Err("a secret value is not left in the app".to_string()),
                "inapp" => r.user.map(|u| (None, Some(u))).ok_or_else(|| "no user to tell in the app".to_string()),
                "email" => match &r.email {
                    None => Err("no e-mail address".into()),
                    Some(e) => {
                        let purpose = if promotional { crate::retention::Purpose::Marketing } else { crate::retention::Purpose::Transactional };
                        if k.may_contact_for(tx, ctx.tenant, e, purpose).await? {
                            Ok((Some(e.clone()), r.user))
                        } else {
                            Err(if promotional { "opted out" } else { "processing of their data is restricted" }.into())
                        }
                    }
                },
                _ => match r.phone.as_deref().map(|p| international(p, &cfg)) {
                    None => Err("no phone number".into()),
                    Some(Err(why)) => Err(why),
                    Some(Ok(n)) => Ok((Some(n), r.user)),
                },
            };
            let (address, user) = match reach {
                Ok(a) => a,
                Err(why) => {
                    skipped.push((ch.clone(), why, r.email.clone().or(r.phone.clone()), r.user));
                    continue;
                }
            };
            let opt_out = match (&address, promotional, k.settings.secret_key.as_ref()) {
                (Some(a), true, Some(key)) => k
                    .person_link(ctx.tenant, Outside, &format!("/api/messages/opt-out?token={}", opt_out_token(key, ctx.tenant, ch, a)))
                    .await
                    .unwrap_or_default(),
                _ => String::new(),
            };
            let vars = variables(doc.clone(), settings.clone(), Value::Object(m.values.clone()), &opt_out);
            let message = render_channel(b, ch, &vars, false)?;
            let to = (address.as_deref(), user);
            let sent = match ch.as_str() {
                "inapp" => {
                    let u = user.unwrap_or_default();
                    let title = message["title"].as_str().unwrap_or_default();
                    let told =
                        crate::notify::send_kind(tx, ctx.tenant, u, "general", "inapp", title, message["text"].as_str(), m.entity.as_deref(), m.record).await?;
                    let (state, why) = if told.is_some() { ("delivered", None) } else { ("skipped", Some("they turned these notices off")) };
                    record(tx, ctx, m, ch, to, language, state, why, None, None).await?
                }
                "email" => {
                    let to_address = address.clone().unwrap_or_default();
                    crate::outbox::check_mail(&to_address, message["subject"].as_str().unwrap_or_default())?;
                    let sent = record(tx, ctx, m, ch, to, language, "queued", None, None, None).await?;
                    let purpose = if promotional { crate::retention::Purpose::Marketing } else { crate::retention::Purpose::Transactional };
                    k.queue_mail_send(
                        tx,
                        ctx.tenant,
                        &to_address,
                        message["subject"].as_str().unwrap_or_default(),
                        message["html"].as_str().unwrap_or_default(),
                        &m.attachments,
                        purpose,
                        Some(sent.id),
                        &masks,
                    )
                    .await?;
                    sent
                }
                kind => {
                    let address = address.clone().unwrap_or_default();
                    let channel_id = connection(tx, ctx.tenant, kind).await?;
                    let refused = if channel_id.is_none() {
                        Some("no channel configured".to_string())
                    } else if promotional && opted_out(tx, ctx.tenant, &address, kind, &k.erased_marks(ctx.tenant, kind, &address)).await? {
                        Some("opted out".into())
                    } else {
                        over_limit(tx, ctx.tenant, &address, &cfg).await?
                    };
                    if let Some(why) = refused {
                        skipped.push((ch.clone(), why, Some(address), user));
                        continue;
                    }
                    let zone = r.time_zone.clone().unwrap_or_else(|| ctx.schema.timezone.clone());
                    let held = promotional && quiet_for(cfg.quiet_hours.as_ref(), &zone, Utc::now()).is_some();
                    let mut pending = json!({"message": message, "to": address, "promotional": promotional, "zone": zone, "language": language});
                    if let Some(key) = key {
                        let plain = json!({"message": message, "mask": masks}).to_string();
                        pending["message"] = Value::Null;
                        pending["sealed"] = json!(key.seal(&sealed_message(ctx.tenant), &plain)?);
                    }
                    let sent = record(tx, ctx, m, ch, to, language, if held { "held" } else { "queued" }, None, Some(pending), channel_id).await?;
                    let mut job = NewJob::new(DELIVER_KIND, json!({"send": sent.id}));
                    job.max_attempts = DELIVERY_ATTEMPTS;
                    job.priority = if promotional { crate::outbox::PRIORITY_BULK } else { crate::outbox::PRIORITY_URGENT };
                    crate::jobs::enqueue(tx, ctx.tenant, None, job).await?;
                    sent
                }
            };
            out.push(sent);
            used = true;
            if !m.all {
                break;
            }
        }
        if !used || m.all {
            for (ch, why, address, user) in skipped {
                out.push(record(tx, ctx, m, &ch, (address.as_deref(), user), language, "skipped", Some(&why), None, None).await?);
            }
        }
    }
    Ok(out)
}

/// Why one more text or chat-app message would pass a limit: the workspace's an hour, or the recipient's. Sends wait for
/// one another in a workspace while they count.
async fn over_limit(tx: &mut Tx, tenant: Uuid, address: &str, s: &Settings) -> Result<Option<String>> {
    sqlx::query("select pg_advisory_xact_lock(hashtextextended('ferrum.messages ' || $1::text, 0))").bind(tenant).execute(&mut **tx).await?;
    let (all, theirs): (i64, i64) = sqlx::query_as(
        "select count(*), count(*) filter (where recipient = $2) from message_send \
         where tenant_id = $1 and channel in ('text', 'chat') and state <> 'skipped' and created_at > now() - interval '1 hour'",
    )
    .bind(tenant)
    .bind(address)
    .fetch_one(&mut **tx)
    .await?;
    Ok(if all >= s.per_hour.unwrap_or(PER_HOUR) {
        Some("the workspace's hourly limit of text and chat-app messages".into())
    } else if theirs >= s.per_recipient_hour.unwrap_or(PER_RECIPIENT_HOUR) {
        Some("this recipient's hourly limit".into())
    } else {
        None
    })
}

/// Who a recipient spec names, as the `message` effect writes them: `owner`, `user:<id>`, `role:<name>`, or
/// `field:<name>`, a field of the record holding an address or a number, or a link to a record whose own address and
/// number fields (by type) are used (a customer, a contact, an employee); or `links:<link>.<table>.<field>`, that field
/// of each line of a table of the record a link field names (the members of an assignment group), read as the
/// workspace as `links` is (BD-14), at most [`crate::automation::MAX_ITEMS`] lines. An expression is the caller's to
/// evaluate ([`Recipient::from_value`]).
pub async fn recipients(k: &Kernel, tx: &mut Tx, ctx: &Ctx, entity: &str, doc: &Value, spec: &str) -> Result<Vec<Recipient>> {
    let users = |ids: Vec<Uuid>| ids.into_iter().map(Recipient::user).collect::<Vec<_>>();
    if spec == "owner" {
        return Ok(users(doc.get("created_by").and_then(Value::as_str).and_then(|s| s.parse().ok()).into_iter().collect()));
    }
    if let Some(u) = spec.strip_prefix("user:") {
        return Ok(users(vec![u.parse().map_err(|_| Error::invalid("message: bad user id"))?]));
    }
    if let Some(role) = spec.strip_prefix("role:") {
        let ids = sqlx::query_scalar("select user_id from user_role where tenant_id = $1 and role = $2 and (expires_at is null or expires_at > now())")
            .bind(ctx.tenant)
            .bind(role)
            .fetch_all(&mut **tx)
            .await?;
        return Ok(users(ids));
    }
    let ent = ctx.schema.entity(entity).cloned().ok_or_else(|| Error::invalid(format!("unknown entity {entity}")))?;
    if let Some(path) = spec.strip_prefix("links:") {
        let (link, table, field) = lines_path(path).ok_or_else(|| Error::invalid(format!("{spec} is links:<link>.<table>.<field>")))?;
        if let Some(m) = lines_problem(&ctx.schema, &ent, path) {
            return Err(Error::invalid(m));
        }
        let names = std::collections::BTreeSet::from([link.to_string()]);
        let linked = crate::links::load(k, ctx, tx, &ent, doc, &names).await?;
        let rows = linked[link][table].as_array().cloned().unwrap_or_default();
        if rows.len() > crate::automation::MAX_ITEMS {
            return Err(Error::invalid(format!(
                "{spec}: {} lines, more than the {} one message goes to; nothing was sent",
                rows.len(),
                crate::automation::MAX_ITEMS
            )));
        }
        let lent = ent.field(link).and_then(|f| f.to.as_deref()).and_then(|t| ctx.schema.entity(t)).cloned();
        let cent = lent.and_then(|l| l.field(table).and_then(|f| f.of.clone())).and_then(|c| ctx.schema.entity(&c).cloned());
        let cent = cent.ok_or_else(|| Error::invalid(format!("{spec}: its lines are not installed")))?;
        let mut out = Vec::new();
        for r in &rows {
            out.extend(of_field(k, tx, ctx, &cent, r, field).await?);
        }
        return Ok(out);
    }
    let Some(field) = spec.strip_prefix("field:") else {
        return Err(Error::invalid(format!(
            "message.to '{spec}' is owner, user:<id>, role:<name>, field:<name>, links:<link>.<table>.<field> or expr:<expression>"
        )));
    };
    of_field(k, tx, ctx, &ent, doc, field).await
}

/// `<link>.<table>.<field>`, split.
pub(crate) fn lines_path(path: &str) -> Option<(&str, &str, &str)> {
    let mut p = path.split('.');
    let (Some(l), Some(t), Some(f), None) = (p.next(), p.next(), p.next(), p.next()) else { return None };
    [l, t, f].iter().all(|s| !s.trim().is_empty()).then_some((l, t, f))
}

/// What is wrong with `<link>.<table>.<field>` on `ent` (`<link>.<table>` for a loop over the lines): a link field
/// of it, a table of the record it names, a field of that table's lines holding an address, a number or a link.
pub(crate) fn lines_problem(schema: &ferrum_meta::Schema, ent: &ferrum_meta::EffEntity, path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('.').collect();
    let (link, table, field) = match parts[..] {
        [l, t] => (l, t, None),
        [l, t, f] => (l, t, Some(f)),
        _ => return Some(format!("links:{path} is links:<link>.<table>.<field>")),
    };
    let Some(to) = ent.field(link).filter(|f| f.ftype == FieldType::Link).and_then(|f| f.to.as_deref()) else {
        return Some(format!("links:{path}: {link} is not a link field of {}", ent.def.label));
    };
    let Some(lent) = schema.entity(to) else { return Some(format!("links:{path}: {link} links to {to}, which is not installed")) };
    let Some(cent) = lent.field(table).filter(|f| f.ftype == FieldType::Table).and_then(|f| f.of.as_deref()).and_then(|c| schema.entity(c)) else {
        return Some(format!("links:{path}: {table} is not a table of lines of {}", lent.def.label));
    };
    match field.map(|f| (f, cent.field(f).map(|d| d.ftype))) {
        None | Some((_, Some(FieldType::Email | FieldType::Phone | FieldType::Data | FieldType::Link))) => None,
        Some((f, Some(_))) => Some(format!("links:{path}: {f} holds no address, number or link")),
        Some((f, None)) => Some(format!("links:{path}: the lines of {table} have no field {f}")),
    }
}

/// Who `field` of `doc` (a record or a line of `ent`) names: an address or a number it holds, or the record it links
/// to's own address and number (by type).
async fn of_field(k: &Kernel, tx: &mut Tx, ctx: &Ctx, ent: &ferrum_meta::EffEntity, doc: &Value, field: &str) -> Result<Vec<Recipient>> {
    let f = ent.field(field).ok_or_else(|| Error::invalid(format!("message.to: {} has no field {field}", ent.def.name)))?;
    let Some(v) = doc.get(field).and_then(Value::as_str).filter(|v| !v.trim().is_empty()) else { return Ok(vec![]) };
    match (f.ftype, f.to.as_deref()) {
        (FieldType::Link, Some(target)) => {
            let sys = Ctx::system(ctx.tenant, ctx.schema.clone());
            let Ok(id) = v.parse::<Uuid>() else { return Ok(vec![]) };
            let linked = match k.get_tx(&sys, tx, target, id, false).await {
                Ok(d) => d,
                Err(Error::NotFound(_)) => return Ok(vec![]),
                Err(e) => return Err(e),
            };
            let tent = k.entity(&sys, target)?;
            let first =
                |t: FieldType| tent.def.fields.iter().filter(|f| f.ftype == t).find_map(|f| linked.get(&f.name).and_then(Value::as_str).map(str::to_string));
            Ok(vec![Recipient { email: first(FieldType::Email), phone: first(FieldType::Phone), ..Default::default() }])
        }
        (FieldType::Phone, _) => Ok(vec![Recipient { phone: Some(v.into()), ..Default::default() }]),
        _ => Ok(vec![Recipient::address(v)]),
    }
}

/// The people of the app `to` names, each once: those of an address (a user's) among them.
pub(crate) async fn users_of(tx: &mut Tx, tenant: Uuid, to: &[Recipient]) -> Result<Vec<Uuid>> {
    let mut out = Vec::new();
    for r in to {
        if let Some(u) = complete(tx, tenant, r).await?.user
            && !out.contains(&u)
        {
            out.push(u);
        }
    }
    Ok(out)
}

// ── delivery ──

/// How a channel connection's provider is called and answers (no provider is named in code): `url`, `method`,
/// `headers` and a `body` (JSON) or `form`, each a template over `to`, `text`, `template_name`, `values`, `language`,
/// `from` and `secret` (the connection's sealed values); where the provider's id and error are in its answer (JSON
/// pointers); which statuses mean "later". `test: true` delivers into the log only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelDesc {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub test: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub form: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporary: Option<Vec<u16>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbound: Option<Inbound>,
}

/// What the provider calls back with (`POST /api/messages/inbound/{channel}`): authenticated by `auth` `hmac` (the
/// body's HMAC-SHA256 in hex in `header`, `sha256=` before it or not) or `token` (`header`, or the `token` query
/// parameter, holding the secret), the secret being the connection's sealed value `secret`. A delivery report gives the
/// provider's id at `id_at` and its status at `status_at`, mapped by `statuses` to `sent`, `delivered` or `failed`; a
/// reply gives the number at `from_at` and the words at `text_at`, `stop` words opting the number out and `start` words in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inbound {
    pub auth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_at: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub statuses: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub start: Vec<String>,
}

impl ChannelDesc {
    /// What is wrong with it: a test channel, or an `https` address (plain `http` too where private addresses are
    /// allowed) with templates that compile and an inbound part that says how it is authenticated.
    fn problem(&self, allow_private: bool) -> Option<String> {
        if !self.test {
            if !(self.url.starts_with("https://") || (allow_private && self.url.starts_with("http://"))) {
                return Some("url must be an https address".into());
            }
            if self.method.as_deref().is_some_and(|m| !["POST", "PUT", "GET"].contains(&m)) {
                return Some("method is POST, PUT or GET".into());
            }
            if self.body.is_some() && !self.form.is_empty() {
                return Some("a request has a body or a form, not both".into());
            }
            let env = environment(false, false);
            for t in std::iter::once(&self.url).chain(self.headers.values()).chain(self.body.iter()).chain(self.form.values()) {
                if let Err(e) = env.template_from_str(t) {
                    return Some(format!("'{t}': {e}"));
                }
            }
        }
        for p in [&self.id_at, &self.error_at].into_iter().flatten() {
            if !p.starts_with('/') {
                return Some(format!("{p} is not a JSON pointer (/a/0/b)"));
            }
        }
        if let Some(i) = &self.inbound {
            if !matches!(i.auth.as_str(), "hmac" | "token") || (i.auth == "hmac" && i.header.is_none()) {
                return Some("inbound.auth is hmac (with a header) or token".into());
            }
            if i.statuses.values().any(|s| !["sent", "delivered", "failed"].contains(&s.as_str())) {
                return Some("inbound.statuses map a provider's status to sent, delivered or failed".into());
            }
        }
        None
    }
}

pub(crate) fn sealed_label(tenant: Uuid, id: Uuid) -> String {
    format!("message_channel:{tenant}:{id}")
}

/// What `POST /api/admin/messages/channels` takes. `secrets` are sealed and never read back; left out, the ones there
/// are stay.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelInput {
    pub kind: String,
    pub name: String,
    pub description: ChannelDesc,
    #[serde(default)]
    pub secrets: Option<BTreeMap<String, String>>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

/// Deliveries the test channel made, read by tests (codes in the clear, never stored): `(tenant, to, message)`.
static TEST_SINK: std::sync::Mutex<Vec<(Uuid, String, Value)>> = std::sync::Mutex::new(Vec::new());

#[doc(hidden)]
pub fn test_deliveries(tenant: Uuid) -> Vec<(String, Value)> {
    TEST_SINK.lock().unwrap_or_else(|p| p.into_inner()).iter().filter(|d| d.0 == tenant).map(|d| (d.1.clone(), d.2.clone())).collect()
}

/// `{a}` with each of `mask` written as dots.
pub(crate) fn masked(v: &Value, mask: &[String]) -> Value {
    match v {
        Value::String(s) => Value::String(mask.iter().filter(|m| !m.is_empty()).fold(s.clone(), |s, m| s.replace(m.as_str(), "••••••"))),
        Value::Array(a) => Value::Array(a.iter().map(|x| masked(x, mask)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), masked(x, mask))).collect()),
        _ => v.clone(),
    }
}

impl Kernel {
    /// What a message with secret values holds sealed (`{message, mask}`, or a mail's `{subject, html, mask}`), opened.
    pub(crate) fn open_message(&self, tenant: Uuid, sealed: &Value) -> Result<(Value, Vec<String>)> {
        let open: Value = serde_json::from_str(&self.open_sealed(&sealed_message(tenant), sealed.as_str().unwrap_or_default())?).map_err(Error::internal)?;
        let mask = open["mask"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
        Ok((open, mask))
    }

    /// `plain` sealed as a message with secret values is.
    pub(crate) fn seal_message(&self, tenant: Uuid, plain: &Value) -> Result<String> {
        let key = self.settings.secret_key.as_ref().ok_or_else(|| Error::Unavailable("a message with a secret value needs the server's secret key".into()))?;
        key.seal(&sealed_message(tenant), &plain.to_string())
    }
}

struct Deliver;

#[async_trait]
impl JobHandler for Deliver {
    fn kind(&self) -> &'static str {
        DELIVER_KIND
    }

    fn title(&self) -> &'static str {
        "Message delivery"
    }

    async fn run(&self, k: &Kernel, job: &Job) -> Result<Value> {
        k.deliver_message(job).await
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(60)
    }
}

pub(crate) fn handler(kind: &str) -> Option<Arc<dyn JobHandler>> {
    (kind == DELIVER_KIND).then(|| Arc::new(Deliver) as Arc<dyn JobHandler>)
}

impl Kernel {
    async fn settle(&self, tenant: Uuid, id: Uuid, state: &str, provider: Option<&str>, error: Option<&str>, content: Option<Value>) -> Result<()> {
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        sqlx::query(
            "update message_send set state = $3, provider_id = coalesce($4, provider_id), error = $5, content = coalesce($6, content), pending = null, updated_at = now() \
             where tenant_id = $1 and id = $2",
        )
        .bind(tenant)
        .bind(id)
        .bind(state)
        .bind(provider)
        .bind(error.map(|e| e.chars().take(1000).collect::<String>()))
        .bind(content)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// One text or chat-app message to its channel's provider ([`DELIVER_KIND`]).
    async fn deliver_message(&self, job: &Job) -> Result<Value> {
        let id: Uuid = job.payload["send"].as_str().and_then(|s| s.parse().ok()).ok_or_else(|| Error::invalid("names its send"))?;
        let tenant = job.tenant_id;
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let row: Option<(Option<Value>, Option<Uuid>, String)> =
            sqlx::query_as("select pending, channel_id, channel from message_send where tenant_id = $1 and id = $2 and state in ('queued', 'held') for update")
                .bind(tenant)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((Some(pending), channel_id, kind)) = row else { return Ok(json!({"done": "already"})) };
        let (cfg, _) = workspace(&mut tx, tenant).await?;
        if pending["promotional"] == json!(true)
            && let Some(secs) = quiet_for(cfg.quiet_hours.as_ref(), pending["zone"].as_str().unwrap_or("UTC"), Utc::now())
        {
            sqlx::query("update message_send set state = 'held', updated_at = now() where tenant_id = $1 and id = $2")
                .bind(tenant)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Err(Error::Wait("quiet hours".into(), secs));
        }
        let channel: Option<(Value, Option<String>)> =
            sqlx::query_as("select description, sealed from message_channel where tenant_id = $1 and id = $2 and enabled and kind = $3")
                .bind(tenant)
                .bind(channel_id)
                .bind(&kind)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        let Some((desc, sealed)) = channel else {
            self.settle(tenant, id, "failed", None, Some("the channel was removed or turned off"), None).await?;
            return Ok(json!({"failed": "no channel"}));
        };
        let desc: ChannelDesc = serde_json::from_value(desc).map_err(Error::internal)?;
        let (message, mask) = match pending.get("sealed") {
            Some(sealed) => {
                let (open, mask) = self.open_message(tenant, sealed)?;
                (open["message"].clone(), mask)
            }
            None => (pending["message"].clone(), vec![]),
        };
        let message = &message;
        let to = pending["to"].as_str().unwrap_or_default();
        if desc.test {
            let content = masked(message, &mask);
            {
                let mut sink = TEST_SINK.lock().unwrap_or_else(|p| p.into_inner());
                if sink.len() >= 500 {
                    sink.remove(0);
                }
                sink.push((tenant, to.to_string(), message.clone()));
            }
            self.settle(tenant, id, "delivered", Some(&format!("test-{id}")), None, Some(json!({"to": to, "message": content}))).await?;
            return Ok(json!({"delivered": "test"}));
        }
        let secrets: Value = match sealed {
            Some(s) => serde_json::from_str(&self.open_sealed(&sealed_label(tenant, channel_id.unwrap_or_default()), &s)?).map_err(Error::internal)?,
            None => json!({}),
        };
        let vars = json!({"to": to, "text": message["text"], "template_name": message["template_name"], "values": message["values"],
                          "from": desc.from, "secret": secrets, "language": pending["language"]});
        let r = |s: &str| render(s, false, &vars, false);
        // The described call goes through the connections' guard (BD-29): the address checked hop by hop, a time and size limit.
        let outcome = async {
            let url = reqwest::Url::parse(&r(&desc.url)?).map_err(|_| Error::invalid("the channel's address is not an address"))?;
            let method = reqwest::Method::from_bytes(desc.method.as_deref().unwrap_or("POST").as_bytes()).map_err(Error::internal)?;
            let headers = desc.headers.iter().map(|(h, v)| Ok((h.clone(), r(v)?))).collect::<Result<Vec<_>>>()?;
            let body = if let Some(b) = &desc.body {
                let body: Value = serde_json::from_str(&r(b)?).map_err(|e| Error::invalid(format!("the channel's body is not JSON: {e}")))?;
                Some((body.to_string().into_bytes(), "application/json".to_string()))
            } else if !desc.form.is_empty() {
                let mut f = reqwest::Url::parse("http://form/").map_err(Error::internal)?;
                for (k, v) in &desc.form {
                    f.query_pairs_mut().append_pair(k, &r(v)?);
                }
                Some((f.query().unwrap_or_default().as_bytes().to_vec(), "application/x-www-form-urlencoded".to_string()))
            } else {
                None
            };
            let reach = crate::connections::Reach { hosts: vec![], any: false, timeout: Duration::from_secs(20), max_bytes: 1 << 20 };
            let a = self.fetch(crate::connections::Request { method, url, headers, body }, &reach).await?;
            Ok::<_, Error>((a.status, a.body))
        }
        .await;
        let temporary = desc.temporary.clone().unwrap_or_else(|| crate::connections::TEMPORARY.to_vec());
        let last = job.attempts >= job.max_attempts;
        match outcome {
            Ok((status, answer)) if (200..300).contains(&status) => {
                self.settle(tenant, id, "sent", at(&answer, desc.id_at.as_deref()).as_deref(), None, None).await?;
                Ok(json!({"sent": status}))
            }
            Ok((status, answer)) if temporary.contains(&status) && !last => {
                Err(Error::Unavailable(format!("the provider answered {status}: {}", at(&answer, desc.error_at.as_deref()).unwrap_or_default())))
            }
            Ok((status, answer)) => {
                let why = format!("{status}: {}", at(&answer, desc.error_at.as_deref()).unwrap_or_else(|| answer.to_string().chars().take(300).collect()));
                let why = masked(&Value::String(why), &mask);
                self.settle(tenant, id, "failed", None, why.as_str(), None).await?;
                Ok(json!({"failed": status}))
            }
            Err(e @ Error::Unavailable(_)) if !last => Err(e),
            Err(e) => {
                let why = masked(&Value::String(e.to_string()), &mask);
                self.settle(tenant, id, "failed", None, why.as_str(), None).await?;
                Ok(json!({"failed": why}))
            }
        }
    }

    /// Adds a channel connection, or changes one. Its secrets are sealed; a description that is wrong is refused.
    pub async fn save_message_channel(&self, ctx: &Ctx, id: Option<Uuid>, input: &ChannelInput) -> Result<Uuid> {
        admin_only(ctx)?;
        if !matches!(input.kind.as_str(), "text" | "chat") {
            return Err(Error::field("kind", "text or chat"));
        }
        let name = input.name.trim();
        if name.is_empty() || name.chars().count() > 100 {
            return Err(Error::field("name", "give it a name of at most 100 characters"));
        }
        if let Some(p) = input.description.problem(self.settings.webhooks_allow_private) {
            return Err(Error::field("description", p));
        }
        let id = id.unwrap_or_else(Uuid::now_v7);
        let sealed = match &input.secrets {
            Some(s) => {
                let key = self
                    .settings
                    .secret_key
                    .as_ref()
                    .ok_or_else(|| Error::Unavailable("channel secrets need a configured secret key (FERRUM_SECRET_KEY)".into()))?;
                Some(key.seal(&sealed_label(ctx.tenant, id), &serde_json::to_string(s).map_err(Error::internal)?)?)
            }
            None => None,
        };
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        if input.enabled {
            sqlx::query("update message_channel set enabled = false where tenant_id = $1 and kind = $2 and id <> $3")
                .bind(ctx.tenant)
                .bind(&input.kind)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(
            "insert into message_channel (tenant_id, id, kind, name, description, sealed, enabled) values ($1, $2, $3, $4, $5, $6, $7) \
             on conflict (tenant_id, id) do update set kind = excluded.kind, name = excluded.name, description = excluded.description, \
             sealed = coalesce(excluded.sealed, message_channel.sealed), enabled = excluded.enabled",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(&input.kind)
        .bind(name)
        .bind(serde_json::to_value(&input.description).map_err(Error::internal)?)
        .bind(sealed)
        .bind(input.enabled)
        .execute(&mut *tx)
        .await?;
        audit::record_admin(&mut tx, ctx, "MessageChannel", id, "save", None, json!({"kind": input.kind, "name": name, "secrets": input.secrets.is_some()}))
            .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// The channel connections, never a secret: `{id, kind, name, description, enabled, has_secrets, refused, inbound}`.
    pub async fn message_channels(&self, ctx: &Ctx) -> Result<Vec<Value>> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows = sqlx::query_scalar(
            "select jsonb_build_object('id', id, 'kind', kind, 'name', name, 'description', description, 'enabled', enabled, \
               'has_secrets', sealed is not null, 'refused', refused, 'inbound', '/api/messages/inbound/' || id) \
             from message_channel where tenant_id = $1 order by kind, name",
        )
        .bind(ctx.tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    pub async fn delete_message_channel(&self, ctx: &Ctx, id: Uuid) -> Result<()> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let n = sqlx::query("delete from message_channel where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id).execute(&mut *tx).await?;
        if n.rows_affected() == 0 {
            return Err(Error::NotFound("message channel".into()));
        }
        audit::record_admin(&mut tx, ctx, "MessageChannel", id, "delete", None, json!({})).await?;
        tx.commit().await?;
        Ok(())
    }

    /// A provider's call to a channel (`POST /api/messages/inbound/{channel}`, no session): a delivery report moves its
    /// send on (never back), a reply with a stop or start word records the number's choice. One unknown, unsigned or
    /// wrongly signed is refused and counted.
    pub async fn message_inbound(&self, channel: Uuid, headers: &BTreeMap<String, String>, query: &BTreeMap<String, String>, body: &[u8]) -> Result<Value> {
        let mut tx = self.db.system_tx().await?;
        let row: Option<(Uuid, String, Value, Option<String>)> =
            sqlx::query_as("select tenant_id, kind, description, sealed from message_channel where id = $1 and enabled")
                .bind(channel)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        let found = row.and_then(|(t, k, d, s)| Some((t, k, serde_json::from_value::<ChannelDesc>(d).ok()?.inbound?, s?)));
        let Some((tenant, kind, inbound, sealed)) = found else {
            REFUSED_UNKNOWN.fetch_add(1, Ordering::Relaxed);
            return Err(Error::NotFound("channel".into()));
        };
        let secrets: BTreeMap<String, String> = serde_json::from_str(&self.open_sealed(&sealed_label(tenant, channel), &sealed)?).map_err(Error::internal)?;
        let header = |h: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(h)).map(|(_, v)| v.trim().to_string());
        let secret = secrets.get(&inbound.secret).cloned().unwrap_or_default();
        let ok = !secret.is_empty()
            && match inbound.auth.as_str() {
                "hmac" => inbound.header.as_deref().and_then(header).is_some_and(|sig| {
                    use hmac::Mac;
                    let sig = sig.strip_prefix("sha256=").unwrap_or(&sig).to_string();
                    let mut mac = hmac::Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
                    mac.update(body);
                    hex::decode(sig).is_ok_and(|s| mac.verify_slice(&s).is_ok())
                }),
                _ => inbound.header.as_deref().map_or_else(|| query.get("token").cloned(), header).is_some_and(|t| subtle_eq(t.as_bytes(), secret.as_bytes())),
            };
        if !ok {
            let mut tx = self.db.system_tenant_tx(tenant).await?;
            sqlx::query("update message_channel set refused = refused + 1 where tenant_id = $1 and id = $2")
                .bind(tenant)
                .bind(channel)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Err(Error::Unauthorized("this call is not signed by the channel's provider".into()));
        }
        let form = header("content-type").is_some_and(|c| c.starts_with("application/x-www-form-urlencoded"));
        let said: Value = if form {
            let text = String::from_utf8_lossy(body);
            let url = reqwest::Url::parse(&format!("http://form/?{text}")).map_err(|_| Error::invalid("not a form"))?;
            Value::Object(url.query_pairs().map(|(k, v)| (k.into_owned(), Value::String(v.into_owned()))).collect())
        } else {
            serde_json::from_slice(body).map_err(|_| Error::invalid("not JSON"))?
        };
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        let status = at(&said, inbound.status_at.as_deref());
        if let (Some(provider_id), Some(status)) = (at(&said, inbound.id_at.as_deref()), status) {
            let Some(state) = inbound.statuses.get(&status) else {
                tx.commit().await?;
                return Ok(json!({"ignored": status}));
            };
            // Forward only: a late "sent" does not undo "delivered"; a failure is final.
            let n = sqlx::query(
                "update message_send set state = $4, error = coalesce($5, error), updated_at = now() where tenant_id = $1 and channel_id = $2 and provider_id = $3 \
                 and state not in ('failed', 'skipped') and (case state when 'delivered' then 3 when 'sent' then 2 else 1 end) < (case $4 when 'sent' then 2 else 3 end)",
            )
            .bind(tenant)
            .bind(channel)
            .bind(&provider_id)
            .bind(state)
            .bind(at(&said, inbound.error_at.as_deref()))
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(json!({"updated": n.rows_affected()}));
        }
        if let (Some(from), Some(text)) = (at(&said, inbound.from_at.as_deref()), at(&said, inbound.text_at.as_deref())) {
            let (cfg, _) = workspace(&mut tx, tenant).await?;
            let word = text.trim().to_uppercase();
            let choice = if inbound.stop.iter().any(|w| w.to_uppercase() == word) {
                Some(false)
            } else if inbound.start.iter().any(|w| w.to_uppercase() == word) {
                Some(true)
            } else {
                None
            };
            let number = international(&from, &cfg).map_err(Error::invalid)?;
            if let Some(granted) = choice {
                record_choice(&mut tx, tenant, None, &number, &kind, granted, "a reply to a message").await?;
            }
            tx.commit().await?;
            return Ok(json!({"reply": true, "opted_in": choice}));
        }
        tx.commit().await?;
        Ok(json!({"ignored": true}))
    }
}

/// Equal, in time that does not depend on where they differ.
pub(crate) fn subtle_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

// ── choice ──

/// Records whether `address` takes promotional messages on `channel` (the consent records there are). Audited.
async fn record_choice(tx: &mut Tx, tenant: Uuid, by: Option<Uuid>, address: &str, channel: &str, granted: bool, source: &str) -> Result<()> {
    sqlx::query(
        "insert into contact_consent (tenant_id, email, purpose, granted, source, recorded_by, channel) values ($1, $2, 'marketing', $3, $4, $5, $6) \
         on conflict (tenant_id, email, purpose, channel) do update set granted = excluded.granted, source = excluded.source, recorded_by = excluded.recorded_by, recorded_at = now()",
    )
    .bind(tenant)
    .bind(address)
    .bind(granted)
    .bind(source)
    .bind(by)
    .bind(channel)
    .execute(&mut **tx)
    .await?;
    let sys = Ctx::system(tenant, Arc::new(ferrum_meta::Schema::default()));
    audit::record_admin(
        tx,
        &sys,
        "ContactConsent",
        Uuid::nil(),
        if granted { "consent_granted" } else { "consent_withdrawn" },
        None,
        json!({"channel": channel, "source": source}),
    )
    .await
}

/// A signed link's token: the workspace, channel and address, and their HMAC under the server's key.
fn opt_out_token(key: &crate::secret::SecretKey, tenant: Uuid, channel: &str, address: &str) -> String {
    use base64::Engine as _;
    let said = format!("{tenant}|{channel}|{address}");
    let sig = key.fingerprint("message opt-out", &said);
    format!("{}.{sig}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(said))
}

impl Kernel {
    /// Staff recording a person's choice for promotional messages on a channel (`email`, `text`, `chat`): a number is
    /// put in international form first.
    pub async fn set_message_opt_out(&self, ctx: &Ctx, address: &str, channel: &str, opted_out: bool) -> Result<()> {
        admin_only(ctx)?;
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        // An erased one is named by its row (the listing's `erased`), and can only be taken away.
        if address.starts_with(ERASED) {
            if opted_out {
                return Err(Error::field("address", "an erased opt-out can only be removed"));
            }
            let gone = sqlx::query("delete from contact_consent where tenant_id = $1 and purpose = 'marketing' and channel = $2 and email = $3")
                .bind(ctx.tenant)
                .bind(channel)
                .bind(address)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if gone == 0 {
                return Err(Error::NotFound("opt-out".into()));
            }
            audit::record_admin(&mut tx, ctx, "ContactConsent", Uuid::nil(), "erased_opt_out_removed", None, json!({"channel": channel})).await?;
            tx.commit().await?;
            return Ok(());
        }
        let address = match channel {
            "email" => crate::privacy::checked_email(address)?,
            "text" | "chat" => international(address, &workspace(&mut tx, ctx.tenant).await?.0).map_err(|e| Error::field("address", e))?,
            _ => return Err(Error::field("channel", "email, text or chat")),
        };
        record_choice(&mut tx, ctx.tenant, (!ctx.user.is_nil()).then_some(ctx.user), &address, channel, !opted_out, "recorded by staff").await?;
        tx.commit().await?;
        Ok(())
    }

    /// The opt-outs there are, newest first, each with who recorded it and when (`GET /api/admin/messages/opt-outs`):
    /// `{opt_outs: [{address, erased, channel, source, recorded_at, recorded_by, recorded_by_name}], total}`. An erased
    /// person's has no address and names its row in `erased`; `q` finds part of an address or a number (never an erased
    /// one), `channel` one channel; `limit` 1 to 200.
    pub async fn message_opt_outs(&self, ctx: &Ctx, q: Option<&str>, channel: Option<&str>, limit: i64, offset: i64) -> Result<Value> {
        admin_only(ctx)?;
        if !(1..=200).contains(&limit) || offset < 0 {
            return Err(Error::field("limit", "1 to 200, from an offset of 0 or more"));
        }
        let q = q.map(str::trim).filter(|q| !q.is_empty()).map(|q| q.replace(['%', '_', '\\'], ""));
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let rows: Vec<Value> = sqlx::query_scalar(
            "select jsonb_build_object('address', case when c.email::text like 'erased:%' then null else c.email::text end, \
               'erased', case when c.email::text like 'erased:%' then c.email::text end, 'channel', c.channel, 'source', c.source, 'recorded_at', c.recorded_at, \
               'recorded_by', c.recorded_by, 'recorded_by_name', coalesce(nullif(u.full_name, ''), u.email::text), 'total', count(*) over ()) \
             from contact_consent c left join app_user u on u.tenant_id = c.tenant_id and u.id = c.recorded_by \
             where c.tenant_id = $1 and c.purpose = 'marketing' and not c.granted and ($2::text is null or c.channel = $2) \
               and ($3::text is null or (c.email::text not like 'erased:%' and c.email::text ilike '%' || $3 || '%')) order by c.recorded_at desc, c.email limit $4 offset $5",
        )
        .bind(ctx.tenant)
        .bind(channel)
        .bind(q)
        .bind(limit)
        .bind(offset)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let total = rows.first().and_then(|r| r["total"].as_i64()).unwrap_or(0);
        let rows: Vec<Value> = rows
            .into_iter()
            .map(|mut r| {
                r.as_object_mut().map(|m| m.remove("total"));
                r
            })
            .collect();
        Ok(json!({"opt_outs": rows, "total": total}))
    }

    /// The recipient following a signed link (`POST /api/messages/opt-out?token=`): no session. Returns the channel.
    pub async fn opt_out_by_link(&self, token: &str) -> Result<String> {
        use base64::Engine as _;
        let refused = || Error::invalid("this link is not one of ours, or was changed");
        let key = self.settings.secret_key.as_ref().ok_or_else(refused)?;
        let (said, sig) = token.split_once('.').ok_or_else(refused)?;
        let said = String::from_utf8(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(said).map_err(|_| refused())?).map_err(|_| refused())?;
        if !subtle_eq(key.fingerprint("message opt-out", &said).as_bytes(), sig.as_bytes()) {
            return Err(refused());
        }
        let mut parts = said.splitn(3, '|');
        let (Some(tenant), Some(channel), Some(address)) = (parts.next().and_then(|t| t.parse::<Uuid>().ok()), parts.next(), parts.next()) else {
            return Err(refused());
        };
        let mut tx = self.db.system_tenant_tx(tenant).await?;
        record_choice(&mut tx, tenant, None, address, channel, false, "the link in a message").await?;
        tx.commit().await?;
        Ok(channel.to_string())
    }

    // ── one-time codes ──

    /// Sends a one-time code to `to` on `channel` through `template` (which reads it as `values.code`), for `purpose`
    /// (`sign.signer`, `bookings.guest`): the earlier ones for it no longer hold. At most [`CODES_PER_HOUR`] a recipient
    /// an hour. The code is kept only as a salted digest; returns the send.
    pub async fn send_code(&self, tx: &mut Tx, ctx: &Ctx, purpose: &str, template: &str, to: &Recipient, channel: &str) -> Result<Sent> {
        let (cfg, _) = workspace(tx, ctx.tenant).await?;
        let key = code_recipient(to, &cfg)?;
        let recent: i64 =
            sqlx::query_scalar("select count(*) from message_code where tenant_id = $1 and recipient = $2 and created_at > now() - interval '1 hour'")
                .bind(ctx.tenant)
                .bind(&key)
                .fetch_one(&mut **tx)
                .await?;
        if recent >= CODES_PER_HOUR {
            return Err(Error::TooManyRequests("too many codes were sent to this recipient in the last hour".into()));
        }
        let code = format!("{:06}", rand::random_range(0..1_000_000u32));
        let id = Uuid::new_v4();
        sqlx::query("update message_code set used_at = now() where tenant_id = $1 and purpose = $2 and recipient = $3 and used_at is null")
            .bind(ctx.tenant)
            .bind(purpose)
            .bind(&key)
            .execute(&mut **tx)
            .await?;
        sqlx::query(
            "insert into message_code (tenant_id, id, purpose, recipient, digest, expires_at) values ($1, $2, $3, $4, $5, now() + make_interval(mins => $6))",
        )
        .bind(ctx.tenant)
        .bind(id)
        .bind(purpose)
        .bind(&key)
        .bind(code_digest(id, &code))
        .bind(CODE_MINUTES as i32)
        .execute(&mut **tx)
        .await?;
        let m = Message {
            template: template.into(),
            to: vec![to.clone()],
            channels: vec![channel.into()],
            values: Map::from_iter([("code".to_string(), json!(code))]),
            secret: vec!["code".into()],
            ..Default::default()
        };
        let sent = Box::pin(send_with(self, tx, ctx, &m)).await?.into_iter().next().ok_or_else(|| Error::invalid("there is no one to send the code to"))?;
        if sent.state == "skipped" {
            return Err(Error::invalid(format!("the code cannot be sent: {}", sent.reason.unwrap_or_default())));
        }
        Ok(sent)
    }

    /// Whether `code` is the one sent to `to` for `purpose`: once, before it expires, within [`CODE_TRIES`] tries.
    pub async fn verify_code(&self, tx: &mut Tx, ctx: &Ctx, purpose: &str, to: &Recipient, code: &str) -> Result<bool> {
        let (cfg, _) = workspace(tx, ctx.tenant).await?;
        let key = code_recipient(to, &cfg)?;
        let found: Option<(Uuid, String, i32)> = sqlx::query_as(
            "select id, digest, attempts from message_code where tenant_id = $1 and purpose = $2 and recipient = $3 and used_at is null and expires_at > now() \
             order by created_at desc limit 1 for update",
        )
        .bind(ctx.tenant)
        .bind(purpose)
        .bind(&key)
        .fetch_optional(&mut **tx)
        .await?;
        let Some((id, digest, attempts)) = found else { return Ok(false) };
        if attempts >= CODE_TRIES {
            return Ok(false);
        }
        let right = subtle_eq(code_digest(id, code.trim()).as_bytes(), digest.as_bytes());
        sqlx::query("update message_code set attempts = attempts + 1, used_at = case when $3 then now() end where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(id)
            .bind(right)
            .execute(&mut **tx)
            .await?;
        Ok(right)
    }
}

/// Who a code is for, as codes are kept: an address, or a number in international form.
fn code_recipient(to: &Recipient, s: &Settings) -> Result<String> {
    match (&to.email, &to.phone) {
        (_, Some(p)) => international(p, s).map_err(|e| Error::field("to", e)),
        (Some(e), None) => crate::privacy::checked_email(e),
        _ => Err(Error::field("to", "a code goes to an address or a number")),
    }
}

// ponytail: a salted SHA-256 of a six-digit code can be searched in a moment by whoever reads the table; it holds for
// the ten minutes a code lives and its five tries. Key it with the server's secret if the table is ever exposed.
fn code_digest(salt: Uuid, code: &str) -> String {
    hex::encode(Sha256::digest(format!("{salt}:{code}")))
}

// ── the log ──

impl Kernel {
    /// The sends about a record, newest first, for whoever may read it; or to a recipient (an address or a number, put
    /// in international form), for administrators.
    pub async fn message_log(&self, ctx: &Ctx, record: Option<(&str, Uuid)>, recipient: Option<&str>, limit: i64) -> Result<Vec<Value>> {
        let mut tx = self.db.tenant_tx(ctx.tenant).await?;
        let cols = "jsonb_build_object('id', id, 'template', template, 'channel', channel, 'recipient', recipient, 'user_id', user_id, 'entity', entity, \
                    'record_id', record_id, 'language', language, 'state', state, 'reason', reason, 'provider_id', provider_id, 'error', error, 'content', content, \
                    'created_at', created_at, 'updated_at', updated_at)";
        let rows = match (record, recipient) {
            (Some((entity, id)), _) => {
                self.get_tx(ctx, &mut tx, entity, id, false).await?;
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "select {cols} from message_send where tenant_id = $1 and entity = $2 and record_id = $3 order by created_at desc, id desc limit $4"
                )))
                .bind(ctx.tenant)
                .bind(entity)
                .bind(id)
                .bind(limit.clamp(1, 500))
                .fetch_all(&mut *tx)
                .await?
            }
            (None, Some(r)) => {
                admin_only(ctx)?;
                let (cfg, _) = workspace(&mut tx, ctx.tenant).await?;
                let r = if r.contains('@') { r.trim().to_lowercase() } else { international(r, &cfg).map_err(|e| Error::field("recipient", e))? };
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "select {cols} from message_send where tenant_id = $1 and lower(recipient) = $2 order by created_at desc, id desc limit $3"
                )))
                .bind(ctx.tenant)
                .bind(r)
                .bind(limit.clamp(1, 500))
                .fetch_all(&mut *tx)
                .await?
            }
            (None, None) => return Err(Error::invalid("name a record or a recipient")),
        };
        tx.commit().await?;
        Ok(rows)
    }
}

// ── personal data ──

/// The sends and codes of a person (their addresses, their account, their number's digits), for their report.
pub(crate) async fn held_report(tx: &mut Tx, tenant: Uuid, account: Option<Uuid>, emails: &[String], digits: Option<&str>) -> Result<Vec<Value>> {
    Ok(sqlx::query_scalar(
        "select v from (select jsonb_build_object('template', template, 'channel', channel, 'recipient', recipient, 'state', state, 'created_at', created_at) v, created_at \
         from message_send where tenant_id = $1 and (lower(recipient) = any($2) or user_id = $3 \
           or ($4::text is not null and regexp_replace(recipient, '\\D', '', 'g') like '%' || ltrim($4, '0'))) \
         union all select jsonb_build_object('choice', case when granted then 'consented' else 'opted out' end, 'purpose', purpose, 'channel', channel, \
           'recipient', email::text, 'created_at', recorded_at), recorded_at from contact_consent where tenant_id = $1 and email::text not like 'erased:%' \
           and (lower(email::text) = any($2) or ($4::text is not null and channel <> 'email' and regexp_replace(email::text, '\\D', '', 'g') like '%' || ltrim($4, '0')))) h order by created_at",
    )
    .bind(tenant)
    .bind(emails)
    .bind(account)
    .bind(digits)
    .fetch_all(&mut **tx)
    .await?)
}

/// The choices recorded for a person's addresses and number, as `(address, channel, granted)`.
pub(crate) async fn choices_of(tx: &mut Tx, tenant: Uuid, emails: &[String], digits: Option<&str>) -> Result<Vec<(String, String, bool)>> {
    Ok(sqlx::query_as(
        "select email::text, channel, granted from contact_consent where tenant_id = $1 and purpose = 'marketing' and email::text not like 'erased:%' \
           and (lower(email::text) = any($2) or ($3::text is not null and channel <> 'email' and regexp_replace(email::text, '\\D', '', 'g') like '%' || ltrim($3, '0')))",
    )
    .bind(tenant)
    .bind(emails)
    .bind(digits)
    .fetch_all(&mut **tx)
    .await?)
}

/// Erases the address of each of these choices: a consent given goes, an opt-out keeps its effect under its
/// [`erased_mark`], with its channel, source, date and who recorded it (BD-20). Without the server's key the choices
/// stay as they are. Returns the marks, for the erasure's journal (they name no address).
pub(crate) async fn erase_choices(
    tx: &mut Tx,
    tenant: Uuid,
    key: Option<&crate::secret::SecretKey>,
    choices: &[(String, String, bool)],
) -> Result<Vec<String>> {
    let Some(key) = key else {
        if !choices.is_empty() {
            tracing::warn!(%tenant, "no secret key: an erased person's opt-outs are kept with their address");
        }
        return Ok(vec![]);
    };
    let mut marks = Vec::new();
    for (address, channel, granted) in choices {
        let mark = erased_mark(key, tenant, channel, address);
        // A consent goes; an opt-out replaces any it was erased as before.
        sqlx::query("delete from contact_consent where tenant_id = $1 and purpose = 'marketing' and channel = $2 and email = $3")
            .bind(tenant)
            .bind(channel)
            .bind(if *granted { address } else { &mark })
            .execute(&mut **tx)
            .await?;
        sqlx::query("update contact_consent set email = $4 where tenant_id = $1 and purpose = 'marketing' and channel = $2 and email = $3")
            .bind(tenant)
            .bind(channel)
            .bind(address)
            .bind(&mark)
            .execute(&mut **tx)
            .await?;
        marks.push(mark);
    }
    Ok(marks)
}

/// Erases a person's sends and codes; returns the sends' ids, for the erasure's journal. Their choices:
/// [`erase_choices`].
pub(crate) async fn erase_held(tx: &mut Tx, tenant: Uuid, account: Option<Uuid>, emails: &[String], digits: Option<&str>) -> Result<Vec<Uuid>> {
    sqlx::query(
        "delete from message_code where tenant_id = $1 and (lower(recipient) = any($2) \
           or ($3::text is not null and regexp_replace(recipient, '\\D', '', 'g') like '%' || ltrim($3, '0')))",
    )
    .bind(tenant)
    .bind(emails)
    .bind(digits)
    .execute(&mut **tx)
    .await?;
    Ok(sqlx::query_scalar(
        "delete from message_send where tenant_id = $1 and (lower(recipient) = any($2) or user_id = $3 \
           or ($4::text is not null and regexp_replace(recipient, '\\D', '', 'g') like '%' || ltrim($4, '0'))) returning id",
    )
    .bind(tenant)
    .bind(emails)
    .bind(account)
    .bind(digits)
    .fetch_all(&mut **tx)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A link carrying a secret token, printed in a link's address, an attribute, text, JSON and drawn as a QR code and a
    /// barcode: the mail as kept holds none of its forms (BD-44).
    #[test]
    fn a_secret_is_masked_in_every_form_a_template_prints_it() {
        let token = "Tk/9+a&b=c'd <e>";
        let body = r#"<a href="https://shop.example/accept?t={{ values.token }}" title="{{ values.token }}">Accept</a>
            <p>{{ values.token }}</p><pre>{{ values.token|json }}</pre>{{ qr(values.token) }}{{ barcode(values.token) }}"#;
        let html = render(body, true, &json!({"values": {"token": token}}), false).unwrap();
        let masks = encodings(&[token.to_string()]);
        let kept = masked(&json!(html), &masks);
        let kept = kept.as_str().unwrap();
        for form in ["{{ v }}", "{{ v|json }}"] {
            for html in [true, false] {
                let shown = environment(html, false).render_str(form, minijinja::context! { v => token }).unwrap();
                assert!(!kept.contains(shown.trim_matches('"')), "{form} ({html}) left in: {kept}");
            }
        }
        assert!(!kept.contains("<svg"), "no drawing of it: {kept}");
        assert!(kept.contains("https://shop.example/accept?t=••••••") && kept.contains("Accept"), "{kept}");
        // A channel's request percent-encodes what it puts in an address: masked too.
        let address = "https://sms.example/send?text=Tk%2F9%2Ba%26b%3Dc%27d%20%3Ce%3E&to=1";
        assert_eq!(masked(&json!(address), &masks), json!("https://sms.example/send?text=••••••&to=1"));
    }

    #[test]
    fn numbers_are_international_or_refused() {
        let none = Settings::default();
        assert_eq!(international("+49 151 2345-6789", &none).unwrap(), "+4915123456789");
        assert!(international("0151 23456789", &none).unwrap_err().contains("no country calling code"), "never guessed");
        let de = Settings { calling_code: Some("+49".into()), trunk_prefix: Some("0".into()), ..Default::default() };
        assert_eq!(international("0151 23456789", &de).unwrap(), "+4915123456789");
        let plain = Settings { calling_code: Some("+1".into()), ..Default::default() };
        assert_eq!(international("(415) 555-0100", &plain).unwrap(), "+14155550100");
        assert!(international("+49 abc", &de).is_err());
        assert!(international("12+3", &de).is_err());
        assert!(international("+0123456789", &none).is_err());
    }

    #[test]
    fn quiet_hours_wrap_past_midnight() {
        let q = QuietHours { from: "21:00".into(), to: "08:00".into() };
        let at = |h: u32, m: u32| chrono::NaiveDate::from_ymd_opt(2026, 11, 12).unwrap().and_hms_opt(h, m, 0).unwrap().and_utc();
        assert_eq!(quiet_for(Some(&q), "UTC", at(22, 0)), Some(10 * 3600));
        assert_eq!(quiet_for(Some(&q), "UTC", at(7, 30)), Some(1800));
        assert_eq!(quiet_for(Some(&q), "UTC", at(12, 0)), None);
        assert_eq!(quiet_for(None, "UTC", at(22, 0)), None);
        // 12:30 UTC is 21:30 in Tokyo: the recipient's zone decides.
        assert_eq!(quiet_for(Some(&q), "Asia/Tokyo", at(12, 30)), Some(10 * 3600 + 1800));
        let day = QuietHours { from: "12:00".into(), to: "14:00".into() };
        assert_eq!(quiet_for(Some(&day), "UTC", at(13, 0)), Some(3600));
        assert_eq!(quiet_for(Some(&day), "UTC", at(14, 0)), None);
    }

    #[test]
    fn a_code_is_masked_where_it_is_logged() {
        let v = json!({"text": "Your code is 123456", "values": ["123456", "x"]});
        assert_eq!(masked(&v, &["123456".into()]), json!({"text": "Your code is ••••••", "values": ["••••••", "x"]}));
        assert_eq!(masked(&v, &[]), v);
    }

    #[test]
    fn opt_out_links_are_signed() {
        let key = crate::secret::SecretKey::new([3u8; 32]);
        let t = opt_out_token(&key, Uuid::nil(), "text", "+15550100");
        let (said, sig) = t.split_once('.').unwrap();
        assert_eq!(sig, key.fingerprint("message opt-out", &format!("{}|text|+15550100", Uuid::nil())));
        assert!(!said.is_empty());
        assert!(subtle_eq(b"abc", b"abc") && !subtle_eq(b"abc", b"abd") && !subtle_eq(b"abc", b"ab"));
    }
}
