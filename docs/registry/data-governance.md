# Data-governance catalog

What Ferrum keeps about people, and what a request can reach. The table comes from packages'
`personal_data` declarations at the revisions in [release-manifest.yaml](release-manifest.yaml).

## What a request reaches today

A request ([privacy.rs](../../crates/kernel/src/privacy.rs)) finds a person under every address they
have had, and reaches the records declared below, the files attached to them and their login
account. Records marked *retained* are exported and left, and the answer says why. Installed
packages that declare nothing are named in every answer as `not_searched`.

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

The ten apps' declarations for Sign, Learn, POS, Payroll, HR, Drive, Projects, Assets, Commerce and
Chat are on local branches `privacy/declarations`, waiting for the platform's `retain` to be pushed
first; until then those apps are in `not_searched`.

## What it does not reach

Data known only by name or phone, search words, analytics exports, and comments' text, which stays
under an anonymized author. Submitted financial documents keep the names they carry, by design.

## By kind of data

| Kind | Retained | Export | Erasure | Legal hold |
|---|---|---|---|---|
| Declared records | Until erased | Yes | Blanked | Holds on the person or record |
| Retained records | As their package says | Yes | Left, with the reason given | Holds on the person or record |
| Financial documents | Kept for bookkeeping law | Yes | Refused, by design | Holds on the record |
| Audit trail | Kept; diffs of erased fields redacted, hashes intact | Yes | Redacted | None |
| Events | `event_retention_days` once processed; dead ones stay | No | Purged by age | None |
| Files attached to reached records | Until deleted | Named | Deleted, from storage too | Holds on the record |
| Backups | Until they expire | n/a | Reapplied after a restore from the journal | n/a |
| Login account | Until erased | Yes | Anonymized, deactivated, what hangs off it deleted | Holds on the person |

Retention by category and restriction of processing do not exist yet (`FERRUM-PRIVACY-003`,
`FERRUM-PRIVACY-005`). Whether what is here, or what a package says it retains, meets any
jurisdiction's law is for counsel (`BLOCK-LEGAL-REVIEW`).

**Residency.** One deployment is one region; data stays where the deployment is. External destinations
are the providers an administrator configures. No catalog of them per workspace exists yet.
