# Actions declared `permission: read` that are not read-only, reviewed

The rule (docs/guides/05, "Actions any reader presses"): an action needs exactly its declared `permission` on the
record, or a rule naming it. `permission: read` on an action that is not `read_only` (one that may write) is allowed
only when this registry says which of these it is:

- **A**: the presser's own part (their enrolment, feed, acknowledgement or vote, or a tally every reader adds to); the
  handler touches nothing that is someone else's.
- **B**: an outside person's answer; the handler refuses members (`ctx.outside`).
- **C**: its record stays as it is: it reads, or makes or changes other records under the presser's own rights.
- **D**: the handler checks a narrower right itself (a role, Drive's own model, a field right) and changes its own
  record only where the presser's Write scope reaches it.

Anything else declares `write` or stricter. Each row names the test that presses the action as someone it must refuse
(`—`: not written yet). A share or the reporting line presses only `read_only` actions, which run in a read-only
transaction, so none of this widens what they reach. `scripts/read_actions.py` fails on an action missing here, on a
row for an action gone, and on a row naming a test its app does not have; `--strict` also on a row not yet classed
(`?`) or without its test. `--write` adds new actions as `?` and drops gone ones, keeping every reviewed row.

The survey behind the classes (S9 design, 19 November) found in the apps' packages A 19, C 60, D 74, B 0, and 11 that
change under the rule; in ERP's add-ons C 20 and D 3. The nineteen that change (`permission: write`, or a check of
their own) land in their apps (CRM's three in K0-3, the other sixteen in K0-6); each row says so until its app's
commit removes the action from this list or names its test.

| App | Action | Class | Evidence | Narrower right | Test |
|---|---|---|---|---|---|
| bookings | `BookingService.find_slots` | ? |  |  | — |
| bookings | `BookingStaff.calendar_feed` | ? |  |  | — |
| bookings | `BookingStaff.rotate_calendar_feed` | ? |  |  | — |
| campaigns | `MarketingSegment.preview` | ? | changes (T5, K0-6): returns the counts and stores none; test to come `a_reader_previews_without_changing_the_segment` |  | — |
| commerce | `SalesOrder.web_cancel` | ? | changes (T5, K0-6): `permission: write`; test to come `a_reader_cannot_cancel_edit_or_resend_a_web_order` |  | — |
| commerce | `SalesOrder.web_invoice` | ? |  |  | — |
| commerce | `SalesOrder.web_resend_mail` | ? | changes (T5, K0-6): `permission: write`; test to come `a_reader_cannot_cancel_edit_or_resend_a_web_order` |  | — |
| commerce | `SalesOrder.web_ship` | ? |  |  | — |
| commerce | `SalesOrder.web_shopper_cancel` | ? | changes (S9 T5, K0-6): refused unless `ctx.outside`; test to come `a_member_cannot_cancel_as_the_shopper` |  | — |
| commerce | `SalesOrder.web_shopper_return` | ? |  |  | — |
| commerce | `SalesOrder.web_tracking_link` | ? |  |  | — |
| commerce | `SalesOrder.web_update_items` | ? | changes (T5, K0-6): `permission: write`; test to come `a_reader_cannot_cancel_edit_or_resend_a_web_order` |  | — |
| crm | `Lead.make_opportunity` | ? | changes (T5, K0-3): the lead set Qualified as the system after a narrower gate |  | — |
| crm | `Opportunity.make_quotation` | ? |  |  | — |
| crm | `Quotation.customer_accept` | ? | changes (T5, K0-3) |  | — |
| crm | `Quotation.customer_ask` | ? |  |  | — |
| crm | `Quotation.customer_decline` | ? | changes (T5, K0-3) |  | — |
| crm | `Quotation.make_sales_order` | ? |  |  | — |
| drive | `DriveSpace.activity` | ? |  |  | — |
| drive | `DriveSpace.add_comment` | ? |  |  | — |
| drive | `DriveSpace.add_team_member` | ? |  |  | — |
| drive | `DriveSpace.append_to_document` | ? |  |  | — |
| drive | `DriveSpace.check_in` | ? |  |  | — |
| drive | `DriveSpace.check_out` | ? |  |  | — |
| drive | `DriveSpace.copy` | ? |  |  | — |
| drive | `DriveSpace.create_document` | ? |  |  | — |
| drive | `DriveSpace.create_folder` | ? |  |  | — |
| drive | `DriveSpace.create_from_template` | ? |  |  | — |
| drive | `DriveSpace.create_link` | ? |  |  | — |
| drive | `DriveSpace.create_team_folder` | ? |  |  | — |
| drive | `DriveSpace.delete_comment` | ? |  |  | — |
| drive | `DriveSpace.delete_forever` | ? |  |  | — |
| drive | `DriveSpace.delete_template` | ? |  |  | — |
| drive | `DriveSpace.download` | ? |  |  | — |
| drive | `DriveSpace.empty_trash` | ? |  |  | — |
| drive | `DriveSpace.item_info` | ? |  |  | — |
| drive | `DriveSpace.keep_version` | ? |  |  | — |
| drive | `DriveSpace.list_comments` | ? |  |  | — |
| drive | `DriveSpace.list_folder` | ? |  |  | — |
| drive | `DriveSpace.list_links` | ? |  |  | — |
| drive | `DriveSpace.list_shares` | ? |  |  | — |
| drive | `DriveSpace.list_templates` | ? |  |  | — |
| drive | `DriveSpace.list_trash` | ? |  |  | — |
| drive | `DriveSpace.list_versions` | ? |  |  | — |
| drive | `DriveSpace.move` | ? |  |  | — |
| drive | `DriveSpace.read_document` | ? |  |  | — |
| drive | `DriveSpace.recent` | ? |  |  | — |
| drive | `DriveSpace.remove_team_member` | ? |  |  | — |
| drive | `DriveSpace.rename` | ? |  |  | — |
| drive | `DriveSpace.request_access` | ? |  |  | — |
| drive | `DriveSpace.resolve_comment` | ? |  |  | — |
| drive | `DriveSpace.restore` | ? |  |  | — |
| drive | `DriveSpace.restore_version` | ? |  |  | — |
| drive | `DriveSpace.revoke_link` | ? |  |  | — |
| drive | `DriveSpace.save_as_template` | ? |  |  | — |
| drive | `DriveSpace.search` | ? |  |  | — |
| drive | `DriveSpace.set_colour` | ? |  |  | — |
| drive | `DriveSpace.set_team_quota` | ? |  |  | — |
| drive | `DriveSpace.share` | ? |  |  | — |
| drive | `DriveSpace.shared_with_me` | ? |  |  | — |
| drive | `DriveSpace.star` | ? |  |  | — |
| drive | `DriveSpace.starred` | ? |  |  | — |
| drive | `DriveSpace.trash` | ? |  |  | — |
| drive | `DriveSpace.unshare` | ? |  |  | — |
| drive | `DriveSpace.unstar` | ? |  |  | — |
| drive | `DriveSpace.upload` | ? |  |  | — |
| drive | `DriveSpace.upload_version` | ? |  |  | — |
| drive | `DriveSpace.usage` | ? |  |  | — |
| erp | `BankAccount.accept_matches` | ? |  |  | — |
| erp | `BankTransaction.get_suggestions` | ? |  |  | — |
| erp | `CarrierShipment.track` | ? | changes (T5, K0-6): `permission: write`; test to come `a_reader_cannot_refresh_a_shipment_s_tracking` |  | — |
| erp | `DeliveryNote.make_carrier_shipment` | ? |  |  | — |
| erp | `DeliveryNote.make_packing_slip` | ? |  |  | — |
| erp | `DeliveryNote.make_quality_inspections` | ? |  |  | — |
| erp | `DeliveryNote.make_sales_invoice` | ? |  |  | — |
| erp | `DeliveryNote.make_sales_return` | ? |  |  | — |
| erp | `MaterialRequest.make_purchase_order` | ? |  |  | — |
| erp | `MaterialRequest.make_rfq` | ? |  |  | — |
| erp | `MaterialRequest.make_stock_entry` | ? |  |  | — |
| erp | `MaterialRequest.make_work_order` | ? |  |  | — |
| erp | `PickList.make_delivery_note` | ? |  |  | — |
| erp | `PickList.make_stock_entry` | ? |  |  | — |
| erp | `ProductionPlan.make_material_requests` | ? | changes (T5, K0-6): `permission: write`; test to come `a_reader_cannot_make_work_orders_from_a_plan` |  | — |
| erp | `ProductionPlan.make_work_orders` | ? | changes (T5, K0-6): `permission: write`; test to come `a_reader_cannot_make_work_orders_from_a_plan` |  | — |
| erp | `PurchaseInvoice.make_debit_note` | ? |  |  | — |
| erp | `PurchaseInvoice.make_payment` | ? |  |  | — |
| erp | `PurchaseInvoice.make_quality_inspections` | ? |  |  | — |
| erp | `PurchaseInvoice.make_refund` | ? |  |  | — |
| erp | `PurchaseOrder.make_purchase_invoice` | ? |  |  | — |
| erp | `PurchaseOrder.make_purchase_receipt` | ? |  |  | — |
| erp | `PurchaseReceipt.make_carrier_shipment` | ? |  |  | — |
| erp | `PurchaseReceipt.make_purchase_invoice` | ? |  |  | — |
| erp | `PurchaseReceipt.make_purchase_return` | ? |  |  | — |
| erp | `PurchaseReceipt.make_quality_inspections` | ? |  |  | — |
| erp | `PurchaseReceipt.return_rejected` | ? |  |  | — |
| erp | `RequestForQuotation.make_supplier_quotation` | ? |  |  | — |
| erp | `SalesInvoice.make_credit_note` | ? |  |  | — |
| erp | `SalesInvoice.make_debit_note` | ? |  |  | — |
| erp | `SalesInvoice.make_inter_company_invoice` | ? |  |  | — |
| erp | `SalesInvoice.make_payment` | ? |  |  | — |
| erp | `SalesInvoice.make_quality_inspections` | ? |  |  | — |
| erp | `SalesInvoice.make_refund` | ? |  |  | — |
| erp | `SalesOrder.make_delivery_note` | ? |  |  | — |
| erp | `SalesOrder.make_pick_list` | ? |  |  | — |
| erp | `SalesOrder.make_sales_invoice` | ? |  |  | — |
| erp | `StockEntry.make_quality_inspections` | ? |  |  | — |
| erp | `StockEntry.receive_transfer` | ? |  |  | — |
| erp | `StoredValueKind.hold` | ? |  |  | — |
| erp | `StoredValueKind.issue` | ? |  |  | — |
| erp | `StoredValueKind.redeem` | ? |  |  | — |
| erp | `StoredValueKind.refund` | ? |  |  | — |
| erp | `StoredValueKind.release` | ? |  |  | — |
| erp | `StoredValueKind.reverse` | ? |  |  | — |
| erp | `StoredValueKind.transfer` | ? |  |  | — |
| erp | `SubcontractingOrder.make_material_transfer` | ? |  |  | — |
| erp | `SubcontractingOrder.make_purchase_order` | ? | changes (T5, K0-6): `permission: write` |  | — |
| erp | `SubcontractingOrder.make_subcontracting_receipt` | ? |  |  | — |
| erp | `SubcontractingOrder.return_excess_materials` | ? |  |  | — |
| erp | `SupplierQuotation.make_purchase_order` | ? |  |  | — |
| field-service | `ServiceRequest.make_work_order` | ? | changes (T5, K0-6): `permission: write`; test to come `a_reader_cannot_convert_a_request_or_invoice_a_work_order` |  | — |
| field-service | `ServiceRequest.tracking_link` | ? |  |  | — |
| field-service | `ServiceWorkOrder.create_invoice` | ? | changes (T5, K0-6): `permission: write`; test to come `a_reader_cannot_convert_a_request_or_invoice_a_work_order` |  | — |
| field-service | `ServiceWorkOrder.suggest_technicians` | ? |  |  | — |
| grc | `Policy.acknowledge` | ? |  |  | — |
| helpdesk | `KbArticle.mark_helpful` | A | a tally every reader adds to (counting a person more than once is Helpdesk's own matter) |  | — |
| helpdesk | `KbArticle.mark_not_helpful` | A | a tally every reader adds to |  | — |
| helpdesk | `MaintenanceSchedule.make_visit` | ? |  |  | — |
| helpdesk | `SavedReply.used` | A | a tally every reader adds to |  | — |
| helpdesk | `Ticket.rate` | ? | changes (T5, K0-6): only the ticket's raiser; test to come `only_the_raiser_rates_a_ticket` |  | — |
| hr | `Employee.issue_appointment_letter` | ? |  |  | — |
| hr | `Employee.issue_employment_certificate` | ? |  |  | — |
| hr | `Employee.issue_warning_letter` | ? |  |  | — |
| hr | `Employee.request_letter` | ? |  |  | — |
| hr | `EmployeeAdvance.make_payment_entry` | ? |  |  | — |
| hr | `EmployeeAdvance.make_return_entry` | ? |  |  | — |
| hr | `EmployeeSeparation.issue_relieving_letter` | ? |  |  | — |
| hr | `EmployeeSeparation.make_full_and_final` | ? |  |  | — |
| hr | `ExpenseClaim.make_payment_entry` | ? |  |  | — |
| hr | `FullAndFinalStatement.make_payment_entry` | ? |  |  | — |
| hr | `JobOffer.issue_offer_letter` | ? |  |  | — |
| hr | `JobOffer.make_employee` | ? | changes (T5, K0-6): `permission: write`; test to come `a_reader_cannot_make_an_employee_from_an_offer` |  | — |
| hr | `LeavePolicy.accrue` | ? |  |  | — |
| hr | `LeavePolicy.assign_allocations` | ? |  |  | — |
| hr | `PayrollEntry.bank_file` | ? | changes (T5, K0-6): `permission: write`; test to come `a_reader_cannot_write_a_bank_file_onto_a_payroll_run` |  | — |
| hr | `PayrollEntry.make_bank_entry` | ? |  |  | — |
| hr | `SalaryStructureAssignment.issue_salary_certificate` | ? |  |  | — |
| intranet | `Announcement.acknowledge` | ? |  |  | — |
| itsm | `ChangeTemplate.raise_change` | ? |  |  | — |
| itsm | `Incident.confirm_resolution` | ? | changes (T5, K0-6): `permission: write`, IT Self Service's rule names it; test to come `a_change_manager_cannot_close_an_incident_for_its_caller` |  | — |
| itsm | `Incident.reject_resolution` | ? | changes (T5, K0-6): `permission: write`; test to come `a_change_manager_cannot_close_an_incident_for_its_caller` |  | — |
| lms | `LmsCourse.buy` | ? |  |  | — |
| lms | `LmsCourse.discuss` | ? |  |  | — |
| lms | `LmsCourse.follow` | ? |  |  | — |
| lms | `LmsCourse.report_post` | ? |  |  | — |
| lms | `LmsCourseBundle.buy` | ? |  |  | — |
| lms | `LmsEnrollment.complete_lesson` | ? |  |  | — |
| lms | `LmsEnrollment.open_lesson` | ? |  |  | — |
| lms | `LmsEnrollment.outline` | ? |  |  | — |
| lms | `LmsEnrollment.reactivate` | ? |  |  | — |
| lms | `LmsEnrollment.scorm_commit` | ? |  |  | — |
| lms | `LmsEnrollment.start_quiz` | ? |  |  | — |
| lms | `LmsEnrollment.video_progress` | ? |  |  | — |
| lms | `LmsEnrollment.withdraw` | A | the learner's own enrolment |  | — |
| lms | `LmsEnrollmentRule.preview` | ? |  |  | — |
| lms | `LmsLearningPath.join` | ? |  |  | — |
| lms | `LmsQuizAttempt.save_answers` | ? |  |  | — |
| lms | `LmsQuizAttempt.submit_answers` | ? |  |  | — |
| metrics | `Metric.snapshot` | ? |  |  | — |
| projects | `Project.create_from_template` | ? |  |  | — |
| projects | `Project.make_sales_invoice` | ? |  |  | — |
| projects | `Task.make_milestone_invoice` | ? |  |  | — |
| projects | `Timesheet.make_sales_invoice` | ? |  |  | — |
| sign | `SignRequest.verify` | ? |  |  | — |
| sign | `SignTemplate.create_request` | ? |  |  | — |
| social | `SocialBrand.media_from_address` | ? |  |  | — |
| social | `SocialBrand.media_take` | ? |  |  | — |
| social | `SocialBrand.write_new` | ? |  |  | — |
| social | `SocialPost.adapt` | ? |  |  | — |
| social | `SocialPost.describe` | ? |  |  | — |
| social | `SocialPost.hashtags` | ? |  |  | — |
| social | `SocialPost.picture` | ? |  |  | — |
| social | `SocialPost.rewrite` | ? |  |  | — |
| social | `SocialPost.save_as_template` | ? |  |  | — |
| social | `SocialPost.thread` | ? |  |  | — |
| social | `SocialPost.write` | ? |  |  | — |
| social | `SocialSet.use_set` | ? |  |  | — |
| social | `SocialTemplate.use_template` | ? |  |  | — |
| studio | `Report.run` | ? |  |  | — |
