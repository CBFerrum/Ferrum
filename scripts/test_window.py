#!/usr/bin/env python3
"""window.py: a change an older build cannot live with fails until COMPATIBLE_FROM moves with it; additive ones pass."""

import subprocess
import tempfile
from pathlib import Path

import window

# Statements, one by one.
for sql in [
    "create table a (id int not null, x text); create unique index a_x on a (x); alter table a add constraint c check (id > 0);",
    "alter table outbox add column build_date timestamptz;",
    "alter table t add column n int not null default 0;",
    "create or replace function f() returns int language sql as $$ select 1; drop table t; $$;",
    "create index i on t (x);",
    "-- window: safe, nothing reads it yet\nalter table t drop column x;",
]:
    assert window.sql_hazards(sql) == [], (sql, window.sql_hazards(sql))
for sql, what in [
    ("alter table t drop column x;", "drops a column"),
    ("drop table if exists t;", "drops a table"),
    ("alter table t rename column x to y;", "renames"),
    ("alter table t alter column x type bigint;", "changes a column's type"),
    ("alter table t alter column x set not null;", "makes a column required"),
    ("alter table t add column n numeric(28,9) not null;", "adds a required column without a default"),
    ("alter table t add constraint c unique (x);", "adds a constraint"),
    ("create unique index u on t (x);", "adds a unique index"),
    ("drop function if exists f(int);", "drops a function"),
]:
    found = window.sql_hazards(sql)
    assert len(found) == 1 and found[0].startswith(what), (sql, found)

# Entities, as the schema sync makes tables of them.
old = "- name: A\n  fields:\n    - {name: x, type: data}\n    - {name: y, type: link, to: B}\n- name: Gone\n  fields: []\n"
assert window.entity_hazards("e.yaml", old, old + "- name: New\n  fields: [{name: z, type: int}]\n") == []
assert window.entity_hazards("e.yaml", old, old.replace("    - {name: x, type: data}\n", "    - {name: x, type: data}\n    - {name: w, type: int}\n")) == []
found = window.entity_hazards("e.yaml", old, "- name: A\n  unique: [[x]]\n  fields:\n    - {name: x, type: int, unique: true}\n")
assert sorted(f.split(": ", 1)[1] for f in found) == sorted(
    ["entity Gone taken out", "A.y taken out", "A.x type changed from data to int", "A.x made unique", "A gets a new unique group ['x']"]
), found


# A repository, from its merge base.
def git(*args, cwd):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


with tempfile.TemporaryDirectory() as d:
    repo = Path(d)
    git("init", "-q", cwd=repo)
    git("config", "user.email", "t@t", cwd=repo)
    git("config", "user.name", "t", cwd=repo)
    (repo / "crates/kernel/src").mkdir(parents=True)
    (repo / "crates/kernel/migrations").mkdir(parents=True)
    constant = (repo / window.WINDOW)
    constant.write_text('pub const COMPATIBLE_FROM: &str = "2026-11-13T20:00:00Z";\n')
    (repo / "crates/kernel/migrations/1_first.sql").write_text("create table t (x text);\n")
    git("add", ".", cwd=repo)
    git("commit", "-qm", "first", cwd=repo)
    base = git("rev-parse", "HEAD", cwd=repo)

    (repo / "crates/kernel/migrations/2_more.sql").write_text("alter table t add column y text;\n")
    assert window.check(repo, base, None) == ([], False)
    (repo / "crates/kernel/migrations/2_more.sql").write_text("alter table t add column y text not null;\n")
    assert len(window.check(repo, base, None)[0]) == 1, "a file not yet added was not looked at"
    (repo / "crates/kernel/migrations/2_more.sql").write_text("alter table t add column y text;\n")
    (repo / "crates/kernel/migrations/3_less.sql").write_text("alter table t drop column x;\n")
    found, moved = window.check(repo, base, None)
    assert len(found) == 1 and "3_less.sql: drops a column" in found[0] and not moved, found
    constant.write_text('pub const COMPATIBLE_FROM: &str = "2026-12-01T00:00:00Z";\n')
    git("add", ".", cwd=repo)
    git("commit", "-qm", "the removal, a release later", cwd=repo)
    found, moved = window.check(repo, base, "HEAD")
    assert len(found) == 1 and moved, (found, moved)
    # A migration that was there before is not looked at again.
    assert window.check(repo, "HEAD", None) == ([], False)

print("window.py: ok")
