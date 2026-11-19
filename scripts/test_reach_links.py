#!/usr/bin/env python3
"""scripts/reach_links.py finds an app's code reading records as the system by a link to a mergeable entity that the
entity does not declare in `reach_links` and the registry does not explain."""

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import reach_links  # noqa: E402

ENTITIES = """
- name: Customer
  label: Customer
  kind: master
  mergeable: true
  fields:
    - {name: customer_name, type: data}
- name: Invoice
  label: Invoice
  kind: master
  fields:
    - {name: customer, type: link, to: Customer}
    - {name: total, type: currency}
"""

PORTAL = """
pub async fn invoices(k: &Kernel, ctx: &Ctx, tx: &mut Tx, customer: Uuid) -> Result<Vec<Doc>> {
    k.find_tx(&sys(ctx), tx, "Invoice", &[Filter::eq("customer", customer.to_string())], None, 100).await
}

pub async fn mine(k: &Kernel, ctx: &Ctx, tx: &mut Tx, customer: Uuid) -> Result<Vec<Doc>> {
    k.find_tx(ctx, tx, "Invoice", &[Filter::eq("customer", customer.to_string())], None, 100).await
}

#[cfg(test)]
mod tests {
    async fn t(k: &Kernel, ctx: &Ctx, tx: &mut Tx) {
        k.find_tx(&sys(ctx), tx, "Invoice", &[Filter::eq("customer", "x")], None, 1).await;
    }
}
"""


def app(entities: str) -> Path:
    root = Path(tempfile.mkdtemp())
    (root / "packages/x/entities").mkdir(parents=True)
    (root / "packages/x/entities/x.yaml").write_text(entities)
    (root / "src").mkdir()
    (root / "src/portal.rs").write_text(PORTAL)
    return root


def test_an_undeclared_system_read_by_a_link_fails():
    root = app(ENTITIES)
    found = reach_links.findings("x", root, reach_links.entities(root), set())
    # Only the system's read: the person's own is scoped by their rules, and tests are not read.
    assert found == [("x", "src/portal.rs", 2, "Invoice", "customer")], found


def test_a_declared_link_or_a_registry_row_passes():
    root = app(ENTITIES.replace("  kind: master\n  fields:\n    - {name: customer, type: link", "  kind: master\n  reach_links: [customer]\n  fields:\n    - {name: customer, type: link"))
    assert reach_links.findings("x", root, reach_links.entities(root), set()) == []
    root = app(ENTITIES)
    assert reach_links.findings("x", root, reach_links.entities(root), {("x", "Invoice", "customer")}) == []


def test_a_link_to_an_entity_that_does_not_merge_is_not_read():
    root = app(ENTITIES.replace("  mergeable: true\n", ""))
    assert reach_links.findings("x", root, reach_links.entities(root), set()) == []


if __name__ == "__main__":
    for name, f in list(globals().items()):
        if name.startswith("test_"):
            f()
    print("ok")
