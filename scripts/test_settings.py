#!/usr/bin/env python3
"""scripts/settings.py's own parts on small inputs: where a configuration file reads each variable, what a Rust line
reads, and the guide's table."""

import importlib.util
from pathlib import Path

spec = importlib.util.spec_from_file_location("settings", Path(__file__).with_name("settings.py"))
s = importlib.util.module_from_spec(spec)
spec.loader.exec_module(s)

config = Path(__file__).resolve().parent.parent / "target" / "test_settings_config.yaml"
config.parent.mkdir(exist_ok=True)
config.write_text(
    'server:\n  port: <%= get_env(name="PORT", default="5150") %>\n  cors:\n    allow_origins: [<%= get_env(name="CORS_ORIGINS", default="\\"https://a\\"") %>]\n'
    'settings:\n  zones: \'<%= get_env(name="FERRUM_ZONES", default="") %>\'\n  user: "<%= get_env(name="MAILER_USER") %>"\n'
)
assert s.config_paths(config) == {
    "PORT": {"server.port"},
    "CORS_ORIGINS": {"server.cors.allow_origins"},
    "FERRUM_ZONES": {"settings.zones"},
    "MAILER_USER": {"settings.user"},
}, s.config_paths(config)
config.unlink()

line = 'let a = std::env::var("FERRUM_X").ok(); let b = ferrum_kernel::env::var("FERRUM_Y"); some("BAO_TOKEN"); env!("OUT_DIR"); x.var(name)'
assert s.READ.findall(line) == ["FERRUM_X", "FERRUM_Y", "BAO_TOKEN", "OUT_DIR"], s.READ.findall(line)
assert s.TEST_NAME.search("FERRUM_TEST_DATABASE_URL") and s.TEST_NAME.search("FERRUM_COEDIT_TEST_DATABASE_URL")
assert not s.TEST_NAME.search("FERRUM_TESTER")

table = s.table([{"key": "a", "env": "FERRUM_A", "type": "choice", "values": ["", "x"], "default": "", "applies": "live", "doc": "one | two"},
                 {"key": "b", "env": "B", "secret": True, "doc": "kept"}])
assert "| `a` | `FERRUM_A` | empty, `x` |  | installation, at once | one \\| two |" in table, table
assert "| `b` | `B` | text |  | secret: the environment | kept |" in table, table
platform = [{"key": "pdf_url", "env": "FERRUM_PDF_URL", "doc": "x"},
            {"key": "drive.max_file_mb", "env": "FERRUM_DRIVE_MAX_FILE_MB", "environment": "Drive", "app": "drive", "doc": "x"}]
apps = [("drive", [{"key": "max_file_mb", "env": "FERRUM_DRIVE_MAX_FILE_MB", "type": "int", "doc": "y"}])]
all_, clashes = s.merged(platform, apps)
assert not clashes and [e["key"] for e in all_] == ["pdf_url", "drive.max_file_mb"] and "environment" not in all_[1], (all_, clashes)
_, clashes = s.merged(platform, [("comms", [{"key": "pdf", "env": "FERRUM_PDF_URL", "doc": "z"}])])
assert clashes and "pdf_url" in clashes[0] and "comms.pdf" in clashes[0], clashes
assert s.platform_findings([{"key": "x.y", "env": "X_Y", "app": "x", "doc": "d"}]), "moves, so still the environment's"
sound = s.registry_findings([{"key": "drive.a", "env": "FERRUM_A", "app": "drive", "environment": "Drive", "doc": "d"},
                             {"key": "move_source_region", "env": "FERRUM_MOVE_SOURCE_REGION", "input": "move_workspace", "doc": "d"}])
assert not [f for f in sound if f.startswith(("drive.a", "move_source_region"))], sound
assert s.registry_findings([{"key": "a", "env": "FERRUM_A", "app": "drive", "doc": "d"}]), "an app's key is <app>.<key>"
grouped = s.table(platform + [{"key": "move_source_region", "env": "FERRUM_MOVE_SOURCE_REGION", "input": "move_workspace", "doc": "d"}])
assert "The app `drive`'s settings:" in grouped and "an input of `ferrum task move_workspace`" in grouped, grouped
print("scripts/settings.py reads what it should")
