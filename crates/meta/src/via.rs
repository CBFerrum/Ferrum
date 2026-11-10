//! Owner paths (`via` on an own-record rule, docs/builder/decisions.md BD-02): how a record belongs
//! to someone it does not name. From the record, through at most two links and then optionally one
//! child table, to a field holding an email: `patient.email` (an appointment is the patient's),
//! `student.guardians.email` (a report card is each guardian's). A path is resolved against the
//! entities it crosses each time it is used, so one whose field is gone reaches nothing.

use crate::types::{EntityDef, FieldType};

/// At most this many links on a path: ownership stays a short, indexed walk.
pub const MAX_LINKS: usize = 2;

/// One step of a resolved path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViaStep {
    /// A link field and the entity it reaches.
    Link { field: String, to: String },
    /// A child table and the entity of its rows; only ever just before the email.
    Table { field: String, of: String },
    /// The field holding the email, on the entity (or child row) reached.
    Email { field: String },
}

/// Resolves `path` from entity `from`, with `def` looking entities up by name.
pub fn resolve<'a>(from: &str, path: &str, def: impl Fn(&str) -> Option<&'a EntityDef>) -> Result<Vec<ViaStep>, String> {
    let parts: Vec<&str> = path.split('.').map(str::trim).collect();
    if parts.is_empty() || parts.len() > MAX_LINKS + 2 || parts.iter().any(|p| p.is_empty()) {
        return Err(format!("{path:?} is not a path: give fields joined by dots, at most {MAX_LINKS} links then a child table, ending at an email"));
    }
    let mut steps = Vec::with_capacity(parts.len());
    let mut at = def(from).ok_or_else(|| format!("{from} is not an entity"))?;
    let mut in_table = false;
    for (i, name) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let field = at.fields.iter().find(|f| f.name == *name).ok_or_else(|| format!("{} has no field {name}", at.name))?;
        if last {
            if !matches!(field.ftype, FieldType::Email | FieldType::Data) {
                return Err(format!("{}.{name} holds no email: a path ends at an email field", at.name));
            }
            steps.push(ViaStep::Email { field: field.name.clone() });
            break;
        }
        if in_table {
            return Err(format!("{path:?} goes on after a child table: the table's row field must be the last"));
        }
        match field.ftype {
            FieldType::Link => {
                let to = field.to.clone().ok_or_else(|| format!("{}.{name} links to nothing", at.name))?;
                if steps.iter().filter(|s| matches!(s, ViaStep::Link { .. })).count() == MAX_LINKS {
                    return Err(format!("{path:?} follows more than {MAX_LINKS} links"));
                }
                at = def(&to).ok_or_else(|| format!("{}.{name} links to {to}, which is not here", at.name))?;
                steps.push(ViaStep::Link { field: field.name.clone(), to });
            }
            FieldType::Table => {
                let of = field.of.clone().ok_or_else(|| format!("{}.{name} is a table of nothing", at.name))?;
                at = def(&of).ok_or_else(|| format!("{}.{name} holds rows of {of}, which is not here", at.name))?;
                steps.push(ViaStep::Table { field: field.name.clone(), of });
                in_table = true;
            }
            _ => return Err(format!("{}.{name} is neither a link nor a child table, so a path cannot go through it", at.name)),
        }
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FieldDef;

    fn field(name: &str, ftype: FieldType, to: Option<&str>) -> FieldDef {
        let mut f: FieldDef = serde_json::from_value(serde_json::json!({"name": name, "type": "data"})).unwrap();
        f.ftype = ftype;
        match ftype {
            FieldType::Table => f.of = to.map(Into::into),
            _ => f.to = to.map(Into::into),
        }
        f
    }

    fn entity(name: &str, fields: Vec<FieldDef>) -> EntityDef {
        let mut e: EntityDef = serde_json::from_value(serde_json::json!({"name": name, "label": name, "kind": "master", "fields": []})).unwrap();
        e.fields = fields;
        e
    }

    fn school() -> Vec<EntityDef> {
        vec![
            entity("ReportCard", vec![field("student", FieldType::Link, Some("Student")), field("term", FieldType::Data, None)]),
            entity(
                "Student",
                vec![
                    field("guardians", FieldType::Table, Some("StudentGuardian")),
                    field("email", FieldType::Email, None),
                    field("school", FieldType::Link, Some("School")),
                    field("age", FieldType::Int, None),
                ],
            ),
            entity("StudentGuardian", vec![field("email", FieldType::Email, None), field("home", FieldType::Link, Some("School"))]),
            entity("School", vec![field("district", FieldType::Link, Some("District")), field("head_email", FieldType::Email, None)]),
            entity("District", vec![field("region", FieldType::Link, Some("Region")), field("office_email", FieldType::Email, None)]),
            entity("Region", vec![field("email", FieldType::Email, None)]),
        ]
    }

    fn resolve_in(from: &str, path: &str) -> Result<Vec<ViaStep>, String> {
        let all = school();
        resolve(from, path, |n| all.iter().find(|e| e.name == n))
    }

    #[test]
    fn paths_reach_an_email_through_links_and_one_table() {
        assert_eq!(resolve_in("Student", "email").unwrap(), vec![ViaStep::Email { field: "email".into() }]);
        assert_eq!(
            resolve_in("ReportCard", "student.guardians.email").unwrap(),
            vec![
                ViaStep::Link { field: "student".into(), to: "Student".into() },
                ViaStep::Table { field: "guardians".into(), of: "StudentGuardian".into() },
                ViaStep::Email { field: "email".into() },
            ]
        );
        assert_eq!(resolve_in("ReportCard", "student.school.head_email").unwrap().len(), 3);
    }

    #[test]
    fn paths_that_wander_are_refused() {
        for (from, path, why) in [
            ("ReportCard", "student.school.district.office_email", "more than two links"),
            ("ReportCard", "student.guardians.home.head_email", "on after a child table"),
            ("ReportCard", "student.age", "an end that holds no email"),
            ("ReportCard", "term.email", "through a field that is no link"),
            ("ReportCard", "student.nobody", "a field that is not there"),
            ("ReportCard", "", "nothing"),
            ("ReportCard", "student..email", "an empty step"),
            ("Nowhere", "email", "an entity that is not there"),
        ] {
            assert!(resolve_in(from, path).is_err(), "accepted {path:?} on {from}: {why}");
        }
    }
}
