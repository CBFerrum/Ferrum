-- What the assistant was told, by version: each answer records the version of the instructions,
-- tools and model it answered under (`assistant_message.usage->>'prompt'`), so an evaluation, a
-- complaint or an audit can be traced to what the model saw.
create table assistant_prompt (
  tenant_id uuid not null references tenant(id) on delete cascade,
  digest text not null,
  model text not null,
  system text not null,
  tools jsonb not null,
  first_used timestamptz not null default now(),
  primary key (tenant_id, digest)
);

alter table assistant_prompt enable row level security;
alter table assistant_prompt force row level security;
create policy tenant_isolation on assistant_prompt
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
