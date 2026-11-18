-- An app that holds a connection (docs/builder/decisions.md BD-82; crate::connections::hold): deleting the connection is
-- refused, naming the app and why, while any hold is there. Turning it off stays possible.
create table conn_hold (
  tenant_id uuid not null,
  conn_id uuid not null,
  app text not null check (length(app) between 1 and 63),
  reason text not null check (length(reason) between 1 and 200),
  primary key (tenant_id, conn_id, app, reason),
  foreign key (tenant_id, conn_id) references conn(tenant_id, id) on delete cascade
);
alter table conn_hold enable row level security;
alter table conn_hold force row level security;
create policy tenant_isolation on conn_hold
  using (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()))
  with check (tenant_id = (select ferrum_tenant()) or (select ferrum_is_system()));
