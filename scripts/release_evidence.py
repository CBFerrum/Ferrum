#!/usr/bin/env python3
"""What a release is made of, read from the things that decide it, so that it cannot go stale.

    scripts/release_evidence.py manifest                 # docs/registry/release-manifest.yaml, as text
    scripts/release_evidence.py evidence [--image DIGEST]   # one build's evidence, as JSON

The manifest says what Cargo.lock builds, and `scripts/registry.py check` fails if it says anything
else. The evidence is written by CI for each build and kept with its run: the commits, the lockfile,
the frontend, the features, the image, the migrations, the run, and what has been qualified, which
says so plainly when the answer is nothing. A requirement marked tested links to a test; a device
marked qualified names equipment and firmware. Nothing here turns one into the other.
"""

import hashlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parent.parent
OWNER = "CBFerrum"


def git_sources() -> dict[str, dict]:
    """The other repositories Cargo.lock builds, each at one commit."""
    text = (ROOT / "Cargo.lock").read_text()
    repositories: dict[str, dict] = {}
    for package in text.split("[[package]]")[1:]:
        name = re.search(r'^name = "(.+)"$', package, re.M)
        source = re.search(rf'^source = "git\+https://github\.com/{OWNER}/([^?"#]+)[^#"]*#([0-9a-f]+)"$', package, re.M)
        if not (name and source):
            continue
        entry = repositories.setdefault(source.group(1), {"repository": f"{OWNER}/{source.group(1)}", "commit": source.group(2), "crates": []})
        entry["crates"].append(name.group(1))
    return repositories


def frontend() -> str:
    found = re.search(r"^ARG FRONTEND_REV=([0-9a-f]+)$", (ROOT / "Dockerfile").read_text(), re.M)
    if not found:
        raise SystemExit("Dockerfile names no FRONTEND_REV")
    return found.group(1)


def manifest() -> str:
    lines = [
        "# What this commit builds: generated from Cargo.lock and the Dockerfile by scripts/release_evidence.py,",
        "# and checked by scripts/registry.py. A build's own evidence is release-evidence.json, on its CI run.",
        "components:",
        f"  - {{repository: {OWNER}/Ferrum-Frontend, pinned: {frontend()}}}",
    ]
    lines += [f"  - {{repository: {r['repository']}, pinned: {r['commit']}}}" for _, r in sorted(git_sources().items())]
    return "\n".join(lines) + "\n"


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def migrations() -> list[dict]:
    """Package SQL here, which is applied once and never edited, and the kernel's own migrations."""
    files = sorted((ROOT / "crates").glob("*/packages/*/sql/*.sql")) + sorted((ROOT / "crates").glob("*/migrations/*.sql"))
    return [{"file": str(f.relative_to(ROOT)), "sha256": sha256(f)} for f in files]


def features() -> list[str]:
    server = (ROOT / "server" / "Cargo.toml").read_text()
    found = re.search(r"^default = \[(.*)\]$", server, re.M)
    return re.findall(r'"([^"]+)"', found.group(1)) if found else []


def registry() -> tuple[int, int]:
    total, verified = 0, 0
    for path in (ROOT / "docs" / "registry").glob("*.yaml"):
        data = yaml.safe_load(path.read_text()) or {}
        for req in data.get("requirements", []) or []:
            total += 1
            verified += req.get("status") == "verified_complete"
    return total, verified


def evidence(image: str | None) -> dict:
    total, verified = registry()
    blockers = yaml.safe_load((ROOT / "docs" / "registry" / "blockers.yaml").read_text())["blockers"]
    commit = os.environ.get("GITHUB_SHA") or subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True, check=True).stdout.strip()
    repository, run = os.environ.get("GITHUB_REPOSITORY", f"{OWNER}/Ferrum"), os.environ.get("GITHUB_RUN_ID")
    return {
        "platform": {"repository": repository, "commit": commit},
        "components": [{k: r[k] for k in ("repository", "commit", "crates")} for _, r in sorted(git_sources().items())],
        "frontend": {"repository": f"{OWNER}/Ferrum-Frontend", "commit": frontend()},
        "lockfile_sha256": sha256(ROOT / "Cargo.lock"),
        "features": features(),
        "artifacts": {"server_image": image},
        "migrations": migrations(),
        "workflow": {"run": run, "attempt": os.environ.get("GITHUB_RUN_ATTEMPT"), "url": f"https://github.com/{repository}/actions/runs/{run}" if run else None},
        "equipment_profiles": [],
        "exclusions": [{"blocker": b["id"], "needs": b["needs"]} for b in blockers],
        "qualification": {
            "status": "not qualified",
            "requirements": total,
            "verified_complete": verified,
            "says": "Tests passed on this build. No release has been qualified against the registry, and nothing has run on physical equipment.",
        },
    }


def main() -> int:
    if len(sys.argv) < 2 or sys.argv[1] not in ("manifest", "evidence"):
        print(__doc__, file=sys.stderr)
        return 2
    if sys.argv[1] == "manifest":
        print(manifest(), end="")
        return 0
    image = sys.argv[sys.argv.index("--image") + 1] if "--image" in sys.argv else None
    print(json.dumps(evidence(image), indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
