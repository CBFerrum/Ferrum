-- A pull on a connection each person signs in to runs as each person connected (docs/builder/decisions.md BD-50):
-- one row, one cursor and one failure streak per person. `user_id` is nil for the workspace's own pull.
alter table conn_pull add column user_id uuid not null default '00000000-0000-0000-0000-000000000000';
alter table conn_pull drop constraint conn_pull_pkey;
alter table conn_pull add primary key (tenant_id, conn_id, name, user_id);
