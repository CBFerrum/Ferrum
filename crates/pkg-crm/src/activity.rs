//! Activities (calls, meetings, emails, tasks and notes) on leads, opportunities, customers and
//! contacts. Completing one with a lead records the contact on the lead and moves a new lead on
//! to Contacted.

use crate::{check_assignee, lookup, today};
use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};
use ferrum_domain::util::{get, id, s, set, sys};
use ferrum_kernel::value::parse_datetime;
use ferrum_kernel::{Doc, DocHook, Error, HookCtx, Result};
use serde_json::{Value, json};

pub struct ActivityHook;

/// Copies a link from a related record when the activity does not have one yet.
fn inherit(doc: &mut Doc, from: &Doc, field: &str) {
    if s(doc, field).is_none()
        && let Some(v) = s(from, field)
    {
        set(doc, field, v.to_string());
    }
}

#[async_trait]
impl DocHook for ActivityHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if s(doc, "activity_type") == Some("Note") && s(doc, "status") == Some("Open") {
            set(doc, "status", "Done");
        }
        // Put the activity on every timeline it belongs to.
        if let Some(o) = lookup(hc, "Opportunity", id(doc, "opportunity")).await? {
            inherit(doc, &o, "lead");
            inherit(doc, &o, "customer");
        }
        if let Some(c) = lookup(hc, "Contact", id(doc, "contact")).await? {
            inherit(doc, &c, "customer");
        }
        if let Some(l) = lookup(hc, "Lead", id(doc, "lead")).await? {
            inherit(doc, &l, "customer");
        }
        if s(doc, "status") != Some("Done") {
            set(doc, "completed_at", Value::Null);
        } else if s(doc, "completed_at").is_none() {
            set(doc, "completed_at", Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true));
        }
        Ok(())
    }

    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        check_assignee(hc, doc, old).await?;
        let done_now = s(doc, "status") == Some("Done") && old.is_none_or(|o| s(o, "status") != Some("Done"));
        // A note records something; it is not an interaction with the lead.
        if done_now
            && s(doc, "activity_type") != Some("Note")
            && let Some(lead) = id(doc, "lead")
        {
            // A past call logged today counts on the day it happened.
            let on = s(doc, "due_at").and_then(|t| parse_datetime(t).ok()).map(|t| t.date_naive()).filter(|d| *d <= today()).unwrap_or_else(today);
            let sctx = sys(hc.ctx);
            let l = get(hc.kernel, hc.ctx, hc.tx, "Lead", lead).await?;
            let mut v = Doc::new();
            if s(&l, "status") == Some("New") {
                v.insert("status".into(), json!("Contacted"));
            }
            if s(&l, "last_contacted_on").is_none_or(|last| last < on.to_string().as_str()) {
                v.insert("last_contacted_on".into(), json!(on.to_string()));
            }
            if !v.is_empty() {
                hc.kernel.set_values_tx(&sctx, hc.tx, "Lead", lead, v).await?;
            }
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "complete" {
            return Ok(None);
        }
        let aid = id(doc, "id").ok_or_else(|| Error::internal("activity without id"))?;
        let mut patch = Doc::new();
        patch.insert("status".into(), json!("Done"));
        let saved = hc.kernel.update_tx(hc.ctx, hc.tx, "Activity", aid, patch, None).await?;
        Ok(Some(json!({"status": saved.get("status"), "completed_at": saved.get("completed_at")})))
    }
}
