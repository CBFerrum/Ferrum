#!/usr/bin/env python3
"""What a release is made of, read from the things that decide it, so that it cannot go stale.

    scripts/release_evidence.py manifest                 # docs/registry/release-manifest.yaml, as text
    scripts/release_evidence.py evidence [--image DIGEST]   # one build's evidence, as JSON
    scripts/release_evidence.py qualified [--stamp ID [REV]]  # whether each qualification ran at this build

A qualification (docs/logic/evidence/qualification.yaml) names the exact build it ran at: Ferrum's code, the
frontend and every repository Cargo.lock builds (FLC-QUAL-001). Once any of them changes it no longer stands:
`qualified` names what changed and fails, so a release waits until the qualification's scripts are run again
and `--stamp` records the build they ran at.

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
    now = build_identity()
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
        # Whether each profile qualification ran at this very build (FLC-QUAL-001).
        "profile_qualifications": [
            {"id": q["id"], "stands": not stale(q["ran_at"], now), "changed": stale(q["ran_at"], now), "evidence": q["evidence"]}
            for q in yaml.safe_load(QUALIFICATIONS.read_text())["qualifications"]
        ],
        "qualification": {
            "status": "not qualified",
            "requirements": total,
            "verified_complete": verified,
            "says": "Tests passed on this build. No release has been qualified against the registry, and nothing has run on physical equipment.",
        },
    }


QUALIFICATIONS = ROOT / "docs" / "logic" / "evidence" / "qualification.yaml"
# What decides what runs: a change anywhere here is a different build.
CODE = ["crates", "server", "packages", "Cargo.toml", "Cargo.lock", "Dockerfile"]


def git(*args: str) -> str:
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True, check=True).stdout


def build_identity(rev: str = "HEAD") -> dict:
    """The build `rev` makes: a digest of Ferrum's own code and the commit of everything it pins."""
    objects = git("rev-parse", *[f"{rev}:{path}" for path in CODE]).split()
    lock, dockerfile = git("show", f"{rev}:Cargo.lock"), git("show", f"{rev}:Dockerfile")
    pins = {}
    for package in lock.split("[[package]]")[1:]:
        source = re.search(rf'^source = "git\+https://github\.com/{OWNER}/([^?"#]+)[^#"]*#([0-9a-f]+)"$', package, re.M)
        if source:
            pins[f"{OWNER}/{source.group(1)}"] = source.group(2)
    front = re.search(r"^ARG FRONTEND_REV=([0-9a-f]+)$", dockerfile, re.M)
    pins[f"{OWNER}/Ferrum-Frontend"] = front.group(1) if front else "none"
    code = "sha256:" + hashlib.sha256(" ".join(f"{p}={o}" for p, o in zip(CODE, objects)).encode()).hexdigest()
    return {"ferrum_code": code, "pins": dict(sorted(pins.items()))}


def stale(ran_at: dict, now: dict) -> list[str]:
    """What differs between the build a qualification ran at and `now`; empty when it still stands."""
    changed = []
    if ran_at.get("ferrum_code") != now["ferrum_code"]:
        changed.append("Ferrum's code")
    was = ran_at.get("pins", {})
    for repo in sorted(set(was) | set(now["pins"])):
        if was.get(repo) != now["pins"].get(repo):
            changed.append(f"{repo}: {(was.get(repo) or 'absent')[:10]} -> {(now['pins'].get(repo) or 'absent')[:10]}")
    return changed


def self_test() -> None:
    now = {"ferrum_code": "sha256:a", "pins": {"CBFerrum/ferrum-app-edge": "17a7c41", "CBFerrum/Ferrum-Frontend": "2f201d5"}}
    assert stale(now, now) == [], "the same build stands"
    moved = {"ferrum_code": "sha256:a", "pins": {"CBFerrum/ferrum-app-edge": "abcdef1", "CBFerrum/Ferrum-Frontend": "2f201d5"}}
    assert stale(now, moved) == ["CBFerrum/ferrum-app-edge: 17a7c41 -> abcdef1"], "an app's pin changed after it passed"
    assert stale(now, dict(now, ferrum_code="sha256:b")) == ["Ferrum's code"]
    assert stale(now, {"ferrum_code": "sha256:a", "pins": dict(now["pins"], **{"CBFerrum/new-app": "1"})}) == ["CBFerrum/new-app: absent -> 1"]


def qualified(argv: list[str]) -> int:
    data = yaml.safe_load(QUALIFICATIONS.read_text())
    if "--stamp" in argv:
        at = argv.index("--stamp")
        name, rev = argv[at + 1], (argv[at + 2] if len(argv) > at + 2 else "HEAD")
        entry = next((q for q in data["qualifications"] if q["id"] == name), None)
        if entry is None:
            raise SystemExit(f"no qualification {name}")
        entry["ran_at"] = dict(build_identity(rev), commit=git("rev-parse", rev).strip())
        text = QUALIFICATIONS.read_text().split("qualifications:")[0]
        QUALIFICATIONS.write_text(text + yaml.safe_dump({"qualifications": data["qualifications"]}, sort_keys=False, width=120))
        print(f"{name}: stamped at {rev}")
        return 0
    now, failing = build_identity(), 0
    for q in data["qualifications"]:
        changed = stale(q["ran_at"], now)
        if changed:
            failing += 1
            print(f"{q['id']}: stale since it ran at {q['ran_at'].get('commit', '?')[:10]}: {', '.join(changed)}. Run again: {', '.join(q['scripts'])}")
            print(f"  rows standing on it: {', '.join(q['rows'])}")
        else:
            print(f"{q['id']}: ran at this build")
    return 1 if failing else 0


def main() -> int:
    self_test()
    if len(sys.argv) < 2 or sys.argv[1] not in ("manifest", "evidence", "qualified"):
        print(__doc__, file=sys.stderr)
        return 2
    if sys.argv[1] == "qualified":
        return qualified(sys.argv[2:])
    if sys.argv[1] == "manifest":
        print(manifest(), end="")
        return 0
    image = sys.argv[sys.argv.index("--image") + 1] if "--image" in sys.argv else None
    print(json.dumps(evidence(image), indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
