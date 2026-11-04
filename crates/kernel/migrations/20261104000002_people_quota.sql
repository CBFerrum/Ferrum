-- A workspace's people quota (`tenant.quotas->>'people'`, the operator's or its plan's) holds on
-- every path that makes someone an active member: added by an administrator, by SCIM or single
-- sign-on, joining by invitation, turned on again. Service accounts and people turned off never
-- count. Adds to one workspace wait for each other, so two at once cannot both take the last seat.
create or replace function ferrum_people_quota() returns trigger language plpgsql as $$
declare
  cap bigint;
  used bigint;
begin
  if new.kind <> 'member' or not new.is_active then
    return new;
  end if;
  if tg_op = 'UPDATE' and old.is_active and old.kind = 'member' then
    return new;
  end if;
  select (quotas->>'people')::bigint into cap from tenant where id = new.tenant_id;
  if cap is null then
    return new;
  end if;
  perform pg_advisory_xact_lock(hashtext('ferrum_people_quota'), hashtext(new.tenant_id::text));
  select count(*) into used from app_user where tenant_id = new.tenant_id and is_active and kind = 'member' and id <> new.id;
  if used >= cap then
    raise exception 'ferrum: this workspace has room for % people on its plan: turn someone off, or ask for a larger plan', cap;
  end if;
  return new;
end $$;

create trigger app_user_people_quota before insert or update of is_active, kind on app_user
  for each row execute function ferrum_people_quota();
