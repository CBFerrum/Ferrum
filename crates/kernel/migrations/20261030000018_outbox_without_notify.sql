-- The outbox no longer wakes its dispatcher with NOTIFY from the writing transaction: PostgreSQL
-- serializes the commits of every transaction that notifies (a global lock on the notification
-- queue), so under concurrent writes it capped throughput. Dispatchers poll the pending index
-- instead (every 250 ms when idle).
drop trigger if exists outbox_notify on outbox;
drop function if exists ferrum_outbox_notify();
