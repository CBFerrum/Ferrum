-- Views and votes on published record pages (docs/builder/decisions.md BD-42). No personal data: a visitor is a
-- keyed digest of a first-party cookie's random value (under the workspace's own key, `published_key`), a view's
-- digest taken with its day, so one day's cannot be matched to another's, a vote's with its record; an address is
-- a keyed digest with its hour, kept two hours to limit how much one address counts.
create table published_key (
  tenant_id uuid primary key references tenant (id) on delete cascade,
  key bytea not null
);
create table published_view (
  tenant_id uuid not null references tenant (id) on delete cascade,
  list text not null,
  record_id uuid not null,
  day date not null,
  visitor bytea not null,
  primary key (tenant_id, list, record_id, day, visitor)
);
create index published_view_day on published_view (tenant_id, day);
create table published_vote (
  tenant_id uuid not null references tenant (id) on delete cascade,
  list text not null,
  record_id uuid not null,
  visitor bytea not null,
  up boolean not null,
  primary key (tenant_id, list, record_id, visitor)
);
create table published_hit (
  tenant_id uuid not null references tenant (id) on delete cascade,
  address bytea not null,
  hour timestamptz not null,
  n int not null,
  primary key (tenant_id, address, hour)
);
do $$
declare t text;
begin
  foreach t in array array['published_key', 'published_view', 'published_vote', 'published_hit'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format('create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
      'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
