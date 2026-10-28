//! Knowledge base articles: the publication date and readers' helpful / not helpful votes.

use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};
use ferrum_domain::util::{id, s, set, sys};
use ferrum_kernel::{Doc, DocHook, Error, HookCtx, Result, store};
use serde_json::{Value, json};

pub struct ArticleHook;

#[async_trait]
impl DocHook for ArticleHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if s(doc, "status") != Some("Published") {
            return Ok(());
        }
        let was = match id(doc, "id") {
            Some(i) => store::load(hc.tx, &hc.ctx.schema, hc.entity, hc.ctx.tenant, i, false).await?.and_then(|o| s(&o, "status").map(str::to_string)),
            None => None,
        };
        if was.as_deref() != Some("Published") {
            set(doc, "published_at", Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true));
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        let field = match name {
            "mark_helpful" => "helpful_count",
            "mark_not_helpful" => "not_helpful_count",
            _ => return Ok(None),
        };
        // The kernel loaded the article locked for this action, so concurrent votes queue up.
        let article = id(doc, "id").ok_or_else(|| Error::internal("article without id"))?;
        let mut v = Doc::new();
        v.insert(field.into(), json!(doc.get(field).and_then(Value::as_i64).unwrap_or(0) + 1));
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "KbArticle", article, v.clone()).await?;
        Ok(Some(Value::Object(v)))
    }
}
