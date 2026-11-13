-- The two-version window (crates/kernel/src/window.rs): events and jobs say which build wrote them, and a server takes
-- only those of its own build or an older one, so what a newer build wrote waits for a newer replica. Empty: written
-- before this, or by SQL; anyone takes it.
alter table outbox add column build_date timestamptz;
alter table job add column build_date timestamptz;
