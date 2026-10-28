//! One operation over many records of an entity (a list selection or everything a filter
//! matches). Each record runs in its own transaction with the caller's permissions, so one
//! failure never undoes the others, and every record reports its own outcome.

use crate::error::{Error, Result};
use crate::query::{Filter, ListQuery};
use crate::{Ctx, Doc, Kernel};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

/// Most records one bulk request may touch.
pub const MAX_BULK: usize = 500;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum BulkOp {
    Update {
        values: Doc,
    },
    Delete,
    Submit,
    Cancel,
    /// A workflow transition.
    Workflow {
        action: String,
    },
    /// A document action from metadata.
    Action {
        action: String,
        #[serde(default)]
        params: Value,
    },
    Assign {
        user: Uuid,
        #[serde(default)]
        note: Option<String>,
        #[serde(default)]
        due_date: Option<chrono::NaiveDate>,
        #[serde(default)]
        priority: Option<String>,
    },
    AddTags {
        tags: Vec<String>,
    },
    RemoveTags {
        tags: Vec<String>,
    },
}

/// What a bulk request applies to: explicit ids, or every record a filter matches.
pub enum BulkTarget {
    Ids(Vec<Uuid>),
    Filters(Vec<Filter>),
}

impl Kernel {
    /// Runs `op` on every target record → `{results: [{id, ok, error?}], succeeded, failed}`.
    pub async fn bulk(&self, ctx: &Ctx, entity: &str, target: BulkTarget, op: &BulkOp) -> Result<Value> {
        let ids = match target {
            BulkTarget::Ids(mut ids) => {
                ids.sort();
                ids.dedup();
                ids
            }
            BulkTarget::Filters(filters) => {
                let q = ListQuery { filters, fields: Some(vec!["id".into()]), limit: Some(MAX_BULK as u32), with_count: true, ..Default::default() };
                let page = self.list(ctx, entity, &q).await?;
                if page.total.is_some_and(|t| t > MAX_BULK as i64) {
                    return Err(Error::invalid(format!("the filter matches {} records; at most {MAX_BULK} at once", page.total.unwrap_or_default())));
                }
                page.rows.iter().filter_map(|r| r.get("id").and_then(Value::as_str).and_then(|s| s.parse().ok())).collect()
            }
        };
        if ids.is_empty() {
            return Err(Error::invalid("no records selected"));
        }
        if ids.len() > MAX_BULK {
            return Err(Error::invalid(format!("at most {MAX_BULK} records at once")));
        }
        let mut results = Vec::with_capacity(ids.len());
        let mut failed = 0;
        for id in ids {
            let mut outcome = self.bulk_one(ctx, entity, id, op).await;
            // A serialization conflict with a concurrent writer is worth one more try.
            if outcome.as_ref().is_err_and(Error::is_retryable) {
                outcome = self.bulk_one(ctx, entity, id, op).await;
            }
            match outcome {
                Ok(()) => results.push(json!({"id": id, "ok": true})),
                Err(e) => {
                    failed += 1;
                    results.push(json!({"id": id, "ok": false, "error": row_error(&e)}));
                }
            }
        }
        Ok(json!({"succeeded": results.len() - failed, "failed": failed, "results": results}))
    }

    async fn bulk_one(&self, ctx: &Ctx, entity: &str, id: Uuid, op: &BulkOp) -> Result<()> {
        match op {
            BulkOp::Update { values } => self.update(ctx, entity, id, values.clone(), None).await.map(drop),
            BulkOp::Delete => self.delete(ctx, entity, id, None).await,
            BulkOp::Submit => self.submit(ctx, entity, id, None).await.map(drop),
            BulkOp::Cancel => self.cancel(ctx, entity, id, None).await.map(drop),
            BulkOp::Workflow { action } => self.transition(ctx, entity, id, action, None).await.map(drop),
            BulkOp::Action { action, params } => self.run_action(ctx, entity, id, action, params.clone()).await.map(drop),
            BulkOp::Assign { user, note, due_date, priority } => {
                self.assign(ctx, entity, id, *user, note.as_deref(), *due_date, priority.as_deref()).await.map(drop)
            }
            BulkOp::AddTags { tags } | BulkOp::RemoveTags { tags } => {
                let mut current = self.tags(ctx, entity, id).await?;
                if matches!(op, BulkOp::AddTags { .. }) {
                    current.extend(tags.iter().cloned());
                } else {
                    current.retain(|t| !tags.contains(t));
                }
                self.set_tags(ctx, entity, id, &current).await.map(drop)
            }
        }
    }
}

/// A record's failure, shaped like an API error body.
fn row_error(e: &Error) -> Value {
    match e {
        Error::Validation(fields) => json!({"error": e.code(), "message": "Please correct the highlighted fields", "fields": fields}),
        Error::Db(_) | Error::Internal(_) => {
            let id = Uuid::now_v7();
            tracing::error!(error_id = %id, "bulk operation failed on a record: {e}");
            json!({"error": e.code(), "message": "An internal error occurred", "error_id": id})
        }
        _ => json!({"error": e.code(), "message": e.to_string()}),
    }
}
