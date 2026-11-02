-- A command may name what it takes over: a warehouse task, a production run. While a command holding
-- a name can still act, or nobody knows whether it did, no other can be written under that name, so a
-- retry cannot put two intents on one physical thing. Only new commands carry a name: a migration
-- cannot choose between two that are already alive, and a person settles those.
alter table op_command add column exclusive_key text;
create unique index op_command_one_live on op_command (tenant_id, exclusive_key)
  where exclusive_key is not null and state in ('Proposed', 'Authorized', 'Queued', 'Accepted', 'Executing', 'OutcomeUnknown');
