-- A workspace's slug names addresses: its apps' origins of their own ({app}--{slug}.{apps base},
-- docs/plan/apps-ui.md U8), whose browser storage must never reach another workspace. So once a
-- workspace has used a slug, no other workspace ever does: renaming or deleting a workspace retires
-- its old slug for good. Enforced here, whatever code changes slugs.
--
-- No row-level security: the guard must see every retired slug from any context (a rename made in
-- the workspace's own context included), and the table holds slugs and ids only.
create table tenant_slug_retired (
  slug text primary key,
  tenant_id uuid not null,
  retired_at timestamptz not null default now()
);

create function ferrum_tenant_slug_guard() returns trigger language plpgsql as $$
begin
  -- One slug at a time: a rename retiring a slug and a new workspace claiming it cannot interleave.
  if tg_op = 'DELETE' or (tg_op = 'UPDATE' and new.slug is distinct from old.slug) then
    perform pg_advisory_xact_lock(hashtextextended('ferrum-slug:' || old.slug, 0));
    insert into tenant_slug_retired (slug, tenant_id) values (old.slug, old.id) on conflict (slug) do nothing;
  end if;
  if tg_op = 'INSERT' or (tg_op = 'UPDATE' and new.slug is distinct from old.slug) then
    perform pg_advisory_xact_lock(hashtextextended('ferrum-slug:' || new.slug, 0));
    if exists (select 1 from tenant_slug_retired r where r.slug = new.slug and r.tenant_id <> new.id) then
      raise exception using
        errcode = 'unique_violation',
        constraint = 'tenant_slug_retired_pkey',
        message = format('the slug %s belonged to another workspace and is never used again', new.slug);
    end if;
  end if;
  if tg_op = 'DELETE' then
    return old;
  end if;
  return new;
end $$;

create trigger tenant_slug_guard before insert or update of slug or delete on tenant
  for each row execute function ferrum_tenant_slug_guard();
