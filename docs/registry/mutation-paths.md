# Mutation paths outside the document pipeline

FERRUM-PLATFORM-011 asks that every path that changes data keeps the same integrity. The document
pipeline (`Kernel::insert`, `update`, `submit`, `cancel`, `delete`, `run_action`, `transition`)
validates, runs hooks, versions and audits. This review lists what writes entity tables without
it: the statements in the apps compiled in at the revisions in
[release-manifest.yaml](release-manifest.yaml) that `update`, `insert into` or `delete from` an
entity table (`e_*`) directly. It was made on 5 November 2026: each statement classified by the
function it is in, and those that share, approve, consent or delete read in full for what they
record. Nobody independent of the code has reviewed it.

`scripts/mutation_paths.py` keeps it true (FEO-010): CI fails when an app pinned here has a statement
this table does not list, or the table lists one the code no longer has, until a person reads the new
one and gives it a kind (`--write` rewrites the table from the code and keeps the kinds given). Ten
were added on 9 November, each read: Campaigns' second update when a batch is handed over and its
adoption of opt-in group members, Comms' withholding, DAM's download count on a shared link, Drive's
departure outcomes and a workspace link's owner, Edge's condition figures, and Learn's course
progress after a lesson changes.

## What holds on every path

- **Workspaces stay apart.** Every entity table forces row-level security: a statement in one
  workspace's transaction reads and writes that workspace's rows only, whatever its SQL says.
- **Held records stay.** A record under a legal hold is not deleted by any statement: every entity
  table has the `ferrum_held` guard (migration `20261105000006`).
- **Actions are on the record.** The kernel audits every document action (`action:<name>`, with
  its parameters) before its handler runs, so what an action's handler then writes directly is
  under that entry.

## The 106 statements, by kind

| Kind | How many | What keeps them right |
|---|---|---|
| Engine work: delivery and send statuses, counts, queues, upkeep (expiry, overdue, closing) | 67 | Consequences of work the app does, like the kernel's own `set_values_tx`: values the app computes, no person's input; not audited one by one, as derived values are not |
| Consequence of an audited document operation (a hook of submit, cancel, save or an action) | 20 | The operation that runs the hook is on the record |
| A person's action on a public link: opens and clicks, subscribe, confirm, unsubscribe, viewing a shared file | 11 | The record they change carries its evidence (the consent's source and time, the link's views) |
| Records its own audit or activity entry | 8 | Drive's activity log ("Link Created", "Link Revoked", "Shared With Guest", "Share Expired", "Delete"), Edge's and MES's audit entries |

By app: Campaigns 34, Drive 23, Comms 11, ERP 10, Commerce 5, POS 5, Projects 5, Edge 4, HR 3, LMS 2,
and one each in Bookings, DAM, Field Service and Forms.

## Findings

- Drive removes a leaver's shares and prunes old file versions without an activity entry; stars
  are a person's own. Removing a leaver's shares answers the platform's event for their leaving, which is audited.
- Campaigns' suppression list keeps who asked not to be mailed and when as rows; a suppression is
  lifted only by that person confirming a subscription, whose consent is recorded on their contact.
- No statement bypasses workspace isolation or legal holds: the database enforces both.
- **Versions, checked statement by statement on 6 November.** Of the 79 statements that `update`
  an entity table, 14 move the record's `version` and 65 do not. Neither is a fault: the kernel's
  own engine write (`set_values_tx`) leaves the version as it is too, since a derived value (a
  status, a count, an outstanding amount) is not a person's edit. The version is what a person's
  edit is checked against (`If-Match`), and that edit carries only the fields they changed, which
  are never these; no read answers `If-None-Match` from it, so nothing can be served stale.
- **Public links, read one by one on 6 November.** Each of the 10 checks a token the workspace
  signed (HMAC with its own key) or a link's hashed token, scoped to one record, before it writes:
  Campaigns' opens and clicks count against the recipient the token names, and a click redirects
  only to a link of that email's own content; an unsubscribe shows a page on GET (link scanners only
  GET) and acts on POST, as Comms' does after verifying its signed address; a subscription counts
  after the form's own checks, and confirming needs a signed, expiring token; Drive's links refuse
  revoked or expired ones, cap a password's body, lock after 10 wrong passwords for 15 minutes, and
  claim each download under the link's row lock so `max_downloads` holds.

## The statements

| App | Where | Writes | In | Kind |
|---|---|---|---|---|
| bookings | `src/departures.rs:21` | `update e_booking_staff` | `on_event` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/campaign.rs:218` | `update e_campaign_recipient` | `on_cancel` | consequence of an audited document operation (hook) |
| campaigns | `src/campaign.rs:382` | `update e_email_campaign` | `record_error` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/campaign.rs:454` | `insert into e_campaign_recipient` | `start` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/campaign.rs:525` | `update e_campaign_recipient` | `send_batch` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/campaign.rs:552` | `update e_campaign_recipient` | `send_batch` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/campaign.rs:609` | `update e_campaign_recipient` | `skip` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/campaign.rs:672` | `update e_campaign_recipient` | `choose_winners` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/campaign.rs:693` | `update e_email_campaign` | `finish` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/campaign.rs:709` | `update e_email_campaign` | `refresh_stats` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/contact.rs:280` | `insert into e_email_suppression` | `suppress` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/contact.rs:295` | `delete from e_email_suppression` | `lift` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/contact.rs:306` | `update e_email_group_member` | `leave_groups` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/delivery.rs:50` | `update e_campaign_recipient` | `sync` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/delivery.rs:59` | `update e_campaign_recipient` | `sync` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/delivery.rs:80` | `update e_campaign_recipient` | `apply` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/delivery.rs:143` | `update e_marketing_contact` | `engaged` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/import.rs:39` | `insert into e_marketing_contact` | `push_insert` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/import.rs:60` | `insert into e_email_suppression` | `push_insert` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/import.rs:138` | `update e_marketing_contact` | `run` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/import.rs:164` | `insert into e_marketing_contact` | `adopt_group_members` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/journey.rs:205` | `update e_journey_enrollment` | `action` | consequence of an audited document operation (hook) |
| campaigns | `src/journey.rs:382` | `insert into e_journey_enrollment` | `push_enroll` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/journey.rs:502` | `update e_journey_enrollment` | `leave` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/journey.rs:513` | `update e_journey_enrollment` | `leave` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/journey.rs:618` | `insert into e_campaign_recipient` | `advance` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/journey.rs:639` | `update e_campaign_recipient` | `advance` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/journey.rs:649` | `update e_campaign_recipient` | `advance` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/journey.rs:763` | `update e_journey_enrollment` | `advance` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/journey.rs:785` | `update e_marketing_journey` | `refresh_counts` | engine work: statuses, counts, queues, upkeep |
| campaigns | `src/public.rs:164` | `update e_campaign_recipient` | `record_open` | a person's action on a public link (tracking, consent, viewing) |
| campaigns | `src/public.rs:197` | `update e_campaign_recipient` | `click` | a person's action on a public link (tracking, consent, viewing) |
| campaigns | `src/public.rs:243` | `update e_campaign_recipient` | `unsubscribe` | a person's action on a public link (tracking, consent, viewing) |
| campaigns | `src/public.rs:432` | `update e_subscription_form` | `subscribe` | a person's action on a public link (tracking, consent, viewing) |
| campaigns | `src/public.rs:521` | `update e_subscription_form` | `confirm` | a person's action on a public link (tracking, consent, viewing) |
| commerce | `src/coupon.rs:147` | `update e_coupon_code` | `redeem` | engine work: statuses, counts, queues, upkeep |
| commerce | `src/coupon.rs:163` | `update e_coupon_code` | `release` | engine work: statuses, counts, queues, upkeep |
| commerce | `src/payment.rs:597` | `update e_payment_request` | `on_cancel` | consequence of an audited document operation (hook) |
| commerce | `src/upkeep.rs:133` | `update e_shopping_cart` | `carts` | engine work: statuses, counts, queues, upkeep |
| commerce | `src/upkeep.rs:149` | `update e_shopping_cart` | `carts` | engine work: statuses, counts, queues, upkeep |
| comms | `src/chat.rs:741` | `update e_chat_channel` | `post` | engine work: statuses, counts, queues, upkeep |
| comms | `src/inbound.rs:501` | `update e_email_account` | `poll_account` | engine work: statuses, counts, queues, upkeep |
| comms | `src/inbound.rs:505` | `update e_email_account` | `poll_account` | engine work: statuses, counts, queues, upkeep |
| comms | `src/inbound.rs:551` | `update e_email_account` | `poll_account` | engine work: statuses, counts, queues, upkeep |
| comms | `src/mail.rs:577` | `update e_communication` | `deliver` | engine work: statuses, counts, queues, upkeep |
| comms | `src/mail.rs:683` | `update e_communication` | `withhold` | engine work: statuses, counts, queues, upkeep |
| comms | `src/mail.rs:714` | `update e_communication` | `record` | engine work: statuses, counts, queues, upkeep |
| comms | `src/mail.rs:757` | `update e_communication` | `retry_due` | engine work: statuses, counts, queues, upkeep |
| comms | `src/newsletter.rs:123` | `update e_newsletter` | `run` | engine work: statuses, counts, queues, upkeep |
| comms | `src/newsletter.rs:147` | `update e_newsletter` | `update_counts` | engine work: statuses, counts, queues, upkeep |
| comms | `src/newsletter.rs:405` | `update e_email_group_member` | `unsubscribe` | a person's action on a public link (tracking, consent, viewing) |
| dam | `src/sharing.rs:139` | `update e_asset_share` | `open` | a person's action on a public link (tracking, consent, viewing) |
| drive | `src/files.rs:364` | `delete from e_drive_file_version` | `prune` | engine work: statuses, counts, queues, upkeep |
| drive | `src/leaver.rs:39` | `update e_drive_file` | `on_event` | engine work: statuses, counts, queues, upkeep |
| drive | `src/leaver.rs:44` | `delete from e_drive_share` | `on_event` | engine work: statuses, counts, queues, upkeep |
| drive | `src/leaver.rs:53` | `update e_drive_link` | `on_event` | engine work: statuses, counts, queues, upkeep |
| drive | `src/leaver.rs:65` | `update e_drive_link` | `on_event` | engine work: statuses, counts, queues, upkeep |
| drive | `src/leaver.rs:76` | `update e_drive_link` | `on_event` | engine work: statuses, counts, queues, upkeep |
| drive | `src/leaver.rs:88` | `update e_drive_link` | `on_event` | engine work: statuses, counts, queues, upkeep |
| drive | `src/leaver.rs:127` | `update e_drive_link` | `on_event` | engine work: statuses, counts, queues, upkeep |
| drive | `src/ops.rs:1141` | `delete from e_drive_file` | `purge` | records its own audit or activity entry |
| drive | `src/ops.rs:1146` | `delete from e_drive_folder` | `purge` | records its own audit or activity entry |
| drive | `src/ops.rs:1186` | `insert into e_drive_star` | `star` | engine work: statuses, counts, queues, upkeep |
| drive | `src/ops.rs:1197` | `delete from e_drive_star` | `star` | engine work: statuses, counts, queues, upkeep |
| drive | `src/ops.rs:1300` | `update e_drive_comment` | `resolve_comment` | records its own audit or activity entry |
| drive | `src/sharing.rs:168` | `update e_drive_link` | `share_with_guest` | records its own audit or activity entry |
| drive | `src/sharing.rs:229` | `insert into e_drive_link` | `insert_link` | engine work: statuses, counts, queues, upkeep |
| drive | `src/sharing.rs:438` | `update e_drive_link` | `create_link` | consequence of an audited document operation (hook) |
| drive | `src/sharing.rs:480` | `update e_drive_link` | `revoke_link` | records its own audit or activity entry |
| drive | `src/sharing.rs:709` | `update e_drive_link` | `view` | a person's action on a public link (tracking, consent, viewing) |
| drive | `src/sharing.rs:739` | `update e_drive_link` | `unlock` | a person's action on a public link (tracking, consent, viewing) |
| drive | `src/sharing.rs:750` | `update e_drive_link` | `unlock` | a person's action on a public link (tracking, consent, viewing) |
| drive | `src/sharing.rs:793` | `update e_drive_link` | `fetch` | a person's action on a public link (tracking, consent, viewing) |
| drive | `src/tree.rs:587` | `insert into e_drive_activity` | `log` | engine work: statuses, counts, queues, upkeep |
| drive | `src/upkeep.rs:109` | `delete from e_drive_share` | `expire_shares` | engine work: statuses, counts, queues, upkeep |
| edge | `src/facts.rs:280` | `update e_edge_condition` | `condition` | engine work: statuses, counts, queues, upkeep |
| edge | `src/reports.rs:220` | `update e_edge_report_schedule` | `noted` | engine work: statuses, counts, queues, upkeep |
| edge | `src/site.rs:374` | `update e_device_definition` | `allow_joining` | records its own audit or activity entry |
| edge | `src/site.rs:477` | `update e_edge_dashboard` | `set_share` | records its own audit or activity entry |
| erp | `addons/finance/src/bank.rs:77` | `update e_bank_account` | `after_save` | consequence of an audited document operation (hook) |
| erp | `addons/finance/src/dunning.rs:100` | `update e_dunning_type` | `after_save` | consequence of an audited document operation (hook) |
| erp | `addons/finance/src/dunning.rs:395` | `update e_dunning` | `upkeep` | engine work: statuses, counts, queues, upkeep |
| erp | `addons/finance/src/dunning.rs:402` | `update e_dunning` | `upkeep` | engine work: statuses, counts, queues, upkeep |
| erp | `addons/mes/src/runs.rs:745` | `update e_production_revision` | `approve` | records its own audit or activity entry |
| erp | `addons/quality/src/lib.rs:229` | `update e_inspection_plan` | `approve_plan` | consequence of an audited document operation (hook) |
| erp | `addons/wms/src/counts.rs:38` | `update e_storage_bin` | `start` | engine work: statuses, counts, queues, upkeep |
| erp | `addons/wms/src/counts.rs:210` | `update e_storage_bin` | `unfreeze` | engine work: statuses, counts, queues, upkeep |
| erp | `addons/wms/src/posting.rs:211` | `update e_managed_warehouse` | `start_managing` | engine work: statuses, counts, queues, upkeep |
| erp | `addons/wms/src/tasks.rs:518` | `update e_handling_unit` | `place_unit` | engine work: statuses, counts, queues, upkeep |
| field-service | `src/contract.rs:138` | `update e_service_contract` | `expire` | engine work: statuses, counts, queues, upkeep |
| forms | `src/form.rs:393` | `update e_form_response` | `after_save` | consequence of an audited document operation (hook) |
| hr | `src/attendance.rs:530` | `update e_employee_checkin` | `on_cancel` | consequence of an audited document operation (hook) |
| hr | `src/talent.rs:65` | `delete from e_training_record` | `on_cancel` | consequence of an audited document operation (hook) |
| hr | `src/talent.rs:77` | `update e_job_opening` | `close_openings` | engine work: statuses, counts, queues, upkeep |
| lms | `src/catalog.rs:315` | `update e_lms_course` | `refresh_course_stats` | consequence of an audited document operation (hook) |
| lms | `src/enrollment.rs:398` | `update e_lms_enrollment` | `refresh_course` | consequence of an audited document operation (hook) |
| pos | `src/closing.rs:286` | `update e_pos_invoice` | `on_cancel` | consequence of an audited document operation (hook) |
| pos | `src/closing.rs:465` | `update e_pos_invoice` | `consolidate` | consequence of an audited document operation (hook) |
| pos | `src/loyalty.rs:110` | `update e_loyalty_point_entry` | `consume` | consequence of an audited document operation (hook) |
| pos | `src/loyalty.rs:230` | `update e_loyalty_point_entry` | `on_cancel` | consequence of an audited document operation (hook) |
| pos | `src/loyalty.rs:267` | `update e_loyalty_point_entry` | `expire_points` | engine work: statuses, counts, queues, upkeep |
| projects | `src/billing.rs:344` | `update e_timesheet__time_logs` | `mark_billed` | consequence of an audited document operation (hook) |
| projects | `src/billing.rs:352` | `update e_task` | `mark_billed` | consequence of an audited document operation (hook) |
| projects | `src/billing.rs:378` | `update e_timesheet__time_logs` | `unmark_billed` | consequence of an audited document operation (hook) |
| projects | `src/billing.rs:384` | `update e_task` | `unmark_billed` | consequence of an audited document operation (hook) |
| projects | `src/lib.rs:87` | `update e_task` | `mark_overdue` | engine work: statuses, counts, queues, upkeep |
