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
print("scripts/settings.py reads what it should")
