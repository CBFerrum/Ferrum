#!/usr/bin/env python3
"""The frozen baseline (docs/platform): what exists, as four separate facts.

    scripts/baseline.py            # writes docs/platform/baseline.json and baseline.md
    scripts/baseline.py --offline  # the same, leaving out what needs GitHub or a database

A repository's head, the revision the platform pins, what was built and what a workspace has
installed can all differ, so none is inferred from another. What could not be read is recorded with
the reason, because a thing that could not be seen is not a thing that is absent.
"""

import hashlib
import json
import os
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "docs" / "platform"
# Where each repository may be checked out here, nearest first. Absent ones are recorded as absent.
CHECKOUTS = {
    "CBFerrum/Ferrum": ["."],
    "CBFerrum/Ferrum-Frontend": ["web"],
    "CBFerrum/ferrum-sdk": ["../ferrum-sdk"],
    "CBFerrum/ferrum-store": ["../ferrum-store"],
    "CBFerrum/ferrum-app-erp": ["../docs-work/ferrum-app-erp", "../ferrum-app-erp"],
    "CBFerrum/ferrum-app-edge": ["../docs-work/ferrum-app-edge", "../ferrum-app-edge"],
}
LOCAL_DB = "postgres://ferrum:ferrum@localhost:5432/ferrum_dev"


def run(*cmd, cwd=None) -> tuple:
    try:
        p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=120)
    except (OSError, subprocess.TimeoutExpired) as e:
        return 1, "", str(e)
    return p.returncode, p.stdout.strip(), p.stderr.strip()


def digest(path: Path):
    return "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest() if path.exists() else None


def lock_pins() -> dict:
    """Every git dependency in Cargo.lock: the repository and the revision the platform builds."""
    pins = {}
    text = (ROOT / "Cargo.lock").read_text()
    for name, version, source in re.findall(r'\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"\nsource = "git\+https://github.com/([^?#"]+)[^#"]*#([0-9a-f]+)"'.replace('source = "git\\+https://github.com/([^?#"]+)[^#"]*#([0-9a-f]+)"', 'source = "(git[^"]+)"'), text):
        m = re.match(r"git\+https://github.com/([^?#]+)(?:\?[^#]*)?#([0-9a-f]+)", source)
        if not m:
            continue
        repo = f"CBFerrum/{m.group(1).split('/')[-1]}" if m.group(1).lower().startswith("cbferrum/") else m.group(1)
        pins.setdefault(repo, {"revision": m.group(2), "crates": {}})["crates"][name] = version
    return pins


def patches(where: Path) -> dict:
    """Local dependency patches: one Cargo did not use is silently not what anyone meant to build."""
    lock = where / "Cargo.lock"
    text = lock.read_text() if lock.exists() else ""
    unused = re.findall(r'\[\[patch\.unused\]\]\nname = "([^"]+)"\nversion = "([^"]+)"', text)
    return {"local_patch": (where / ".cargo" / "config.toml").exists() and "[patch" in (where / ".cargo" / "config.toml").read_text(), "unused_patches": [f"{n} {v}" for n, v in unused]}


def repositories(offline: bool) -> list:
    out = []
    names = dict(CHECKOUTS)
    code, listing, why = (1, "", "offline") if offline else run("gh", "repo", "list", "CBFerrum", "--limit", "100", "--json", "name,visibility,defaultBranchRef,isArchived")
    remote = {}
    if code == 0:
        for r in json.loads(listing):
            remote[f"CBFerrum/{r['name']}"] = r
            names.setdefault(f"CBFerrum/{r['name']}", [f"../{r['name']}"])
    for repo, places in sorted(names.items()):
        where = next((ROOT / p for p in places if (ROOT / p / ".git").exists()), None)
        row = {"repository": repo, "visibility": remote.get(repo, {}).get("visibility"), "checkout": None}
        if where is not None:
            row["checkout"] = "ferrum" if where.resolve() == ROOT.resolve() else os.path.relpath(where.resolve(), ROOT.parent.resolve())
            row["branch"] = run("git", "rev-parse", "--abbrev-ref", "HEAD", cwd=where)[1]
            row["commit"] = run("git", "rev-parse", "HEAD", cwd=where)[1]
            row["dirty"] = sorted(line.split(None, 1)[1] for line in run("git", "status", "--porcelain", cwd=where)[1].splitlines())
            row["lock_digest"] = digest(where / "Cargo.lock")
            row.update(patches(where))
        if code == 0 and repo in remote:
            row["default_branch"] = remote[repo]["defaultBranchRef"]["name"]
            got = run("gh", "api", f"repos/{repo}/commits/{row['default_branch']}", "--jq", ".sha")
            row["default_head"] = got[1] if got[0] == 0 else None
        out.append(row)
    # A pin on something nothing here can read is an access limit, not a missing repository.
    known = {r["repository"] for r in out}
    pinned = set().union(*(depended_on(p) for r in out if r["checkout"] for p in places_of(names[r["repository"]])))
    out += [{"repository": repo, "visibility": None, "checkout": None, "unread": "a dependency pins it, and nothing here can read it"} for repo in sorted(pinned - known)]
    return out


def places_of(places: list) -> list:
    return [ROOT / p for p in places if (ROOT / p / "Cargo.lock").exists()]


def depended_on(where: Path) -> set:
    """Repositories a lock file pins."""
    return set(re.findall(r'source = "git\+https://github\.com/([^/"]+/[^/?#"]+?)(?:\.git)?[?#]', (where / "Cargo.lock").read_text()))


def apps() -> list:
    """The version each compiled-in app declares, from the checkouts."""
    found = []
    for manifest in sorted(ROOT.parent.glob("ferrum-app-*/app.yaml")) + sorted(ROOT.parent.glob("docs-work/ferrum-app-*/app.yaml")) + sorted(ROOT.parent.glob("docs-work/ferrum-app-erp/addons/*/app.yaml")):
        text = manifest.read_text()
        name = re.search(r"^(?:id|name): *(\S+)", text, re.M)
        version = re.search(r"^version: *(\S+)", text, re.M)
        stamp = manifest.parent / "ui" / "dist" / ".source"
        found.append({"manifest": str(manifest.resolve().relative_to(ROOT.parent.resolve())), "id": name.group(1) if name else None, "version": version.group(1) if version else None, "ui_source": stamp.read_text().strip() if stamp.exists() else None})
    return found


def migrations() -> dict:
    return {
        "server": sorted(p.name for p in (ROOT / "server" / "migration" / "src").glob("m*.rs")),
        "package_sql": {str(p.relative_to(ROOT)): digest(p) for p in sorted(ROOT.glob("crates/*/packages/*/sql/*.sql"))},
    }


def features() -> dict:
    text = (ROOT / "server" / "Cargo.toml").read_text()
    return {"default": re.search(r'^default = \[(.*?)\]', text, re.M).group(1).replace('"', "").split(", ")}


def ci(offline: bool):
    if offline:
        return {"unread": "offline"}
    out = {}
    for repo in ("CBFerrum/Ferrum", "CBFerrum/ferrum-app-erp", "CBFerrum/ferrum-app-edge"):
        code, text, why = run("gh", "run", "list", "--repo", repo, "--branch", "main", "--limit", "6", "--json", "databaseId,name,headSha,status,conclusion")
        out[repo] = json.loads(text) if code == 0 else {"unread": why[:200]}
    return out


def images(offline: bool):
    if offline:
        return {"unread": "offline"}
    code, text, why = run("gh", "api", "orgs/CBFerrum/packages?package_type=container")
    return json.loads(text) if code == 0 else {"unread": why[:200] or text[:200]}


def installed(offline: bool):
    """What workspaces have installed. Only the local database is reachable from here."""
    if offline:
        return {"unread": "offline"}
    query = "select coalesce(json_agg(row_to_json(t) order by t.slug, t.app_id), '[]') from (select s.slug, a.app_id, a.version, a.enabled from tenant_app a join tenant s on s.id = a.tenant_id) t"
    code, text, why = run("psql", LOCAL_DB, "-Atc", "select set_config('ferrum.system','on',false)", "-c", query)
    if code != 0:
        return {"unread": why[:200]}
    return {"database": "ferrum_dev (local development only)", "rows": json.loads(text.splitlines()[-1]), "production": "No production environment is reachable from this checkout, so no production tenant's versions are known"}


def markdown(b: dict) -> str:
    pins = b["dependency_pins"]
    out = [
        "# Frozen baseline",
        "",
        f"Generated by `scripts/baseline.py` at {b['generated_at']}; edit nothing here by hand. The JSON beside this holds everything.",
        "Head, pin, build and installed version are four separate facts: none is inferred from another.",
        "",
        "## Repositories",
        "",
        "| Repository | Visibility | Default head | Checkout | Local commit | Dirty | Pinned by this platform commit |",
        "|---|---|---|---|---|---|---|",
    ]
    for r in b["repositories"]:
        head = (r.get("default_head") or "?")[:10]
        pin = pins.get(r["repository"], {}).get("revision", "-")[:10]
        drift = " **behind**" if pin not in ("-",) and r.get("default_head") and not r["default_head"].startswith(pin) else ""
        out.append(f"| {r['repository']} | {r.get('visibility') or '?'} | `{head}` | {r.get('checkout') or 'not here'} | `{(r.get('commit') or '-')[:10]}` on {r.get('branch', '-')} | {len(r.get('dirty', []))} | `{pin}`{drift} |")
    out += [f"\n{r['repository']}: {r['unread']}." for r in b["repositories"] if r.get("unread")]
    out += ["", "## Uncommitted work, preserved", ""]
    dirty = [(r["repository"], f) for r in b["repositories"] for f in r.get("dirty", [])]
    out += [f"- {repo}: `{f}`" for repo, f in dirty] or ["None."]
    patched = [(r["repository"], r) for r in b["repositories"] if r.get("local_patch") or r.get("unused_patches")]
    out += ["", "## Local dependency patches", ""]
    out += [f"- {repo}: {'a local patch' if r.get('local_patch') else 'no patch file'}" + (f", unused: {', '.join(r['unused_patches'])}" if r.get("unused_patches") else "") for repo, r in patched] or ["None."]
    out += ["", "## Artifacts", "", f"Frontend pinned by the Dockerfile: `{b['frontend_rev']}`.", ""]
    image = b["images"]
    out.append(f"Container images: could not be listed ({image['unread'][:120]}). Their absence is not known." if isinstance(image, dict) and "unread" in image else f"Container images: {len(image)} packages.")
    out += ["", "## Installed", ""]
    i = b["installed"]
    if "rows" in i:
        out += [f"Read from {i['database']}: {len(i['rows'])} installations.", "", i["production"] + "."]
    else:
        out.append(f"Not read: {i['unread']}.")
    out += ["", "## Migrations", "", f"{len(b['migrations']['server'])} server migrations, {len(b['migrations']['package_sql'])} package SQL files, with digests in the JSON.", ""]
    return "\n".join(out) + "\n"


def main() -> int:
    offline = "--offline" in sys.argv
    baseline = {
        "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platform_commit": run("git", "rev-parse", "HEAD", cwd=ROOT)[1],
        "repositories": repositories(offline),
        "dependency_pins": lock_pins(),
        "frontend_rev": re.search(r"FRONTEND_REV=([0-9a-f]{40})", (ROOT / "Dockerfile").read_text()).group(1),
        "native_features": features(),
        "apps_declared": apps(),
        "migrations": migrations(),
        "ci_runs": ci(offline),
        "images": images(offline),
        "installed": installed(offline),
    }
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / "baseline.json").write_text(json.dumps(baseline, indent=1, sort_keys=True) + "\n")
    (OUT / "baseline.md").write_text(markdown(baseline))
    print(f"{len(baseline['repositories'])} repositories, {len(baseline['dependency_pins'])} pinned, {len(baseline['apps_declared'])} apps declared")
    return 0


if __name__ == "__main__":
    sys.exit(main())
