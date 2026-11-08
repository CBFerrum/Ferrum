//! Studio's endpoints for customer solutions, their runs and people's tasks (docs/logic/studio.md). Each is a
//! kernel function's: the kernel decides who may do what, and these only carry requests and answers.
//!
//! | Method & path | Body / query → response |
//! |---|---|
//! | `GET /solutions` | → `{drafts, releases, calendars, profile}` |
//! | `POST /solutions/drafts` | `{source}` → `{id, version}` |
//! | `GET /solutions/drafts/{id}` | → the draft; `ETag: "v{version}"` |
//! | `PUT /solutions/drafts/{id}` | `If-Match: "v{version}"`, `{source}` → `{version}`; 409 when another save came first |
//! | `DELETE /solutions/drafts/{id}` | `If-Match: "{version}"` |
//! | `POST /solutions/drafts/{id}/release` | `If-Match: "{version}"` → the kept release |
//! | `POST /solutions/import/automations` | each automation as a draft Solution (administrators) |
//! | `POST /solutions/compile` | `{source}` → `{ok, release?, diagnostics}`; keeps nothing |
//! | `POST /solutions/simulate` | `{source \| release, scenario}` → the timeline; reaches nothing |
//! | `GET /solutions/releases/{id}` | → the release, what runs now, reviews, and the semantic diff |
//! | `POST /solutions/releases/{id}/review` | `{digest, privileges, approve, reason}` |
//! | `POST /solutions/releases/{id}/activate` | `{expected_generation}` → `{generation}` |
//! | `POST /solutions/{solution}/deactivate` | `{expected_generation}` |
//! | `GET /solutions/refusals` | evidence of saves the rules refused |
//! | `GET /runs` | `?cursor=` → `{runs, cursor, resync}`: the runs, or those that changed since the cursor |
//! | `GET /runs/{id}` | → the run with its tasks |
//! | `POST /runs/{id}/cancel` | `{reason}` |
//! | `GET /runs/about/{entity}/{record}` | → the processes about a record its reader may see |
//! | `GET /tasks` | → the open tasks the caller may decide (administrators: what needs recovering too) |
//! | `GET /tasks/{id}` | → the task and what it is about |
//! | `POST /tasks/{id}/decide` | `{approve, digest, values?, reason?}` |
//! | `POST /tasks/{id}/delegate` | `{to (an email or a user id), reason?}` |
//! | `POST /tasks/{id}/done` | `{task, values?}`: one of a case's tasks |
//! | `POST /tasks/{id}/close` | closes a case |
//! | `POST /tasks/{id}/recover` | `{resume, reason}` |
//! | `POST /tasks/{id}/acknowledge` | `{reason}` |
//! | `GET /calendars`, `PUT /calendars/{name}` | `{zone, weekends, holidays}` → `{revision}` |

use ferrum_kernel::{Ctx, Error, Kernel, Result, SiteRequest, SiteResponse};
use ferrum_logic::calendar::Calendar;
use serde_json::{Value, json};
use uuid::Uuid;

fn ok(v: Value) -> SiteResponse {
    SiteResponse::json(200, &v)
}

fn id(seg: &str, what: &str) -> Result<Uuid> {
    seg.parse().map_err(|_| Error::NotFound(what.into()))
}

fn body(req: &SiteRequest) -> Result<Value> {
    if req.body.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    serde_json::from_slice(&req.body).map_err(|e| Error::invalid(format!("the request body must be JSON: {e}")))
}

fn text<'a>(b: &'a Value, k: &str) -> &'a str {
    b.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// The version an editor read, from `If-Match: "v3"` as the platform's records take it (or `"3"`, `3`, `W/"v3"`).
fn if_match(req: &SiteRequest) -> Result<i32> {
    req.headers
        .get("if-match")
        .and_then(Value::as_str)
        .map(|v| v.trim().trim_start_matches("W/").trim_matches('"').trim_start_matches('v'))
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| Error::field("If-Match", "say which version you edited: If-Match: \"<version>\""))
}

fn generation(b: &Value) -> Option<i64> {
    b.get("expected_generation").and_then(Value::as_i64)
}

/// The routes this module answers, or `None` for the caller to go on.
pub async fn handle(k: &Kernel, ctx: &Ctx, req: &SiteRequest, path: &[&str]) -> Option<Result<SiteResponse>> {
    let m = req.method.as_str();
    let routed = matches!(path.first(), Some(&("solutions" | "runs" | "tasks" | "calendars")));
    if !routed {
        return None;
    }
    Some(route(k, ctx, req, m, path).await)
}

async fn route(k: &Kernel, ctx: &Ctx, req: &SiteRequest, m: &str, path: &[&str]) -> Result<SiteResponse> {
    Ok(match (m, path) {
        ("GET", ["solutions"]) => ok(json!({
            "drafts": k.logic_drafts(ctx).await?,
            "releases": if ctx.is_admin { k.logic_releases(ctx).await? } else { vec![] },
            "calendars": k.logic_calendars(ctx).await?,
            "profile": k.logic_profile(),
        })),
        ("POST", ["solutions", "drafts"]) => ok(k.create_logic_draft(ctx, text(&body(req)?, "source")).await?),
        ("GET", ["solutions", "drafts", d]) => {
            let draft = k.logic_draft(ctx, id(d, "draft")?).await?;
            let mut resp = ok(draft.clone());
            resp.headers.push(("etag".into(), format!("\"v{}\"", draft["version"])));
            resp
        }
        ("PUT", ["solutions", "drafts", d]) => ok(k.save_logic_draft(ctx, id(d, "draft")?, if_match(req)?, text(&body(req)?, "source")).await?),
        ("DELETE", ["solutions", "drafts", d]) => {
            k.delete_logic_draft(ctx, id(d, "draft")?, if_match(req)?).await?;
            ok(json!({}))
        }
        ("POST", ["solutions", "drafts", d, "release"]) => ok(k.release_logic_draft(ctx, id(d, "draft")?, if_match(req)?).await?),
        // Each automation as a draft Solution, for a person to finish and someone else to review (FLC-MIG-001).
        ("POST", ["solutions", "import", "automations"]) => ok(json!({"imported": k.import_automations(ctx).await?})),
        ("POST", ["solutions", "compile"]) => {
            let b = body(req)?;
            let source = text(&b, "source");
            if source.len() > ferrum_kernel::authoring::MAX_SOURCE {
                return Err(Error::field("source", "a solution's source is at most 1 MB"));
            }
            match k.compile_solution(ctx, source).await? {
                Ok(r) => ok(json!({
                    "ok": true,
                    "release": {"solution": r.solution, "version": r.version, "source_digest": r.source_digest, "artifact_digest": r.artifact_digest,
                        "lock": r.lock, "types": r.ir.types},
                    "diagnostics": r.notes,
                })),
                Err(d) => ok(json!({"ok": false, "diagnostics": d})),
            }
        }
        ("POST", ["solutions", "simulate"]) => {
            let b = body(req)?;
            let release = b.get("release").and_then(Value::as_str).map(|s| id(s, "release")).transpose()?;
            let source = b.get("source").and_then(Value::as_str);
            ok(k.simulate_solution(ctx, source, release, b.get("scenario").cloned().unwrap_or(Value::Null)).await?)
        }
        ("GET", ["solutions", "refusals"]) => ok(json!(k.logic_refusals(ctx, 200).await?)),
        ("GET", ["solutions", "releases", r]) => ok(k.logic_release_review(ctx, id(r, "release")?).await?),
        ("POST", ["solutions", "releases", r, "review"]) => {
            let b = body(req)?;
            let privileges: Vec<String> =
                b.get("privileges").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
            let approve = b.get("approve").and_then(Value::as_bool).ok_or_else(|| Error::field("approve", "true or false"))?;
            k.review_release_consenting(ctx, id(r, "release")?, text(&b, "digest"), &privileges, approve, text(&b, "reason")).await?;
            ok(json!({}))
        }
        ("POST", ["solutions", "releases", r, "activate"]) => {
            let generation = k.activate_release(ctx, id(r, "release")?, generation(&body(req)?)).await?;
            ok(json!({"generation": generation}))
        }
        ("POST", ["solutions", solution, "deactivate"]) => {
            let g = generation(&body(req)?).ok_or_else(|| Error::field("expected_generation", "the generation you saw running"))?;
            k.deactivate_solution(ctx, solution, g).await?;
            ok(json!({}))
        }
        ("GET", ["runs"]) => {
            let cursor = req.query.get("cursor").and_then(Value::as_str).filter(|c| !c.is_empty());
            ok(k.logic_run_changes(ctx, cursor).await?)
        }
        ("GET", ["runs", "about", entity, record]) => ok(json!(k.logic_runs_about(ctx, entity, id(record, "record")?).await?)),
        ("GET", ["runs", r]) => ok(k.logic_run(ctx, id(r, "run")?).await?),
        ("POST", ["runs", r, "cancel"]) => {
            k.cancel_logic_run(ctx, id(r, "run")?, text(&body(req)?, "reason")).await?;
            ok(json!({}))
        }
        ("GET", ["tasks"]) => ok(json!(k.my_logic_tasks(ctx).await?)),
        ("GET", ["tasks", t]) => ok(k.logic_task(ctx, id(t, "task")?).await?),
        ("POST", ["tasks", t, "decide"]) => {
            let b = body(req)?;
            let approve = b.get("approve").and_then(Value::as_bool).ok_or_else(|| Error::field("approve", "true or false"))?;
            let values = b.get("values").cloned().unwrap_or(json!({}));
            ok(k.decide_logic_task(ctx, id(t, "task")?, approve, text(&b, "digest"), &values, text(&b, "reason")).await?)
        }
        ("POST", ["tasks", t, "delegate"]) => {
            let b = body(req)?;
            // By the email people know each other by, or a user id.
            let to = match Uuid::parse_str(text(&b, "to").trim()) {
                Ok(u) => u,
                Err(_) => k.member_by_email(ctx, text(&b, "to")).await?,
            };
            k.delegate_logic_task(ctx, id(t, "task")?, to, text(&b, "reason")).await?;
            ok(json!({}))
        }
        ("POST", ["tasks", t, "done"]) => {
            let b = body(req)?;
            k.case_task_done(ctx, id(t, "task")?, text(&b, "task"), &b.get("values").cloned().unwrap_or(json!({}))).await?;
            ok(json!({}))
        }
        ("POST", ["tasks", t, "close"]) => {
            k.close_case(ctx, id(t, "task")?).await?;
            ok(json!({}))
        }
        ("POST", ["tasks", t, "recover"]) => {
            let b = body(req)?;
            let resume = b.get("resume").and_then(Value::as_bool).ok_or_else(|| Error::field("resume", "true resumes, false ends it"))?;
            k.recover_logic_task(ctx, id(t, "task")?, resume, text(&b, "reason")).await?;
            ok(json!({}))
        }
        ("POST", ["tasks", t, "acknowledge"]) => {
            k.acknowledge_logic_attention(ctx, id(t, "task")?, text(&body(req)?, "reason")).await?;
            ok(json!({}))
        }
        ("GET", ["calendars"]) => ok(json!(k.logic_calendars(ctx).await?)),
        ("PUT", ["calendars", name]) => {
            let mut b = body(req)?;
            b["name"] = json!(name);
            b["revision"] = json!(0);
            let cal: Calendar = serde_json::from_value(b).map_err(|e| Error::field("calendar", e.to_string()))?;
            ok(json!({"revision": k.set_logic_calendar(ctx, &cal).await?}))
        }
        _ => return Err(Error::NotFound("endpoint".into())),
    })
}
