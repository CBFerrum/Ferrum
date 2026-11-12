-- Fields a permission rule neither shows nor accepts (docs/builder/decisions.md BD-40), and whose a rule row is:
-- `packaged` true, still as its package wrote it, so its package's later releases rewrite it; false, an
-- administrator's (their edit makes it theirs), never rewritten; null, written before this release by either, so
-- only its `hide`, which nobody could have set yet, follows the package while it is empty.
alter table role_permission add column hide text[] not null default '{}';
alter table role_permission add column packaged boolean;
