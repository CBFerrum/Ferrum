-- Floors of the two-version window (crates/kernel/src/window.rs; docs/operations.md, "Protections an older build would
-- not enforce"): one row per protection in use, with the date of the first build that enforces it. The recorded
-- `schema_compatible_from` is never earlier than the latest of them, so a rollback to a build that would drop one of
-- these protections is refused at start. A temporary one (a move under way) has a row per holder and goes with the
-- last; a lasting one has the holder ''.
create table window_floor (
  feature text not null,
  holder text not null default '',
  since timestamptz not null,
  what text not null,
  temporary boolean not null default false,
  switched_on_at timestamptz not null default now(),
  switched_on_by timestamptz not null,
  primary key (feature, holder)
);
