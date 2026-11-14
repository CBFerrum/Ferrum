-- A key's own networks (docs/operations.md, "Staff networks"): it works only from an address inside one of them.
-- Empty: from anywhere, as before.
alter table api_key add column networks text[];
