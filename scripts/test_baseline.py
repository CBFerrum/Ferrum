#!/usr/bin/env python3
"""baseline.py on a checkout with someone else's work in it: it reports, and it touches nothing."""

import subprocess
import tempfile
from pathlib import Path

import baseline


def git(*args, cwd):
    subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True)


with tempfile.TemporaryDirectory() as d:
    repo = Path(d)
    git("init", "-q", cwd=repo)
    git("config", "user.email", "t@t", cwd=repo)
    git("config", "user.name", "t", cwd=repo)
    (repo / "Cargo.lock").write_text('[[package]]\nname = "a"\nversion = "1.0.0"\nsource = "git+https://github.com/CBFerrum/hidden?branch=main#abc"\n\n[[patch.unused]]\nname = "ferrum-app-edge"\nversion = "0.4.0"\n')
    (repo / "kept.txt").write_text("first\n")
    git("add", ".", cwd=repo)
    git("commit", "-qm", "first", cwd=repo)
    (repo / "kept.txt").write_text("someone's unfinished work\n")
    (repo / "new.txt").write_text("not added\n")
    (repo / ".cargo").mkdir()
    (repo / ".cargo" / "config.toml").write_text('[patch."https://github.com/x/y"]\ny = { path = "../y" }\n')

    found = baseline.patches(repo)
    assert found["unused_patches"] == ["ferrum-app-edge 0.4.0"], found
    assert found["local_patch"] is True, found

    baseline.CHECKOUTS = {"x/y": [str(repo)]}
    row = next(r for r in baseline.repositories(offline=True) if r["repository"] == "x/y")
    assert row["dirty"] == sorted([".cargo/", "kept.txt", "new.txt"]), row["dirty"]
    assert row["unused_patches"] == ["ferrum-app-edge 0.4.0"], row
    assert (repo / "kept.txt").read_text() == "someone's unfinished work\n", "their work is still there"
    # AT-BASE-008: a repository nothing here can read is a limit on what is known, not a thing that is missing.
    hidden = next(r for r in baseline.repositories(offline=True) if r["repository"] == "CBFerrum/hidden")
    assert "nothing here can read it" in hidden["unread"] and hidden["checkout"] is None, hidden
    assert not subprocess.run(["git", "diff", "--cached", "--quiet"], cwd=repo).returncode, "and nothing was staged"
print("ok")
