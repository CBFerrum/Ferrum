//! API-1 (gapclose/design/A11-app-apis.md 2.3.1): what a package declares about callers outside the workspace, checked
//! when it loads. Each test starts from a package that loads and changes one thing.

use ferrum_meta::catalog::{load_package_dir, load_package_files};
use ferrum_meta::{ActionDef, Catalog, Overlay, OverlayOp};
use std::collections::BTreeMap;
use std::path::Path;

/// A shop: a web item anyone reads and lists, a cart anyone starts (its handle grants `add_item`), a review anyone
/// submits, and a visitor role. Every piece below is valid; a test replaces one.
const SHOP: &str = r#"
- name: WebItem
  label: Web Item
  kind: master
  search_fields: [item_name]
  fields:
    - {name: item_name, label: Item Name, type: data}
    - {name: slug, label: Slug, type: data}
    - {name: ranking, label: Ranking, type: int}
    - {name: published, label: Published, type: check}
    - {name: cost, label: Cost, type: int, perm_group: costing}
    - {name: pin, label: PIN, type: secret}
    - {name: contact, label: Contact, type: email}
    - {name: supplier, label: Supplier, type: link, to: Company}
    - {name: supplier_name, label: Supplier Name, type: data, fetch_from: supplier.company_name}
  api:
    read: {audience: anyone, fields: [item_name, slug, ranking], cache: 60}
    list: {audience: anyone, fields: [item_name, slug, ranking], filters: [slug], sort: ["ranking desc"], limit: 48, search: true, cache: 60}
- name: Cart
  label: Cart
  kind: document
  fields:
    - {name: currency, label: Currency, type: data}
  actions:
    - name: start
      label: Start
      collection: true
      permission: create
      params: [{name: currency, label: Currency, type: data}]
      contract:
        effect: transactional_action
        returns: {type: object, additionalProperties: false, properties: {cart: {type: string, format: uuid, x-ferrum-link: Cart}, lines: {type: array, items: {type: object, properties: {qty: {type: string}}}}}}
        completion: An empty cart exists
        audience: anyone
        guard: {scope: shop.cart}
        handle: {field: cart, days: 30, grants: [add_item]}
        rate: [{per: address, max: 30, seconds: 3600}]
    - name: add_item
      label: Add Item
      permission: write
      params: [{name: qty, label: Qty, type: int}]
      contract:
        effect: transactional_action
        returns: {type: object, properties: {qty: {type: ["string", "null"]}}}
        completion: The item is in the cart
        audience: anyone
        rate: [{per: handle, max: 60, seconds: 60}]
    - name: peek
      label: Peek
      read_only: true
      permission: read
      contract: {effect: query, returns: {type: object, properties: {qty: {type: integer}}}, completion: What the cart holds, audience: anyone, cache: 30}
    - name: checkout_v1
      label: Checkout (old)
      contract: {deprecated: checkout replaces it, deprecated_since: 2027-01-10, sunset: 2027-04-10, rate: [{per: key, max: 10, seconds: 60}]}
- name: Review
  label: Review
  kind: master
  fields:
    - {name: body, label: Body, type: text}
    - {name: status, label: Status, type: select, options: [New, Approved]}
  actions:
    - name: submit_review
      label: Submit a Review
      collection: true
      permission: create
      params: [{name: body, label: Body, type: text}, {name: email, label: Email, type: data, required: true}]
      contract:
        effect: transactional_action
        returns: {type: object, properties: {received: {type: boolean}}}
        completion: The review is kept for moderation
        audience: anyone
        guard: {scope: shop.review, address: email}
        refusals: {forbidden: You have already reviewed this item}
"#;

const MANIFEST: &str = r#"package: shop
version: 0.1.0
depends: [core]
roles:
  - name: Store Visitor
    visitor: true
    permissions:
      - {entity: WebItem, read: true, where: {published: true}}
      - {entity: Cart, actions: [start, peek]}
      - {entity: Review, actions: [submit_review]}
"#;

fn core() -> ferrum_meta::Package {
    load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core")
}

fn package(manifest: &str, entities: &str) -> Result<ferrum_meta::Package, String> {
    let files = BTreeMap::from([("package.yaml".to_string(), manifest.to_string()), ("entities/shop.yaml".to_string(), entities.to_string())]);
    load_package_files(&files).map_err(|e| e.to_string())
}

/// The shop with `from` replaced by `to` in its entities (`in_manifest`: in its manifest), loaded with core.
fn load(from: &str, to: &str, in_manifest: bool) -> Result<Catalog, String> {
    let (mut m, mut e) = (MANIFEST.to_string(), SHOP.to_string());
    let target = if in_manifest { &mut m } else { &mut e };
    assert!(from.is_empty() || target.contains(from), "the fixture holds {from}");
    *target = target.replacen(from, to, 1);
    Catalog::from_packages(vec![core(), package(&m, &e)?]).map_err(|e| e.to_string())
}

#[track_caller]
fn refused(from: &str, to: &str, why: &str) {
    match load(from, to, false) {
        Ok(_) => panic!("{to} loads"),
        Err(e) => assert!(e.contains(why), "{to}: {e}"),
    }
}

#[track_caller]
fn refused_role(from: &str, to: &str, why: &str) {
    match load(from, to, true) {
        Ok(_) => panic!("{to} loads"),
        Err(e) => assert!(e.contains(why), "{to}: {e}"),
    }
}

#[test]
fn the_shop_loads() {
    load("", "", false).expect("every declaration of the fixture is valid");
}

#[test]
fn audience_on_an_entity_of_a_package_with_internal_entities_is_refused() {
    let internal =
        "- name: Payslip\n  label: Payslip\n  kind: document\n  internal: true\n  fields: [{name: amount, label: Amount, type: int}]\n- name: WebItem\n";
    match load("- name: WebItem\n", internal, false) {
        Ok(_) => panic!("loads"),
        Err(e) => assert!(e.contains("internal") && e.contains("WebItem"), "{e}"),
    }
}

#[test]
fn record_writes_cannot_face_customers() {
    refused("    read: {audience: anyone", "    create: {audience: anyone}\n    read: {audience: anyone", "create");
    refused_role("{entity: Review, actions: [submit_review]}", "{entity: Review, create: true}", "visitor");
}

#[test]
fn an_overlay_cannot_open_a_floor_entity() {
    let payroll = package(
        "package: payroll\nversion: 0.1.0\ndepends: [core]\n",
        "- name: Payslip\n  label: Payslip\n  kind: document\n  internal: true\n  fields: [{name: amount, label: Amount, type: int}]\n",
    )
    .unwrap();
    let opener = package(
        "package: opener\nversion: 0.1.0\ndepends: [payroll]\noverlays:\n  - entity: Payslip\n    ops:\n      - {op: add_action, action: {name: show, label: Show, read_only: true, permission: read, contract: {effect: query, returns: {type: object, properties: {a: {type: string}}}, completion: Shown, audience: anyone}}}\n",
        "[]",
    )
    .unwrap();
    let e = Catalog::from_packages(vec![core(), payroll, opener]).map(|_| ()).unwrap_err().to_string();
    assert!(e.contains("opener") && e.contains("Payslip"), "{e}");
}

#[test]
fn cache_only_on_anonymous_queries_and_at_most_300() {
    refused("cache: 30}", "cache: 301}", "cache");
    refused("audience: anyone, cache: 30}", "audience: customer, cache: 30}", "cache");
    refused("        rate: [{per: address, max: 30, seconds: 3600}]", "        rate: [{per: address, max: 30, seconds: 3600}]\n        cache: 10", "cache");
    refused(
        "read: {audience: anyone, fields: [item_name, slug, ranking], cache: 60}",
        "read: {audience: customer, fields: [item_name, slug, ranking], cache: 60}",
        "cache",
    );
}

#[test]
fn every_anonymous_create_needs_a_guard() {
    refused("        guard: {scope: shop.cart}\n", "", "guard");
    refused("guard: {scope: shop.cart}", "guard: {scope: Shop Cart}", "guard");
    refused("guard: {scope: shop.review, address: email}", "guard: {scope: shop.review, address: body}", "address");
}

#[test]
fn handle_names_a_linked_result_field_and_grants_anyone_actions_of_its_package() {
    refused("handle: {field: cart,", "handle: {field: lines,", "handle");
    refused("days: 30, grants", "days: 91, grants", "days");
    refused("grants: [add_item]", "grants: []", "grants");
    refused("grants: [add_item]", "grants: [start]", "start");
    refused("grants: [add_item]", "grants: [nothing]", "nothing");
    // A grant of an action whose audience is staff.
    refused("        completion: The item is in the cart\n        audience: anyone\n", "        completion: The item is in the cart\n", "add_item");
}

#[test]
fn customer_facing_returns_are_closed() {
    refused("returns: {type: object, properties: {received: {type: boolean}}}", "returns: {type: object}", "returns");
    refused(
        "returns: {type: object, properties: {received: {type: boolean}}}",
        "returns: {type: object, properties: {received: {anyOf: [{type: boolean}]}}}",
        "anyOf",
    );
    refused(
        "returns: {type: object, properties: {received: {type: boolean}}}",
        "returns: {type: object, additionalProperties: true, properties: {received: {type: boolean}}}",
        "additionalProperties",
    );
    refused(
        "returns: {type: object, properties: {received: {type: boolean}}}",
        "returns: {type: object, properties: {received: {type: [boolean, string]}}}",
        "type",
    );
    refused("returns: {type: object, properties: {received: {type: boolean}}}", "returns: {type: object, properties: {r: {type: array}}}", "items");
    refused("        completion: The review is kept for moderation\n", "", "completion");
}

#[test]
fn rate_subjects_fit_the_audience_and_windows_end_at_an_hour() {
    refused("rate: [{per: address, max: 30, seconds: 3600}]", "rate: [{per: address, max: 30, seconds: 3601}]", "seconds");
    refused("rate: [{per: address, max: 30, seconds: 3600}]", "rate: [{per: address, max: 0, seconds: 60}]", "max");
    refused("rate: [{per: address, max: 30, seconds: 3600}]", "rate: [{per: customer, max: 30, seconds: 60}]", "customer");
    refused("rate: [{per: address, max: 30, seconds: 3600}]", "rate: [{per: key, max: 30, seconds: 60}]", "key");
    refused("rate: [{per: address, max: 30, seconds: 3600}]", "rate: [{per: handle, max: 30, seconds: 60}]", "handle");
    refused(
        "rate: [{per: address, max: 30, seconds: 3600}]",
        "rate: [{per: address, max: 1, seconds: 1}, {per: address, max: 2, seconds: 2}, {per: address, max: 3, seconds: 3}, {per: address, max: 4, seconds: 4}]",
        "three",
    );
}

#[test]
fn sunset_is_ninety_days_after_deprecation() {
    refused("sunset: 2027-04-10", "sunset: 2027-04-09", "90 days");
    refused("deprecated_since: 2027-01-10, sunset: 2027-04-10, ", "sunset: 2027-04-10, ", "deprecated_since");
    refused("deprecated: checkout replaces it, ", "", "deprecated");
}

#[test]
fn collection_actions_take_no_docstatus_condition_or_effects_and_their_rules_no_where() {
    refused(
        "      collection: true\n      permission: create\n      params: [{name: currency",
        "      collection: true\n      docstatus: [0]\n      permission: create\n      params: [{name: currency",
        "docstatus",
    );
    refused(
        "      collection: true\n      permission: create\n      params: [{name: currency",
        "      collection: true\n      condition: \"true\"\n      permission: create\n      params: [{name: currency",
        "condition",
    );
    refused(
        "      collection: true\n      permission: create\n      params: [{name: currency",
        "      collection: true\n      permission: write\n      params: [{name: currency",
        "permission",
    );
    refused_role("{entity: Cart, actions: [start, peek]}", "{entity: Cart, actions: [start], where: {currency: EUR}}", "start");
}

#[test]
fn entity_api_refuses_secret_grouped_personal_rollup_and_linked_fields_for_anyone() {
    for field in ["pin", "cost", "contact", "supplier_name"] {
        refused("read: {audience: anyone, fields: [item_name, slug, ranking]", &format!("read: {{audience: anyone, fields: [item_name, {field}]"), field);
    }
    refused("read: {audience: anyone, fields: [item_name, slug, ranking]", "read: {audience: anyone, fields: [item_name, nope]", "nope");
    refused("read: {audience: anyone, fields: [item_name, slug, ranking], cache: 60}", "read: {audience: staff, fields: [item_name]}", "audience");
    // A customer may read an email, never a field group or a secret.
    load("read: {audience: anyone, fields: [item_name, slug, ranking], cache: 60}", "read: {audience: customer, fields: [item_name, contact]}", false)
        .expect("a customer reads an email");
}

#[test]
fn filters_and_sort_are_fields_shown() {
    refused("filters: [slug]", "filters: [published]", "published");
    refused("sort: [\"ranking desc\"]", "sort: [\"published desc\"]", "published");
    refused("sort: [\"ranking desc\"]", "sort: [\"ranking sideways\"]", "sort");
    refused("limit: 48", "limit: 101", "limit");
    refused("list: {audience: anyone, fields: [item_name, slug, ranking]", "list: {audience: anyone, fields: [slug, ranking]", "search");
}

#[test]
fn visitor_rules_read_with_where_or_reference_data_and_name_actions_only() {
    refused_role("{entity: WebItem, read: true, where: {published: true}}", "{entity: WebItem, read: true}", "where");
    refused_role("{entity: WebItem, read: true, where: {published: true}}", "{entity: \"*\", read: true}", "*");
    refused_role("{entity: Cart, actions: [start, peek]}", "{entity: Cart, actions: [start, add_item]}", "add_item");
    refused_role(
        "{entity: WebItem, read: true, where: {published: true}}",
        "{entity: WebItem, read: true, where: {published: true}, read_groups: [costing]}",
        "visitor",
    );
    // Reference data (no personal data) may be read whole.
    load(
        "{entity: WebItem, read: true, where: {published: true}}",
        "{entity: WebItem, read: true, where: {published: true}}\n      - {entity: Review, read: true}",
        true,
    )
    .expect("reference data, whole");
}

#[test]
fn visitor_and_portal_are_exclusive() {
    refused_role("    visitor: true\n", "    visitor: true\n    portal: true\n", "visitor or portal");
}

#[test]
fn called_for_outside_only_on_a_floor_entity_in_its_own_package() {
    let floor = |extra: &str| {
        package(
            "package: pay\nversion: 0.1.0\ndepends: [core]\n",
            &format!(
                "- name: Intent\n  label: Intent\n  kind: document\n  internal: true\n  {extra}\n  fields:\n    - {{name: source_entity, label: Source, type: data}}\n    - {{name: source_id, label: Source Id, type: data}}\n    - {{name: amount, label: Amount, type: int}}\n  actions: [{{name: refund, label: Refund}}]\n"
            ),
        )
        .and_then(|p| Catalog::from_packages(vec![core(), p]).map(|_| ()).map_err(|e| e.to_string()))
    };
    floor("called_for_outside: {via: [source_entity, source_id], operations: [create, read, refund]}").expect("a floor entity of its own package");
    assert!(floor("called_for_outside: {via: [amount, source_id], operations: [read]}").unwrap_err().contains("amount"));
    assert!(floor("called_for_outside: {via: [source_entity, source_id], operations: [delete]}").unwrap_err().contains("delete"));
    // Not a floor package.
    refused(
        "  kind: master\n  search_fields: [item_name]\n",
        "  kind: master\n  called_for_outside: {via: [slug, slug], operations: [read]}\n  search_fields: [item_name]\n",
        "called_for_outside",
    );
}

#[test]
fn customizations_cannot_declare_an_audience() {
    let cat = load("", "", false).unwrap();
    let action: ActionDef = serde_yaml::from_str(
        "{name: x_show, label: Show, permission: write, effects: [{type: set_fields, values: {status: \"'Approved'\"}}], contract: {completion: Shown, audience: anyone}}",
    )
    .unwrap();
    let overlay = Overlay { entity: "Review".into(), ops: vec![OverlayOp::AddAction { action }] };
    let schema = ferrum_meta::effective::build(&cat, &["shop".to_string()], &[overlay], 0).unwrap();
    assert!(!schema.entity("Review").unwrap().def.actions.iter().any(|a| a.name == "x_show"), "not added");
    assert!(schema.warnings.iter().any(|w| w.message.contains("audience")), "{:?}", schema.warnings);
}
