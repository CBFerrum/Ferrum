-- Sending the audit log elsewhere (docs/operations.md): each audit event names the transaction that wrote it, so
-- the sender reads events in an order no late commit can step back into. Empty for events written before this
-- (the sender starts after them). A default, not a value from the writer: an older build's writes carry it too.
alter table audit_event add column xid xid8;
alter table audit_event alter column xid set default pg_current_xact_id();
create index audit_event_xid on audit_event (xid, id) where xid is not null;
