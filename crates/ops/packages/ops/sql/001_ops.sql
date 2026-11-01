-- What is not a record: identity, the command ledger, fencing and telemetry (docs/plan/industrial.md).

-- Keys are public keys: nothing here can be stolen and used. `last_time_ms` is the newest signed
-- timestamp accepted, so a request cannot be replayed.
create table op_device_key (
  tenant_id uuid not null,
  id uuid not null,
  device_id uuid not null,
  public_key bytea not null check (octet_length(public_key) = 32),
  last_time_ms bigint not null default 0,
  created_at timestamptz not null default now(),
  not_after timestamptz,
  revoked_at timestamptz,
  primary key (tenant_id, id)
);
create index op_device_key_device on op_device_key (tenant_id, device_id);

create table op_enrollment (
  tenant_id uuid not null,
  code_hash text not null,
  device_id uuid not null,
  expires_at timestamptz not null,
  used_at timestamptz,
  created_by uuid,
  created_at timestamptz not null default now(),
  primary key (tenant_id, code_hash)
);

-- One holder per domain. The epoch rises with every change of holder, and everything a holder
-- reports or dispatches carries it, so an old holder cannot act on what it believes.
create table op_authority (
  tenant_id uuid not null,
  domain text not null check (length(domain) between 1 and 100),
  site_id uuid not null,
  epoch bigint not null default 1 check (epoch >= 1),
  holder uuid,
  changed_by uuid,
  changed_at timestamptz not null default now(),
  reason text,
  primary key (tenant_id, domain)
);
create unique index op_authority_holder on op_authority (tenant_id, holder) where holder is not null;

create table op_command (
  tenant_id uuid not null,
  id uuid not null,
  site_id uuid not null,
  domain text not null,
  target_id uuid not null,
  operation text not null check (length(operation) between 1 and 100),
  schema_version int not null default 1,
  issued_by uuid,
  issued_kind text not null default 'person' check (issued_kind in ('person', 'engine', 'automation', 'ai')),
  approval_ref text,
  epoch bigint not null,
  expected_state jsonb not null default 'null',
  issued_at timestamptz not null default now(),
  expires_at timestamptz not null,
  payload jsonb not null,
  payload_hash text not null,
  workflow_id text,
  step_id text,
  correlation_id text,
  config_version text,
  dedupe_key text,
  state text not null check (state in ('Proposed', 'Refused', 'Authorized', 'Queued', 'Expired', 'Accepted', 'Rejected', 'Executing', 'Completed', 'Failed', 'Aborted', 'OutcomeUnknown')),
  state_at timestamptz not null default now(),
  -- Sent to the site at least once: from here only the site can say what happened.
  sent_at timestamptz,
  -- The site said its journal has it, so it stops being sent.
  held_at timestamptz,
  cancel_requested_at timestamptz,
  cancel_requested_by uuid,
  -- A second, different ending arrived. The first stands and a person looks.
  conflict boolean not null default false,
  outcome jsonb,
  check (expires_at > issued_at),
  primary key (tenant_id, id)
);
create unique index op_command_dedupe on op_command (tenant_id, dedupe_key) where dedupe_key is not null;
create index op_command_queue on op_command (tenant_id, domain, issued_at) where state = 'Queued';
create index op_command_needs_person on op_command (tenant_id, site_id) where state in ('Proposed', 'OutcomeUnknown') or conflict;
create index op_command_site on op_command (tenant_id, site_id, issued_at desc);
create index op_command_workflow on op_command (tenant_id, workflow_id) where workflow_id is not null;

-- Everything ever said about a command, including what changed nothing. `report_id` makes a report
-- idempotent however often a site repeats it.
create table op_command_event (
  tenant_id uuid not null,
  command_id uuid not null,
  seq bigint generated always as identity,
  report_id uuid,
  state text not null,
  step text not null check (step in ('move', 'keep', 'conflict', 'refuse')),
  at_source timestamptz,
  received_at timestamptz not null default now(),
  actor uuid,
  epoch bigint,
  detail jsonb not null default 'null',
  evidence jsonb,
  primary key (tenant_id, command_id, seq),
  foreign key (tenant_id, command_id) references op_command (tenant_id, id)
);
create unique index op_command_event_report on op_command_event (tenant_id, report_id) where report_id is not null;
create trigger op_command_event_append_only before update or delete on op_command_event
  for each row execute function ferrum_forbid_change();
create trigger op_command_event_no_truncate before truncate on op_command_event
  for each statement execute function ferrum_forbid_change();

-- Business facts recorded at a site, applied once, in the order of arrival, by whoever subscribes.
create table op_fact (
  tenant_id uuid not null,
  id uuid not null,
  site_id uuid not null,
  kind text not null check (length(kind) between 1 and 100),
  at_source timestamptz not null,
  actor text,
  body jsonb not null,
  received_at timestamptz not null default now(),
  primary key (tenant_id, id)
);
create index op_fact_kind on op_fact (tenant_id, kind, at_source);

create table op_gateway (
  tenant_id uuid not null,
  device_id uuid not null,
  boot uuid,
  last_seen_at timestamptz,
  contract int,
  config_version text,
  health jsonb not null default '{}',
  primary key (tenant_id, device_id)
);

-- People who may work at a site's console with the link down. A site cannot ask anyone: this is the
-- list it was last given, with a PIN that means nothing anywhere else.
create table op_operator_pin (
  tenant_id uuid not null,
  site_id uuid not null,
  user_id uuid not null,
  pin_hash text not null,
  set_by uuid,
  set_at timestamptz not null default now(),
  primary key (tenant_id, site_id, user_id)
);

-- Equipment's states as they were reported, not intervals: a late report simply lands in its place.
create table op_equipment_event (
  tenant_id uuid not null,
  device_id uuid not null,
  at timestamptz not null,
  state text not null check (state in ('Running', 'Idle', 'Stopped', 'Maintenance', 'Unknown')),
  reason text,
  fact_id uuid not null,
  primary key (tenant_id, device_id, at, fact_id)
);

-- Measurements by day, so that retention drops a day rather than deleting rows. `day` is the source's
-- day, held within the days that exist: a wrong clock cannot ask for a partition of another year. Boot
-- and sequence make a measurement unique, so a batch sent twice lands once.
create table op_measurement (
  tenant_id uuid not null,
  device_id uuid not null,
  day date not null,
  boot uuid not null,
  seq bigint not null,
  metric text not null,
  source_time timestamptz not null,
  received_at timestamptz not null default now(),
  value double precision not null,
  unit text,
  quality smallint not null,
  class smallint not null,
  primary key (tenant_id, device_id, day, boot, seq)
) partition by range (day);
create index op_measurement_metric on op_measurement (tenant_id, device_id, metric, source_time desc);

-- What dashboards read, kept longer than the samples. Only values that are not bad count.
create table op_rollup_minute (
  tenant_id uuid not null,
  device_id uuid not null,
  metric text not null,
  minute timestamptz not null,
  samples int not null default 0,
  bad int not null default 0,
  min_value double precision,
  max_value double precision,
  sum_value double precision not null default 0,
  last_value double precision,
  last_time timestamptz,
  primary key (tenant_id, device_id, metric, minute)
);
create index op_rollup_minute_age on op_rollup_minute (minute);

do $$
declare t text;
begin
  foreach t in array array['op_device_key', 'op_enrollment', 'op_authority', 'op_command', 'op_command_event', 'op_fact', 'op_gateway', 'op_operator_pin',
                           'op_equipment_event', 'op_measurement', 'op_rollup_minute'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format(
      'create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
      'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
