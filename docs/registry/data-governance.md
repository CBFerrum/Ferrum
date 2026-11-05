# Data-governance catalog

What Ferrum keeps about people, and what a request can reach. The table comes from packages'
`personal_data` declarations at the revisions in [release-manifest.yaml](release-manifest.yaml).

## What a request reaches today

A request ([privacy.rs](../../crates/kernel/src/privacy.rs)) finds a person under every address they
have had, and reaches the records declared below, the files attached to them and their login
account. Records marked *retained* are exported and left, and the answer says why. Installed
packages that declare nothing are named in every answer as `not_searched`.

| Package | Record | Found by | Erases |
|---|---|---|---|
| contacts | Contact | `email`; `phone` (phone); `full_name` (name) | first_name, last_name, full_name, designation, department, email, phone, emails, phones, notes |
| contacts | Address | `email`; `phone` (phone) | address_title, address_line1, address_line2, postal_code, email, phone, display |
| parties | Customer | `email`; `phone` (phone); `customer_name` (name); where customer_type is Individual | customer_name, email, phone, tax_id, billing_address, shipping_address |
| parties | Customer | `email`; `phone` (phone); where customer_type is Company | email, phone |
| parties | Supplier | `email`; `phone` (phone) | email, phone |
| assets | Asset | `custodian` | custodian |
| assets | AssetMaintenanceLog | `assigned_to` | assigned_to |
| assets | AssetVerification | `verified_by` | verified_by |
| bookings | Booking | `guest_email`; `guest_phone` (phone); `guest_name` (name) | guest_name, guest_email, guest_phone, answers, notes, cancellation_reason |
| campaigns | MarketingContact | `email` | email, first_name, last_name, full_name, consent_source, consent_ip_hash, attributes |
| commerce | ShoppingCart | `email`; `phone` (phone) | email, user_email, phone, contact_name, ship_address_line1, ship_address_line2, ship_city, ship_state, ship_postal_code, bill_address_line1, bill_address_line2, bill_city, bill_state, bill_postal_code, notes |
| commerce | ItemReview | `reviewer_email` | reviewer_name, reviewer_email |
| commerce | PaymentRequest | `payer_email` | Retained: Payment records |
| comms | Communication | `sender` | sender, sender_name, subject, content, text_content, attachments |
| comms | ChatMessage | `sender` | sender, body |
| comms | EmailGroupMember | `email` | email, member_name |
| comms | CalendarFeed | `person` | person |
| crm | Lead | `email`; `phone` (phone); `lead_name` (name) | first_name, last_name, lead_name, job_title, email, phone, mobile |
| drive | DriveSpace | `owner` | Retained: A space needs an owner: hand it over, then ask again |
| drive | DriveFolder | `owner` | Retained: A folder needs an owner: hand it over, then ask again |
| drive | DriveFile | `owner` | Retained: A file needs an owner: hand it over, then ask again |
| drive | DriveFileVersion | `author` | author |
| drive | DriveShare | `email` | email |
| drive | DriveLink | `email` | email |
| drive | DriveComment | `author` | author |
| drive | DriveActivity | `actor` | actor |
| drive | DriveStar | `email` | email |
| education | StudentApplicant | `email`; `applicant_name` (name) | applicant_name, email, date_of_birth, guardian_name, guardian_email, guardian_phone, notes |
| education | Student | `email`; `student_name` (name) | student_name, email, date_of_birth |
| field_service | ServiceRequest | `contact_email`; `contact_phone` (phone) | contact_name, contact_email, contact_phone |
| field_service | ServiceWorkOrder | `contact_email`; `contact_phone` (phone) | contact_name, contact_email, contact_phone |
| field_service | Technician | `user_email`; `phone` (phone); `technician_name` (name) | user_email, phone |
| forms | FormResponse | `respondent_email` | respondent_email, respondent_name, respondent_user, answers, quiz_detail, ip_hash |
| helpdesk | Ticket | `contact_email` | contact_name, contact_email |
| hospitality | Stay | `guest_email`; `guest_phone` (phone); `guest_name` (name) | guest_name, guest_email, guest_phone, notes |
| hospitality | Folio | `guest_email`; `guest_name` (name) | guest_name, guest_email |
| hr | Employee | `personal_email`; `cell_number` (phone); `employee_name` (name) | personal_email, cell_number, current_address, permanent_address, date_of_birth, emergency_contact_name, emergency_phone, emergency_relation, bank_name, bank_account_no, ifsc_code, iban |
| hr | JobApplicant | `email_id`; `phone_number` (phone); `applicant_name` (name) | applicant_name, email_id, phone_number |
| hr | JobOffer | `applicant_email` | applicant_name, applicant_email |
| hr | Attendance | `employee_user` | Retained: Employment records |
| hr | AttendanceRequest | `employee_user` | Retained: Employment records |
| hr | EmployeeCheckin | `employee_user` | Retained: Employment records |
| hr | ShiftAssignment | `employee_user` | Retained: Employment records |
| hr | LeaveAllocation | `employee_user` | Retained: Employment records |
| hr | LeaveApplication | `employee_user` | Retained: Employment records |
| hr | LeaveLedgerEntry | `employee_user` | Retained: Employment records |
| hr | EmployeeSeparation | `employee_user` | Retained: Employment records |
| hr | EmployeePromotion | `employee_user` | Retained: Employment records |
| hr | EmployeeTransfer | `employee_user` | Retained: Employment records |
| hr | Appraisal | `employee_user` | Retained: Employment records |
| payroll | SalarySlip | `employee_user` | Retained: Payroll and tax records |
| payroll | EmployeeTaxExemptionDeclaration | `employee_user` | Retained: Payroll and tax records |
| payroll | LeaveEncashment | `employee_user` | Retained: Payroll and tax records |
| lms | LmsEnrollment | `learner` | learner, learner_name |
| lms | LmsLessonProgress | `learner` | learner |
| lms | LmsPackageProgress | `learner` | learner, cmi |
| lms | LmsQuizAttempt | `learner` | learner, learner_name |
| lms | LmsAssignmentSubmission | `learner` | learner, learner_name, answer, feedback |
| lms | LmsCertificate | `learner` | learner, learner_name |
| lms_payments | LmsCoursePurchase | `learner` | learner, learner_name |
| nonprofit | Member | `email`; `phone` (phone); `member_name` (name) | member_name, email, phone, notes |
| nonprofit | Donor | `email`; `donor_name` (name) | donor_name, email, tax_id |
| nonprofit | Volunteer | `email`; `phone` (phone); `volunteer_name` (name) | volunteer_name, email, phone |
| pos | PosOpeningEntry | `user` | Retained: Cash records of the session they ran |
| pos | PosClosingEntry | `user` | Retained: Cash records of the session they ran |
| projects | Timesheet | `user` | Retained: Billing records |
| projects | ActivityCost | `user` | Retained: Costing records |
| sign | SignEvent | `signer_email` | Retained: Evidence of the signature: erasing it would undo what was signed |
| sign | SignRequest | `sender_email` | Retained: Evidence of the signature: erasing it would undo what was signed |

A declaration finds a person by the address in its first field, under every address they have had,
and by the digits of a phone field where it names one; records that hold no address are matched by
their phone field alone, or through a link to a record found by address or number (a patient's
appointments through the patient). A name field only lists records as possibly
theirs, for someone to judge: nothing is erased for a name. Of the 36 apps compiled in at these
revisions, Construction, Contracts, Control, DAM, Docs, Edge, ERP, Government, GRC, Healthcare,
ITSM, Logistics, Metrics, Objectives, Property, Restaurant, Retail and Social declare nothing, and
are named in every answer as `not_searched` (ERP's customers and suppliers are the platform's
`parties`).

## What it does not reach

Comments' text, which stays under an anonymized author; addresses held only in a child table's rows
(Edge's customer people and report recipients), which a declaration cannot name, since the rows are
not records of their own; and the records of the packages that declare nothing. Submitted financial documents keep the names they carry, by design.

Some things hold nothing to reach: search keeps no words (it runs live over records, and the request
log and traces carry paths without their query strings), and Studio keeps no results (every query
runs live, with the viewer's rights). A workspace's stored export is a copy from before, so an
erasure that reaches anything deletes them all, and an export it lands on while being made starts
again ([export.rs](../../crates/kernel/src/export.rs)).

## By kind of data

| Kind | Retained | Export | Erasure | Legal hold |
|---|---|---|---|---|
| Declared records | Until erased | Yes | Blanked | Holds on the person or record |
| Retained records | As their package says | Yes | Left, with the reason given | Holds on the person or record |
| Financial documents | Kept for bookkeeping law | Yes | Refused, by design | Holds on the record |
| Audit trail | Kept; diffs of erased fields redacted, hashes intact | Yes | Redacted | None |
| Events | `event_retention_days` once processed; dead ones stay | No | Purged by age | None |
| Files attached to reached records | Until deleted | Named | Deleted, from storage too | Holds on the record |
| Workspace exports | 7 days | n/a | Deleted by an erasure; one being made starts again | None |
| Backups | Until they expire | n/a | Reapplied after a restore from the journal | n/a |
| Login account | Until erased | Yes | Anonymized, deactivated, what hangs off it deleted | Holds on the person |

Retention by category and restriction of processing do not exist yet (`FERRUM-PRIVACY-003`,
`FERRUM-PRIVACY-005`). Whether what is here, or what a package says it retains, meets any
jurisdiction's law is for counsel (`BLOCK-LEGAL-REVIEW`).

**Residency.** One deployment is one region; data stays where the deployment is. External destinations
are the providers an administrator configures. No catalog of them per workspace exists yet.
