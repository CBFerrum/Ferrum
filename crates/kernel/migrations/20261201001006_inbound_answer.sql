-- A connection's inbound call (TP-3): the address's query as its handler sees it (the verifying parameter taken out),
-- dropped once the call is processed, and the answer its app gave in the caller's request, given again to a repeat.
alter table conn_inbound add column query jsonb, add column answer jsonb;
