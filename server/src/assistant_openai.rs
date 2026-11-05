//! The assistant over an OpenAI-compatible chat API (`FERRUM_ASSISTANT_PROTOCOL=openai`): the
//! request the assistant builds in Anthropic's shape goes out as a chat completion, and the
//! answer's stream comes back as the events the assistant reads. Text, tool calls and their
//! results, and usage carry over; reasoning, citations, caching and fallbacks are Anthropic's and
//! are left out.

use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// The text of a content that is a string or a list of blocks.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// The chat completion for an Anthropic Messages request.
pub fn request(body: &Value) -> Value {
    let mut messages = Vec::new();
    let system = text_of(&body["system"]);
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system}));
    }
    for m in body["messages"].as_array().into_iter().flatten() {
        let role = m["role"].as_str().unwrap_or("user");
        let Some(blocks) = m["content"].as_array() else {
            messages.push(json!({"role": role, "content": text_of(&m["content"])}));
            continue;
        };
        if role == "assistant" {
            let text = text_of(&m["content"]);
            let calls: Vec<Value> = blocks
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .map(|b| json!({"id": b["id"], "type": "function", "function": {"name": b["name"], "arguments": b["input"].to_string()}}))
                .collect();
            let mut out = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) }});
            if !calls.is_empty() {
                out["tool_calls"] = json!(calls);
            }
            messages.push(out);
            continue;
        }
        // A person's turn: what tools answered goes first, each as its own message, then their words.
        for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
            let content = text_of(&b["content"]);
            let content = if b["is_error"] == json!(true) { format!("Error: {content}") } else { content };
            messages.push(json!({"role": "tool", "tool_call_id": b["tool_use_id"], "content": content}));
        }
        let text = text_of(&m["content"]);
        if !text.is_empty() {
            messages.push(json!({"role": "user", "content": text}));
        }
    }
    let tools: Vec<Value> = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["input_schema"].is_object())
        .map(|t| json!({"type": "function", "function": {"name": t["name"], "description": t["description"], "parameters": t["input_schema"]}}))
        .collect();
    let mut out = json!({
        "model": body["model"],
        "max_tokens": body["max_tokens"],
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": messages,
    });
    if !tools.is_empty() {
        out["tools"] = json!(tools);
    }
    out
}

/// Turns a chat completion's streamed chunks into Anthropic's stream events.
#[derive(Default)]
pub struct Stream {
    /// The text block under way, by its index.
    text: Option<usize>,
    /// Tool calls under way: the API's index to the block's.
    calls: BTreeMap<u64, usize>,
    next: usize,
    started: bool,
    stopped: bool,
}

impl Stream {
    fn open(&mut self, block: Value, out: &mut Vec<Value>) -> usize {
        let i = self.next;
        self.next += 1;
        out.push(json!({"type": "content_block_start", "index": i, "content_block": block}));
        i
    }

    /// The events one chunk makes.
    pub fn events(&mut self, chunk: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        // The message's start names the model, and opens the usage the deltas add to.
        if !self.started {
            self.started = true;
            out.push(json!({"type": "message_start", "message": {"model": chunk["model"], "usage": {"input_tokens": 0, "output_tokens": 0}}}));
        }
        if let Some(e) = chunk.get("error") {
            out.push(json!({"type": "error", "error": {"type": "api_error", "message": e["message"].as_str().unwrap_or("the model's service failed")}}));
            return out;
        }
        for choice in chunk["choices"].as_array().into_iter().flatten() {
            let delta = &choice["delta"];
            if let Some(t) = delta["content"].as_str().filter(|t| !t.is_empty()) {
                let i = match self.text {
                    Some(i) => i,
                    None => {
                        let i = self.open(json!({"type": "text", "text": ""}), &mut out);
                        self.text = Some(i);
                        i
                    }
                };
                out.push(json!({"type": "content_block_delta", "index": i, "delta": {"type": "text_delta", "text": t}}));
            }
            for call in delta["tool_calls"].as_array().into_iter().flatten() {
                let at = call["index"].as_u64().unwrap_or(0);
                let i = match self.calls.get(&at) {
                    Some(i) => *i,
                    None => {
                        let block = json!({"type": "tool_use", "id": call["id"], "name": call["function"]["name"], "input": {}});
                        let i = self.open(block, &mut out);
                        self.calls.insert(at, i);
                        i
                    }
                };
                if let Some(args) = call["function"]["arguments"].as_str().filter(|a| !a.is_empty()) {
                    out.push(json!({"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": args}}));
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                let blocks: Vec<usize> = self.text.into_iter().chain(self.calls.values().copied()).collect();
                for i in blocks {
                    out.push(json!({"type": "content_block_stop", "index": i}));
                }
                let reason = match reason {
                    "tool_calls" | "function_call" => "tool_use",
                    "length" => "max_tokens",
                    _ => "end_turn",
                };
                out.push(json!({"type": "message_delta", "delta": {"stop_reason": reason}}));
                self.stopped = true;
            }
        }
        if let Some(u) = chunk["usage"].as_object() {
            let mut usage = Map::new();
            usage.insert("input_tokens".into(), u.get("prompt_tokens").cloned().unwrap_or(json!(0)));
            usage.insert("output_tokens".into(), u.get("completion_tokens").cloned().unwrap_or(json!(0)));
            out.push(json!({"type": "message_delta", "delta": {}, "usage": usage}));
        }
        out
    }

    /// What the stream's end makes: the message's stop once it said why it stopped.
    pub fn end(&mut self) -> Vec<Value> {
        if self.stopped { vec![json!({"type": "message_stop"})] } else { vec![] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_goes_out_as_a_chat_completion() {
        let body = json!({
            "model": "m", "max_tokens": 100, "stream": true, "thinking": {"type": "adaptive"},
            "system": [{"type": "text", "text": "Be brief."}],
            "tools": [{"name": "find", "description": "Find records", "input_schema": {"type": "object"}}, {"type": "web_search"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Who is Ana?"}]},
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "hm"}, {"type": "text", "text": "Looking."}, {"type": "tool_use", "id": "t1", "name": "find", "input": {"q": "Ana"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "Ana Rao"}]}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t2", "is_error": true, "content": "denied"}, {"type": "text", "text": "And Bo?"}]},
            ],
        });
        let out = request(&body);
        assert_eq!(out["messages"][0], json!({"role": "system", "content": "Be brief."}));
        assert_eq!(out["messages"][1], json!({"role": "user", "content": "Who is Ana?"}));
        assert_eq!(out["messages"][2]["content"], json!("Looking."), "the reasoning stays behind");
        assert_eq!(out["messages"][2]["tool_calls"][0]["function"], json!({"name": "find", "arguments": "{\"q\":\"Ana\"}"}));
        assert_eq!(out["messages"][3], json!({"role": "tool", "tool_call_id": "t1", "content": "Ana Rao"}));
        assert_eq!(out["messages"][4], json!({"role": "tool", "tool_call_id": "t2", "content": "Error: denied"}));
        assert_eq!(out["messages"][5], json!({"role": "user", "content": "And Bo?"}));
        assert_eq!(out["tools"].as_array().map(Vec::len), Some(1), "a server tool of Anthropic's is left out");
        assert!(out.get("thinking").is_none() && out["stream_options"]["include_usage"] == json!(true));
    }

    #[test]
    fn its_answer_comes_back_as_the_events_the_assistant_reads() {
        let mut s = Stream::default();
        let mut events = Vec::new();
        for chunk in [
            json!({"choices": [{"delta": {"role": "assistant", "content": "Ana "}}]}),
            json!({"choices": [{"delta": {"content": "is here."}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "find", "arguments": "{\"q\""}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": ":\"Bo\"}"}}]}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 12, "completion_tokens": 7}}),
        ] {
            events.extend(s.events(&chunk));
        }
        events.extend(s.end());
        let kinds: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "content_block_stop",
                "message_delta",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(events[4]["content_block"], json!({"type": "tool_use", "id": "c1", "name": "find", "input": {}}));
        assert_eq!(events[9]["delta"]["stop_reason"], "tool_use");
        assert_eq!(events[10]["usage"], json!({"input_tokens": 12, "output_tokens": 7}));
        let mut cut = Stream::default();
        cut.events(&json!({"choices": [{"delta": {"content": "half"}}]}));
        assert!(cut.end().is_empty(), "an answer that never said why it stopped was cut off");
        assert_eq!(Stream::default().events(&json!({"error": {"message": "no"}}))[1]["type"], "error");
    }
}
