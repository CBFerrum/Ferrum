#!/usr/bin/env python3
"""Links an app's code reaches records by, as the system, that no rule names (docs/guides/05; S9 K0-2, R19).

    scripts/reach_links.py            # fails on an undeclared one

An app's code that lists or queries records as the system (`sys(`, `elevated(`, `Ctx::system`, `system_for`) and
filters them by a link to a mergeable entity (an invoice a portal shows by its customer) decides who reaches those
records by that link, though no rule says so. A merge moving such a link must be an administrator's, so the link is
declared in its entity's `reach_links`, or `docs/registry/reach-links.md` says why it decides no one's reach. Reads
the apps the lock compiles in (as scripts/mutation_paths.py does), their sources but not their tests, and their
packages' entities.
"""

import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

ROOT = Path(__file__).resolve().parent.parent
REGISTRY = ROOT / "docs/registry/reach-links.md"
SYSTEM = re.compile(r"\bsys\(|\belevated\(|Ctx::system\b|\bsystem_for\(")
ENTITY_LITERAL = re.compile(r'"([A-Z][A-Za-z0-9]*)"')
# A filter on a field: Filter::eq("f", ..), Filter { field: "f".into() .. }, a JSON filter ["f", "=", ..].
FILTER = re.compile(r'Filter::eq\(\s*"([a-z_][a-z0-9_]*)"|field:\s*"([a-z_][a-z0-9_]*)"|\[\s*"([a-z_][a-z0-9_]*)"\s*,\s*"(?:=|in)"')
ROW = re.compile(r"^\| ([a-z0-9-]+) \| `([A-Za-z0-9]+)\.([a-z0-9_]+)` \| (.+) \|$")


def entities(root: Path) -> dict:
    """{entity: {"links": {field: target}, "mergeable": bool, "reach_links": set}} from the app's packages."""
    out = {}
    for path in sorted(root.glob("packages/*/entities/*.yaml")):
        current = None
        for line in path.read_text(errors="replace").split("\n"):
            m = re.match(r"^- name:\s*([A-Za-z0-9]+)", line)
            if m:
                current = out.setdefault(m.group(1), {"links": {}, "mergeable": False, "reach_links": set()})
                continue
            if current is None:
                continue
            if re.match(r"^\s+mergeable:\s*true", line):
                current["mergeable"] = True
            m = re.match(r"^\s+reach_links:\s*\[([^\]]*)\]", line)
            if m:
                current["reach_links"] |= {x.strip() for x in m.group(1).split(",") if x.strip()}
            m = re.search(r"\{name:\s*([a-z_][a-z0-9_]*),\s*type:\s*link,\s*to:\s*([A-Za-z0-9]+)", line)
            if m:
                current["links"][m.group(1)] = m.group(2)
    return out


def findings(app: str, root: Path, known: dict, registered: set, platform: dict | None = None) -> list:
    """(app, file, line, entity, field) of each undeclared system read by a link to a mergeable entity (the app's own,
    or the platform's: `platform`, its packages' entities)."""
    mergeable = {e for e, d in {**(platform or {}), **known}.items() if d["mergeable"]}
    out = []
    for path in sorted(root.rglob("*.rs")):
        rel = path.relative_to(root).as_posix()
        if {"tests", "target", "examples", "benches"} & set(rel.split("/")):
            continue
        text = path.read_text(errors="replace")
        text = text.split("#[cfg(test)]")[0]
        # One function at a time: one that runs something as the system, names an entity and filters by a link of it.
        for start, statement in statements(text):
            if not SYSTEM.search(statement):
                continue
            for e in ENTITY_LITERAL.findall(statement):
                d = known.get(e)
                if not d:
                    continue
                for m in FILTER.finditer(statement):
                    f = next(g for g in m.groups() if g)
                    hit = (app, rel, start, e, f)
                    if d["links"].get(f) in mergeable and f not in d["reach_links"] and (app, e, f) not in registered and hit not in out:
                        out.append(hit)
    return out


def statements(text: str) -> list:
    """(line, text) of each function: a system read is often built in one statement and run in the next."""
    out = []
    buf, start = [], 1
    for n, line in enumerate(text.split("\n"), 1):
        if re.match(r"\s*(pub(\([^)]*\))?\s+)?(async\s+)?fn\s", line):
            if buf:
                out.append((start, "\n".join(buf)))
            buf, start = [], n
        buf.append(line)
    if buf:
        out.append((start, "\n".join(buf)))
    return out


def registered() -> set:
    rows = set()
    if REGISTRY.exists():
        for line in REGISTRY.read_text().split("\n"):
            m = ROW.match(line)
            if m:
                rows.add((m.group(1), m.group(2), m.group(3)))
    return rows


def main() -> int:
    from mutation_paths import repositories

    found = []
    reg = registered()
    platform = entities(ROOT)
    for app, root in sorted(repositories().items()):
        found += findings(app, root, entities(root), reg, platform)
    for app, rel, n, e, f in found:
        print(f"{app} {rel}:{n}: reads {e} by {f} as the system: declare `reach_links: [{f}]` on {e}, or say why in {REGISTRY.relative_to(ROOT)}")
    print(f"{len(found)} undeclared system reads by a link to a mergeable entity")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main())
