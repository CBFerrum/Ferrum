-- Connections, part three (docs/builder/decisions.md BD-53 to BD-61).
--
-- One row per request a key made, or makes: the digest of its inputs, the clock its templates read (fixed at the first
-- attempt, so that a retry sends the same moment), whether a request went out whose answer is not known (`sending`:
-- it may have been delivered, so it is not sent again blindly), and a loop's or a wait's progress.
create table conn_attempt (
  tenant_id uuid not null,
  conn_id uuid not null,
  operation text not null,
  key text not null,
  input_digest text not null,
  clock timestamptz not null default now(),
  sending boolean not null default false,
  progress jsonb not null default '{}',
  at timestamptz not null default now(),
  primary key (tenant_id, conn_id, operation, key),
  foreign key (tenant_id, conn_id) references conn (tenant_id, id) on delete cascade
);

-- A description's `limit`: calls made in each window of `every` seconds, counted across workers.
create table conn_rate (
  tenant_id uuid not null,
  conn_id uuid not null,
  slot bigint not null,
  n integer not null default 0,
  primary key (tenant_id, conn_id, slot),
  foreign key (tenant_id, conn_id) references conn (tenant_id, id) on delete cascade
);

do $$
declare t text;
begin
  foreach t in array array['conn_attempt', 'conn_rate'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format('create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
                   'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
