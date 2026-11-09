#!/usr/bin/env python3
"""The statements apps run that write entity tables outside the document pipeline, kept explicit
(docs/registry/mutation-paths.md, FERRUM-PLATFORM-011, FEO-010).

    scripts/mutation_paths.py            # CI runs this: fails when the code and the review differ
    scripts/mutation_paths.py --write    # rewrites the review's table and counts from the code

Reads `cargo metadata`, so it reads the very checkouts the build uses: every repository of CBFerrum's
apps the lock compiles in, their sources but not their tests. A statement is an `update`, `insert into`
or `delete from` of an `e_*` table, known by its app, file, what it writes and the function it is in.
One the review does not list fails the check until a person reads it and gives it a kind; one the
review lists that the code no longer has fails it too, until it is taken out. `--write` keeps each
listed statement's kind, and writes `unclassified` for a new one, which still fails the check.
"""

import json
import re
import subprocess
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REVIEW = ROOT / "docs/registry/mutation-paths.md"
STATEMENT = re.compile(r"\b(update|insert\s+into|delete\s+from)\s+(e_[a-z0-9_]+)\b", re.I)
FUNCTION = re.compile(r"\bfn\s+([a-z_][a-z0-9_]*)")
UNCLASSIFIED = "unclassified: a person reads it and gives it a kind"
ROW = re.compile(r"^\| ([a-z0-9-]+) \| `([^`:]+):(\d+)` \| `([^`]+)` \| `([^`]+)` \| (.+) \|$")


def repositories() -> dict:
    """Each app repository the lock compiles in: its short name and its checkout's root."""
    run = subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked", "--all-features"], cwd=ROOT, capture_output=True, text=True, check=True)
    out = {}
    for p in json.loads(run.stdout)["packages"]:
        m = re.match(r"git\+https://github\.com/CBFerrum/ferrum-app-([a-z0-9-]+)", p.get("source") or "")
        if not m:
            continue
        here = Path(p["manifest_path"]).parent
        parts = here.parts
        # Cargo keeps each revision whole, at checkouts/<repository>-<hash>/<revision>/.
        root = Path(*parts[: parts.index("checkouts") + 3]) if "checkouts" in parts else next((d for d in [here, *here.parents] if (d / ".git").exists()), here)
        out[m.group(1)] = root
    return out


def statements(app: str, root: Path) -> list:
    """(app, file, writes, function, line) for every direct write in an app's sources."""
    found = []
    for path in sorted(root.rglob("*.rs")):
        rel = path.relative_to(root).as_posix()
        parts = rel.split("/")
        if "tests" in parts or "target" in parts or "examples" in parts or "benches" in parts:
            continue
        lines = path.read_text(errors="replace").split("\n")
        function = None
        in_test = False
        for n, line in enumerate(lines, 1):
            if re.match(r"\s*#\[cfg\(test\)\]", line):
                in_test = True
            m = FUNCTION.search(line)
            if m and not line.lstrip().startswith("//"):
                function = m.group(1)
            if in_test or line.lstrip().startswith("//"):
                continue
            for s in STATEMENT.finditer(line):
                verb = re.sub(r"\s+", " ", s.group(1).lower())
                found.append((app, rel, f"{verb} {s.group(2).lower()}", function or "?", n))
    return found


def reviewed() -> list:
    """(app, file, writes, function, line, kind) as the review lists them."""
    out = []
    for line in REVIEW.read_text().split("\n"):
        m = ROW.match(line)
        if m:
            app, file, n, writes, function, kind = m.groups()
            out.append((app, file, writes, function, int(n), kind))
    return out


def key(row: tuple) -> tuple:
    return row[:4]


def write(code: list, review: list) -> None:
    kinds = {}
    for r in review:
        kinds.setdefault(key(r), []).append(r[5])
    rows = []
    for c in sorted(code, key=lambda c: (c[0], c[1], c[4])):
        listed = kinds.get(key(c)) or []
        kind = listed.pop(0) if listed else UNCLASSIFIED
        rows.append(f"| {c[0]} | `{c[1]}:{c[4]}` | `{c[2]}` | `{c[3]}` | {kind} |")
    text = REVIEW.read_text()
    head, rest = text.split("## The statements\n", 1)
    table_start = rest.index("| App |")
    lines = rest[table_start:].split("\n")
    end = next(i for i, l in enumerate(lines) if i > 1 and not l.startswith("|"))
    rest = rest[:table_start] + "\n".join(lines[:2] + rows + lines[end:])
    # The counts above the table follow it.
    by_kind = Counter(r.split(" | ")[4] for r in rows)
    def count(m):
        label = m.group(1)
        first = label.split(":")[0].split(" (")[0].lower()
        n = sum(v for k, v in by_kind.items() if k.lower().startswith(first))
        return f"| {label} | {n} |"
    head = re.sub(r"^\| ([^|]+?) \| \d+ \|", count, head, flags=re.M)
    head = re.sub(r"## The \d+ statements, by kind", f"## The {len(rows)} statements, by kind", head)
    apps = Counter(c[0] for c in code)
    named = {"erp": "ERP", "pos": "POS", "hr": "HR", "lms": "LMS", "dam": "DAM"}
    order = sorted(apps.items(), key=lambda a: (-a[1], a[0]))
    many = [f"{named.get(a, a.replace('-', ' ').title())} {n}" for a, n in order if n > 1]
    ones = [named.get(a, a.replace("-", " ").title()) for a, n in order if n == 1]
    line = "By app: " + ", ".join(many) + (f",\nand one each in {', '.join(ones[:-1])} and {ones[-1]}." if len(ones) > 1 else (f", and {ones[0]} 1." if ones else "."))
    head = re.sub(r"By app: .*?\.\n", line + "\n", head, count=1, flags=re.S)
    head = re.sub(r"(the statements in the apps compiled in at the revisions in\n)", r"\1", head)
    REVIEW.write_text(head + "## The statements\n" + rest)


def main() -> int:
    code = [s for app, root in sorted(repositories().items()) for s in statements(app, root)]
    review = reviewed()
    if "--write" in sys.argv:
        write(code, review)
        review = reviewed()
    have, listed = Counter(key(c) for c in code), Counter(key(r) for r in review)
    new, gone = have - listed, listed - have
    unclassified = [r for r in review if r[5] == UNCLASSIFIED]
    for k, n in sorted(new.items()):
        print(f"not in the review ({n}): {k[0]} {k[1]} `{k[2]}` in `{k[3]}`")
    for k, n in sorted(gone.items()):
        print(f"in the review, not in the code ({n}): {k[0]} {k[1]} `{k[2]}` in `{k[3]}`")
    for r in unclassified:
        print(f"no kind yet: {r[0]} {r[1]}:{r[4]} `{r[2]}` in `{r[3]}`")
    apps = len({c[0] for c in code})
    print(f"{len(code)} statements in {apps} apps write entity tables directly; {len(review)} reviewed")
    return 1 if new or gone or unclassified else 0


if __name__ == "__main__":
    sys.exit(main())
