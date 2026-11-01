#!/usr/bin/env python3
"""The requirements registry (docs/registry): checks it and writes what is generated from it.

    scripts/registry.py check      # fails on anything wrong, or a generated file out of date
    scripts/registry.py generate   # writes docs/capabilities.md and docs/registry/backlog.md
    scripts/registry.py export     # the whole registry, each entry in full, as JSON

A requirement does not become complete because someone says so: `verified_complete` needs test
runs, the digest of a release and a named reviewer. See docs/registry/README.md.
"""

import datetime
import json
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parent.parent
REGISTRY = ROOT / "docs" / "registry"
CAPABILITIES = ROOT / "docs" / "capabilities.md"
BACKLOG = REGISTRY / "backlog.md"
AREAS = ["platform", "identity", "privacy", "experience", "apps", "ecosystem", "extensions", "ai", "cloud", "operations", "verification"]

STATUSES = {
    "verified_complete": "Verified complete",
    "implemented_unqualified": "Implemented, qualification incomplete",
    "partial": "Partially implemented",
    "documented_only": "Documented only",
    "missing": "Missing",
    "blocked_external": "Blocked by external dependency",
    "not_applicable": "Not applicable",
}
BASES = {"tested": "tests run in this audit", "read": "code read", "probe": "keyword probe"}
WORKSTREAMS = {
    1: "Evidence inventory and registry",
    2: "Shared domain and architecture contracts",
    3: "Identity, authorization, transactions, secrets",
    4: "Jobs, files, search, notifications, governance",
    5: "Existing application completion",
    6: "Cross-application orchestration",
    7: "SDK, plugins, connector infrastructure",
    8: "Additional applications and industry packs",
    9: "Ferrum Cloud and partner operations",
    10: "Marketplace operations",
    11: "Full-system qualification",
    12: "Production release and operating handover",
}
ACCEPTANCE = ["functional", "authorization", "cross_tenant", "concurrency", "migration", "recovery", "user_interface"]
# Sibling checkouts, where a file of another repository is looked for. Absent ones (CI) are skipped.
SIBLINGS = {"ferrum-app-erp": ["../erp4/ferrum-app-erp", "../ferrum-app-erp"]}
# What the documents must no longer say, because the code says otherwise.
DRIFT = [
    ("docs/limits.md", r"security keys \(WebAuthn\)", "passkeys exist: crates/kernel/src/passkey.rs"),
    ("docs/security.md", r"Security keys \(WebAuthn\).*not supported", "passkeys exist: crates/kernel/src/passkey.rs"),
    ("docs/limits.md", r"no SCIM provisioning", "SCIM exists: crates/kernel/src/scim.rs"),
    ("docs/limits.md", r"a sale can be completed offline only", "POS prices offline: ferrum-app-pos/ui/src/lib/offline.ts"),
]


def normalize(entry: dict, defaults: dict) -> dict:
    e = {**defaults, **entry}
    implementation = e.get("implementation") or {}
    evidence = e.get("evidence") or {}
    return {
        "id": e["id"],
        "title": e["title"],
        "component": e.get("component"),
        "status": e.get("status"),
        "basis": e.get("basis"),
        "spec": str(e.get("spec", "")),
        "workstream": e.get("workstream"),
        "repositories": e.get("repositories", []),
        "dependencies": e.get("deps", []),
        "implementation": {"files": e.get("files", implementation.get("files", [])), "commits": implementation.get("commits", [])},
        "acceptance": {k: (e.get("acceptance") or {}).get(k, []) for k in ACCEPTANCE},
        "evidence": {
            "test_runs": e.get("tests", evidence.get("test_runs", [])),
            "release_digest": evidence.get("release_digest"),
            "reviewer": evidence.get("reviewer"),
        },
        "external_dependencies": e.get("external", []),
        "known_limitations": e.get("limits", []),
        "boundary": e.get("boundary"),
    }


def load() -> list:
    out = []
    for area in AREAS:
        doc = yaml.safe_load((REGISTRY / f"{area}.yaml").read_text())
        defaults = doc.get("defaults", {})
        out.extend(normalize(r, defaults) for r in doc["requirements"])
    return out


def located(path: str):
    """Where a cited path lives, or None when its repository is not checked out here."""
    if ":" not in path:
        return ROOT / path
    repo, rest = path.split(":", 1)
    for base in [f"../{repo}", *SIBLINGS.get(repo, [])]:
        if (ROOT / base).is_dir():
            return ROOT / base / rest
    return None


def problems(reqs: list) -> list:
    bad = []
    ids = Counter(r["id"] for r in reqs)
    bad += [f"{i}: appears {n} times" for i, n in ids.items() if n > 1]
    blockers = {b["id"] for b in yaml.safe_load((REGISTRY / "blockers.yaml").read_text())["blockers"]}
    used = set()
    for r in reqs:
        i, status = r["id"], r["status"]
        if not re.fullmatch(r"FERRUM-[A-Z]+-\d{3}", i):
            bad.append(f"{i}: ids look like FERRUM-AREA-001")
        if status not in STATUSES:
            bad.append(f"{i}: unknown status {status}")
        if r["basis"] not in BASES:
            bad.append(f"{i}: says how it was established with basis: {', '.join(BASES)}")
        if r["workstream"] not in WORKSTREAMS:
            bad.append(f"{i}: no workstream")
        for d in r["dependencies"]:
            if d not in ids:
                bad.append(f"{i}: depends on unknown {d}")
        for b in r["external_dependencies"]:
            used.add(b)
            if b not in blockers:
                bad.append(f"{i}: names unknown blocker {b}")
        for path in r["implementation"]["files"] + r["evidence"]["test_runs"]:
            where = located(path)
            if where is not None and not where.exists():
                bad.append(f"{i}: cites {path}, which does not exist")
        files, tests = r["implementation"]["files"], r["evidence"]["test_runs"]
        if status == "verified_complete":
            digest = r["evidence"]["release_digest"] or ""
            if not tests or not re.fullmatch(r"sha256:[0-9a-f]{64}", digest) or not r["evidence"]["reviewer"]:
                bad.append(f"{i}: verified_complete needs test runs, a release digest and a reviewer")
            if not any(r["acceptance"].values()):
                bad.append(f"{i}: verified_complete needs acceptance criteria")
        if status in ("implemented_unqualified", "partial") and not files:
            bad.append(f"{i}: {status} cites no implementation")
        if status == "partial" and not r["known_limitations"]:
            bad.append(f"{i}: partial must say what is missing")
        if status == "implemented_unqualified" and r["basis"] == "tested" and not tests:
            bad.append(f"{i}: basis tested names no tests")
        if status == "missing" and files:
            bad.append(f"{i}: missing, yet it cites an implementation")
        if status == "blocked_external" and not r["external_dependencies"]:
            bad.append(f"{i}: blocked, but on nothing named")
        if status == "not_applicable" and not r["boundary"]:
            bad.append(f"{i}: not_applicable needs its boundary")
    return bad


def advisory_problems() -> list:
    ignored = set(re.findall(r'"(RUSTSEC-[\d-]+)"', (ROOT / ".cargo" / "audit.toml").read_text()))
    entries = yaml.safe_load((REGISTRY / "advisories.yaml").read_text())["advisories"]
    bad = [f"{i}: ignored in .cargo/audit.toml with no entry in advisories.yaml" for i in ignored - {a["id"] for a in entries}]
    bad += [f"{a['id']}: no longer ignored; remove its entry" for a in entries if a["id"] not in ignored]
    today = datetime.date.today()
    for a in entries:
        missing = [k for k in ("owner", "reason", "applicability", "review_by", "resolved_when") if not a.get(k)]
        if missing:
            bad.append(f"{a['id']}: lacks {', '.join(missing)}")
        elif a["review_by"] < today:
            bad.append(f"{a['id']}: its review was due on {a['review_by']}")
    return bad


def drift_problems() -> list:
    bad = []
    for path, pattern, why in DRIFT:
        file = ROOT / path
        if file.exists() and re.search(pattern, file.read_text()):
            bad.append(f"{path}: says /{pattern}/, but {why}")
    return bad


def matrix(reqs: list) -> str:
    counts = Counter(r["status"] for r in reqs)
    out = [
        "# Capabilities",
        "",
        "Generated from [docs/registry](registry/README.md) by `scripts/registry.py generate`; edit the registry, not this.",
        "This is what Ferrum can claim, and how it knows. *Basis* says how far an entry was established:",
        ", ".join(f"**{k}** ({v})" for k, v in BASES.items()) + ".",
        "",
        "Nothing is `Verified complete` until its tests have run against a released digest and a person has",
        "reviewed it. An entry resting on a keyword probe is a lead, not a finding.",
        "",
        "| Status | Requirements |",
        "|---|---|",
    ]
    out += [f"| {label} | {counts[key]} |" for key, label in STATUSES.items() if counts[key]]
    out += [f"| **All** | **{len(reqs)}** |", ""]
    by_area = defaultdict(list)
    for r in reqs:
        by_area[r["component"]].append(r)
    for area in by_area:
        out += [f"## {area.capitalize()}", "", "| Id | Requirement | Status | Basis | Known limitations |", "|---|---|---|---|---|"]
        for r in by_area[area]:
            limits = "; ".join(r["known_limitations"]).replace("|", "\\|")
            out.append(f"| {r['id']} | {r['title']} | {STATUSES[r['status']]} | {r['basis']} | {limits} |")
        out.append("")
    return "\n".join(out)


def backlog(reqs: list) -> str:
    by_id = {r["id"]: r for r in reqs}
    ready_states = ("implemented_unqualified", "verified_complete")
    open_items = [r for r in reqs if r["status"] in ("partial", "documented_only", "missing")]
    ready = [r for r in open_items if all(by_id[d]["status"] in ready_states for d in r["dependencies"]) and not r["external_dependencies"]]
    waiting = [r for r in open_items if r not in ready]
    blocked = [r for r in reqs if r["status"] == "blocked_external"]
    unqualified = [r for r in reqs if r["status"] == "implemented_unqualified"]
    out = [
        "# Backlog",
        "",
        "Generated by `scripts/registry.py generate`. Work in this order: workstreams follow the specification's",
        "dependency order, and an item is ready when everything it depends on is at least implemented.",
        "",
        f"**{len(ready)} ready**, {len(waiting)} waiting on other work or a blocker, {len(blocked)} blocked outside",
        f"this repository, {len(unqualified)} built and awaiting qualification.",
        "",
    ]
    grouped = defaultdict(list)
    for r in ready:
        grouped[r["workstream"]].append(r)
    for w in sorted(grouped):
        out += [f"## Ready: {w}. {WORKSTREAMS[w]}", ""]
        out += [f"- `{r['id']}` {r['title']}" + (f" ({r['known_limitations'][0]})" if r["known_limitations"] else "") for r in grouped[w]]
        out.append("")
    out += ["## Waiting", ""]
    for r in sorted(waiting, key=lambda r: (r["workstream"], r["id"])):
        waits = [d for d in r["dependencies"] if by_id[d]["status"] not in ready_states] + r["external_dependencies"]
        out.append(f"- `{r['id']}` {r['title']}: waits for {', '.join(waits)}")
    out += ["", "## Blocked outside this repository", ""]
    out += [f"- `{r['id']}` {r['title']}: {', '.join(r['external_dependencies'])}" for r in blocked]
    return "\n".join(out) + "\n"


def main() -> int:
    command = sys.argv[1] if len(sys.argv) > 1 else "check"
    reqs = load()
    if command == "export":
        print(json.dumps(reqs, indent=2))
        return 0
    bad = problems(reqs) + advisory_problems() + drift_problems()
    if bad:
        print("\n".join(bad), file=sys.stderr)
        return 1
    generated = {CAPABILITIES: matrix(reqs) + "\n", BACKLOG: backlog(reqs)}
    if command == "generate":
        for path, text in generated.items():
            path.write_text(text)
        print(f"{len(reqs)} requirements: " + ", ".join(f"{n} {s}" for s, n in Counter(r["status"] for r in reqs).most_common()))
        return 0
    stale = [str(p.relative_to(ROOT)) for p, text in generated.items() if not p.exists() or p.read_text() != text]
    if stale:
        print(f"out of date, run scripts/registry.py generate: {', '.join(stale)}", file=sys.stderr)
        return 1
    print(f"{len(reqs)} requirements check out")
    return 0


if __name__ == "__main__":
    sys.exit(main())
