-- The brand a host shows (docs/builder/decisions.md BD-79): a Brand record of the workspace (core package, e_brand).
-- Null: the workspace's default brand, else the platform's look. Not a foreign key: tenant_host is a cluster table
-- read before any session, and the kernel checks the id when it is set (Kernel::set_host_brand).
alter table tenant_host add column brand uuid;
