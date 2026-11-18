//! The Brand a workspace's public pages wear (the `core` package, docs/builder/decisions.md BD-79). Every save
//! computes the colours pages show (`shown_color`, `shown_color_dark`: the accent nudged to 4.5:1 in light and dark,
//! [`ferrum_kernel::brand::tokens`]), so what is served is what the record shows; checks its files are images (or a
//! typeface) uploaded for this brand and its links are safe to put on a page; and keeps one default brand at most.
//! Beside it, the Embedding record: which outside sites may frame which public pages (BD-80).

use crate::util::{b, id, rows, s, set};
use async_trait::async_trait;
use ferrum_kernel::brand::{FILE_MAX, FILE_PARTS, LINKS_MAX, file_id, hex, safe_link, tokens};
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use uuid::Uuid;

pub struct BrandHook;

#[async_trait]
impl DocHook for BrandHook {
    fn name(&self) -> &'static str {
        "core.brand"
    }

    fn fills(&self) -> &'static [&'static str] {
        &["primary_color", "primary_color_dark", "shown_color", "shown_color_dark"]
    }

    async fn before_validate(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        for f in ["primary_color", "primary_color_dark"] {
            if let Some(c) = s(doc, f).and_then(hex) {
                set(doc, f, c);
            }
        }
        let t = tokens(s(doc, "primary_color").unwrap_or_default(), s(doc, "primary_color_dark"));
        set(doc, "shown_color", t.light);
        set(doc, "shown_color_dark", t.dark);
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        let links = rows(doc, "links");
        if links.len() > LINKS_MAX {
            errors.push(FieldError::new("links", format!("at most {LINKS_MAX} links")));
        }
        for (i, r) in links.iter().enumerate() {
            if !s(r, "url").is_some_and(safe_link) {
                errors.push(
                    FieldError::new("url", "must start with https:// or mailto:, or be a path of the page's own address such as /privacy").in_row("links", i),
                );
            }
        }
        if s(doc, "font") == Some("Own") && s(doc, "font_file").is_none() {
            errors.push(FieldError::new("font_file", "is needed when the font is Own"));
        }
        let brand = id(doc, "id").unwrap_or_default();
        for (field, types) in FILE_PARTS {
            let Some(value) = s(doc, field) else { continue };
            let what = if field == "font_file" {
                "a WOFF2 file"
            } else if types.len() == 1 {
                "a PNG image"
            } else {
                "a PNG, JPEG or WebP image"
            };
            let refused = || FieldError::new(field, format!("must be {what} uploaded to this brand, at most {} MB", FILE_MAX >> 20));
            let Some(file) = file_id(Some(value)) else {
                errors.push(refused());
                continue;
            };
            let row: Option<(Option<String>, Option<Uuid>, String, i64)> =
                sqlx::query_as("select entity, record_id, content_type, size_bytes from file_object where tenant_id = $1 and id = $2")
                    .bind(hc.ctx.tenant)
                    .bind(file)
                    .fetch_optional(&mut **hc.tx)
                    .await?;
            // Uploaded for this brand: not yet tied to a record (the save ties it here), or tied to this one already.
            let ok = row.is_some_and(|(entity, record, ctype, size)| {
                let here = match (entity.as_deref(), record) {
                    (None, None) => true,
                    (Some("Brand"), Some(r)) => r == brand,
                    _ => false,
                };
                here && types.contains(&ctype.as_str()) && size <= FILE_MAX
            });
            if !ok {
                errors.push(refused());
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, _old: Option<&Doc>) -> Result<()> {
        if !b(doc, "is_default") {
            return Ok(());
        }
        // One default: saves of a default brand take their turn per workspace, and each clears every other one. Readers
        // take the oldest default, so even one that slipped past would show one brand.
        sqlx::query("select pg_advisory_xact_lock(hashtextextended('brand-default:' || $1::text, 0))").bind(hc.ctx.tenant).execute(&mut **hc.tx).await?;
        sqlx::query("update e_brand set is_default = false where tenant_id = $1 and id <> $2 and is_default")
            .bind(hc.ctx.tenant)
            .bind(id(doc, "id"))
            .execute(&mut **hc.tx)
            .await?;
        Ok(())
    }
}

/// The Embedding record (docs/builder/decisions.md BD-80): a page is a plain path of lower-case words (no wildcard:
/// it is compared as text), its origins are outside origins, one per line, ten at most.
pub struct EmbeddingHook;

/// `forms/f/contact`: one to eight parts of lower-case letters, digits and dashes.
pub fn embed_page(page: &str) -> bool {
    let parts: Vec<&str> = page.split('/').collect();
    parts.len() <= 8 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
}

fn origin_lines(doc: &Doc) -> Vec<String> {
    s(doc, "origins").unwrap_or_default().lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()
}

#[async_trait]
impl DocHook for EmbeddingHook {
    fn name(&self) -> &'static str {
        "core.embedding"
    }

    fn fills(&self) -> &'static [&'static str] {
        &["origins"]
    }

    async fn before_validate(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if doc.contains_key("origins") {
            // Each as the platform keeps an origin (lower case, no trailing slash) when they are all good.
            let lines = origin_lines(doc);
            set(doc, "origins", ferrum_kernel::action_link::frame_origins(&lines).unwrap_or(lines).join("\n"));
        }
        Ok(())
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        if !s(doc, "page").is_some_and(embed_page) {
            errors.push(FieldError::new("page", "must be a page's address after the workspace, such as forms/f/contact: lower-case words joined by /"));
        }
        let origins = origin_lines(doc);
        if origins.is_empty() {
            errors.push(FieldError::new("origins", "name at least one site, as https://www.example.com"));
        }
        if let Err(e) = ferrum_kernel::action_link::frame_origins(&origins) {
            let said = match e {
                Error::Validation(v) => v.into_iter().map(|f| f.message).collect::<Vec<_>>().join("; "),
                other => other.to_string(),
            };
            errors.push(FieldError::new("origins", said));
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }
}
