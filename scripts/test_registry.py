#!/usr/bin/env python3
"""A claim needs what it claims. Each of these is a way of saying more than the evidence does."""

import re

import registry

good = {
    "id": "FCP-QUAL-001",
    "title": "Requirement traceability",
    "workstream": "W10",
    "status": "verified_software",
    "files": ["scripts/registry.py"],
    "tests": ["scripts/test_registry.py"],
    "limits": "None",
}
fine = len(registry.fcp_problems([good]))  # What is missing from a list of one.


def says(**changes) -> list:
    return [p for p in registry.fcp_problems([{**good, **changes}]) if p.startswith("FCP-QUAL-001") and "missing from" not in p]


assert says() == []
# AT-QUAL-001: evidence taken away.
assert any("names no tests" in p for p in says(tests=[]))
assert any("cites no implementation" in p for p in says(files=[]))
assert any("does not exist" in p for p in says(tests=["scripts/gone.py"]))
digest = "sha256:" + "a" * 64
qualified = {"status": "qualified_profile", "profile": "RP-01", "release_digest": digest}
assert any("needs a profile" in p for p in says(status="qualified_profile"))
# AT-QUAL-003: an AI's own review is not an independent one.
assert any("not an independent reviewer" in p for p in says(**qualified, reviewer="Claude"))
# AT-QUAL-004 and 005: a mock or a simulator cannot be labelled qualified while the real thing is missing.
assert any("while blocked" in p for p in says(**qualified, reviewer="Claude", blockers=["BLOCK-REAL-TENANTS"]))
assert any("while blocked" in p for p in says(**qualified, reviewer="Claude", blockers=["BLOCK-INDUSTRIAL-HARDWARE"]))
assert any("blocked, but on nothing named" in p for p in says(status="blocked_external"))
assert any("unknown blocker" in p for p in says(blockers=["BLOCK-NOT-A-THING"]))
assert any("says what it does not yet do" in p for p in says(limits=""))
# AT-SECRET-008 and AT-AI-007: what a document may not claim.
for said in ("Operators cannot read your files", "It is zero-knowledge", "Your data stays on your device"):
    assert any(re.search(pattern, said) for _, pattern, _ in registry.DRIFT), said
assert not any(re.search(p, "Nobody here can honestly claim that an operator of a machine cannot read what it processes") for _, p, _ in registry.DRIFT)
print("ok")
