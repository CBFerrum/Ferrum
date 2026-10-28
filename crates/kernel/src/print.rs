//! HTML print formats (Jinja syntax via MiniJinja, auto-escaped, fuel-limited so a tenant
//! template cannot loop forever). Browsers print or save the result as PDF.

use crate::Kernel;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::value::parse_decimal;
use ferrum_meta::{EffEntity, FieldType};
use minijinja::{AutoEscape, Environment, Value as JValue};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

const DEFAULT_TEMPLATE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>{{ entity.label }} {{ doc.number or doc.id }}</title>
<style>
 body{font:13px/1.45 system-ui,-apple-system,Segoe UI,sans-serif;color:#111;margin:32px}
 h1{font-size:20px;margin:0 0 4px} .muted{color:#666}
 table{border-collapse:collapse;width:100%;margin:12px 0} th,td{border-bottom:1px solid #ddd;padding:6px 8px;text-align:left;vertical-align:top}
 th{background:#f6f6f6;font-weight:600} td.num,th.num{text-align:right;font-variant-numeric:tabular-nums}
 .grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:4px 32px;margin:16px 0}
 .status{display:inline-block;padding:2px 8px;border-radius:10px;background:#eee;font-size:11px}
 @media print{body{margin:0}}
</style></head><body>
<h1>{{ entity.label }} <span class="muted">{{ doc.number or "" }}</span></h1>
<span class="status">{{ ["Draft","Submitted","Cancelled"][doc.docstatus or 0] }}</span>
<div class="grid">
{% for f in entity.fields if f.kind == "value" and doc[f.name] is not none and doc[f.name] != "" %}
 <div><span class="muted">{{ f.label }}</span><br>{{ display(f, doc) }}</div>
{% endfor %}
</div>
{% for t in entity.fields if t.kind == "table" and doc[t.name] %}
<h3>{{ t.label }}</h3>
<table><thead><tr>{% for c in t.columns %}<th class="{{ 'num' if c.numeric }}">{{ c.label }}</th>{% endfor %}</tr></thead>
<tbody>{% for row in doc[t.name] %}<tr>{% for c in t.columns %}<td class="{{ 'num' if c.numeric }}">{{ display(c, row) }}</td>{% endfor %}</tr>{% endfor %}</tbody></table>
{% endfor %}
<p class="muted">Printed {{ now }}</p>
</body></html>"#;

fn group_thousands(int_part: &str) -> String {
    let (sign, digits) = int_part.strip_prefix('-').map(|d| ("-", d)).unwrap_or(("", int_part));
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{sign}{out}")
}

/// `1234567.5` → `1,234,567.50` with `precision` decimals.
pub fn format_money(v: &Value, precision: u32) -> String {
    let Ok(d) = parse_decimal(v) else { return String::new() };
    let mut d = d.round_dp_with_strategy(precision, rust_decimal::RoundingStrategy::MidpointAwayFromZero);
    d.rescale(precision);
    let s = d.to_string();
    match s.split_once('.') {
        Some((i, f)) => format!("{}.{f}", group_thousands(i)),
        None => group_thousands(&s),
    }
}

fn field_meta(ent: &EffEntity, schema: &ferrum_meta::Schema) -> Value {
    let fields: Vec<Value> = ent
        .def
        .fields
        .iter()
        .filter(|f| !f.hidden)
        .map(|f| {
            if f.ftype == FieldType::Table {
                let cols: Vec<Value> = f
                    .of
                    .as_deref()
                    .and_then(|c| schema.entity(c))
                    .map(|c| {
                        c.def
                            .fields
                            .iter()
                            .filter(|cf| !cf.hidden && (cf.in_list || c.def.list.columns.contains(&cf.name)))
                            .map(|cf| json!({"name": cf.name, "label": cf.label(), "type": cf.ftype, "numeric": cf.ftype.is_numeric(), "precision": cf.precision}))
                            .collect()
                    })
                    .unwrap_or_default();
                json!({"name": f.name, "label": f.label(), "kind": "table", "columns": cols})
            } else {
                json!({"name": f.name, "label": f.label(), "kind": "value", "type": f.ftype, "numeric": f.ftype.is_numeric(), "precision": f.precision})
            }
        })
        .collect();
    json!({"name": ent.def.name, "label": ent.def.label, "fields": fields})
}

impl Kernel {
    pub async fn render_print(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Result<String> {
        let ent = self.entity(ctx, entity)?;
        let doc = self.get(ctx, entity, id, true).await?;
        let template = ent.print_template.clone().unwrap_or_else(|| DEFAULT_TEMPLATE.to_string());
        let mut env = Environment::new();
        env.set_auto_escape_callback(|_| AutoEscape::Html);
        env.set_fuel(Some(2_000_000));
        env.add_filter("money", |v: JValue, precision: Option<u32>| -> String {
            let j: Value = serde_json::to_value(&v).unwrap_or(Value::Null);
            format_money(&j, precision.unwrap_or(2))
        });
        env.add_function("display", |f: JValue, row: JValue| -> String {
            let f: Value = serde_json::to_value(&f).unwrap_or(Value::Null);
            let row: Value = serde_json::to_value(&row).unwrap_or(Value::Null);
            let name = f.get("name").and_then(Value::as_str).unwrap_or_default();
            if let Some(t) = row.get("_titles").and_then(|t| t.get(name)).and_then(Value::as_str) {
                return t.to_string();
            }
            let v = row.get(name).cloned().unwrap_or(Value::Null);
            let ty = f.get("type").and_then(Value::as_str).unwrap_or_default();
            match (ty, &v) {
                (_, Value::Null) => String::new(),
                ("currency", _) => format_money(&v, f.get("precision").and_then(Value::as_u64).unwrap_or(2) as u32),
                ("decimal" | "percent", _) => parse_decimal(&v).map(|d: Decimal| d.normalize().to_string()).unwrap_or_default(),
                ("check", Value::Bool(b)) => {
                    if *b {
                        "Yes".into()
                    } else {
                        "No".into()
                    }
                }
                (_, Value::String(s)) => s.clone(),
                (_, other) => other.to_string(),
            }
        });
        env.add_template_owned("print".to_string(), template).map_err(|e| Error::invalid(format!("print template: {e}")))?;
        let tmpl = env.get_template("print").map_err(Error::internal)?;
        tmpl.render(minijinja::context! {
            doc => JValue::from_serialize(&doc),
            entity => JValue::from_serialize(field_meta(&ent, &ctx.schema)),
            now => chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string(),
        })
        .map_err(|e| Error::invalid(format!("print template failed: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_formatting() {
        assert_eq!(format_money(&json!("1234567.5"), 2), "1,234,567.50");
        assert_eq!(format_money(&json!("-999"), 0), "-999");
        assert_eq!(format_money(&json!("-1000.005"), 2), "-1,000.01");
    }
}
