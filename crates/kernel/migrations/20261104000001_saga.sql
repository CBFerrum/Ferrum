-- Workflows that cross systems (crates/kernel/src/saga.rs) are jobs that undo what they did when
-- they cannot finish, or are asked to stop: asked, such a job is not dropped where it stands, nor
-- marked cancelled while queued, but run again to undo its steps.
alter table job add column undo_on_cancel boolean not null default false;
