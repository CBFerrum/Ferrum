//! ERPNext/Frappe DocType importer: converts `*/doctype/*/*.json` definitions into a Ferrum
//! package (manifest + entity YAML), then validates the result by loading it as a catalog.
//! Untranslatable pieces (dynamic links, unknown link targets, JS-only conditions) are kept as
//! plain fields and listed in the report instead of being silently dropped.

use ferrum_meta::{Expr, RESERVED_FIELDS, is_ident};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, serde::Serialize)]
pub struct ImportReport {
    pub entities: usize,
    pub fields: usize,
    pub notes: Vec<String>,
}

fn entity_name(doctype: &str) -> String {
    doctype
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut c = s.chars();
            c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
        })
        .collect()
}

fn find_doctypes(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == "node_modules" || n == "tests") {
                continue;
            }
            find_doctypes(&p, out);
        } else if p.extension().is_some_and(|x| x == "json") && p.parent().and_then(|d| d.parent()).and_then(|d| d.file_name()).is_some_and(|n| n == "doctype")
        {
            out.push(p);
        }
    }
}

/// Translates a Frappe `eval:` expression to CEL when it uses the common subset.
fn translate_condition(raw: &str) -> Option<String> {
    let s = raw.trim();
    let body = s.strip_prefix("eval:").unwrap_or(s).trim().trim_end_matches(';');
    if body.is_empty() || body.contains("frappe.") || body.contains("cur_frm") || body.contains("=>") {
        return None;
    }
    let mut out = body.replace("===", "==").replace("!==", "!=");
    if !body.contains(['=', '<', '>', '!', '&', '|', '(']) {
        // A bare field name (`depends_on: is_return`) means "truthy".
        if is_ident(body) {
            out = format!("doc.{body} == true || (doc.{body} != null && doc.{body} != '' && doc.{body} != 0)");
        } else {
            return None;
        }
    }
    let cel = out.replace("in_list(", "__inlist(");
    if cel.contains("__inlist(") {
        return None;
    }
    Expr::compile(&cel).ok().map(|_| cel)
}

fn yaml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

pub fn import(src: &Path, out: &Path, package: &str) -> Result<ImportReport, String> {
    if !ferrum_meta::is_package_name(package) {
        return Err(format!("invalid package name {package}"));
    }
    let mut files = Vec::new();
    find_doctypes(src, &mut files);
    files.sort();
    let mut docs: Vec<Value> = Vec::new();
    for f in &files {
        let text = fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?;
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("doctype").and_then(Value::as_str) == Some("DocType") {
            docs.push(v);
        }
    }
    let known: BTreeSet<String> = docs.iter().filter_map(|d| d.get("name").and_then(Value::as_str)).map(entity_name).collect();
    let mut report = ImportReport::default();
    let mut by_module: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for d in &docs {
        let dt = d.get("name").and_then(Value::as_str).unwrap_or_default();
        let name = entity_name(dt);
        if name.is_empty() || !ferrum_meta::is_entity_name(&name) {
            report.notes.push(format!("skipped doctype {dt}: cannot derive an entity name"));
            continue;
        }
        let module = d.get("module").and_then(Value::as_str).unwrap_or("imported").to_lowercase().replace(' ', "_");
        let flag = |k: &str| d.get(k).and_then(Value::as_i64).unwrap_or(0) == 1;
        let kind = if flag("istable") {
            "child"
        } else if flag("issingle") {
            "settings"
        } else if flag("is_submittable") {
            "document"
        } else {
            "master"
        };
        if flag("is_virtual") {
            report.notes.push(format!("{dt}: virtual doctype imported as a normal table"));
        }
        let mut y = String::new();
        y.push_str(&format!("- name: {name}\n  label: {}\n  kind: {kind}\n", yaml_str(dt)));
        if kind != "child" {
            y.push_str(&format!("  module: {module}\n"));
        }
        if flag("is_submittable") {
            y.push_str("  submittable: true\n");
        }
        let mut sections: Vec<(Option<String>, Vec<String>)> = vec![(None, vec![])];
        let mut fields_yaml = String::new();
        let mut field_names = BTreeSet::new();
        for f in d.get("fields").and_then(Value::as_array).cloned().unwrap_or_default() {
            let fname = f.get("fieldname").and_then(Value::as_str).unwrap_or_default().to_string();
            let ftype = f.get("fieldtype").and_then(Value::as_str).unwrap_or_default();
            let label = f.get("label").and_then(Value::as_str).map(str::to_string);
            if ftype == "Section Break" || ftype == "Tab Break" {
                sections.push((label, vec![]));
                continue;
            }
            let mapped = match ftype {
                "Data" | "Read Only" | "Time" | "Barcode" | "Dynamic Link" | "Autocomplete" => "data",
                "Small Text" | "Long Text" | "Text" | "Text Editor" | "HTML Editor" | "Signature" => "text",
                "Markdown Editor" => "markdown",
                "Code" => "code",
                "Int" | "Duration" => "int",
                "Float" | "Rating" => "decimal",
                "Currency" => "currency",
                "Percent" => "percent",
                "Date" => "date",
                "Datetime" => "datetime",
                "Check" => "check",
                "Select" => "select",
                "Link" => "link",
                "Table" | "Table MultiSelect" => "table",
                "Attach" | "Attach Image" => "attach",
                "Color" => "color",
                "JSON" | "Geolocation" => "json",
                "Phone" => "phone",
                _ => continue,
            };
            if fname.is_empty() || !is_ident(&fname) {
                report.notes.push(format!("{dt}: skipped field '{fname}' (invalid name)"));
                continue;
            }
            let safe = if RESERVED_FIELDS.contains(&fname.as_str()) { format!("{fname}_value") } else { fname.clone() };
            if !field_names.insert(safe.clone()) {
                continue;
            }
            let mut t = mapped.to_string();
            let mut extra = String::new();
            let options = f.get("options").and_then(Value::as_str).unwrap_or_default();
            match mapped {
                "link" => {
                    let target = entity_name(options);
                    if known.contains(&target) {
                        extra.push_str(&format!(", to: {target}"));
                    } else {
                        t = "data".into();
                        report.notes.push(format!("{dt}.{fname}: link to {options} (not imported) kept as data"));
                    }
                }
                "table" => {
                    let target = entity_name(options);
                    if known.contains(&target) {
                        extra.push_str(&format!(", of: {target}"));
                    } else {
                        report.notes.push(format!("{dt}.{fname}: child table {options} not found; skipped"));
                        continue;
                    }
                }
                "select" => {
                    let opts: Vec<String> = options.lines().map(str::trim).filter(|s| !s.is_empty()).map(yaml_str).collect();
                    if opts.is_empty() {
                        t = "data".into();
                    } else {
                        extra.push_str(&format!(", options: [{}]", opts.join(", ")));
                    }
                }
                _ => {}
            }
            if ftype == "Dynamic Link" {
                report.notes.push(format!("{dt}.{fname}: dynamic link imported as text"));
            }
            let get_flag = |k: &str| f.get(k).and_then(Value::as_i64).unwrap_or(0) == 1;
            if get_flag("reqd") && t != "table" {
                extra.push_str(", required: true");
            }
            if get_flag("read_only") || ftype == "Read Only" {
                extra.push_str(", read_only: true");
            }
            if get_flag("hidden") {
                extra.push_str(", hidden: true");
            }
            if get_flag("in_list_view") && t != "table" {
                extra.push_str(", in_list: true");
            }
            if get_flag("in_standard_filter") && t != "table" {
                extra.push_str(", in_filter: true");
            }
            if let Some(l) = &label {
                extra.push_str(&format!(", label: {}", yaml_str(l)));
            }
            if let Some(def) = f.get("default").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                match (def.to_lowercase().as_str(), t.as_str()) {
                    ("today", "date") => extra.push_str(", default_expr: today"),
                    (_, "check") => extra.push_str(&format!(", default: {}", def == "1")),
                    (_, "int") if def.parse::<i64>().is_ok() => extra.push_str(&format!(", default: {def}")),
                    (_, "data" | "select" | "text") if !def.starts_with("__") && !def.contains('{') => extra.push_str(&format!(", default: {}", yaml_str(def))),
                    _ => report.notes.push(format!("{dt}.{fname}: default '{def}' not translated")),
                }
            }
            for (src_key, dst_key) in [("depends_on", "visible_if"), ("mandatory_depends_on", "required_if"), ("read_only_depends_on", "read_only_if")] {
                if let Some(c) = f.get(src_key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    match translate_condition(c) {
                        Some(cel) => extra.push_str(&format!(", {dst_key}: {}", yaml_str(&cel))),
                        None => report.notes.push(format!("{dt}.{fname}: {src_key} '{c}' needs manual translation")),
                    }
                }
            }
            if let Some(desc) = f.get("description").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                extra.push_str(&format!(", description: {}", yaml_str(&desc.chars().take(300).collect::<String>())));
            }
            fields_yaml.push_str(&format!("    - {{name: {safe}, type: {t}{extra}}}\n"));
            if let Some(last) = sections.last_mut() {
                last.1.push(safe.clone());
            }
            report.fields += 1;
        }
        if fields_yaml.is_empty() {
            report.notes.push(format!("{dt}: no importable fields; skipped"));
            continue;
        }
        y.push_str("  fields:\n");
        y.push_str(&fields_yaml);
        if let Some(t) = d.get("title_field").and_then(Value::as_str).filter(|t| field_names.contains(*t)) {
            y.push_str(&format!("  title_field: {t}\n"));
        }
        let form: Vec<&(Option<String>, Vec<String>)> = sections.iter().filter(|(_, f)| !f.is_empty()).collect();
        if kind != "child" && form.len() > 1 {
            y.push_str("  form:\n");
            for (label, fields) in form {
                let l = label.as_deref().map(yaml_str).unwrap_or_else(|| "null".into());
                y.push_str(&format!("    - {{label: {l}, fields: [{}]}}\n", fields.join(", ")));
            }
        }
        by_module.entry(module).or_default().push(y);
        report.entities += 1;
    }
    fs::create_dir_all(out.join("entities")).map_err(|e| e.to_string())?;
    fs::write(
        out.join("package.yaml"),
        format!(
            "package: {package}\nversion: 0.1.0\nlabel: {}\ndescription: Imported from Frappe/ERPNext DocTypes at {}\n",
            yaml_str(package),
            yaml_str(&src.display().to_string())
        ),
    )
    .map_err(|e| e.to_string())?;
    for (module, entities) in &by_module {
        fs::write(out.join("entities").join(format!("{module}.yaml")), entities.concat()).map_err(|e| e.to_string())?;
    }
    // Validate: the generated package must load as a catalog on its own.
    if let Err(e) = ferrum_meta::Catalog::load_dir(out.parent().unwrap_or(out)).map(|_| ()) {
        report.notes.push(format!("validation: {e}"));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_conditions() {
        assert_eq!(translate_condition("eval:doc.is_return===1").as_deref(), Some("doc.is_return==1"));
        assert!(translate_condition("eval:frappe.user.has_role('x')").is_none());
        assert!(translate_condition("is_return").is_some());
        assert_eq!(entity_name("Sales Invoice Item"), "SalesInvoiceItem");
    }
}
