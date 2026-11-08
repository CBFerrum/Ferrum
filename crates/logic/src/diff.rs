//! What changed between two releases of a solution, by meaning (FLC-UI-005): rules, decisions and their rows,
//! processes and each of their steps, parameters, and the capabilities it may use. Layout is not in a release,
//! so moving a box changes nothing here; a label people read, a form or an instruction is meaning and shows.
//!
//! `privilege_growth` is what a reviewer consents to again: the capabilities that change something which the
//! new release may use and the old one could not (FLC-UI-005, the package's "re-consent on privilege growth").

use crate::compile::Release;
use crate::solution::Node;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Added,
    Removed,
    Changed,
}

/// One difference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    /// `parameter`, `rule`, `decision`, `decision_row`, `process`, `step` or `capability`.
    pub area: String,
    /// What it is: a rule's id, `process/step` for a step, `decision/row` for a row.
    pub id: String,
    pub kind: Kind,
    /// For a change, the settings that differ; for a capability, its versions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
    /// For a capability: whether it changes something.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub changes_records: bool,
    /// For a change, each differing setting as it was and as it is (`null` where absent), so a reviewer reads
    /// the new recipient, not only that the input changed. A value longer than [`SHOWN`] characters of JSON
    /// is replaced by a note of its length.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, [Value; 2]>,
}

/// The most characters of JSON a changed value is shown with.
pub const SHOWN: usize = 2000;

fn shown(v: Option<&Value>) -> Value {
    let v = v.cloned().unwrap_or(Value::Null);
    let len = v.to_string().chars().count();
    if len > SHOWN { Value::String(format!("({len} characters of JSON: read it in the source)")) } else { v }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Diff {
    pub changes: Vec<Change>,
    pub privilege_growth: Vec<String>,
}

/// The top-level settings of two JSON objects that differ, `id` aside, each as it was and as it is.
fn differing(a: &Value, b: &Value) -> BTreeMap<String, [Value; 2]> {
    let (Some(ao), Some(bo)) = (a.as_object(), b.as_object()) else {
        return if a == b { BTreeMap::new() } else { BTreeMap::from([("value".to_string(), [shown(Some(a)), shown(Some(b))])]) };
    };
    let keys: BTreeSet<&String> = ao.keys().chain(bo.keys()).collect();
    keys.into_iter().filter(|k| k.as_str() != "id" && ao.get(*k) != bo.get(*k)).map(|k| (k.clone(), [shown(ao.get(k)), shown(bo.get(k))])).collect()
}

fn changed(area: &str, id: &str, o: &Value, n: &Value) -> Change {
    let values = differing(o, n);
    Change { area: area.into(), id: id.into(), kind: Kind::Changed, fields: values.keys().cloned().collect(), changes_records: false, values }
}

/// Items by id, as JSON.
fn by_id<T: Serialize>(items: &[T], id: impl Fn(&T) -> String) -> BTreeMap<String, Value> {
    items.iter().map(|i| (id(i), serde_json::to_value(i).unwrap_or(Value::Null))).collect()
}

fn compare(area: &str, old: &BTreeMap<String, Value>, new: &BTreeMap<String, Value>, out: &mut Vec<Change>) {
    for (id, n) in new {
        match old.get(id) {
            None => out.push(Change { area: area.into(), id: id.clone(), kind: Kind::Added, fields: vec![], changes_records: false, values: BTreeMap::new() }),
            Some(o) if o != n => out.push(changed(area, id, o, n)),
            _ => {}
        }
    }
    for id in old.keys().filter(|id| !new.contains_key(*id)) {
        out.push(Change { area: area.into(), id: id.clone(), kind: Kind::Removed, fields: vec![], changes_records: false, values: BTreeMap::new() });
    }
}

/// Every step of a process, structured steps' blocks included, by `process/step`, without the blocks they
/// hold (those steps are compared on their own).
fn steps(process: &str, nodes: &[Node], out: &mut BTreeMap<String, Value>) {
    for n in nodes {
        let mut v = serde_json::to_value(n).unwrap_or(Value::Null);
        if let Some(m) = v.as_object_mut() {
            for held in ["branches", "body"] {
                if let Some(b) = m.get_mut(held) {
                    // What a structured step holds is compared step by step; here only which blocks it has.
                    let held_blocks = match &*b {
                        Value::Array(list) => Value::Array(list.iter().map(|x| serde_json::json!({"id": x["id"], "entry": x["entry"]})).collect()),
                        other => serde_json::json!({"id": other["id"], "entry": other["entry"]}),
                    };
                    *b = held_blocks;
                }
            }
        }
        out.insert(format!("{process}/{}", n.id()), v);
        for b in n.blocks() {
            steps(process, &b.nodes, out);
        }
    }
}

/// What changed from `old` (the release that runs now, if any) to `new`.
pub fn diff(old: Option<&Release>, new: &Release) -> Diff {
    let mut changes = vec![];
    let empty = Release { lock: vec![], ..new.clone() };
    let empty = Release {
        ir: crate::compile::Ir { parameters: BTreeMap::new(), rules: vec![], decisions: vec![], processes: vec![], types: BTreeMap::new() },
        ..empty
    };
    let old = old.unwrap_or(&empty);

    let params = |r: &Release| r.ir.parameters.iter().map(|(k, v)| (k.clone(), serde_json::to_value(v).unwrap_or(Value::Null))).collect::<BTreeMap<_, _>>();
    compare("parameter", &params(old), &params(new), &mut changes);
    compare("rule", &by_id(&old.ir.rules, |r| r.id.clone()), &by_id(&new.ir.rules, |r| r.id.clone()), &mut changes);

    // A decision's rows are compared one by one; the table itself on everything else.
    let tables = |r: &Release| {
        r.ir.decisions
            .iter()
            .map(|d| {
                let mut v = serde_json::to_value(d).unwrap_or(Value::Null);
                if let Some(m) = v.as_object_mut() {
                    m.remove("rows");
                }
                (d.id.clone(), v)
            })
            .collect::<BTreeMap<_, _>>()
    };
    compare("decision", &tables(old), &tables(new), &mut changes);
    let rows = |r: &Release| {
        let mut out = BTreeMap::new();
        for d in &r.ir.decisions {
            for row in &d.rows {
                out.insert(format!("{}/{}", d.id, row.id), serde_json::to_value(row).unwrap_or(Value::Null));
            }
        }
        out
    };
    compare("decision_row", &rows(old), &rows(new), &mut changes);

    // A process on its own settings; its steps one by one.
    let procs = |r: &Release| {
        r.ir.processes
            .iter()
            .map(|p| {
                let mut v = serde_json::to_value(p).unwrap_or(Value::Null);
                if let Some(m) = v.as_object_mut() {
                    m.remove("nodes");
                }
                (p.id.clone(), v)
            })
            .collect::<BTreeMap<_, _>>()
    };
    compare("process", &procs(old), &procs(new), &mut changes);
    let all_steps = |r: &Release| {
        let mut out = BTreeMap::new();
        for p in &r.ir.processes {
            steps(&p.id, &p.nodes, &mut out);
        }
        out
    };
    compare("step", &all_steps(old), &all_steps(new), &mut changes);

    // Capabilities: added or removed, or another version or contract of the same one.
    let olds: BTreeMap<&str, _> = old.lock.iter().map(|l| (l.id.as_str(), l)).collect();
    let news: BTreeMap<&str, _> = new.lock.iter().map(|l| (l.id.as_str(), l)).collect();
    let mut growth = vec![];
    for (id, n) in &news {
        match olds.get(id) {
            None => {
                if n.effect.changes() {
                    growth.push(id.to_string());
                }
                changes.push(Change {
                    area: "capability".into(),
                    id: id.to_string(),
                    kind: Kind::Added,
                    fields: vec![n.version.clone()],
                    changes_records: n.effect.changes(),
                    values: BTreeMap::new(),
                });
            }
            Some(o) if o.digest != n.digest || o.version != n.version => {
                // A query that now changes something is new power too.
                if n.effect.changes() && !o.effect.changes() {
                    growth.push(id.to_string());
                }
                changes.push(Change {
                    area: "capability".into(),
                    id: id.to_string(),
                    kind: Kind::Changed,
                    fields: vec![o.version.clone(), n.version.clone()],
                    changes_records: n.effect.changes(),
                    values: BTreeMap::new(),
                });
            }
            _ => {}
        }
    }
    for (id, o) in olds.iter().filter(|(id, _)| !news.contains_key(*id)) {
        changes.push(Change {
            area: "capability".into(),
            id: id.to_string(),
            kind: Kind::Removed,
            fields: vec![o.version.clone()],
            changes_records: o.effect.changes(),
            values: BTreeMap::new(),
        });
    }
    // In a stable order: by area, then what, then how.
    changes.sort_by(|a, b| (&a.area, &a.id, a.kind).cmp(&(&b.area, &b.id, b.kind)));
    growth.sort();
    Diff { changes, privilege_growth: growth }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::RuntimeProfile;
    use crate::testkit::{compile_value, purchase, structured};
    use serde_json::json;

    fn release(v: Value) -> Release {
        compile_value(v, RuntimeProfile::FullProcess).unwrap_or_else(|d| panic!("{d:#?}"))
    }

    fn find<'a>(d: &'a Diff, area: &str, id: &str) -> Option<&'a Change> {
        d.changes.iter().find(|c| c.area == area && c.id == id)
    }

    #[test]
    fn a_first_release_is_all_new_and_asks_for_its_changing_capabilities() {
        let r = release(purchase());
        let d = diff(None, &r);
        assert!(find(&d, "process", "purchase_exception").is_some_and(|c| c.kind == Kind::Added));
        assert!(find(&d, "step", "purchase_exception/review").is_some_and(|c| c.kind == Kind::Added));
        assert_eq!(d.privilege_growth, vec!["ferrum.mail".to_string(), "procurement.material_request.make_purchase_order".to_string()]);
        assert!(find(&d, "capability", "procurement.material_request.read").is_some_and(|c| !c.changes_records), "a query is no new power");
    }

    #[test]
    fn the_same_release_has_no_changes_and_layout_is_no_change() {
        let r = release(purchase());
        assert_eq!(diff(Some(&r), &r), Diff::default());
        let mut moved = purchase();
        moved["ui"]["layout"]["review"]["x"] = json!(400);
        assert_eq!(diff(Some(&r), &release(moved)), Diff::default(), "boxes moved");
    }

    #[test]
    fn changes_are_named_by_what_they_are() {
        let old = release(purchase());
        let mut v = purchase();
        v["decisions"][0]["rows"][0]["match"]["total"]["gt"]["amount"] = json!("200000.00");
        v["processes"][0]["nodes"][2]["label"] = json!("Two reviewers look at it");
        v["parameters"]["review_limit"]["value"]["amount"] = json!("150000.00");
        v["tests"] = json!([]);
        let new = release(v);
        let d = diff(Some(&old), &new);
        assert!(
            find(&d, "decision_row", "requires_review/high_amount").is_some_and(|c| c.kind == Kind::Changed && c.fields == vec!["match".to_string()]),
            "{d:#?}"
        );
        assert!(find(&d, "parameter", "review_limit").is_some_and(|c| c.fields == vec!["value".to_string()]), "{d:#?}");
        assert!(find(&d, "step", "purchase_exception/review").is_some_and(|c| c.fields == vec!["label".to_string()]), "a label people read is meaning: {d:#?}");
        let review = find(&d, "step", "purchase_exception/review").unwrap();
        assert_eq!(review.values["label"][1], json!("Two reviewers look at it"), "the reviewer reads what it says now");
        assert_ne!(review.values["label"][0], review.values["label"][1]);
        assert!(d.privilege_growth.is_empty(), "nothing new may change records");
    }

    #[test]
    fn a_long_value_is_named_by_its_length() {
        let long = json!("x".repeat(SHOWN + 10));
        let values = differing(&json!({"id": "a", "form": long}), &json!({"id": "a", "form": "short"}));
        assert_eq!(values["form"][1], json!("short"));
        assert!(values["form"][0].as_str().unwrap().starts_with(&format!("({} characters of JSON", SHOWN + 12)), "{values:?}");
        assert_eq!(differing(&json!({"id": "a", "to": "x"}), &json!({"id": "b", "to": "x"})), BTreeMap::new(), "the id is the change's own name");
    }

    #[test]
    fn steps_inside_blocks_are_compared_one_by_one_and_new_power_shows() {
        let old = release(purchase());
        let new = release(structured());
        let d = diff(Some(&old), &new);
        assert!(find(&d, "step", "purchase_exception/hold_line").is_some_and(|c| c.kind == Kind::Added), "a step inside a for-each: {d:#?}");
        assert!(find(&d, "step", "purchase_exception/decision").is_some_and(|c| c.kind == Kind::Removed));
        assert!(find(&d, "step", "purchase_exception/review").is_some_and(|c| c.kind == Kind::Changed), "moved into the repeat's body, and changed");
        assert!(d.privilege_growth.contains(&"example.inventory.place_hold".to_string()), "{:?}", d.privilege_growth);
        assert!(find(&d, "capability", "example.inventory.release_hold").is_some_and(|c| c.kind == Kind::Added && c.changes_records));
    }
}
