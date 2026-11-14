-- Whose addresses the kept hosts were when that workspace moved away (`finish_move`) rather than being deleted: it may
-- come back, and takes back those it holds again; every other workspace, one moving in among them, is refused them until
-- `until` (crates/kernel/src/workspace_move.rs). Empty for a deleted workspace, which never comes back, so its id is not
-- kept. Not `tenant_id` and no reference to `tenant`, so nothing that deletes, copies or moves a workspace's rows reaches it.
alter table host_quarantine add column workspace uuid;
