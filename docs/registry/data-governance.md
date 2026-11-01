# Data-governance catalog

What Ferrum keeps about people, and what a request can reach. The table comes from packages'
`personal_data` declarations at the revisions in [release-manifest.yaml](release-manifest.yaml).

## What a request reaches today

`GET`/`DELETE` on a person's data ([privacy.rs](../../crates/kernel/src/privacy.rs)) reaches these
records, matched by email:

| Package | Record | Matched by | Erases |
|---|---|---|---|
| bookings | Booking | `guest_email` | guest_name, guest_email, guest_phone, answers, notes, cancellation_reason |
| campaigns | MarketingContact | `email` | email, first_name, last_name, full_name, consent_source, consent_ip_hash, attributes |
| commerce | ShoppingCart | `email` | email, user_email, phone, contact_name, ship_address_line1, ship_address_line2, ship_city, ship_state, ship_postal_code, bill_address_line1, bill_address_line2, bill_city, bill_state, bill_postal_code, notes |
| commerce | ItemReview | `reviewer_email` | reviewer_name, reviewer_email |
| comms | Communication | `sender` | sender, sender_name, subject, content, text_content, attachments |
| crm | Lead | `email` | first_name, last_name, lead_name, job_title, email, phone, mobile |
| field_service | ServiceRequest | `contact_email` | contact_name, contact_email, contact_phone |
| field_service | ServiceWorkOrder | `contact_email` | contact_name, contact_email, contact_phone |
| forms | FormResponse | `respondent_email` | respondent_email, respondent_name, respondent_user, answers, quiz_detail, ip_hash |
| helpdesk | Ticket | `contact_email` | contact_name, contact_email |
| hr | Employee | `personal_email` | personal_email, cell_number, current_address, permanent_address, date_of_birth, emergency_contact_name, emergency_phone, emergency_relation, bank_name, bank_account_no, ifsc_code, iban |
| hr | JobApplicant | `email_id` | applicant_name, email_id, phone_number |
| contacts | Contact | `email` | first_name, last_name, full_name, designation, department, email, phone, emails, phones, notes |
| contacts | Address | `email` | address_title, address_line1, address_line2, postal_code, email, phone, display |
| parties | Customer | `email` | customer_name, email, phone, tax_id, billing_address, shipping_address |
| parties | Customer | `email` | email, phone |
| parties | Supplier | `email` | email, phone |

## What it does not reach

**Eleven of the 32 packages declare anything at all.** A person's data in the others is not exported
and not erased, and nothing tells the administrator so. Some of those hold no personal data
(`core`, `products`). These certainly do, and come first (`FERRUM-PRIVACY-002`):

| Package | Holds |
|---|---|
| `sign` | Signers' names, emails, signatures, addresses (salted) |
| `lms` | Learners, grades, certificates |
| `pos` | Customers on receipts, loyalty |
| `payroll`, `hr_erp` | Salaries, bank details |
| `drive`, `docs` | Authors, shares, whatever the files say |
| `projects`, `assets` | Assignees, custodians |

Also unreached: attachments on records, chat, notifications, search words, and analytics exports.
Submitted financial documents keep the names they carry, by design.

## By kind of data

| Kind | Retained | Export | Erasure | Legal hold |
|---|---|---|---|---|
| Declared records | Until erased | Yes | Blanked | None |
| Financial documents | Kept for bookkeeping law | Yes | Refused, by design | None |
| Audit trail | Kept; diffs of erased fields redacted, hashes intact | Yes | Redacted | None |
| Events | `event_retention_days` once processed; dead ones stay | No | Purged by age | None |
| Files | Until deleted | Per app | Not reached | None |
| Backups | Until they expire | n/a | Not reached; no journal to reapply (`FERRUM-PRIVACY-006`) | None |
| Login account | Until erased | Yes | Anonymized, deactivated | None |

Retention by category, restriction of processing and legal holds do not exist yet
(`FERRUM-PRIVACY-003`, `FERRUM-PRIVACY-004`, `FERRUM-PRIVACY-005`). Whether what is here meets any
jurisdiction's law is for counsel (`BLOCK-LEGAL-REVIEW`).

**Residency.** One deployment is one region; data stays where the deployment is. External destinations
are the providers an administrator configures. No catalog of them per workspace exists yet.
