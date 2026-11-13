#!/usr/bin/env python3
"""The two-version window (crates/kernel/src/window.rs, docs/operations.md): the build before a change must still run
on the database the change leaves. A change that an older build cannot live with moves `COMPATIBLE_FROM` in the same
change; this check fails one that does not.

    scripts/window.py                   # what this checkout adds since its merge base with origin/main (CI)
    scripts/window.py --range A..B      # what B adds since A

What it looks at:
* new SQL files (the kernel's migrations, packages' SQL): a statement that drops a table, view, type, column or
  function, renames anything, changes a column's type, makes a column required, adds a required column without a
  default, or adds a constraint or a unique index to a table that is not new in the same file;
* packages' entities (packages/*/entities/*.yaml), as the schema sync makes tables of them: an entity or a field
  taken out, a field's type or link target changed, a field made unique, a new unique group or exclusion.
A statement that is safe all the same says so in a comment inside it: `-- window: safe, <why>`.
"""

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WINDOW = "crates/kernel/src/window.rs"
SQL = re.compile(r"^(crates/kernel/migrations/.*|packages/.*)\.sql$")
ENTITIES = re.compile(r"^packages/[^/]+/entities/[^/]+\.ya?ml$")
CONSTANT = re.compile(r'pub const COMPATIBLE_FROM: &str = "([^"]+)"')
NAME = r'"?([a-z0-9_.]+)"?'


def git(repo: Path, *args: str, ok=False) -> str:
    run = subprocess.run(["git", *args], cwd=repo, capture_output=True, text=True)
    if run.returncode != 0:
        if ok:
            return ""
        sys.exit(f"git {' '.join(args)}: {run.stderr.strip()}")
    return run.stdout


def read(repo: Path, rev: str | None, path: str) -> str:
    """The file at `rev`, or in the working tree when `rev` is None; empty when it is not there."""
    if rev is None:
        f = repo / path
        return f.read_text() if f.exists() else ""
    return git(repo, "show", f"{rev}:{path}", ok=True)


def statements(sql: str):
    """Each statement, lowercased, with function bodies and block comments taken out; line comments kept for markers."""
    sql = re.sub(r"\$([a-z_]*)\$.*?\$\1\$", "''", sql, flags=re.S | re.I)
    sql = re.sub(r"/\*.*?\*/", " ", sql, flags=re.S)
    for s in sql.split(";"):
        if s.strip():
            yield s.lower()


def sql_hazards(sql: str) -> list[str]:
    """What in this file an older build cannot live with."""
    found = []
    created = set(re.findall(r"\bcreate\s+table\s+(?:if\s+not\s+exists\s+)?" + NAME, sql, re.I))
    created = {c.lower() for c in created}
    for raw in statements(sql):
        if "window: safe" in raw:
            continue
        s = " ".join(re.sub(r"--[^\n]*", " ", raw).split())
        if not s:
            continue
        table = re.match(r"alter\s+table\s+(?:if\s+exists\s+)?(?:only\s+)?" + NAME, s)
        table = table.group(1) if table else None
        new_table = table in created
        said = s[:120]

        def hazard(what):
            found.append(f"{what}: {said}")

        m = re.search(r"\bdrop\s+(table|view|materialized\s+view|type|schema|sequence|domain|function|procedure)\s+(?:if\s+exists\s+)?" + NAME, s)
        if m and not (m.group(1) == "table" and m.group(2) in created):
            hazard(f"drops a {m.group(1)}")
        if re.search(r"\bdrop\s+column\b", s) and not new_table:
            hazard("drops a column")
        if re.search(r"\brename\b", s):
            hazard("renames")
        if re.search(r"\balter\s+column\s+\S+\s+(set\s+data\s+)?type\b", s):
            hazard("changes a column's type")
        if re.search(r"\bset\s+not\s+null\b", s) and not new_table:
            hazard("makes a column required")
        if table and not new_table:
            for part in re.split(r"\badd\s+column\b", s)[1:]:
                part = re.split(r"\badd\s+(?:constraint|check|unique|primary|foreign|exclude)\b", part)[0]
                if re.search(r"\bnot\s+null\b", part) and not re.search(r"\bdefault\b", part):
                    hazard("adds a required column without a default")
            if re.search(r"\badd\s+(constraint\s+\S+\s+)?(check|unique|foreign\s+key|exclude|primary\s+key)\b", s):
                hazard("adds a constraint to a table that is not new")
        m = re.search(r"\bcreate\s+unique\s+index\b.*?\bon\s+(?:only\s+)?" + NAME, s)
        if m and m.group(1) not in created:
            hazard("adds a unique index to a table that is not new")
    return found


def entities(text: str) -> dict:
    import yaml

    out = {}
    for e in yaml.safe_load(text) or []:
        if isinstance(e, dict) and e.get("name"):
            out[e["name"]] = e
    return out


def entity_hazards(path: str, old: str, new: str) -> list[str]:
    """What a change to an entity file does that an older build's tables cannot live with."""
    found = []
    before, after = entities(old) if old else {}, entities(new) if new else {}
    for name, was in before.items():
        now = after.get(name)
        if now is None:
            found.append(f"{path}: entity {name} taken out")
            continue
        fields = {f["name"]: f for f in now.get("fields") or [] if isinstance(f, dict) and "name" in f}
        for f in was.get("fields") or []:
            if not isinstance(f, dict) or "name" not in f:
                continue
            g = fields.get(f["name"])
            if g is None:
                found.append(f"{path}: {name}.{f['name']} taken out")
                continue
            for key, what in (("type", "type"), ("to", "link target")):
                if f.get(key) != g.get(key):
                    found.append(f"{path}: {name}.{f['name']} {what} changed from {f.get(key)} to {g.get(key)}")
            if g.get("unique") and not f.get("unique"):
                found.append(f"{path}: {name}.{f['name']} made unique")
        for key, what in (("unique", "unique group"), ("exclusion", "exclusion")):
            for item in now.get(key) or []:
                if item not in (was.get(key) or []):
                    found.append(f"{path}: {name} gets a new {what} {item}")
    return found


def check(repo: Path, base: str, head: str | None) -> tuple[list[str], bool]:
    """(what an older build cannot live with, whether COMPATIBLE_FROM moved) from `base` to `head` (None: the working tree)."""
    diff = ["diff", "--name-status", "--no-renames", base] + ([head] if head else [])
    found = []
    lines = git(repo, *diff).splitlines()
    if head is None:
        # Files not yet added are part of the working tree too.
        lines += [f"A\t{p}" for p in git(repo, "ls-files", "--others", "--exclude-standard").splitlines()]
    for line in lines:
        status, path = line.split("\t", 1)
        if SQL.match(path) and status == "A":
            found += [f"{path}: {h}" for h in sql_hazards(read(repo, head, path))]
        elif ENTITIES.match(path):
            found += entity_hazards(path, read(repo, base, path), read(repo, head, path))
    was = CONSTANT.search(read(repo, base, WINDOW))
    now = CONSTANT.search(read(repo, head, WINDOW))
    moved = bool(was and now and now.group(1) > was.group(1))
    return found, moved


def main(argv: list[str]) -> int:
    if len(argv) == 2 and argv[0] == "--range" and ".." in argv[1]:
        base, head = argv[1].split("..", 1)
    elif not argv:
        base, head = git(ROOT, "merge-base", "origin/main", "HEAD").strip(), None
    else:
        print(__doc__)
        return 2
    found, moved = check(ROOT, base, head)
    if not found:
        print("window: every schema change here is additive")
        return 0
    for f in found:
        print(f"  {f}")
    if moved:
        print(f"window: {len(found)} changes an older build cannot live with, and COMPATIBLE_FROM moved with them")
        return 0
    print(
        f"window: {len(found)} changes an older build cannot live with, and COMPATIBLE_FROM ({WINDOW}) did not move. "
        "Ship the removal a release after the change that stopped using it and move COMPATIBLE_FROM in the same commit, "
        "or mark a statement that is safe all the same with `-- window: safe, <why>`"
    )
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
