-- Events say where they came from and how urgent they are (docs/events.md). The trace columns
-- default to what the writing transaction was given (db.rs, Trace), so no caller has to pass them.
-- current_setting is stable, so existing rows are not rewritten.
alter table outbox
  add column priority smallint not null default coalesce(nullif(current_setting('ferrum.priority', true), '')::smallint, 1),
  add column correlation_id text default nullif(current_setting('ferrum.correlation', true), ''),
  add column causation_id bigint default nullif(current_setting('ferrum.causation', true), '')::bigint,
  add column traceparent text default nullif(current_setting('ferrum.traceparent', true), ''),
  add column actor uuid,
  add column legal_entity uuid,
  add column source_version int,
  add column schema_version smallint not null default 1;

-- Claiming walks the workspaces that have work, then each one's most urgent events.
drop index if exists outbox_pending;
create index outbox_due on outbox (tenant_id, priority, id) where processed_at is null and not dead;
