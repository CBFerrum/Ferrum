//! Example Ferrum plugin: refuses sales discounts above 20 % unless a Sales Manager saves the
//! document. Shows the whole plugin contract: `ferrum_alloc`, `ferrum_hook` and JSON in/out.

use serde_json::{Value, json};

const LIMIT: f64 = 20.0;

/// Hands the host a buffer of `len` bytes for the input document.
#[unsafe(no_mangle)]
pub extern "C" fn ferrum_alloc(len: i32) -> i32 {
    let mut buf = Vec::<u8>::with_capacity(len.max(0) as usize);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr as i32
}

/// Reads the input JSON and returns `(pointer << 32) | length` of the reply JSON.
#[unsafe(no_mangle)]
pub extern "C" fn ferrum_hook(ptr: i32, len: i32) -> i64 {
    // SAFETY: the host wrote exactly `len` bytes into the buffer returned by `ferrum_alloc`.
    let input = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let reply = handle(input).to_string().into_bytes().into_boxed_slice();
    let (p, l) = (reply.as_ptr() as i64, reply.len() as i64);
    std::mem::forget(reply);
    (p << 32) | l
}

fn handle(input: &[u8]) -> Value {
    let Ok(req) = serde_json::from_slice::<Value>(input) else { return json!({"error": "discount-guard: unreadable input"}) };
    let manager = req["user"]["roles"].as_array().is_some_and(|roles| roles.iter().any(|r| r == "Sales Manager"));
    if manager {
        return json!({});
    }
    // One error per offending line, pointed at that row of the items table.
    let errors: Vec<Value> = req["doc"]["items"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, line)| {
            let pct = line["discount_percent"].as_str()?.parse::<f64>().ok()?;
            (pct > LIMIT).then(|| {
                json!({"field": "discount_percent", "row": ["items", i], "message": format!("discounts above {LIMIT}% need a Sales Manager (found {pct}%)")})
            })
        })
        .collect();
    if errors.is_empty() { json!({}) } else { json!({"errors": errors}) }
}
