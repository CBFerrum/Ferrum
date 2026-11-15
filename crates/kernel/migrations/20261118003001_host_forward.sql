-- A forwarding address (docs/operations.md, "Addresses of a workspace"): a `shell` address that serves no workspace and
-- sends each page on to the workspace's main address (`/` to its `home` there). One that served before it forwards keeps
-- serving those signed in there until `leaving_until`, as a removed address does. An older build reads none of it and
-- serves the workspace there, as before: nothing protective is lost.
alter table tenant_host add column forward boolean not null default false;
