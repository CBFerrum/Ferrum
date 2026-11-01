-- What personal-data requests rest on (docs/registry/data-governance.md).

-- The addresses someone has had: a request made under any of them finds them, and a change of
-- address cannot lose what is filed under the old one. The trigger catches every path that changes
-- an address. Erasure clears these rows with the account.
create table person_identifier (
  tenant_id uuid not null,
  user_id uuid not null,
  email citext not null,
  replaced_at timestamptz not null default now(),
  primary key (tenant_id, user_id, email),
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);
create index person_identifier_email on person_identifier (tenant_id, email);

create function ferrum_person_identifier() returns trigger language plpgsql as $$
begin
  insert into person_identifier (tenant_id, user_id, email) values (old.tenant_id, old.id, old.email) on conflict do nothing;
  return new;
end $$;

create trigger app_user_email_history after update of email on app_user
  for each row when (old.email is distinct from new.email) execute function ferrum_person_identifier();

-- What must not be erased or deleted while a matter is open: everything about a person, or one
-- record. Released holds stay as the record of what was held.
create table legal_hold (
  tenant_id uuid not null references tenant (id) on delete cascade,
  id uuid not null,
  email citext,
  entity text,
  record_id uuid,
  reason text not null check (length(reason) between 1 and 500),
  created_by uuid not null,
  created_at timestamptz not null default now(),
  released_at timestamptz,
  released_by uuid,
  primary key (tenant_id, id),
  check ((email is not null) <> (record_id is not null)),
  check ((entity is null) = (record_id is null))
);
create index legal_hold_active on legal_hold (tenant_id, entity, record_id) where released_at is null;

do $$
declare t text;
begin
  foreach t in array array['person_identifier', 'legal_hold'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format('create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
      'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
