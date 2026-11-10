#!/usr/bin/env python3
"""The requirements registry (docs/registry): checks it and writes what is generated from it.

    scripts/registry.py check      # fails on anything wrong, or a generated file out of date
    scripts/registry.py generate   # writes docs/capabilities.md, backlog.md and release-manifest.yaml
    scripts/registry.py export     # the whole registry, each entry in full, as JSON

A requirement does not become complete because someone says so: `verified_complete` needs test
runs, the digest of a release and a named reviewer. See docs/registry/README.md.
"""

import datetime
import json
import os
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

import release_evidence
import yaml

ROOT = Path(__file__).resolve().parent.parent
REGISTRY = ROOT / "docs" / "registry"
CAPABILITIES = ROOT / "docs" / "capabilities.md"
BACKLOG = REGISTRY / "backlog.md"
MANIFEST = REGISTRY / "release-manifest.yaml"
AREAS = ["platform", "identity", "privacy", "experience", "apps", "ecosystem", "extensions", "ai", "cloud", "operations", "industrial", "verification"]

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
# The states of the Code, Deploy, Access and Resources program (docs/registry/fcp.yaml). Each asks for
# more evidence than the last, and none stands in for a later one.
FCP_STATES = {
    "not_started": "Not started",
    "partial": "Partial",
    "implemented_unverified": "Implemented, unverified",
    "verified_software": "Verified in software",
    "qualified_profile": "Qualified for a profile",
    "production_supported": "Production supported",
    "verification_failed": "Verification failed",
    "blocked_external": "Blocked externally",
    "deprecated": "Deprecated",
    "not_applicable_with_approved_rationale": "Not applicable",
}
FCP_WORKSTREAMS = {
    "W00": "Baseline and regression reconciliation",
    "W01": "Identity, policy and control plane",
    "W02": "Resource management",
    "W03": "Code",
    "W04": "Build and artifact supply chain",
    "W05": "Deploy, static delivery and domains",
    "W06": "SSR, private apps and workers",
    "W07": "Access, network and endpoints",
    "W08": "Distributed execution",
    "W09": "Corporate operations",
    "W10": "Qualification and rollout",
}
PLATFORM = ROOT / "docs" / "platform"
TRACEABILITY = PLATFORM / "traceability.md"
# The Logic Platform program (docs/registry/flc.yaml, docs/logic): the same states, its own workstreams.
LOGIC = ROOT / "docs" / "logic"
FLC_WORKSTREAMS = {
    "W0": "Establish reality and decision records",
    "W1": "Shared types and business capabilities",
    "W2": "Compiler, pure rules and hook integration",
    "W3": "Durable processes and human work",
    "W4": "Complete Studio product",
    "W5": "Generic edge and event integration",
    "W6": "Analytical semantics and engine reuse",
    "W7": "Intelligence experience and governed proposals",
    "W8": "Migration, release and operations",
}
# The Ferrum-only closure review (docs/registry/feo.yaml, docs/closure): its work packages, in the execution
# brief's dependency order.
CLOSURE = ROOT / "docs" / "closure"
FEO_WORKSTREAMS = {
    "A": "Baseline and failure regressions",
    "B": "Shared authority, identity, consent and receipts",
    "C": "Finance and high-use business cycles",
    "D": "Authoring, customization, AI and semantics",
    "E": "Connectors, SDK, office, accessibility and offline experience",
    "F": "Installation, migration, upgrade, restore, capacity and support",
    "G": "Industry operating cycles and their qualification",
}
# What a package was found to be (the brief's rule 4); a mixed one names more than one.
FEO_CLASSES = {"reproduced_defect", "source_flaw", "documented_incomplete", "qualification_gap", "product_boundary", "stale_evidence"}
# Where each of its four levels stands (the brief's "nonnegotiable distinction"): the mechanism a library
# implements, its adoption by the apps, the customer workflow using it, and a supported release proving it.
FEO_LEVELS = ("mechanism", "adoption", "workflow", "release")
FEO_LEVEL_STATES = {"none", "partial", "built", "verified", "proven", "not_applicable"}
# The builder-first program (docs/registry/bf.yaml, docs/builder): its gates, in the plan's order.
BUILDER = ROOT / "docs" / "builder"
BF_WORKSTREAMS = {
    "BF0": "Decisions and inventory",
    "BF1": "Portal access by configuration",
    "BF2": "Public pages",
    "BF3": "Approvals and independence",
    "BF4": "Money and stock as capabilities",
    "BF5": "Rules without source",
    "BF6": "Schedules on records",
    "BF7": "Reconciliation in Studio",
    "BF8": "Custom entities that carry an industry",
    "BF9": "Packs, and the proof",
}
# A program: its registry, its catalog of requirements and cases, its workstreams and its generated page.
PROGRAMS = {
    "fcp": (REGISTRY / "fcp.yaml", PLATFORM / "catalog.json", FCP_WORKSTREAMS, TRACEABILITY),
    "flc": (REGISTRY / "flc.yaml", LOGIC / "catalog.json", FLC_WORKSTREAMS, LOGIC / "traceability.md"),
    "feo": (REGISTRY / "feo.yaml", CLOSURE / "catalog.json", FEO_WORKSTREAMS, CLOSURE / "traceability.md"),
    "bf": (REGISTRY / "bf.yaml", BUILDER / "catalog.json", BF_WORKSTREAMS, BUILDER / "traceability.md"),
}
# The folder holding the other checkouts: beside this one, unless a worktree elsewhere says where.
SIB = Path(os.environ.get("FERRUM_SIBLINGS") or ROOT.parent)
# Sibling checkouts, where a file of another repository is looked for. Absent ones (CI) are skipped.
SIBLINGS = {"ferrum-app-erp": ["../docs-work/ferrum-app-erp", "../erp4/ferrum-app-erp", "../ferrum-app-erp"], "ferrum-app-edge": ["../docs-work/ferrum-app-edge", "../ferrum-app-edge"], "Ferrum-Frontend": ["web", "../ferrum/web"]}
# What the documents must no longer say, because the code says otherwise.
DRIFT = [
    ("docs/limits.md", r"security keys \(WebAuthn\)", "passkeys exist: crates/kernel/src/passkey.rs"),
    ("docs/security.md", r"Security keys \(WebAuthn\).*not supported", "passkeys exist: crates/kernel/src/passkey.rs"),
    ("docs/limits.md", r"no SCIM provisioning", "SCIM exists: crates/kernel/src/scim.rs"),
    ("docs/limits.md", r"a sale can be completed offline only", "POS prices offline: ferrum-app-pos/ui/src/lib/offline.ts"),
]
# Two claims nothing here can honestly make, wherever they appear (AT-SECRET-008, AT-AI-007).
CLAIMS = [
    (r"(?i)zero.knowledge|operators? (cannot|can't|never) (see|read|reach)|blind to (your|customer)", "whoever runs a process can reach what it handles"),
    (r"(?i)(data|everything) (stays|remains) on (your|the) (device|machine|laptop)", "a client that calls a hosted model sends it what it reads"),
]
DRIFT += [
    (path, pattern, why)
    for path in ("README.md", "docs/security.md", "docs/support-access.md", "docs/platform/customer-controlled.md", "../ferrum-app-cloud/docs/user-guide.md", "../ferrum-app-cloud/docs/security.md")
    for pattern, why in CLAIMS
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
    bases = [b for b in (SIB / base[3:] if base.startswith("../") else ROOT / base for base in [f"../{repo}", *SIBLINGS.get(repo, [])]) if b.is_dir()]
    return next((base / rest for base in bases if (base / rest).exists()), bases[0] / rest if bases else None)


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


def fcp_load(program: str = "fcp") -> list:
    return yaml.safe_load(PROGRAMS[program][0].read_text())["requirements"]


def fcp_problems(entries: list, program: str = "fcp") -> list:
    """What a status may claim. A file, a menu or a green build is not evidence of a later state."""
    registry_file, catalog_file, _, _ = PROGRAMS[program]
    catalog = json.loads(catalog_file.read_text())
    expected = {r["id"]: r for r in catalog["requirements"]}
    blockers = {b["id"] for b in yaml.safe_load((REGISTRY / "blockers.yaml").read_text())["blockers"]}
    reviewers = set(yaml.safe_load((REGISTRY / "reviewers.yaml").read_text())["independent"] or [])
    bad = [f"{i}: missing from {registry_file.name}" for i in expected.keys() - {e["id"] for e in entries}]
    seen = Counter(e["id"] for e in entries)
    bad += [f"{i}: appears {n} times" for i, n in seen.items() if n > 1]
    for e in entries:
        i, status = e["id"], e.get("status")
        files, tests = e.get("files", []), e.get("tests", [])
        if i not in expected:
            bad.append(f"{i}: not in the catalog")
        elif e["workstream"] != expected[i]["workstream"] or e["title"] != expected[i]["title"]:
            bad.append(f"{i}: differs from the catalog")
        if status not in FCP_STATES:
            bad.append(f"{i}: unknown state {status}")
        for path in files + tests:
            where = located(path)
            if where is not None and not where.exists():
                bad.append(f"{i}: cites {path}, which does not exist")
        if status in ("partial", "implemented_unverified", "verified_software", "qualified_profile", "production_supported") and not files:
            bad.append(f"{i}: {status} cites no implementation")
        if status == "not_started" and files:
            bad.append(f"{i}: not_started, yet it cites an implementation")
        if status in ("verified_software", "qualified_profile", "production_supported") and not tests:
            bad.append(f"{i}: {status} names no tests")
        if status in ("qualified_profile", "production_supported"):
            digest = e.get("release_digest") or ""
            if not re.fullmatch(r"sha256:[0-9a-f]{64}", digest) or not e.get("reviewer") or not e.get("profile"):
                bad.append(f"{i}: {status} needs a profile, a release digest and a reviewer")
            # Someone independent, on the list, by name. Another model instance is not that.
            if e.get("reviewer") and e["reviewer"] not in reviewers:
                bad.append(f"{i}: {e['reviewer']} is not an independent reviewer on record")
            # A qualification does not stand on something still waiting for a provider or equipment.
            if e.get("blockers"):
                bad.append(f"{i}: {status} while blocked on {', '.join(e['blockers'])}")
        if status == "blocked_external" and not e.get("blockers"):
            bad.append(f"{i}: blocked, but on nothing named")
        for b in e.get("blockers", []):
            if b not in blockers:
                bad.append(f"{i}: names unknown blocker {b}")
        if status == "not_applicable_with_approved_rationale" and not (e.get("rationale") and e.get("approved_by")):
            bad.append(f"{i}: needs a rationale and who approved it")
        if status != "production_supported" and not e.get("limits"):
            bad.append(f"{i}: says what it does not yet do")
        if program == "feo":
            bad += feo_row_problems(e)
    return bad


def feo_row_problems(e: dict) -> list:
    """A closure package says what it was found to be and where each of its four levels stands, and none
    claims more than its evidence: a level verified names tests, a release proven is a qualified one, and an
    acceptance case ran only where a test runs it."""
    i, status, tests = e["id"], e.get("status"), e.get("tests", [])
    bad = []
    classes = e.get("classification") or []
    if not classes or any(c not in FEO_CLASSES for c in classes):
        bad.append(f"{i}: classification must be one or more of {sorted(FEO_CLASSES)}")
    levels = e.get("levels") or {}
    if set(levels) != set(FEO_LEVELS) or any(v not in FEO_LEVEL_STATES for v in levels.values()):
        bad.append(f"{i}: levels must give each of {', '.join(FEO_LEVELS)} one of {sorted(FEO_LEVEL_STATES)}")
        return bad
    if any(v in ("verified", "proven") for v in levels.values()) and not tests:
        bad.append(f"{i}: a level verified names its tests")
    if "proven" in [levels[k] for k in FEO_LEVELS[:3]]:
        bad.append(f"{i}: only a release is proven; the other levels are at most verified")
    if levels["release"] == "proven" and status not in ("qualified_profile", "production_supported"):
        bad.append(f"{i}: a release is proven only by a qualification")
    if status in ("verified_software", "qualified_profile", "production_supported") and levels["mechanism"] != "verified":
        bad.append(f"{i}: {status} with its mechanism not verified")
    if status == "not_started" and any(levels[k] in ("built", "verified", "proven") for k in FEO_LEVELS[1:]):
        bad.append(f"{i}: not started, yet a level past the mechanism stands built")
    if "gate" in e and e["gate"] not in BF_WORKSTREAMS:
        bad.append(f"{i}: taken over by {e['gate']}, which is no builder-first gate")
    acceptance = e.get("acceptance", "not_run")
    if acceptance not in ("not_run", "ran_in_software"):
        bad.append(f"{i}: acceptance is not_run or ran_in_software")
    if acceptance == "ran_in_software" and not tests:
        bad.append(f"{i}: its acceptance ran, in tests it does not name")
    return bad


JOURNEY_STATES = {"passes", "passes_in_parts", "blocked", "not_run"}


def journey_problems() -> list:
    """Every golden journey has an honest state. Only one run whole can say it passes, and what was left
    out stays on the page."""
    journeys = yaml.safe_load((PLATFORM / "journeys.yaml").read_text())["journeys"]
    blockers = {b["id"] for b in yaml.safe_load((REGISTRY / "blockers.yaml").read_text())["blockers"]}
    bad = [f"J{n:02}: missing from journeys.yaml" for n in range(1, 13) if f"J{n:02}" not in {j["id"] for j in journeys}]
    for j in journeys:
        i, state = j["id"], j.get("state")
        if state not in JOURNEY_STATES:
            bad.append(f"{i}: unknown state {state}")
        if state in ("passes", "passes_in_parts") and not j.get("tests"):
            bad.append(f"{i}: {state} names no tests")
        if state == "passes" and j.get("missing"):
            bad.append(f"{i}: passes, with steps still missing")
        if state != "passes" and not j.get("missing"):
            bad.append(f"{i}: says what is missing")
        if state == "blocked" and not j.get("blockers"):
            bad.append(f"{i}: blocked, but on nothing named")
        for b in j.get("blockers", []):
            if b not in blockers:
                bad.append(f"{i}: names unknown blocker {b}")
    return bad


def traceability(entries: list, program: str = "fcp") -> str:
    registry_file, catalog_file, workstreams, _ = PROGRAMS[program]
    counts = Counter(e["status"] for e in entries)
    cases = {c["id"]: c for c in json.loads(catalog_file.read_text())["acceptance"]}
    by_requirement = {c["requirement"]: c for c in cases.values()}
    out = [
        "# Traceability",
        "",
        f"Generated from [docs/registry/{registry_file.name}](../registry/{registry_file.name}) by `scripts/registry.py generate`; edit the registry, not this.",
        "Each requirement's acceptance case is in [catalog.json](catalog.json). See [README.md](README.md) for what a state may claim.",
        "",
        "| State | Requirements |",
        "|---|---|",
    ]
    out += [f"| {label} | {counts[key]} |" for key, label in FCP_STATES.items() if counts[key]]
    out += [f"| **All** | **{len(entries)}** |", ""]
    if program == "feo":
        return "\n".join(out + feo_sections(entries, workstreams, catalog_file))
    for w, label in workstreams.items():
        rows = [e for e in entries if e["workstream"] == w]
        out += [f"## {w}: {label}", "", "| Id | Requirement | State | Case | Tests | What it does not yet do |", "|---|---|---|---|---|---|"]
        for e in rows:
            limits = e.get("limits", "").replace("|", "\\|")
            out.append(f"| {e['id']} | {e['title']} | {FCP_STATES[e['status']]} | {by_requirement[e['id']]['id']} | {len(e.get('tests', []))} | {limits} |")
        out.append("")
    return "\n".join(out)


def feo_sections(entries: list, workstreams: dict, catalog_file: Path) -> list:
    """The closure packages by workstream, each with its priority, what it was found to be and its four levels."""
    priority = {r["id"]: r["priority"] for r in json.loads(catalog_file.read_text())["requirements"]}
    out = []
    for w, label in workstreams.items():
        out += [f"## {w}: {label}", "", "| Id | Package | Priority | State | Found to be | Mechanism / adoption / workflow / release | Acceptance | Tests | Builder gate | What it does not yet do |", "|---|---|---|---|---|---|---|---|---|---|"]
        for e in (e for e in entries if e["workstream"] == w):
            limits = e.get("limits", "").replace("|", "\\|")
            levels = " / ".join(e["levels"][k] for k in FEO_LEVELS)
            found = ", ".join(c.replace("_", " ") for c in e["classification"])
            out.append(f"| {e['id']} | {e['title']} | {priority[e['id']]} | {FCP_STATES[e['status']]} | {found} | {levels} | {e.get('acceptance', 'not_run').replace('_', ' ')} | {len(e.get('tests', []))} | {e.get('gate', '')} | {limits} |")
        out.append("")
    return out


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


def self_test() -> None:
    """AT-QUAL-007: a simulator that passed, or a reviewer nobody independent vouches for, is not a qualification.
    Whatever a row claims, the check refuses a qualified status that lacks an independent reviewer on record, a
    release digest and a profile, or that still waits on equipment, a provider or an audit."""
    base = next(e for e in fcp_load("flc") if e["id"] == "FLC-EDGE-011")
    digest = "sha256:" + "0" * 64
    assert not any(p.startswith("FLC-EDGE-011:") for p in fcp_problems([base], "flc")), "the row as recorded is sound"
    claimed = dict(base, status="qualified_profile", tests=["ferrum-app-edge:gateway/tests/critical_it.rs"], profile="edge", release_digest=digest)
    for row, why in [
        (dict(claimed), "no reviewer: the simulator passing is all there is"),
        (dict(claimed, reviewer="Claude"), "a reviewer who is not on record as independent"),
        (dict(claimed, release_digest="sha256:short", reviewer="Claude"), "no exact release digest"),
        (dict(claimed, blockers=["BLOCK-INDUSTRIAL-HARDWARE"]), "still waiting for real equipment"),
    ]:
        assert any(p.startswith("FLC-EDGE-011:") for p in fcp_problems([row], "flc")), f"accepted a qualification with {why}"
    # A closure package claims no level its evidence does not carry.
    sound = next(e for e in fcp_load("feo") if e["id"] == "FEO-004")
    assert not any(p.startswith("FEO-004:") for p in fcp_problems([sound], "feo")), "the row as recorded is sound"
    for row, why in [
        (dict(sound, levels=dict(sound["levels"], release="proven")), "a release proven without a qualification"),
        (dict(sound, tests=[]), "levels verified without tests"),
        (dict(sound, classification=["probably_fine"]), "a class the brief does not have"),
        (dict(sound, levels=dict(sound["levels"], mechanism="partial")), "verified in software, its mechanism not"),
        (dict(sound, gate="BF10"), "a builder gate that does not exist"),
    ]:
        assert any(p.startswith("FEO-004:") for p in fcp_problems([row], "feo")), f"accepted a package with {why}"


def bf_self_test() -> None:
    """BF-BASE-002: a builder-first row claims no state its evidence does not carry."""
    row = next(e for e in fcp_load("bf") if e["id"] == "BF-BASE-002")
    assert not any(p.startswith("BF-BASE-002:") for p in fcp_problems([row], "bf")), "the row as recorded is sound"
    for claimed, why in [
        (dict(row, status="verified_software", tests=[]), "verified in software without tests"),
        (dict(row, status="partial", files=[]), "partial, citing no implementation"),
        (dict(row, workstream="BF1"), "in another gate than its catalog's"),
        (dict(row, limits=""), "saying nothing of what it does not do"),
    ]:
        assert any(p.startswith("BF-BASE-002:") for p in fcp_problems([claimed], "bf")), f"accepted a builder row {why}"


def main() -> int:
    self_test()
    bf_self_test()
    command = sys.argv[1] if len(sys.argv) > 1 else "check"
    reqs = load()
    if command == "export":
        print(json.dumps(reqs, indent=2))
        return 0
    fcp, flc, feo, bf = fcp_load(), fcp_load("flc"), fcp_load("feo"), fcp_load("bf")
    bad = problems(reqs) + fcp_problems(fcp) + fcp_problems(flc, "flc") + fcp_problems(feo, "feo") + fcp_problems(bf, "bf") + journey_problems() + advisory_problems() + drift_problems()
    if bad:
        print("\n".join(bad), file=sys.stderr)
        return 1
    generated = {CAPABILITIES: matrix(reqs) + "\n", BACKLOG: backlog(reqs), MANIFEST: release_evidence.manifest(), TRACEABILITY: traceability(fcp) + "\n", PROGRAMS["flc"][3]: traceability(flc, "flc") + "\n", PROGRAMS["feo"][3]: traceability(feo, "feo") + "\n", PROGRAMS["bf"][3]: traceability(bf, "bf") + "\n"}
    if command == "generate":
        for path, text in generated.items():
            path.write_text(text)
        print(f"{len(reqs)} requirements: " + ", ".join(f"{n} {s}" for s, n in Counter(r["status"] for r in reqs).most_common()))
        return 0
    stale = [str(p.relative_to(ROOT)) for p, text in generated.items() if not p.exists() or p.read_text() != text]
    if stale:
        print(f"out of date, run scripts/registry.py generate: {', '.join(stale)}", file=sys.stderr)
        return 1
    print(f"{len(reqs)} requirements, {len(fcp)} of the platform program, {len(flc)} of the Logic Platform, {len(feo)} closure packages and {len(bf)} of the builder-first program check out")
    return 0


if __name__ == "__main__":
    sys.exit(main())
