-- Step-up by rule (docs/operations.md, "Asking again before sensitive work"): whether the request that queued a job had
-- proved who its person is: empty when it had (or nobody was asked), `no` when it had not, `machine` for a key or
-- another client that cannot. The job does later only what that request could have done. Empty for jobs queued from
-- inside (no request), as for everything queued before this, which runs as before.
alter table job add column step_up text default nullif(current_setting('ferrum.step_up', true), '');
