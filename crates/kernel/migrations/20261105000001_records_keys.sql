-- A records key (records:Customer=read;SalesInvoice=read+create): its owner's rights narrowed to
-- the entities and actions it names, on the records API only (crate::auth::records_scope).
alter table api_key drop constraint api_key_scope_check;
alter table api_key add constraint api_key_scope_check check (
  scope in ('dav', 'scim', 'support') or scope ~ '^records:[A-Za-z0-9_]+=[a-z+]+(;[A-Za-z0-9_]+=[a-z+]+)*$'
);
