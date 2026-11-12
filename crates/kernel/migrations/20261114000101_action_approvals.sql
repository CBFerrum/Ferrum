-- Approval before one of an entity's actions runs (`set_approval` with `on: {action: name}`, docs/builder/decisions.md
-- BD-34). `on_action`: the request is on the action named in `action`, run once as its requester when the last step
-- approves. `sealed`: its secret parameters, each sealed with the server's key and bound to the request; `params`
-- holds them as `[redacted]`, so neither the request's API nor the audit trail can read them.
alter table approval_request add column on_action boolean not null default false;
alter table approval_request add column sealed jsonb;
