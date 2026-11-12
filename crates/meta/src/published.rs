//! What a published list may show (BD-15): an allow-list of an entity's own fields, never one that
//! holds an address, a phone number, a secret or a file, a field some of the workspace's own people
//! may not read, or people's personal data a package declares. A list that names something it may not
//! show is refused when it is saved; one whose entity changed later loses what it may no longer show.

use crate::types::{EntityDef, EntityKind, FieldDef, FieldType, PersonalDataDef, PublishedList, PublishedListDef, Reference};

/// Published lists a workspace holds at most.
pub const MAX_LISTS: usize = 20;
/// Fields a published list shows at most.
pub const MAX_FIELDS: usize = 12;
/// Rows a published list shows at most, and without a `limit`.
pub const MAX_ROWS: u32 = 1000;
pub const DEFAULT_ROWS: u32 = 200;

/// System columns a filter or sort may use; of them, a list may show only `number`.
const SYSTEM: &[&str] = &["number", "docstatus", "workflow_state", "created_at", "updated_at"];

/// An entity as a list is checked against: its definition, whether it is the workspace's own, and
/// what packages declare of it as people's personal data.
pub(crate) type Seen<'a> = (&'a EntityDef, bool, Vec<&'a PersonalDataDef>);

fn shape(l: &PublishedListDef) -> Result<(), String> {
    let n = &l.name;
    if n.is_empty() || n.len() > 60 || n.starts_with('-') || !n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Err("its name is lowercase letters, digits and hyphens (at most 60), as it is part of a public address".into());
    }
    if l.label.trim().is_empty() || l.label.chars().count() > 100 {
        return Err("it needs a label of at most 100 characters".into());
    }
    if l.description.as_deref().is_some_and(|d| d.chars().count() > 1000) {
        return Err("its description is at most 1000 characters".into());
    }
    if l.fields.is_empty() || l.fields.len() > MAX_FIELDS {
        return Err(format!("it shows 1 to {MAX_FIELDS} fields"));
    }
    if let Some(f) = l.fields.iter().enumerate().find(|(i, f)| l.fields[..*i].contains(f)).map(|(_, f)| f) {
        return Err(format!("it names {f} twice"));
    }
    if l.limit.is_some_and(|n| n == 0 || n > MAX_ROWS) {
        return Err(format!("its limit is 1 to {MAX_ROWS} rows"));
    }
    if l.filter.len() > 20 {
        return Err("its filter has at most 20 conditions".into());
    }
    if l.detail.len() > MAX_FIELDS {
        return Err(format!("its detail page shows at most {MAX_FIELDS} more fields"));
    }
    if let Some(f) = l.detail.iter().enumerate().find(|(i, f)| l.detail[..*i].contains(f) || l.fields.contains(f)).map(|(_, f)| f) {
        return Err(format!("it names {f} twice"));
    }
    if (l.summary.is_some() || !l.detail.is_empty()) && l.slug.is_none() {
        return Err("a detail page and its summary need a slug, the field giving each record's address".into());
    }
    if let Some(s) = l.summary.as_ref().filter(|s| !l.fields.contains(s) && !l.detail.contains(s)) {
        return Err(format!("its summary {s} is one of the fields it shows"));
    }
    if l.frame_origins.len() > 10 || l.frame_origins.iter().any(|o| frame_origin(o).as_deref() != Some(o.as_str())) {
        return Err("it may be framed by at most 10 origins, each written like https://example.com".into());
    }
    Ok(())
}

/// An origin a public page may be framed by (BD-24): `https://host[:port]`, or `http://localhost[:port]`; no path,
/// no wildcard, lower case. `None` for anything else.
pub fn frame_origin(o: &str) -> Option<String> {
    let o = o.trim().trim_end_matches('/');
    let rest = o.strip_prefix("https://").or_else(|| o.strip_prefix("http://").filter(|r| r.starts_with("localhost")))?;
    let (host, port) = rest.split_once(':').unwrap_or((rest, ""));
    let host_ok = !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    let port_ok = port.is_empty() || (port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit()));
    (host_ok && port_ok).then(|| o.to_ascii_lowercase())
}

/// Adds `list` of `entity` to the workspace's lists, or replaces the one of the same name.
pub(crate) fn collect(lists: &mut Vec<PublishedList>, entity: &str, list: &PublishedListDef) -> Result<(), String> {
    shape(list).map_err(|m| format!("published list {}: {m}", list.name))?;
    let new = PublishedList { entity: entity.into(), list: list.clone() };
    match lists.iter().position(|l| l.list.name == list.name) {
        Some(i) if lists[i].entity != entity => {
            Err(format!("published list {}: the name is taken by a list of {}; a list's name is one of a kind in the workspace", list.name, lists[i].entity))
        }
        Some(i) => {
            lists[i] = new;
            Ok(())
        }
        None if lists.len() >= MAX_LISTS => Err(format!("published list {}: a workspace publishes at most {MAX_LISTS} lists", list.name)),
        None => {
            lists.push(new);
            Ok(())
        }
    }
}

/// Why `f` of `e` may not be shown to anyone, if it may not.
fn withheld(f: &FieldDef, e: &Seen) -> Option<String> {
    let (def, custom, declared) = e;
    let name = &f.name;
    if matches!(f.ftype, FieldType::Email | FieldType::Phone | FieldType::Secret | FieldType::Attach) {
        let what = match f.ftype {
            FieldType::Email => "an email address",
            FieldType::Phone => "a phone number",
            FieldType::Secret => "a secret",
            _ => "an attachment",
        };
        return Some(format!("{name} holds {what}, which a published list never shows"));
    }
    if f.ftype == FieldType::Table {
        return Some(format!("{name} is a table of lines, which a published list does not show"));
    }
    if let Some(g) = &f.perm_group {
        return Some(format!("{name} is in the field group {g}, which some of the workspace's own people may not read"));
    }
    if f.no_export {
        return Some(format!("{name} is never exported, so it is not published either"));
    }
    if !custom {
        let personal =
            |d: &&PersonalDataDef| [Some(&d.match_field), d.phone.as_ref(), d.name.as_ref()].into_iter().flatten().chain(&d.erase).any(|x| x == name);
        if declared.iter().any(personal) {
            return Some(format!("{name} holds people's personal data, as the package of {} declares", def.name));
        }
    }
    None
}

/// Why the records of `e` may not be published at all, if they may not.
fn closed(e: &Seen) -> Option<String> {
    let (def, custom, declared) = e;
    if !matches!(def.kind, EntityKind::Master | EntityKind::Document) {
        return Some(format!("{} holds lines, ledger rows or settings: a list publishes records saved on their own", def.name));
    }
    if !custom && !declared.is_empty() {
        return Some(format!("the records of {} are people's personal data, as its package declares: they are never published", def.name));
    }
    None
}

fn column(def: &EntityDef, name: &str) -> bool {
    SYSTEM.contains(&name) || def.fields.iter().any(|f| f.name == name && f.ftype.is_stored())
}

/// `list` as its entity now stands: kept without the fields it may no longer show, or gone when it
/// cannot stand, with why for each.
pub(crate) fn settle<'a>(list: &PublishedList, lookup: &impl Fn(&str) -> Option<Seen<'a>>) -> (Option<PublishedList>, Vec<String>) {
    let gone = |m: String| (None, vec![m]);
    let Some(e) = lookup(&list.entity) else { return gone(format!("there is no entity {} here", list.entity)) };
    if let Some(m) = closed(&e) {
        return gone(m);
    }
    let def = e.0;
    // A filter that no longer applies would publish more than was chosen: the list goes instead.
    let named = list.list.filter.iter().map(|(f, _, _)| f).chain(list.list.sort.iter().map(|(f, _)| f));
    for f in named {
        if !column(def, f) {
            return gone(format!("its filter or order names {f}, which is not a stored field of {}", def.name));
        }
    }
    let mut why = Vec::new();
    let mut kept = list.clone();
    let shows = |name: &String, why: &mut Vec<String>| {
        if name == "number" {
            return true;
        }
        let Some(f) = def.fields.iter().find(|f| &f.name == name) else {
            why.push(format!("{name} is not a field of {}", def.name));
            return false;
        };
        if let Some(m) = withheld(f, &e).or_else(|| (f.ftype == FieldType::Link).then(|| linked(f, lookup)).flatten()) {
            why.push(m);
            return false;
        }
        true
    };
    kept.list.fields.retain(|name| shows(name, &mut why));
    kept.list.detail.retain(|name| shows(name, &mut why));
    if let Some(slug) = kept.list.slug.clone() {
        let text = def.fields.iter().any(|f| f.name == slug && matches!(f.ftype, FieldType::Data | FieldType::Text));
        if !text {
            why.push(format!("its slug {slug} is not a text field of {}", def.name));
            kept.list.slug = None;
        } else if !shows(&slug, &mut why) {
            kept.list.slug = None;
        }
        if kept.list.slug.is_none() {
            kept.list.detail.clear();
            kept.list.summary = None;
        }
    }
    if let Some(s) = kept.list.summary.clone().filter(|s| !kept.list.fields.contains(s) && !kept.list.detail.contains(s)) {
        why.push(format!("its summary {s} is no longer shown"));
        kept.list.summary = None;
    }
    if kept.list.fields.is_empty() {
        why.push("none of its fields may be shown, so it is taken down".into());
        return (None, why);
    }
    (Some(kept), why)
}

/// Why a link field may not show the title of what it links to, if it may not: the record linked to
/// is named by its title field (or its number), which must be one a list of that entity could show.
fn linked<'a>(f: &FieldDef, lookup: &impl Fn(&str) -> Option<Seen<'a>>) -> Option<String> {
    let to = f.to.as_deref().unwrap_or_default();
    let Some(t) = lookup(to) else { return Some(format!("{} links to {to}, which is not here", f.name)) };
    let (def, custom, declared) = &t;
    if def.reference == Reference::Restricted {
        return Some(format!("{} links to {to}, which names itself only to those who may read it", f.name));
    }
    if !custom && !declared.is_empty() {
        return Some(format!("{} links to {to}, whose records are people's personal data", f.name));
    }
    let title = def.title_field.as_deref().and_then(|n| def.fields.iter().find(|x| x.name == n))?;
    if title.ftype == FieldType::Link || withheld(title, &t).is_some() {
        return Some(format!("{} links to {to}, whose title ({}) may not be published", f.name, title.name));
    }
    None
}

#[cfg(test)]
mod tests {
    use crate::effective::build;
    use crate::types::{Overlay, Package};
    use crate::{Catalog, Schema};
    use serde_json::{Value, json};

    fn catalog() -> Catalog {
        let entities = serde_yaml::from_str(
            r#"
- {name: Category, label: Category, kind: master, title_field: category_name, fields: [{name: category_name, type: data}]}
- {name: Vault, label: Vault, kind: master, reference: restricted, title_field: vault_name, fields: [{name: vault_name, type: data}]}
- {name: Mailbox, label: Mailbox, kind: master, title_field: address, fields: [{name: address, type: email}]}
- {name: Person, label: Person, kind: master, title_field: full_name, fields: [{name: full_name, type: data}, {name: email, type: email}]}
- {name: PermitLine, label: Permit Line, kind: child, fields: [{name: note, type: data}]}
- name: Permit
  label: Permit
  kind: document
  submittable: true
  fields:
    - {name: holder, type: data}
    - {name: kind, type: select, options: [Trade, Fence]}
    - {name: issued_on, type: date}
    - {name: fee, type: currency}
    - {name: contact_email, type: email}
    - {name: contact_phone, type: phone}
    - {name: api_key, type: secret}
    - {name: scan, type: attach}
    - {name: inspector_note, type: data, perm_group: inspection}
    - {name: bank_account, type: data, no_export: true}
    - {name: lines, type: table, of: PermitLine}
    - {name: category, type: link, to: Category}
    - {name: vault, type: link, to: Vault}
    - {name: mailbox, type: link, to: Mailbox}
    - {name: applicant, type: link, to: Person}
"#,
        )
        .unwrap();
        let manifest = serde_yaml::from_str("{package: core, version: '1', personal_data: [{entity: Person, match: email, erase: [full_name]}]}").unwrap();
        Catalog::from_packages(vec![Package { manifest, entities, sql: vec![], print_formats: vec![] }]).unwrap()
    }

    fn ov(entity: &str, op: Value) -> Overlay {
        Overlay { entity: entity.into(), ops: vec![serde_json::from_value(op).unwrap()] }
    }

    fn publish(entity: &str, list: Value) -> Overlay {
        let mut l = json!({"name": "permits", "label": "Permits", "fields": ["holder", "kind", "issued_on"]});
        for (k, v) in list.as_object().cloned().unwrap_or_default() {
            l[k] = v;
        }
        ov(entity, json!({"op": "publish_list", "list": l}))
    }

    fn with(ovs: &[Overlay]) -> Schema {
        build(&catalog(), &["core".into()], ovs, 1).unwrap()
    }

    fn refused(ovs: &[Overlay], why: &str) {
        let s = with(ovs);
        assert!(s.warnings.iter().any(|w| w.message.contains(why)), "{why}: {:?}", s.warnings);
    }

    #[test]
    fn only_what_may_be_shown_is_published() {
        let s = with(&[publish(
            "Permit",
            json!({"fields": ["number", "holder", "category"], "filter": [["kind", "=", "Trade"]], "sort": ["fee", "desc"], "csv": true}),
        )]);
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        let l = s.published_list("permits").unwrap();
        assert_eq!((l.entity.as_str(), l.list.fields.clone(), l.list.csv), ("Permit", vec!["number".into(), "holder".into(), "category".into()], true));
        // Filter and order on fields not shown, a permission group's included: they pick rows, they are not sent.
        assert!(
            with(&[publish("Permit", json!({"filter": [["inspector_note", "is", "set"], ["docstatus", "=", 1]], "sort": ["inspector_note", "asc"]}))])
                .warnings
                .is_empty()
        );

        let mut many = Vec::new();
        for i in 0..21 {
            many.push(publish("Permit", json!({"name": format!("list-{i}")})));
        }
        let x = |n: &str| {
            json!({"op": "define_entity", "entity": {"name": n, "label": n, "kind": "master", "fields": [
            {"name": "title", "type": "data"}, {"name": "mail", "type": "email"}, {"name": "secret_note", "type": "data", "perm_group": "board"}]}})
        };
        let thirteen: Vec<String> = (0..13).map(|i| format!("f{i}")).collect();
        for (ovs, why) in [
            (vec![publish("Permit", json!({"fields": ["contact_email"]}))], "contact_email holds an email address"),
            (vec![publish("Permit", json!({"fields": ["holder", "contact_phone"]}))], "contact_phone holds a phone number"),
            (vec![publish("Permit", json!({"fields": ["holder", "api_key"]}))], "api_key holds a secret"),
            (vec![publish("Permit", json!({"fields": ["holder", "scan"]}))], "scan holds an attachment"),
            (vec![publish("Permit", json!({"fields": ["holder", "inspector_note"]}))], "inspector_note is in the field group inspection"),
            (vec![publish("Permit", json!({"fields": ["holder", "bank_account"]}))], "bank_account is never exported"),
            (vec![publish("Permit", json!({"fields": ["holder", "lines"]}))], "lines is a table of lines"),
            (vec![publish("Permit", json!({"fields": ["holder", "nothing"]}))], "nothing is not a field of Permit"),
            (vec![publish("Permit", json!({"fields": ["holder", "id"]}))], "id is not a field of Permit"),
            (vec![publish("Permit", json!({"fields": ["holder", "created_by"]}))], "created_by is not a field of Permit"),
            (vec![publish("Permit", json!({"fields": ["holder", "vault"]}))], "names itself only to those who may read it"),
            (vec![publish("Permit", json!({"fields": ["holder", "mailbox"]}))], "whose title (address) may not be published"),
            (vec![publish("Permit", json!({"fields": ["holder", "applicant"]}))], "whose records are people's personal data"),
            (vec![publish("Person", json!({"fields": ["full_name"]}))], "the records of Person are people's personal data"),
            (vec![publish("Person", json!({"fields": ["email"]}))], "Person"),
            (vec![publish("PermitLine", json!({"fields": ["note"]}))], "holds lines"),
            (vec![publish("Nowhere", json!({}))], "there is no entity Nowhere"),
            (vec![publish("Permit", json!({"filter": [["nothing", "=", 1]]}))], "names nothing, which is not a stored field"),
            (vec![publish("Permit", json!({"filter": [["category.category_name", "=", "A"]]}))], "not a stored field"),
            (vec![publish("Permit", json!({"sort": ["lines", "asc"]}))], "names lines"),
            (vec![publish("Permit", json!({"name": "Permits"}))], "lowercase letters, digits and hyphens"),
            (vec![publish("Permit", json!({"name": "per mits"}))], "lowercase letters, digits and hyphens"),
            (vec![publish("Permit", json!({"name": "per_mits"}))], "lowercase letters, digits and hyphens"),
            (vec![publish("Permit", json!({"name": ""}))], "lowercase letters, digits and hyphens"),
            (vec![publish("Permit", json!({"label": " "}))], "needs a label"),
            (vec![publish("Permit", json!({"fields": []}))], "1 to 12 fields"),
            (vec![publish("Permit", json!({"fields": thirteen}))], "1 to 12 fields"),
            (vec![publish("Permit", json!({"fields": ["holder", "holder"]}))], "names holder twice"),
            (vec![publish("Permit", json!({"limit": 1001}))], "limit is 1 to 1000"),
            (vec![publish("Permit", json!({"detail": ["fee"]}))], "need a slug"),
            (vec![publish("Permit", json!({"slug": "holder", "summary": "fee"}))], "its summary fee is one of the fields it shows"),
            (vec![publish("Permit", json!({"slug": "holder", "detail": ["holder"]}))], "names holder twice"),
            (vec![publish("Permit", json!({"slug": "holder", "detail": ["contact_email"]}))], "contact_email holds an email address"),
            (vec![publish("Permit", json!({"slug": "issued_on"}))], "its slug issued_on is not a text field"),
            (vec![publish("Permit", json!({"slug": "inspector_note"}))], "inspector_note is in the field group inspection"),
            (vec![publish("Permit", json!({"frame_origins": ["https://*.example.com"]}))], "framed by at most 10 origins"),
            (vec![publish("Permit", json!({"frame_origins": ["http://shop.example.com"]}))], "framed by at most 10 origins"),
            (vec![publish("Permit", json!({"limit": 0}))], "limit is 1 to 1000"),
            (many, "at most 20 lists"),
            (vec![publish("Permit", json!({})), publish("Category", json!({"fields": ["category_name"]}))], "the name is taken by a list of Permit"),
            (vec![ov("Permit", json!({"op": "unpublish_list", "name": "permits"}))], "no published list permits of Permit"),
            (vec![publish("Permit", json!({})), ov("Category", json!({"op": "unpublish_list", "name": "permits"}))], "no published list permits of Category"),
            // A workspace's own entity: no declaration closes it, and the type rules and field groups still hold.
            (vec![ov("XClass", x("XClass")), publish("XClass", json!({"fields": ["title", "mail"]}))], "mail holds an email address"),
            (vec![ov("XClass", x("XClass")), publish("XClass", json!({"fields": ["title", "secret_note"]}))], "field group board"),
        ] {
            refused(&ovs, why);
        }
        // A package does not publish: its customizations are the workspace's to make.
        let op = crate::types::PublishedListDef {
            name: "a".into(),
            label: "A".into(),
            description: None,
            fields: vec!["holder".into()],
            filter: vec![],
            sort: None,
            csv: false,
            limit: None,
            slug: None,
            detail: vec![],
            summary: None,
            frame_origins: vec![],
        };
        let mut def = catalog().packages().next().unwrap().entities.iter().find(|e| e.name == "Permit").unwrap().clone();
        assert!(
            crate::effective::apply_op(&mut def, &crate::types::OverlayOp::PublishList { list: op.clone() }, crate::catalog::OverlaySource::Package).is_err()
        );
        let own = with(&[ov("XClass", x("XClass")), publish("XClass", json!({"name": "classes", "fields": ["title"]}))]);
        assert!(own.warnings.is_empty() && own.published_list("classes").is_some(), "{:?}", own.warnings);
    }

    #[test]
    fn replaced_and_taken_down() {
        let first = publish("Permit", json!({}));
        let s = with(&[first.clone(), publish("Permit", json!({"label": "Issued permits", "fields": ["holder"]}))]);
        let l = &s.published_lists;
        assert_eq!((l.len(), l[0].list.label.as_str(), l[0].list.fields.len()), (1, "Issued permits", 1), "{:?}", s.warnings);
        let s = with(&[first.clone(), ov("Permit", json!({"op": "unpublish_list", "name": "permits"}))]);
        assert!(s.warnings.is_empty() && s.published_lists.is_empty(), "{:?}", s.warnings);
        // A redefinition of its entity does not take its list down.
        let x = json!({"name": "XClass", "label": "Class", "kind": "master", "fields": [{"name": "title", "type": "data"}, {"name": "room", "type": "data"}]});
        let s = with(&[
            ov("XClass", json!({"op": "define_entity", "entity": x})),
            publish("XClass", json!({"name": "classes", "fields": ["title", "room"]})),
            ov("XClass", json!({"op": "redefine_entity", "entity": x})),
        ]);
        assert!(s.warnings.is_empty() && s.published_list("classes").is_some(), "{:?}", s.warnings);
        // A field that goes from the entity drops out of the list with a warning; the list stands.
        let added = ov("Permit", json!({"op": "add_field", "field": {"name": "x_room", "type": "data"}}));
        let list = publish("Permit", json!({"fields": ["holder", "x_room"]}));
        assert!(with(&[added.clone(), list.clone()]).warnings.is_empty());
        let s = with(std::slice::from_ref(&list));
        assert_eq!(s.published_list("permits").unwrap().list.fields, ["holder"]);
        assert!(s.warnings.iter().any(|w| w.message == "published list permits: x_room is not a field of Permit"), "{:?}", s.warnings);
        // One whose filter field goes is taken down: it would show more than was chosen.
        let s = with(&[publish("Permit", json!({"filter": [["x_room", "=", "A"]]}))]);
        assert!(s.published_lists.is_empty() && s.warnings.iter().any(|w| w.message.contains("names x_room")), "{:?}", s.warnings);
    }
}
