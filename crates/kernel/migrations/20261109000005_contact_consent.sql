-- What a person said about being contacted for a purpose (crates/kernel/src/retention.rs, FEO-015):
-- the latest word for each address. Marketing from any app asks it when a message goes out, so one
-- withdrawal stops every campaign, sequence, automation and connector sending marketing to them;
-- transactional notices do not ask it. A restriction of processing stops both.
create table contact_consent (
  tenant_id uuid not null references tenant (id) on delete cascade,
  email citext not null,
  purpose text not null check (purpose in ('marketing')),
  granted boolean not null,
  source text not null check (length(source) between 1 and 200),
  recorded_by uuid,
  recorded_at timestamptz not null default now(),
  primary key (tenant_id, email, purpose)
);

alter table contact_consent enable row level security;
alter table contact_consent force row level security;
create policy tenant_isolation on contact_consent
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
