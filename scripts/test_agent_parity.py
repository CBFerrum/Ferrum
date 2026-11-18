#!/usr/bin/env python3
"""scripts/agent_parity.py finds an app's routes that write outside the gate and its visible actions without a
contract, leaves out tests and comments, and never lets the allowed numbers grow."""

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import agent_parity  # noqa: E402

SITE = '''
use ferrum_kernel::{Kernel, Site};

pub struct XSite;

impl Site for XSite {
    async fn handle(&self, k: &Kernel, req: SiteRequest) -> Result<SiteResponse> {
        if req.method == "DELETE" { return gone(k).await; }
        let out = match (req.method.as_str(), segs.as_slice()) {
            ("GET", ["things"]) => list(k).await?,
            ("POST", ["things"]) => (201, make(k, &req).await?),
            ("POST", ["things", t, "close"]) => close(k, t).await?,
            ("PUT" | "PATCH", ["things", t]) => {
                k.invoke(&ctx, &Invocation::new("x.thing.rename", body)).await?;
                note(k, t).await?
            }
            ("POST", ["things", "search"]) => search(k, &req).await?,
            // ("POST", ["in", "a", "comment"]) => k.insert(&ctx, doc).await?,
            _ => return Err(Error::NotFound("x".into())),
        };
        Ok(out)
    }
}

async fn make(k: &Kernel, req: &SiteRequest) -> Result<Value> {
    let doc = shape(req)?;
    store(k, doc).await
}

async fn store(k: &Kernel, doc: Doc) -> Result<Value> {
    // k.insert(ctx, doc) in a comment writes nothing
    let ctx = Ctx::system();
    Ok(k.insert(&ctx, doc).await?.to_json())
}

async fn close(k: &Kernel, t: &str) -> Result<Value> {
    k.invoke(&ctx, &Invocation::new("x.thing.close", json!({"id": t})).key(t)).await
}

async fn note(k: &Kernel, t: &str) -> Result<Value> {
    sqlx::query("UPDATE e_x_thing SET noted = true WHERE id = $1").execute(&k.db.pool).await?;
    Ok(json!({}))
}

async fn search(k: &Kernel, req: &SiteRequest) -> Result<Value> {
    let _ = "update means nothing here without a table and set";
    k.list(&ctx, "Thing", q).await
}

async fn gone(k: &Kernel) -> Result<SiteResponse> {
    k.delete(&ctx, "Thing", id).await
}

#[cfg(test)]
mod tests {
    async fn t(k: &Kernel) { k.insert(&ctx, doc).await.unwrap(); }
}
'''

ENTITIES = '''
- name: Thing
  label: Thing
  kind: master
  fields: [{name: title, type: data}]
  actions:
    - {name: close, label: Close, contract: {effect: transactional_action, returns: {type: object}, completion: "Closed"}}
    - {name: reopen, label: Reopen, contract: {effect: transactional_action, completion: "Open again"}}
    - {name: recount, label: Recount, hidden: true}
    - {name: archive, label: Archive}
'''

PACKAGE = '''
package: x
version: 0.1.0
overlays:
  - entity: Contact
    ops:
      - {op: add_field, field: {name: thing, type: link, to: Thing}}
      - op: add_action
        action: {name: make_thing, label: Make Thing}
'''


def app_tree(d: Path) -> Path:
    root = d / "ferrum-app-x"
    (root / "src").mkdir(parents=True)
    (root / "src/site.rs").write_text(SITE)
    (root / "tests").mkdir()
    (root / "tests/it.rs").write_text('impl Site for T { async fn handle() { match m { ("POST", ["t"]) => k.insert(c, d), } } }')
    (root / "packages/x/entities").mkdir(parents=True)
    (root / "packages/x/package.yaml").write_text(PACKAGE)
    (root / "packages/x/entities/thing.yaml").write_text(ENTITIES)
    return root


def routes(root: Path) -> dict:
    return {r[2]: r for r in agent_parity.site_routes("x", root)}


def a_site_arm_reaching_a_write_two_calls_down_is_found(root):
    r = routes(root)['["things"]']
    assert r == ("x:src/site.rs", "POST", '["things"]', "make"), r


def an_arm_that_only_invokes_the_gate_passes(root):
    assert '["things", t, "close"]' not in routes(root)


def an_arm_that_invokes_and_also_writes_is_found(root):
    r = routes(root)['["things", t]']
    assert r[1] == "PUT/PATCH", r


def a_post_that_only_reads_passes(root):
    assert '["things", "search"]' not in routes(root)


def test_code_and_comments_are_left_out(root):
    assert sorted(routes(root)) == ['["things", t]', '["things"]', 'if req.method == "DELETE" { return gone(k).await; }'], routes(root)


def a_method_test_outside_an_arm_is_found_for_a_person_to_read(root):
    r = routes(root)['if req.method == "DELETE" { return gone(k).await; }']
    assert r[1:] == ("DELETE", r[2], "?"), r


def a_hidden_action_needs_no_contract(root):
    found = {a[3] for a in agent_parity.actions("x", root)}
    assert "recount" not in found and "close" not in found, found


def a_visible_action_without_returns_is_found(root):
    found = {a[3]: a for a in agent_parity.actions("x", root)}
    assert found["reopen"] == ("x", "x", "Thing", "reopen") and "archive" in found, found


def an_overlay_add_action_is_read(root):
    found = {a[3]: a for a in agent_parity.actions("x", root)}
    assert found["make_thing"] == ("x", "x", "Contact", "make_thing"), found


def registry(d: Path, routes_allowed: int, actions_allowed: int) -> None:
    agent_parity.REGISTRY = d / "agent-parity.md"
    agent_parity.REGISTRY.write_text(
        f"# Agent parity\n\nRoutes in debt allowed: {routes_allowed}\nActions without a contract allowed: {actions_allowed}\n\n"
        "## Routes\n\n| Where | Method | Route | Handler | Kind |\n|---|---|---|---|---|\n"
        "| `x:src/site.rs` | POST | `[\"a\"]` | `a` | debt |\n"
        "| `x:src/site.rs` | POST | `[\"b\" \\| \"c\"]` | `b` | debt |\n\n"
        "## Actions without a contract\n\n| App | Package | Entity | Action |\n|---|---|---|---|\n| x | x | Thing | archive |\n"
    )


FOUND_ROUTES = [("x:src/site.rs", "POST", '["a"]', "a"), ("x:src/site.rs", "POST", '["b" | "c"]', "b")]
FOUND_ACTIONS = [("x", "x", "Thing", "archive"), ("x", "x", "Thing", "reopen")]


def write_never_raises_the_allowed_numbers(d):
    registry(d, 1, 1)
    reg = agent_parity.read_registry()
    assert len(reg["routes"]) == 2 and reg["routes"][1][2] == '["b" | "c"]', reg["routes"]
    problems = agent_parity.problems(FOUND_ROUTES, FOUND_ACTIONS, reg, set())
    assert any("2 routes in debt, 1 allowed" in p for p in problems), problems
    agent_parity.write_registry(FOUND_ROUTES, FOUND_ACTIONS, reg)
    reg = agent_parity.read_registry()
    assert (reg["allowed_routes"], reg["allowed_actions"]) == (1, 1), reg
    problems = agent_parity.problems(FOUND_ROUTES, FOUND_ACTIONS, reg, set())
    assert any("2 routes in debt, 1 allowed" in p for p in problems), problems
    assert any("2 visible actions without a contract, 1 allowed" in p for p in problems), problems


def write_lowers_them(d):
    registry(d, 5, 5)
    reg = agent_parity.read_registry()
    found = FOUND_ROUTES[:1] + [("x:src/site.rs", "DELETE", '["n"]', "n")]
    assert agent_parity.problems(found, FOUND_ACTIONS[:1], reg, set()) == [
        "not in the registry: `x:src/site.rs` DELETE `[\"n\"]` `n`",
        "in the registry, not in the code: `x:src/site.rs` POST `[\"b\" | \"c\"]` `b`",
    ], agent_parity.problems(found, FOUND_ACTIONS[:1], reg, set())
    agent_parity.write_registry(found, FOUND_ACTIONS[:1], reg)
    reg = agent_parity.read_registry()
    assert (reg["allowed_routes"], reg["allowed_actions"]) == (1, 1), reg
    assert reg["routes"][0][4] == agent_parity.UNCLASSIFIED and reg["routes"][1][4] == "debt", reg["routes"]
    assert agent_parity.problems(found, FOUND_ACTIONS[:1], reg, set()) == ["no kind yet: `x:src/site.rs` DELETE `[\"n\"]` `n`"]
    # A kind given by a person: the check passes, and an operation must name one there is.
    text = agent_parity.REGISTRY.read_text().replace(agent_parity.UNCLASSIFIED, "operation: *.delete")
    agent_parity.REGISTRY.write_text(text)
    assert agent_parity.problems(found, FOUND_ACTIONS[:1], agent_parity.read_registry(), set()) == []
    agent_parity.REGISTRY.write_text(text.replace("*.delete", "remove_thing"))
    assert agent_parity.problems(found, FOUND_ACTIONS[:1], agent_parity.read_registry(), {"delete_record"}) == [
        "not a kind: `x:src/site.rs` DELETE `[\"n\"]` `n`: operation: remove_thing"
    ]


def the_routes_number_is_set_once_every_route_has_a_kind(d):
    registry(d, 0, 9)
    agent_parity.REGISTRY.write_text(agent_parity.REGISTRY.read_text().replace("Routes in debt allowed: 0\n", "").replace("| debt |", "| unclassified |"))
    found = FOUND_ROUTES
    agent_parity.write_registry(found, [], agent_parity.read_registry())
    assert agent_parity.read_registry()["allowed_routes"] is None
    agent_parity.REGISTRY.write_text(agent_parity.REGISTRY.read_text().replace("| unclassified |", "| debt |"))
    agent_parity.write_registry(found, [], agent_parity.read_registry())
    reg = agent_parity.read_registry()
    assert (reg["allowed_routes"], reg["allowed_actions"]) == (2, 0), reg
    assert agent_parity.problems(found, [], reg, set()) == [], agent_parity.problems(found, [], reg, set())


CONTROLLER = '''
pub fn routes() -> Routes {
    use axum::routing::{get, post as p};
    Routes::new()
        .prefix("/api")
        .add("/things", get(list).post(create))
        // .add("/old", post(old))
        .add("/things/{id}", axum::routing::put(change).delete(remove).layer(DefaultBodyLimit::max(MAX)))
        .add("/mcp", p(serve))
}

pub fn more() -> Routes {
    Routes::new().add("/files/{id}", any(files))
}
'''


def platform_routes_are_read_with_their_prefix(d):
    (d / "server/src/controllers").mkdir(parents=True)
    (d / "server/src/controllers/things.rs").write_text(CONTROLLER)
    found = agent_parity.platform_routes(d)
    where = "server/src/controllers/things.rs"
    assert found == [
        (where, "POST", "/api/things", "create"),
        (where, "PUT", "/api/things/{id}", "change"),
        (where, "DELETE", "/api/things/{id}", "remove"),
        (where, "POST", "/api/mcp", "serve"),
        (where, "ANY", "/files/{id}", "files"),
    ], found


CASES = [
    a_site_arm_reaching_a_write_two_calls_down_is_found,
    an_arm_that_only_invokes_the_gate_passes,
    an_arm_that_invokes_and_also_writes_is_found,
    a_post_that_only_reads_passes,
    test_code_and_comments_are_left_out,
    a_method_test_outside_an_arm_is_found_for_a_person_to_read,
    a_hidden_action_needs_no_contract,
    a_visible_action_without_returns_is_found,
    an_overlay_add_action_is_read,
]

with tempfile.TemporaryDirectory() as d:
    root = app_tree(Path(d))
    for case in CASES:
        case(root)
    for case in (write_never_raises_the_allowed_numbers, write_lowers_them, the_routes_number_is_set_once_every_route_has_a_kind,
                 platform_routes_are_read_with_their_prefix):
        case(Path(d))
print("agent_parity: ok")
