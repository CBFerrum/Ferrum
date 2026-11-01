//! How fast the two hot paths go, for `docs/limits.md`. Run on purpose, in release:
//! `cargo test --release -p ferrum-ops-contract --test load -- --ignored --nocapture`.
//! This is decoding and deciding alone, on whatever machine runs it. It says what these two cost,
//! not what a gateway sustains: that depends on its disk and its network, and is not measured here.

use chrono::{TimeDelta, Utc};
use ferrum_ops_contract::driver::{self, Memory};
use ferrum_ops_contract::{flow, sparkplug};
use serde_json::json;
use std::time::{Duration, Instant};

fn rate(what: &str, n: u64, took: Duration) {
    println!("{what}: {:.0} a second", n as f64 / took.as_secs_f64());
}

#[test]
#[ignore = "a measurement, not a test"]
fn decoding() {
    const N: u64 = 300_000;
    let d = driver::parse("name: x\nconnect: mqtt://b:1\nread:\n  - {topic: 'plant/+/state', device: '{topic.1}', values: [{metric: temp, json: $.t, scale: 0.1}, {metric: rpm, json: $.r}, {state: true, json: $.s, map: {'1': Running}}]}\n").unwrap();
    let body = br#"{"t": 215, "r": 1500, "s": 1, "other": {"a": [1, 2, 3], "b": "text"}}"#;
    let start = Instant::now();
    for _ in 0..N {
        assert_eq!(d.decode("plant/P1/state", body).len(), 3);
    }
    rate("JSON messages with three values", N, start.elapsed());

    let d = driver::parse("name: x\nconnect: modbus+tcp://p:502\nread:\n  - {values: [{metric: a, reg: 0, type: i16, scale: 0.1}, {metric: b, reg: 1, type: f32, order: cdab}, {state: true, reg: 3, map: {'1': Running}}]}\n").unwrap();
    let body = [0u8, 215, 0, 0, 0x40, 0x20, 0, 1];
    let start = Instant::now();
    for _ in 0..N * 3 {
        assert_eq!(d.decode("holding/0", &body).len(), 3);
    }
    rate("Modbus reads with three values", N * 3, start.elapsed());

    let d = driver::parse("name: x\nconnect: mqtt://b:1\nread:\n  - {topic: 'spBv1.0/g/+/n/+', format: sparkplug, device: '{topic.3}', each: $.metrics, metric: '{each.name}', json: $.value}\n").unwrap();
    let metrics: Vec<_> = (0..20).map(|i| json!({"name": format!("tag.{i}"), "value": i as f64 * 1.5})).collect();
    let body = sparkplug::encode(&json!({"metrics": metrics})).unwrap();
    let mut memory = Memory::default();
    let start = Instant::now();
    for _ in 0..N / 10 {
        assert_eq!(d.decode_with("spBv1.0/g/DDATA/n/D", &body, &mut memory).len(), 20);
    }
    rate("Sparkplug messages of twenty metrics", N / 10, start.elapsed());
}

#[test]
#[ignore = "a measurement, not a test"]
fn deciding() {
    const N: u64 = 1_000_000;
    let f = flow::parse(
        &json!({
            "nodes": [
                {"id": "t", "kind": "metric", "metric": "temp"},
                {"id": "a", "kind": "window", "over": "1m", "stat": "avg"},
                {"id": "c", "kind": "compare", "op": ">", "value": 80},
                {"id": "h", "kind": "hold", "for": "30s"},
                {"id": "x", "kind": "alarm", "name": "Hot"},
            ],
            "edges": [
                {"from": "t", "to": "a"},
                {"from": "a", "to": "c"},
                {"from": "c", "port": "yes", "to": "h"},
                {"from": "c", "port": "no", "to": "h", "input": "reset"},
                {"from": "c", "port": "no", "to": "x", "input": "clear"},
                {"from": "h", "to": "x"},
            ],
        })
        .to_string(),
    )
    .unwrap();
    let mut state = flow::State::default();
    let inputs: Vec<flow::Input> =
        (0..100).map(|d| flow::Input { device: format!("D{d}"), metric: Some("temp".into()), value: json!(70 + d % 20), ..Default::default() }).collect();
    let other = flow::Input { device: "D1".into(), metric: Some("rpm".into()), value: json!(1), ..Default::default() };
    let base = Utc::now();
    let start = Instant::now();
    for i in 0..N {
        let at = base + TimeDelta::milliseconds(i as i64);
        f.input(&mut state, &inputs[(i % 100) as usize], at);
    }
    rate("readings through a five-step flow, a hundred devices, a minute's window", N, start.elapsed());
    let start = Instant::now();
    for _ in 0..N * 5 {
        f.input(&mut state, &other, base);
    }
    rate("readings a flow does not listen for", N * 5, start.elapsed());
}
