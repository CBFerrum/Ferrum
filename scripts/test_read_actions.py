#!/usr/bin/env python3
"""scripts/read_actions.py: every action declared `permission: read` that is not read_only is in the registry, and
every row's test is one the app has (docs/guides/05; S9 K0-6)."""

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import read_actions  # noqa: E402

PACKAGE = """package: x
version: 0.1.0
overlays:
  - entity: Order
    ops:
      - {op: add_action, action: {name: cancel_mine, label: Cancel, permission: read}}
"""
ENTITIES = """
- name: Ticket
  label: Ticket
  kind: master
  actions:
    - {name: rate, label: Rate, permission: read}
    - {name: summary, label: Summary, permission: read, read_only: true}
    - {name: close, label: Close}
  fields:
    - {name: subject, type: data}
"""
TESTS = "#[test]\nfn only_the_raiser_rates_a_ticket() {}\n"


def app() -> Path:
    root = Path(tempfile.mkdtemp())
    (root / "packages/x/entities").mkdir(parents=True)
    (root / "packages/x/package.yaml").write_text(PACKAGE)
    (root / "packages/x/entities/x.yaml").write_text(ENTITIES)
    (root / "tests").mkdir()
    (root / "tests/x_it.rs").write_text(TESTS)
    return root


def registry(*lines: str) -> list:
    return read_actions.rows("\n".join(lines))


def test_every_read_permission_action_is_in_the_registry():
    root = app()
    found = read_actions.read_actions("x", root)
    # Read-only actions and those declaring another permission are not listed.
    assert found == [("x", "Order", "cancel_mine"), ("x", "Ticket", "rate")], found
    listed = registry("| x | `Ticket.rate` | A | the raiser's own score | | `only_the_raiser_rates_a_ticket` |")
    problems = read_actions.check(found, listed, {"x": root}, False)
    assert problems == ["not in the registry: x Order.cancel_mine is declared `permission: read` and is not read_only"], problems
    # A row for an action gone fails too.
    listed += registry("| x | `Order.cancel_mine` | B | the shopper's | | — |", "| x | `Ticket.gone` | C | | | — |")
    assert read_actions.check(found, listed, {"x": root}, False) == ["in the registry, not in the code: x Ticket.gone"]


def test_every_registry_row_names_a_test_the_app_has():
    root = app()
    found = read_actions.read_actions("x", root)
    listed = registry(
        "| x | `Ticket.rate` | A | the raiser's own score | | `no_such_test` |",
        "| x | `Order.cancel_mine` | ? | | | — |",
    )
    assert read_actions.check(found, listed, {"x": root}, False) == ["x Ticket.rate names the test no_such_test, which the app does not have"]
    # Strict: a row not classed, or without its test, is not reviewed yet.
    listed = registry("| x | `Ticket.rate` | A | own | | `only_the_raiser_rates_a_ticket` |", "| x | `Order.cancel_mine` | ? | | | — |")
    assert read_actions.check(found, listed, {"x": root}, True) == ["not reviewed yet: x Order.cancel_mine (no class)"]
    assert read_actions.check(found, listed, {"x": root}, False) == []


if __name__ == "__main__":
    for name, f in list(globals().items()):
        if name.startswith("test_"):
            f()
    print("ok")
