-- Who made a connection's call (null for the system), so a person's data report and erasure reach the calls they made
-- (docs/builder/decisions.md BD-35).
alter table conn_call add column user_id uuid;
create index conn_call_user on conn_call (tenant_id, user_id) where user_id is not null;
