#!/usr/bin/env python3
"""builder_inventory.py counts what is committed, by app, and nothing else: an uncommitted edit, a private
repository and a no-op upkeep loop are not features (BF-BASE-003)."""

import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent


def git(*args, cwd):
    subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True)


def app(folder: Path, name: str, files: dict, uncommitted: dict = None):
    repo = folder / name
    repo.mkdir()
    git("init", "-q", cwd=repo)
    git("config", "user.email", "t@t", cwd=repo)
    git("config", "user.name", "t", cwd=repo)
    for path, text in files.items():
        (repo / path).parent.mkdir(parents=True, exist_ok=True)
        (repo / path).write_text(text)
    git("add", ".", cwd=repo)
    git("commit", "-qm", "first", cwd=repo)
    for path, text in (uncommitted or {}).items():
        (repo / path).write_text(text)


with tempfile.TemporaryDirectory() as d:
    folder = Path(d)
    app(folder, "ferrum-app-clinic", {
        "src/site.rs": "impl Site for ClinicSite {}\nlet hash = token_hash(&t);\n",
        "src/lib.rs": "pub async fn maintenance(k: &Kernel) -> Result<()> { remind(k).await }\nk.queue_mail(tx, m)?;\n",
        "app.yaml": "shell:\n  shell.portal:\n    - {id: visits}\n",
    })
    app(folder, "ferrum-app-quiet", {
        "src/lib.rs": "pub async fn maintenance(_k: &Kernel) -> Result<()> { Ok(()) }\n",
    }, uncommitted={"src/lib.rs": "impl Site for Later {}\n"})
    app(folder, "ferrum-app-cloud", {"src/site.rs": "impl Site for Private {}\n"})
    run = subprocess.run([sys.executable, str(HERE / "builder_inventory.py"), str(folder)], capture_output=True, text=True, check=True)
    rows = {line.split(" | ")[0].lstrip("| "): line.split(" | ")[2] for line in run.stdout.splitlines() if line.startswith("| ") and " | BF" in line}
    assert rows["Pages or endpoints served by the app's own handler"] == "1", rows  # not the uncommitted one, not the private one
    assert rows["A page of the workspace's portal"] == "1", rows
    assert rows["A guest's own link, hashed and checked by the app"] == "1", rows
    assert rows["A scheduled upkeep loop that does something"] == "1", rows  # the quiet app's loop takes `_k` and does nothing
    assert rows["Mail queued by the app's code"] == "1", rows
    assert rows["Journal entries posted by the app"] == "0", rows
    assert "ferrum-app-cloud" not in run.stdout and "cloud " not in run.stdout, "a private repository is left out"
    assert "`src/site.rs:1`" in run.stdout, "each place is named with its file and line"

print("builder_inventory: committed features only, by app, private repositories left out")
