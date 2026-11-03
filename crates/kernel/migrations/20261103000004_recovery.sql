-- What this database is, to notice when it has become something else: restored from a backup, or
-- restored over itself. One row. Nothing a tenant has anything to do with.
create table platform_incarnation (
  id boolean primary key default true check (id),
  system_identifier bigint not null,
  timeline integer not null,
  database_oid oid not null,
  -- Set from the moment a restore is noticed until a person has looked at what came back.
  held_since timestamptz,
  held_because text,
  released_at timestamptz,
  released_by uuid,
  released_note text,
  -- The last authority this database held. A region that lost it does not take it back by itself.
  authority_epoch bigint not null default 0
);
