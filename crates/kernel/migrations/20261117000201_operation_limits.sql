-- An operation's own `limit` (docs/builder/decisions.md BD-74): its calls counted in their window apart from the
-- description's, under the operation's name ('' for the description's own).
alter table conn_rate add column operation text not null default '';
alter table conn_rate drop constraint conn_rate_pkey;
alter table conn_rate add primary key (tenant_id, conn_id, operation, slot);
