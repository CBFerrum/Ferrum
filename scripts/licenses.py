#!/usr/bin/env python3
"""The licenses of everything compiled into Ferrum, and a check that none is a surprise.

    scripts/licenses.py check      # CI runs this: fails on a license nobody has accepted
    scripts/licenses.py generate   # writes docs/licenses.md

It also holds deployment plans to docs/registry/engines.yaml (FLC-BASE-006): every image a file in deploy/
names must be an engine approved there, its licence accepted here, so an unapproved engine does not deploy.

Reads `cargo metadata`, so it covers Rust dependencies, direct and transitive, on every platform.
The frontend's bundled packages have their own inventory in the release SBOM. A license expression
passes when one of its alternatives is accepted, and every part of a conjunction must pass.
"""

import collections
import json
import re
import subprocess
import sys
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "docs/licenses.md"
ENGINES = ROOT / "docs/registry/engines.yaml"

# Licenses that do not limit how Ferrum is distributed.
PERMISSIVE = {
    "MIT", "MIT-0", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-1-Clause", "BSD-2-Clause", "BSD-3-Clause", "ISC", "0BSD", "Zlib",
    "Unlicense", "CC0-1.0", "Unicode-3.0", "Unicode-DFS-2016", "CDLA-Permissive-2.0", "BSL-1.0", "PostgreSQL",
}
# File-level copyleft: fine linked into AGPL software, and changes to those files stay open.
WEAK_COPYLEFT = {"MPL-2.0"}
# Ferrum's own, and the apps'. Ferrum is AGPL-3.0-or-later; a part under AGPL-3.0-only fixes the
# whole at version 3.
OURS = {"AGPL-3.0-or-later", "AGPL-3.0-only"}
ACCEPTED = PERMISSIVE | WEAK_COPYLEFT | OURS


def packages() -> list:
    # Every feature: the release image turns on some that a plain build leaves off (SAML's xmlsec1).
    run = subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked", "--all-features"], cwd=ROOT, capture_output=True, text=True, check=True)
    return json.loads(run.stdout)["packages"]


def accepted(expression: str) -> bool:
    """Whether an SPDX expression (or crates.io's older `MIT/Apache-2.0`) leaves us a way to comply."""
    tokens = re.findall(r"\(|\)|\bAND\b|\bOR\b|/|[^\s()/]+(?: WITH [^\s()]+)?", expression)
    if not tokens:
        return False
    python = {"(": "(", ")": ")", "AND": "and", "OR": "or", "/": "or"}
    # Only parentheses, `and`, `or` and the two truth values reach eval.
    return eval(" ".join(python.get(t, str(t in ACCEPTED)) for t in tokens))


def image_of(value: str) -> str:
    """The repository an `image:` names, its `${VAR:-default}` resolved to the default, without tag or digest."""
    value = re.sub(r"\$\{[A-Z0-9_]+:-([^}]*)\}", r"\1", value.strip().strip("'\""))
    value = value.split("@", 1)[0]
    last = value.rsplit("/", 1)
    if ":" in last[-1]:
        value = value[: value.rindex(":")]
    return value


def refused_images(files: dict, engines: list) -> list:
    """Each image the deployment files name that is not an approved engine, or is one whose record is not whole."""
    approved = {}
    bad = []
    for e in engines:
        missing = [k for k in ("image", "name", "license", "read", "terms", "decided_in") if not e.get(k)]
        if missing:
            bad.append(f"engine {e.get('image', '?')}: says nothing of {', '.join(missing)}")
        elif not accepted(e["license"]):
            bad.append(f"engine {e['image']}: {e['license']} is not a licence this repository accepts")
        else:
            approved[e["image"]] = e
    for path, text in sorted(files.items()):
        for line in text.splitlines():
            m = re.match(r"\s*image:\s*(\S+)", line)
            if m and image_of(m.group(1)) not in approved:
                bad.append(f"{path}: {image_of(m.group(1))} is not an engine approved in docs/registry/engines.yaml")
    return bad


def deployment_files() -> dict:
    return {str(p.relative_to(ROOT)): p.read_text() for p in sorted((ROOT / "deploy").rglob("*")) if p.suffix in (".yml", ".yaml") and p.is_file()}


def self_test() -> None:
    assert image_of("${FERRUM_IMAGE:-ghcr.io/cbferrum/ferrum:latest}") == "ghcr.io/cbferrum/ferrum"
    assert image_of("postgres:17") == "postgres"
    assert image_of("localhost:5000/x/y:1@sha256:00") == "localhost:5000/x/y"
    engine = {"image": "postgres", "name": "PostgreSQL", "license": "PostgreSQL", "read": "COPYRIGHT", "terms": "none", "decided_in": "x"}
    assert refused_images({"deploy/a.yml": "  image: postgres:17\n"}, [engine]) == []
    assert refused_images({"deploy/a.yml": "  image: redis:7\n"}, [engine]) == ["deploy/a.yml: redis is not an engine approved in docs/registry/engines.yaml"]
    assert refused_images({}, [dict(engine, license="SSPL-1.0")]), "an engine under a licence nobody accepted is not approved"
    assert refused_images({}, [dict(engine, decided_in="")]), "nor one without its decision"

    assert accepted("MIT OR Apache-2.0")
    assert accepted("MIT/Apache-2.0")
    assert accepted("Apache-2.0 / MIT")
    assert accepted("(MIT OR Apache-2.0) AND Unicode-3.0")
    assert accepted("Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT")
    assert accepted("MPL-2.0")
    assert not accepted("GPL-2.0-only")
    assert not accepted("SSPL-1.0")
    assert not accepted("MIT AND Proprietary")
    assert not accepted("(MIT OR Apache-2.0) AND SSPL-1.0")
    assert accepted("MIT OR Proprietary"), "one acceptable alternative is enough"
    assert accepted("ISC AND (Apache-2.0 OR ISC) AND Apache-2.0 AND MIT AND BSD-3-Clause AND (Apache-2.0 OR ISC OR MIT) AND (Apache-2.0 OR ISC OR MIT-0)")
    assert not accepted("")


def report(pkgs: list) -> tuple:
    by_license = collections.defaultdict(set)
    refused = []
    for p in pkgs:
        license = p.get("license") or ""
        by_license[license or "(none declared)"].add(p["name"])
        if not accepted(license):
            refused.append((p["name"], p["version"], license or "(none declared)"))
    return by_license, sorted(refused)


def render(pkgs: list, by_license: dict) -> str:
    out = [
        "# Licenses of what Ferrum compiles in",
        "",
        "Generated by `scripts/licenses.py` from `cargo metadata`; CI fails on a license nobody has accepted.",
        "The frontend's bundled packages are inventoried in each release's SBOM. This is an inventory, not",
        "legal advice: see [plan/licensing.md](plan/licensing.md) for what has still to be reviewed.",
        "",
        f"{len(pkgs)} packages.",
        "",
        "| License | Packages | |",
        "|---|---:|---|",
    ]
    for license, names in sorted(by_license.items(), key=lambda kv: (-len(kv[1]), kv[0])):
        shown = ", ".join(sorted(names)[:6]) + (", ..." if len(names) > 6 else "")
        out.append(f"| `{license}` | {len(names)} | {shown} |")
    return "\n".join(out) + "\n"


def main() -> int:
    self_test()
    command = sys.argv[1] if len(sys.argv) > 1 else "check"
    pkgs = packages()
    by_license, refused = report(pkgs)
    for name, version, license in refused:
        print(f"not accepted: {name} {version}: {license}")
    if command == "generate":
        OUT.write_text(render(pkgs, by_license))
    elif OUT.read_text() != render(pkgs, by_license):
        print("docs/licenses.md is out of date: run scripts/licenses.py generate")
        return 1
    engines = yaml.safe_load(ENGINES.read_text())["engines"]
    images = refused_images(deployment_files(), engines)
    for line in images:
        print(f"not deployable: {line}")
    print(f"{len(pkgs)} packages, {len(by_license)} licenses, {len(refused)} refused; {len(engines)} engines approved, {len(images)} refused")
    return 1 if refused or images else 0


if __name__ == "__main__":
    sys.exit(main())
