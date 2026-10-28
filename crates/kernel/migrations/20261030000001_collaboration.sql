-- Collaboration on any record: comments, assignments (to-dos), tags and followers. Access follows
-- the record itself (checked by the kernel): whoever can read a record can see and add these.
create table record_comment (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  entity text not null,
  record_id uuid not null,
  body text not null check (length(body) between 1 and 20000),
  created_by uuid not null,
  created_at timestamptz not null default now(),
  edited_at timestamptz,
  primary key (tenant_id, id)
);
create index record_comment_record on record_comment (tenant_id, entity, record_id, created_at);

create table record_assignment (
  tenant_id uuid not null references tenant(id) on delete cascade,
  id uuid not null,
  entity text not null,
  record_id uuid not null,
  user_id uuid not null,
  assigned_by uuid not null,
  note text check (length(note) <= 2000),
  due_date date,
  priority text not null default 'Medium' check (priority in ('Low', 'Medium', 'High')),
  status text not null default 'Open' check (status in ('Open', 'Done', 'Cancelled')),
  created_at timestamptz not null default now(),
  closed_at timestamptz,
  closed_by uuid,
  primary key (tenant_id, id),
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);
-- One open assignment per user and record.
create unique index record_assignment_open on record_assignment (tenant_id, entity, record_id, user_id) where status = 'Open';
create index record_assignment_user on record_assignment (tenant_id, user_id, status, due_date);
create index record_assignment_record on record_assignment (tenant_id, entity, record_id);

create table record_tag (
  tenant_id uuid not null references tenant(id) on delete cascade,
  entity text not null,
  record_id uuid not null,
  tag text not null check (length(tag) between 1 and 60),
  created_by uuid,
  created_at timestamptz not null default now(),
  primary key (tenant_id, entity, record_id, tag)
);
create index record_tag_tag on record_tag (tenant_id, entity, tag);

create table record_follower (
  tenant_id uuid not null references tenant(id) on delete cascade,
  entity text not null,
  record_id uuid not null,
  user_id uuid not null,
  created_at timestamptz not null default now(),
  primary key (tenant_id, entity, record_id, user_id),
  foreign key (tenant_id, user_id) references app_user (tenant_id, id) on delete cascade
);
create index record_follower_user on record_follower (tenant_id, user_id);

do $$
declare t text;
begin
  foreach t in array array['record_comment', 'record_assignment', 'record_tag', 'record_follower'] loop
    execute format('alter table %I enable row level security', t);
    execute format('alter table %I force row level security', t);
    execute format(
      'create policy tenant_isolation on %I using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system())) '
      'with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))', t);
  end loop;
end $$;
