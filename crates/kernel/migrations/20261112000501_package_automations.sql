-- Automations a package ships (docs/api.md, Automations): installed in each workspace as its defaults, found again by
-- `package` and `package_key`. `base` is the default as last installed: a row that differs from it is the
-- workspace's copy, kept when a release changes the default, which is then flagged.
alter table automation add column package text;
alter table automation add column package_key text;
alter table automation add column base jsonb;
create unique index automation_package on automation (tenant_id, package, package_key) where package is not null;
