-- Comments and files shared with outside people (docs/builder/decisions.md BD-22). Nothing written before
-- is shared: a portal person sees only what staff mark from now on, and what outside people write themselves.
alter table record_comment add column shared boolean not null default false;
alter table file_object add column shared boolean not null default false;
-- What a portal role's rule lets its holders see of a record's conversation: `read` the shared comments and
-- files, `add` those and their own. Null: nothing.
alter table role_permission add column shared text check (shared is null or shared in ('read', 'add'));
