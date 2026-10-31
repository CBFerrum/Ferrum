-- Web Push (docs/plan/apps-ui.md §12.5; the kernel's webpush.rs): the server's own keys, each
-- device's subscription with its browser's push service, and "push" as a way a kind of
-- notification reaches people (in the bar and on the devices where they turned it on).

-- Keys the server makes for itself, sealed with its secret key. `vapid` signs what it sends to
-- push services (RFC 8292); `public` is the half browsers subscribe with.
create table server_key (
  name text primary key check (length(name) between 1 and 40),
  public text not null default '' check (length(public) <= 200),
  sealed text not null check (length(sealed) between 1 and 4000),
  created_at timestamptz not null default now()
);
alter table server_key enable row level security;
alter table server_key force row level security;
create policy system_only on server_key using ((select ferrum_is_system())) with check ((select ferrum_is_system()));

-- One device's subscription: made in a browser session when the person turns notifications on
-- there, and kept with that session. Signing the device out ends it, and so do signing out
-- everywhere (`token_version` moves on) and a new server key (`vapid`: the public half it was
-- made for). An endpoint belongs to whoever registered it last. Keys are base64url, unpadded.
create table push_subscription (
  tenant_id uuid not null,
  session_id uuid not null,
  user_id uuid not null,
  endpoint text not null check (length(endpoint) between 10 and 2048),
  p256dh text not null check (length(p256dh) = 87),
  auth text not null check (length(auth) = 22),
  vapid text not null check (length(vapid) between 1 and 200),
  token_version integer not null,
  created_at timestamptz not null default now(),
  primary key (tenant_id, session_id),
  foreign key (tenant_id, session_id) references user_session (tenant_id, id) on delete cascade,
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);
create unique index push_subscription_endpoint on push_subscription (endpoint);
create index push_subscription_user on push_subscription (tenant_id, user_id);
alter table push_subscription enable row level security;
alter table push_subscription force row level security;
create policy tenant_isolation on push_subscription
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

alter table notification drop constraint notification_delivery_check;
alter table notification add constraint notification_delivery_check check (delivery in ('inapp', 'email', 'digest', 'push'));
alter table notification_pref drop constraint notification_pref_channel_check;
alter table notification_pref add constraint notification_pref_channel_check check (channel in ('inapp', 'email', 'digest', 'push', 'off'));
