-- A grant already held is no new grant: an identity provider re-syncing someone who held both roles
-- of a pair before it was declared (their insert of a role they hold becomes an update) is not
-- refused. Granting either afresh still is.
create or replace function user_role_conflict() returns trigger language plpgsql as $$
declare
  c record;
begin
  perform 1 from app_user where tenant_id = new.tenant_id and id = new.user_id for no key update;
  if tg_op = 'INSERT' and exists (select 1 from user_role where tenant_id = new.tenant_id and user_id = new.user_id and role = new.role) then
    return new;
  end if;
  select rc.role_a, rc.role_b, rc.reason into c
    from role_conflict rc
    join user_role held on held.tenant_id = rc.tenant_id and held.user_id = new.user_id
     and held.role = case when rc.role_a = new.role then rc.role_b else rc.role_a end
     and (held.expires_at is null or held.expires_at > now())
   where rc.tenant_id = new.tenant_id and new.role in (rc.role_a, rc.role_b)
   limit 1;
  if found then
    raise exception 'ferrum: % and % are not held by one person: %', c.role_a, c.role_b, c.reason;
  end if;
  return new;
end $$;
