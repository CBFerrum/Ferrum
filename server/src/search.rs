//! Search across the workspace (docs/plan/apps-ui.md §12.8): the records of every entity the person
//! may read, found by their title and search fields with the kernel's list search (filtered by
//! the person's permissions as lists are, on trigram indexes), entities in parallel within a time
//! budget, ranked by how well the title matches and how recently the record changed. What is not
//! an entity (mail, chat) comes from the apps' `shell.search` providers, which the bar asks itself.
//! The lists the person may open are matched by name too (`pages`: the bar's "Go to").

use ferrum_kernel::{Access, Action, Ctx, Error, Kernel, ListQuery, Result};
use ferrum_meta::EntityKind;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long one entity may take; slower ones are left out of this answer (`partial`).
const PER_ENTITY: Duration = Duration::from_millis(300);
/// Entities searched at once: a search must not take the whole connection pool.
const AT_ONCE: usize = 6;
/// Records asked of each entity.
const PER_ENTITY_ROWS: u32 = 5;

/// One hit: an entity's record, opened through `/open`.
#[derive(Debug, Clone, PartialEq)]
struct Hit {
    rank: u8,
    updated: String,
    value: Value,
}

/// How well a title matches: the whole of it, its start, a word's start, anywhere.
fn rank(title: &str, q: &str) -> u8 {
    let (t, q) = (title.to_lowercase(), q.to_lowercase());
    if t == q {
        4
    } else if t.starts_with(&q) {
        3
    } else if t.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(&q)) {
        2
    } else if t.contains(&q) {
        1
    } else {
        0
    }
}

/// The entities a person may search: their own records' kinds (not lines, settings or ledgers),
/// with something to find them by.
fn searchable(ctx: &Ctx) -> Vec<String> {
    ctx.schema
        .entities
        .values()
        .filter(|e| matches!(e.def.kind, EntityKind::Master | EntityKind::Document))
        .filter(|e| e.def.title_field.is_some() || !e.def.search_fields.is_empty())
        .filter(|e| ctx.perms.access(&e.def.name, Action::Read) != Access::None)
        .map(|e| e.def.name.clone())
        .collect()
}

/// The lists the person may open whose name matches `q`, best first (at most five): each opens
/// through `/open/{entity}`, in the app that shows it.
fn pages(ctx: &Ctx, q: &str) -> Vec<Value> {
    let mut found: Vec<(u8, &str, Value)> = ctx
        .schema
        .entities
        .values()
        .filter(|e| matches!(e.def.kind, EntityKind::Master | EntityKind::Document))
        .filter(|e| ctx.perms.access(&e.def.name, Action::Read) != Access::None)
        .filter_map(|e| {
            let name = e.def.label_plural.as_deref().unwrap_or(&e.def.label);
            let r = rank(name, q).max(rank(&e.def.label, q));
            let module = e
                .def
                .module
                .as_deref()
                .and_then(|m| ctx.schema.modules.iter().find(|d| d.name == m))
                .map(|d| format!(" · {}", d.label))
                .unwrap_or_default();
            (r > 0).then(|| {
                let page = json!({"kind": "page", "entity": e.def.name, "title": name, "subtitle": format!("List{module}"), "url": format!("/open/{}", e.def.name), "icon": e.def.icon});
                (r, name, page)
            })
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    found.into_iter().take(5).map(|(_, _, v)| v).collect()
}

/// `q` across the workspace: `{results, pages, partial, took_ms}`.
pub async fn search(k: &Arc<Kernel>, ctx: &Ctx, q: &str, limit: usize) -> Result<Value> {
    let started = Instant::now();
    let q = q.trim();
    if q.chars().count() < 2 {
        return Ok(json!({"results": [], "pages": [], "partial": false, "took_ms": 0}));
    }
    if q.len() > 100 {
        return Err(Error::invalid("search text too long"));
    }
    let pages = pages(ctx, q);
    let entities = searchable(ctx);
    let gate = Arc::new(tokio::sync::Semaphore::new(AT_ONCE));
    let mut tasks = Vec::new();
    for name in entities {
        let (k, ctx, gate, q) = (k.clone(), ctx.clone(), gate.clone(), q.to_string());
        tasks.push(tokio::spawn(async move {
            let _slot = gate.acquire_owned().await.ok()?;
            let ent = ctx.schema.entity(&name)?.clone();
            let mut fields = vec!["id".to_string(), "number".into(), "updated_at".into()];
            fields.extend(ent.def.title_field.clone());
            let query = ListQuery { search: Some(q.clone()), fields: Some(fields), sort: Some("updated_at desc".into()), limit: Some(PER_ENTITY_ROWS), ..Default::default() };
            let page = match tokio::time::timeout(PER_ENTITY, k.list(&ctx, &name, &query)).await {
                Ok(Ok(page)) => page,
                // Refused or failed (a field the person may not read, a broken custom entity): left out.
                Ok(Err(_)) => return Some((name, None)),
                Err(_) => return Some((name, None)),
            };
            let label = ent.def.label.clone();
            let hits: Vec<Hit> = page
                .rows
                .into_iter()
                .filter_map(|row| {
                    let id = row.get("id")?.as_str()?.to_string();
                    let number = row.get("number").and_then(Value::as_str).unwrap_or_default().to_string();
                    // A record with neither a title nor a number (a chat message) is left to its
                    // app's own search, which can say what it is.
                    let title = ent
                        .def
                        .title_field
                        .as_ref()
                        .and_then(|f| row.get(f))
                        .and_then(Value::as_str)
                        .filter(|t| !t.trim().is_empty())
                        .map(String::from)
                        .or_else(|| (!number.is_empty()).then(|| number.clone()))?;
                    let updated = row.get("updated_at").and_then(Value::as_str).unwrap_or_default().to_string();
                    let subtitle = if number.is_empty() || number == title { label.clone() } else { format!("{label} · {number}") };
                    Some(Hit {
                        rank: rank(&title, &q).max(u8::from(number.to_lowercase().contains(&q.to_lowercase()))),
                        value: json!({"kind": "record", "entity": name, "id": id, "title": title, "subtitle": subtitle, "url": format!("/open/{name}/{id}"), "updated_at": updated}),
                        updated,
                    })
                })
                .collect();
            Some((name, Some(hits)))
        }));
    }
    let mut hits = Vec::new();
    let mut partial = false;
    for t in tasks {
        match t.await {
            Ok(Some((_, Some(h)))) => hits.extend(h),
            Ok(Some((_, None))) | Ok(None) | Err(_) => partial = true,
        }
    }
    hits.sort_by(|a, b| b.rank.cmp(&a.rank).then_with(|| b.updated.cmp(&a.updated)));
    hits.truncate(limit.clamp(1, 50));
    Ok(json!({
        "results": hits.into_iter().map(|h| h.value).collect::<Vec<_>>(),
        "pages": pages,
        "partial": partial,
        "took_ms": started.elapsed().as_millis() as u64,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_rank_by_how_they_match() {
        assert_eq!(rank("Acme", "acme"), 4);
        assert_eq!(rank("Acme Industries", "acme"), 3);
        assert_eq!(rank("Bharat Acme Retail", "acm"), 2);
        assert_eq!(rank("Fabricacme", "acme"), 1);
        assert_eq!(rank("Bharat Retail", "acme"), 0);
    }
}
