-- Sending the audit log elsewhere (docs/operations.md): what kind of event it is, decided where it is written
-- (`sign_in`, `administration` by the kernel's own changes, `records` by a record's). Empty for events written
-- before this, and by a build from before it: those are told apart by name, as before.
alter table audit_event add column category text;
