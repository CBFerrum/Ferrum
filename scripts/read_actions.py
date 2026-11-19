#!/usr/bin/env python3
"""Actions declared `permission: read` that write, reviewed (docs/guides/05, "Actions any reader presses"; S9 K0-6).

    scripts/read_actions.py            # fails on an action missing from the registry, or a row naming a test the app lacks
    scripts/read_actions.py --strict   # also fails on a row not yet classed or without its test
    scripts/read_actions.py --write    # adds rows for new actions (class `?`, no test) and drops rows for gone ones

An action needs exactly its declared `permission` on the record, or a rule naming it. `permission: read` on an action
that is not `read_only` is allowed when `docs/registry/read-actions.md` says which it is: **A** the presser's own part
(their enrolment, feed, acknowledgement, vote, or a tally every reader adds to); **B** an outside person's answer, the
handler refusing members (`ctx.outside`); **C** its record stays as it is (it reads, or makes or changes other records
under the presser's own rights); **D** the handler checks a narrower right itself and changes its own record only
where the presser's Write scope reaches it. Each row names the test that presses the action as someone it must refuse.
Reads every app the lock compiles in (as scripts/agent_parity.py does) and this workspace's own.
"""

import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

ROOT = Path(__file__).resolve().parent.parent
REGISTRY = ROOT / "docs/registry/read-actions.md"
CLASSES = {"A", "B", "C", "D", "?"}
ROW = re.compile(r"^\|\s*([a-z0-9-]+)\s*\|\s*`([A-Za-z0-9]+)\.([a-z0-9_]+)`\s*\|\s*([A-D?])\s*\|\s*(.*?)\s*\|\s*(.*?)\s*\|\s*(`[a-z0-9_]+`|—)\s*\|$")
HEADER = "| App | Action | Class | Evidence | Narrower right | Test |\n|---|---|---|---|---|---|\n"


def read_actions(app: str, root: Path) -> list:
    """(app, entity, action) of each action declared `permission: read` and not `read_only` in the app's packages."""
    from agent_parity import load

    out = []
    for manifest in sorted(root.rglob("package.yaml")):
        if {"tests", "target", "node_modules", ".git"} & set(manifest.relative_to(root).parts):
            continue
        pkg = load(manifest) or {}
        listed = []
        for f in sorted((manifest.parent / "entities").glob("*.yaml")):
            doc = load(f) or []
            for e in doc if isinstance(doc, list) else [doc]:
                listed += [(e.get("name"), a) for a in e.get("actions") or []]
        for o in pkg.get("overlays") or []:
            listed += [(o.get("entity"), op.get("action") or {}) for op in o.get("ops") or [] if op.get("op") == "add_action"]
        for entity, a in listed:
            if a.get("permission") == "read" and not a.get("read_only"):
                out.append((app, entity, a.get("name")))
    return sorted(set(out))


def has_test(root: Path, name: str) -> bool:
    """Whether the app's sources or tests hold `fn name(`."""
    pattern = re.compile(rf"\bfn\s+{re.escape(name)}\s*\(")
    return any(pattern.search(p.read_text(errors="replace")) for p in root.rglob("*.rs") if "target" not in p.parts)


def rows(text: str) -> list:
    """(app, entity, action, class, evidence, narrower, test or None) of each registry row."""
    out = []
    for line in text.split("\n"):
        m = ROW.match(line)
        if m:
            app, entity, action, cls, evidence, narrower, test = m.groups()
            out.append((app, entity, action, cls, evidence, narrower, None if test == "—" else test.strip("`")))
    return out


def check(found: list, listed: list, roots: dict, strict: bool) -> list:
    """What is wrong, as lines: an action missing from the registry, a row for an action gone, a row naming a test its
    app does not have; with `strict`, a row not classed or without its test."""
    problems = []
    keys = {(r[0], r[1], r[2]) for r in listed}
    for a in found:
        if a not in keys:
            problems.append(f"not in the registry: {a[0]} {a[1]}.{a[2]} is declared `permission: read` and is not read_only")
    have = set(found)
    for r in listed:
        if (r[0], r[1], r[2]) not in have:
            problems.append(f"in the registry, not in the code: {r[0]} {r[1]}.{r[2]}")
        elif r[6] and r[0] in roots and not has_test(roots[r[0]], r[6]):
            problems.append(f"{r[0]} {r[1]}.{r[2]} names the test {r[6]}, which the app does not have")
        elif strict and (r[3] == "?" or not r[6]):
            problems.append(f"not reviewed yet: {r[0]} {r[1]}.{r[2]} ({'no class' if r[3] == '?' else 'no test'})")
    return problems


def write(found: list, listed: list) -> None:
    keep = {(r[0], r[1], r[2]): r for r in listed}
    table = []
    for a in found:
        r = keep.get(a) or (*a, "?", "", "", None)
        table.append(f"| {r[0]} | `{r[1]}.{r[2]}` | {r[3]} | {r[4]} | {r[5]} | {f'`{r[6]}`' if r[6] else '—'} |")
    text = REGISTRY.read_text()
    head, rest = text.split(HEADER, 1)
    tail = "\n".join(l for l in rest.split("\n") if not l.startswith("|"))
    REGISTRY.write_text(head + HEADER + "\n".join(table) + "\n" + tail.lstrip("\n"))


def main() -> int:
    from agent_parity import apps

    roots = apps()
    found = sorted(a for app, root in sorted(roots.items()) for a in read_actions(app, root))
    listed = rows(REGISTRY.read_text())
    if "--write" in sys.argv:
        write(found, listed)
        listed = rows(REGISTRY.read_text())
    problems = check(found, listed, roots, "--strict" in sys.argv)
    for p in problems:
        print(p)
    pending = sum(1 for r in listed if r[3] == "?" or not r[6])
    print(f"{len(found)} actions declared `permission: read` that are not read_only; {len(listed)} in the registry, {pending} not reviewed yet")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
