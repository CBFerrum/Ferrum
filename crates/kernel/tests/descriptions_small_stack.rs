//! A large description (Social's Telegram, fixtures/telegram.json: twelve operations, a result picking among a dozen
//! answers by nested `?:`) loads, validates and has its result worked out on a thread the size of a server's worker
//! (2 MiB). It overflowed before CEL's parser ran on a stack of its own for expressions that nest (13 November 2026).

use ferrum_kernel::connections::Description;
use serde_json::{Value, json};

#[test]
fn a_large_description_on_a_workers_stack() {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(|| {
            let d: Description = serde_json::from_str(include_str!("fixtures/telegram.json")).expect("loads");
            assert_eq!(d.problem(), None, "validates");
            let raw: Value = serde_json::from_str(include_str!("fixtures/telegram.json")).unwrap();
            let expr = raw["operations"]["publish"]["result"]["id"]["expr"].as_str().expect("the nested result");
            let e = ferrum_meta::Expr::compile(expr).unwrap();
            let doc = json!({});
            let scope = ferrum_meta::expr::Scope { doc: &doc, old: None, parent: None, user: &json!({}) };
            let mut answer: serde_json::Map<String, Value> = ["text", "video", "document"].into_iter().map(|k| (k.to_string(), Value::Null)).collect();
            answer.extend((2..10).map(|n| (format!("album_{n}"), Value::Null)));
            answer.insert("photo".into(), json!({"message_id": 42}));
            let out = e.eval_with(&scope, &[("inputs", &json!({"account_id": "c1"})), ("answer", &Value::Object(answer))]).expect("runs");
            assert_eq!(out, json!("c1:42"));
        })
        .unwrap()
        .join()
        .expect("no overflow on 2 MiB");
}
