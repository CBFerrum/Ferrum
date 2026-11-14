-- A sandbox: a copy of a live workspace under a new id, which never reaches outside (crates/kernel/src/sandbox.rs;
-- docs/operations.md, "A sandbox of a workspace"). Set in the same transaction as the sandbox's row, before any of its
-- rows arrives, and never changed after: no setting or task turns a sandbox into a live workspace.
-- window: safe, an older build writes none of these columns, so its rows leave them null, which the check allows
alter table tenant add column sandbox_of uuid, add column sandbox_from text, add column sandbox_until date,
  add constraint tenant_sandbox_whole check ((sandbox_of is null) = (sandbox_until is null) and (sandbox_of is null) = (sandbox_from is null));
create index tenant_sandbox_of on tenant (sandbox_of) where sandbox_of is not null;
create trigger tenant_sandbox_kept before update of sandbox_of, sandbox_from on tenant
  for each row when (old.sandbox_of is distinct from new.sandbox_of or old.sandbox_from is distinct from new.sandbox_from)
  execute function ferrum_forbid_change();
