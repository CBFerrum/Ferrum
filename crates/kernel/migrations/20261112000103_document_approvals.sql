-- Approval before a document is submitted or cancelled (`set_approval`, docs/builder/decisions.md BD-19).
-- `document`: the request is on the document pipeline's submit or cancel (its `action`), not a workflow transition.
-- `failed`: approved, but the submit or cancel, done as its requester when the policy was satisfied, was refused
-- (their rights gone, a check of the app's); `failure` says why, and nothing was submitted or cancelled.
alter table approval_request add column document boolean not null default false;
alter table approval_request add column failure text;
alter table approval_request drop constraint approval_request_status_check;
alter table approval_request add constraint approval_request_status_check
  check (status in ('pending', 'approved', 'rejected', 'recalled', 'cancelled', 'failed'));
