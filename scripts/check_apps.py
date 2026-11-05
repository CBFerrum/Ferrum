#!/usr/bin/env python3
"""The parts of the app completeness contract (docs/app-contract.md) a program can see, checked in
every app the lock compiles in: its manifest, description, history, scenario test, interface and
launcher icon.

    scripts/check_apps.py      # CI runs this: fails when an app lacks a part

Reads `cargo metadata`, so it checks the very checkouts the build uses: each package from one of
CBFerrum's app repositories with an `app.yaml` beside its `Cargo.toml` (an app, or an add-on of one).
"""

import json
import re
import subprocess
import sys
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parent.parent
FIELDS = ["id", "name", "version", "publisher", "summary", "categories", "license", "homepage", "support"]
# What an add-on (`parent: <app>`) may take from its parent's manifest, in its repository's root.
INHERITED = {"license", "homepage", "support", "description"}
LICENSES = ("AGPL-3.0", "GPL-3.0", "LGPL-3.0", "MIT", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "MPL-2.0")


def icons() -> set:
    """The icons the shell draws (server/shell/runtime.js)."""
    text = (ROOT / "server/shell/runtime.js").read_text()
    table = text[text.index("const ICONS = {") : text.index("\n};", text.index("const ICONS = {"))]
    return set(re.findall(r'^\s+"([a-z0-9-]+)":', table, re.M))


def lacks(app_dir: Path, shell_icons: set) -> list:
    """What an app in `app_dir` lacks of the contract."""
    out = []
    manifest = yaml.safe_load((app_dir / "app.yaml").read_text()) or {}
    # An add-on's parent: the manifest of that id nearest above it, its repository's root.
    parent_dir = None
    if manifest.get("parent"):
        for d in app_dir.parents[:3]:
            if (d / "app.yaml").is_file() and (yaml.safe_load((d / "app.yaml").read_text()) or {}).get("id") == manifest["parent"]:
                parent_dir = d
                break
    parent = yaml.safe_load((parent_dir / "app.yaml").read_text()) if parent_dir else {}
    def field(name):
        if manifest.get(name):
            return manifest[name], app_dir
        if name in INHERITED and parent.get(name):
            return parent[name], parent_dir
        return None, None
    for name in FIELDS:
        if not field(name)[0]:
            out.append(f"app.yaml has no {name}")
    version = str(manifest.get("version", ""))
    licence = str(field("license")[0] or "")
    if licence and not licence.startswith(LICENSES):
        out.append(f"{licence} is not a licence Ferrum ships")
    description, where = field("description")
    if not description or not (where / description).is_file():
        out.append("app.yaml names no description file it has")
    # The history lives with the repository: an add-on's is its repository's (one level or two up).
    home = next((d for d in [app_dir, *app_dir.parents][:4] if (d / "CHANGELOG.md").is_file()), None)
    if home is None:
        out.append("no CHANGELOG.md")
    elif version and not re.search(rf"^## .*\b{re.escape(version)}\b", (home / "CHANGELOG.md").read_text(), re.M):
        out.append(f"CHANGELOG.md says nothing of {version}")
    readme = (home or app_dir) / "README.md"
    if readme.is_file() and version and not re.search(rf"\| \*\*[^*|]*\b{re.escape(version)}\b[^*|]*\*\* \|", readme.read_text()):
        out.append(f"the README's changelog has no row for {version}")
    # An add-on's flows may run in its parent's scenario.
    if not any(any((d / "tests").glob("*_it.rs")) for d in [app_dir, parent_dir] if d):
        out.append("no scenario test (tests/*_it.rs)")
    ui = (manifest.get("ui") or {}).get("dist")
    if ui and not ((app_dir / ui / "index.html").is_file() and (app_dir / ui / ".source").is_file()):
        out.append(f"{ui} is not built (index.html and .source)")
    contributes = manifest.get("contributes") or {}
    for app in contributes.get("shell.app") or []:
        if app.get("icon") and app["icon"] not in shell_icons:
            out.append(f"the shell draws no {app['icon']} icon")
    return out


def main() -> int:
    run = subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked", "--all-features"], cwd=ROOT, capture_output=True, text=True, check=True)
    packages = json.loads(run.stdout)["packages"]
    shell_icons = icons()
    seen, failed = set(), 0
    for p in sorted(packages, key=lambda p: p["name"]):
        if not (p.get("source") or "").startswith("git+https://github.com/CBFerrum/ferrum-app-"):
            continue
        app_dir = Path(p["manifest_path"]).parent
        if not (app_dir / "app.yaml").is_file() or app_dir in seen:
            continue
        seen.add(app_dir)
        missing = lacks(app_dir, shell_icons)
        failed += bool(missing)
        print(f"{p['name']}: " + ("; ".join(missing) if missing else "ok"))
    print(f"{len(seen)} apps, {failed} short of the contract")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
