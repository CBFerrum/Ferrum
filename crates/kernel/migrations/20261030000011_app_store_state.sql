-- What each app store last told this cluster: when its newest applied revocation list was
-- generated, so an older (stale or replayed) list is ignored instead of silently holding back
-- newer revocations. Shared by every replica.
create table app_store_state (
  store text primary key,
  revocations_at timestamptz not null,
  updated_at timestamptz not null default now()
);
