#!/usr/bin/env python3
"""Who is acting (docs/builder/decisions.md BD-83): a context is marked as what made it where it is made, and only one
place makes a person at a screen.

    scripts/check_contexts.py          # the platform and every app the lock compiles in
    scripts/check_contexts.py --app .  # one app checkout

Findings, each failing the check:
- `Made::Screen` named outside `crates/kernel/src/auth.rs` (`Kernel::request_ctx_as`) and the type's own file;
- `is_system = true` or `is_system: true` with no `made` within four lines either side: code that elevates a context
  must say it is the system's now (`Ctx::elevated` does both).
Tests, examples and build artefacts are not read (secret_paths.sources and code_lines).
"""
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import secret_paths  # noqa: E402

SCREEN = re.compile(r"\bMade::Screen\b")
SYSTEM = re.compile(r"\bis_system\s*(=|:)\s*true\b")
SCREEN_MAKERS = {"crates/kernel/src/auth.rs", "crates/kernel/src/ctx.rs"}


def findings(root: Path, name: str) -> list:
    out = []
    for path in secret_paths.sources(root):
        if path.suffix != ".rs":
            continue
        lines = dict(secret_paths.code_lines(path.read_text(encoding="utf-8", errors="replace")))
        place = secret_paths.where(name, root, path)
        for n, line in lines.items():
            code = secret_paths.outside_strings(line)
            if SCREEN.search(code) and place not in SCREEN_MAKERS:
                out.append(f"{place}:{n}: only Kernel::request_ctx_as makes a person at a screen")
            if SYSTEM.search(code) and not any("made" in lines.get(m, "") for m in range(n - 4, n + 5)):
                out.append(f"{place}:{n}: is_system set without made (use Ctx::elevated, or set made = Made::System)")
    return out


def main(argv: list) -> int:
    if argv[:1] == ["--app"] and len(argv) == 2:
        roots = [(Path(argv[1]).resolve().name, Path(argv[1]).resolve())]
    else:
        roots = [(name, root) for name, root, _, _ in secret_paths.packages() if name != "packages"]
    seen, found = set(), []
    for name, root in roots:
        if root in seen:
            continue
        seen.add(root)
        found += findings(root, name)
    for f in found:
        print(f)
    print(f"{len(seen)} crates read, {len(found)} contexts made without saying what made them")
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
