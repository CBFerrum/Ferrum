-- An app installed but waiting for its required resources: what it waits for, until it has it.
alter table tenant_app add column waiting_for text[];
