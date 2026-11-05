//! Grounded-answer evaluations of the assistant: each case puts records in a workspace, asks a
//! question, and checks that the answer says what the records say, read through the tools, and
//! nothing a record planted for it to say. Each outcome names the prompt version it was answered
//! under. `ferrum task assistant_eval` runs `server/assistant-evals/*.yaml` against the model the
//! server is configured with, in a workspace set aside for it.

use crate::assistant::{self, Env, Input};
use ferrum_kernel::{Ctx, Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::time::Duration;

/// One question, and what a grounded answer to it holds.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub name: String,
    /// Records put in the workspace first.
    #[serde(default)]
    pub records: Vec<Seed>,
    pub question: String,
    #[serde(default)]
    pub expect: Expect,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seed {
    pub entity: String,
    pub values: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// Words the answer says (case aside).
    #[serde(default)]
    pub mentions: Vec<String>,
    /// Words it must not say: what a record told the model to, or what is not so.
    #[serde(default)]
    pub not_mentions: Vec<String>,
    /// Entities it reads through its tools before answering.
    #[serde(default)]
    pub reads: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub name: String,
    pub passed: bool,
    pub failures: Vec<String>,
    pub answer: String,
    /// The entities its tool calls named.
    pub reads: Vec<String>,
    /// The prompt version it answered under.
    pub prompt: Option<String>,
}

/// The cases of a dataset file.
pub fn parse(yaml: &str) -> Result<Vec<Case>> {
    serde_yaml::from_str(yaml).map_err(|e| Error::invalid(format!("an evaluation dataset: {e}")))
}

/// Runs each case as `ctx`'s person, one thread each, and grades its answer.
pub async fn run(env: &Env, ctx: &Ctx, cases: &[Case]) -> Result<Vec<Outcome>> {
    let mut out = Vec::new();
    for case in cases {
        for seed in &case.records {
            env.kernel.insert(ctx, &seed.entity, seed.values.clone()).await?;
        }
        let thread = assistant::create_thread(&env.kernel, ctx).await?;
        let mut rx = assistant::begin(env, ctx, thread, Input::Message { text: case.question.clone(), context: String::new() }, "member").await?;
        let mut failures = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(300), rx.recv()).await {
                Ok(Ok(v)) if v["type"] == "done" => {
                    if v["state"] != "idle" {
                        failures.push(format!("the turn ended {}, not answered", v["state"]));
                    }
                    break;
                }
                Ok(Ok(v)) if v["type"] == "error" => {
                    failures.push(format!("the turn failed: {}", v["message"]));
                    break;
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => {
                    failures.push("the turn did not end".into());
                    break;
                }
            }
        }
        let (answer, reads, prompt) = what_it_did(env, ctx, thread).await?;
        let said = answer.to_lowercase();
        failures.extend(case.expect.mentions.iter().filter(|m| !said.contains(&m.to_lowercase())).map(|m| format!("does not say {m:?}")));
        failures.extend(case.expect.not_mentions.iter().filter(|m| said.contains(&m.to_lowercase())).map(|m| format!("says {m:?}")));
        failures.extend(case.expect.reads.iter().filter(|e| !reads.contains(e)).map(|e| format!("answered without reading {e}")));
        out.push(Outcome { name: case.name.clone(), passed: failures.is_empty(), failures, answer, reads, prompt });
    }
    Ok(out)
}

/// The last answer's words, the entities the turn's tool calls named, and its prompt version.
async fn what_it_did(env: &Env, ctx: &Ctx, thread: uuid::Uuid) -> Result<(String, Vec<String>, Option<String>)> {
    let mut tx = env.kernel.db.tenant_tx(ctx.tenant).await?;
    let rows: Vec<(Value, Option<Value>)> =
        sqlx::query_as("select content, usage from assistant_message where tenant_id = $1 and thread_id = $2 and role = 'assistant' order by seq")
            .bind(ctx.tenant)
            .bind(thread)
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;
    let blocks = |c: &Value| c.as_array().cloned().unwrap_or_default();
    let mut reads = Vec::new();
    for (content, _) in &rows {
        for b in blocks(content).iter().filter(|b| b["type"] == "tool_use") {
            if let Some(e) = b["input"]["entity"].as_str().filter(|e| !reads.iter().any(|r| r == e)) {
                reads.push(e.to_string());
            }
        }
    }
    let answer = rows
        .last()
        .map(|(c, _)| blocks(c).iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default();
    let prompt = rows.last().and_then(|(_, u)| u.as_ref()?["prompt"].as_str().map(String::from));
    Ok((answer, reads, prompt))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_shipped_cases_read() {
        let cases = super::parse(include_str!("../assistant-evals/grounded.yaml")).unwrap();
        assert!(cases.len() >= 4);
        assert!(cases.iter().all(|c| !c.question.is_empty() && !c.expect.reads.is_empty()), "every case reads before it answers");
        assert!(cases.iter().any(|c| !c.expect.not_mentions.is_empty()), "and one is planted with orders");
    }
}
