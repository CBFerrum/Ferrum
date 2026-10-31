-- Kinds of notification and each person's choice per kind (docs/plan/apps-ui.md §12.8; the
-- kernel's notify.rs): in the bar only, by email at once, in a daily email digest, or not at all.
-- The platform's kinds are mention, comment, assignment, watch and automation; apps declare
-- theirs (`shell.notification`) as `{app}.{id}`.
alter table notification add column kind text not null default 'general';
-- How it was delivered when sent: inapp, email or digest.
alter table notification add column delivery text not null default 'inapp' check (delivery in ('inapp', 'email', 'digest'));
-- When it went out by email (at once, or in a digest).
alter table notification add column emailed_at timestamptz;

create index notification_digest on notification (tenant_id, user_id, created_at) where delivery = 'digest' and emailed_at is null and read_at is null;

create table notification_pref (
  tenant_id uuid not null,
  user_id uuid not null,
  kind text not null check (length(kind) between 1 and 80),
  channel text not null check (channel in ('inapp', 'email', 'digest', 'off')),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, user_id, kind),
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);

alter table notification_pref enable row level security;
alter table notification_pref force row level security;
create policy tenant_isolation on notification_pref
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
