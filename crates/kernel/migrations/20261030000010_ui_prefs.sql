-- Per-user interface preferences (list columns and sort, saved filters, collapsed panels): small
-- JSON documents keyed by a scope such as `list:SalesInvoice`, versioned so two open tabs never
-- silently overwrite each other.
create table ui_pref (
  tenant_id uuid not null references tenant(id) on delete cascade,
  user_id uuid not null,
  scope text not null check (scope ~ '^[A-Za-z0-9:_.-]{1,120}$'),
  value jsonb not null,
  version bigint not null default 1,
  updated_at timestamptz not null default now(),
  primary key (tenant_id, user_id, scope)
);

alter table ui_pref enable row level security;
alter table ui_pref force row level security;
create policy tenant_isolation on ui_pref using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
