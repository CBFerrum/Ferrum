-- Segregation of duties (FERRUM-IDENTITY-014): pairs of roles one person may not hold together, as
-- an administrator declares them (raising suppliers and paying them, say). The database refuses a
-- grant that would join them, on every path a grant takes: by hand, through a SCIM group, from an
-- app. People who held both before the pair was declared are listed beside it until one goes.
create table role_conflict (
  tenant_id uuid not null,
  role_a text not null,
  role_b text not null,
  reason text not null check (length(reason) between 1 and 500),
  created_by uuid,
  created_at timestamptz not null default now(),
  primary key (tenant_id, role_a, role_b),
  check (role_a < role_b),
  foreign key (tenant_id, role_a) references role (tenant_id, name) on delete cascade on update cascade,
  foreign key (tenant_id, role_b) references role (tenant_id, name) on delete cascade on update cascade
);
alter table role_conflict enable row level security;
alter table role_conflict force row level security;
create policy tenant_isolation on role_conflict using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));

create function user_role_conflict() returns trigger language plpgsql as $$
declare
  c record;
begin
  -- One person's grants are checked one at a time, so two arriving at once cannot both slip past.
  perform 1 from app_user where tenant_id = new.tenant_id and id = new.user_id for no key update;
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

create trigger user_role_conflict before insert or update of role, user_id on user_role
  for each row execute function user_role_conflict();
