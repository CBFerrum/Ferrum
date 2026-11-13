-- A short-lived public address of a stored file, for a service that pulls media by address (docs/builder/decisions.md
-- BD-64): which file, for which call (the connection, operation, key and the record it is about), for whom, until
-- when, and when it was ended early. The address is `/files/pull/{id}?s=<HMAC of the id under the server's key>`.
create table conn_file_address (
  tenant_id uuid not null,
  id uuid not null primary key,
  conn_id uuid not null,
  operation text not null,
  name text not null,
  call_key text,
  file_id uuid not null,
  entity text,
  record_id uuid,
  user_id uuid,
  expires_at timestamptz not null,
  ended_at timestamptz,
  at timestamptz not null default now(),
  foreign key (tenant_id, conn_id) references conn (tenant_id, id) on delete cascade
);
create index conn_file_address_call on conn_file_address (tenant_id, conn_id, operation, call_key);

alter table conn_file_address enable row level security;
alter table conn_file_address force row level security;
create policy tenant_isolation on conn_file_address using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
