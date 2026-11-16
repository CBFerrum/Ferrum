-- Approval before one of an entity's actions runs (`set_approval` with `on: {action: name}`, docs/builder/decisions.md
-- BD-34). `on_action`: the request is on the action named in `action`, run once as its requester when the last step
-- approves. `secret_params`: the names of its secret parameters, each kept in the vault and bound to the request;
-- `params` holds them as `[redacted]`, so neither the request's API nor the audit trail can read them.
alter table approval_request add column on_action boolean not null default false;
alter table approval_request add column secret_params text[] not null default '{}';
