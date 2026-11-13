-- The two-version window (crates/kernel/src/window.rs): events and jobs say which build wrote them, and a server takes
-- only those of its own build or an older one, so what a newer build wrote waits for a newer replica. Empty: written
-- before this, or by SQL; anyone takes it.
alter table outbox add column build_date timestamptz;
alter table job add column build_date timestamptz;
-- Events of a newer build still waiting, by workspace and priority: what an older build looks for before it takes one
-- (window::NOT_YET). Empty while no newer build has written any.
create index outbox_newer on outbox (tenant_id, priority, build_date) where processed_at is null and not dead and build_date is not null;
