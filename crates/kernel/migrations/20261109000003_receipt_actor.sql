-- A receipt belongs to the person who claimed its key (FEO-003): anyone else sending the same key is refused,
-- whatever the request. Null for receipts kept before this, and for the system.
alter table idempotency_key add column actor uuid;
