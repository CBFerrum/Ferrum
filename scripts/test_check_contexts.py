#!/usr/bin/env python3
"""scripts/check_contexts.py on fixtures in a temporary directory."""
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_contexts  # noqa: E402


def found(src: str) -> list:
    with tempfile.TemporaryDirectory() as d:
        (Path(d) / "src").mkdir()
        (Path(d) / "src" / "lib.rs").write_text(src)
        return check_contexts.findings(Path(d), "app")


def test_screen_made_elsewhere_is_found():
    assert found("fn f() -> Made { Made::Screen }\n")


def test_elevation_without_made_is_found_and_with_it_passes():
    assert found("fn f(mut c: Ctx) { c.is_system = true; }\n")
    assert not found("fn f(mut c: Ctx) {\n    c.is_system = true;\n    c.made = Made::System;\n}\n")
    assert not found("fn f(c: Ctx) -> Ctx { c.elevated() }\n")


def test_tests_and_comments_are_left_out():
    assert not found("// c.is_system = true;\n#[cfg(test)]\nmod t { fn f() { Made::Screen; } }\n")


if __name__ == "__main__":
    for name, fn in list(globals().items()):
        if name.startswith("test_"):
            fn()
    print("ok")
