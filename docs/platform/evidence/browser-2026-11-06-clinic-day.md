# A browser on a running server, 6 November 2026: a clinic's day, from setting up to a follow-up

Healthcare (FERRUM-INDUSTRY-005) had run only inside its own suite, through the API. This walk took
a clinic's day through the pages, on the walk server: a debug build of Ferrum PR 88 on one machine,
with its own database (`ferrum_walk`), the shell from Ferrum-Frontend 3d36821, and Healthcare 0.1.6
as pinned. The workspace's clock is UTC and the browser's was India's. It ran from 08:43 to 09:10 UTC.

One person, the workspace's administrator, worked as the clinic's manager and at its reception.

## What was walked

| Step | What it showed |
|---|---|
| Healthcare installed | From the App Manager's Store, built into the server; its home, "The day", with a date and "New appointment" |
| Setting up | Made on their pages: a service item for the consultation (CONSULT), Healthcare Settings (the company, the item a visit is billed with, one follow-up within 7 days), a consent form, "Consent to treatment" at revision 1, an appointment type, "Consultation" (30 minutes, needing that consent), a practitioner, Dr Amara Okafor (fee 60, Fridays 09:00 to 17:00), and a patient, Sofia Lind |
| Booking | A consultation for Sofia Lind with Dr Okafor at 14:00, APT-2026-00001. The day's page showed it under her name in her practitioner's column |
| Consent first | Check In was refused: "the patient has not consented to Consent to treatment, version 1". A Patient Consent was recorded on its page, version 1 taken from the form and its time set by the server. Check In then went through |
| The visit billed | Complete the Visit made a draft sales invoice to a customer made from the patient: Consultation, 1 × 60. It also made a Fee Validity covering one follow-up with the same practitioner until 13 November |
| Hours and clashes | Booked through the API from the signed-in page: 18:00 was refused ("Dr Amara Okafor does not see patients on Fridays from 18:00 to 18:30"); 14:15 was taken, overlapping only the visit already completed; 14:20 was then refused ("Dr Amara Okafor is booked then, under APT-2026-00002") |
| The follow-up | APT-2026-00002 checked in and completed on its page: covered by the Fee Validity, which had no follow-ups left after it, and no second invoice. The day's page listed both visits as Completed |

## What it found, and what changed

| Finding | Now |
|---|---|
| The home page's day was UTC's calendar day (`new Date().toISOString()`), not the workspace's: an evening reception west of Greenwich, or an early one east of it, would see the wrong day | Healthcare 0.1.7 (ferrum-app-healthcare PR 2): `todayIso`, the workspace's day as the server counts it. The same mistake was in Commerce, the Control Plane, Digital Assets, Property and Retail, fixed in their rebuilds on Ferrum-Frontend a76295e, and in Logistics, told to the session working on it |
| Patients' names on the day's page came from a list of at most 500 patients, so a larger clinic would see blank names | Healthcare 0.1.7: from each appointment's own link title |
| An empty clinic showed a bare heading, with nothing saying where to start | Healthcare 0.1.7: "No practitioners yet", saying to add a practitioner with their hours and an appointment type, and linking to it. A practitioner with nothing booked says "Nothing booked." |
| A completed visit's patient, practitioner, type, date and time looked editable. The server refused the change on save ("a visit under way or over keeps its patient, practitioner and time"). Its minutes past midnight, an internal check, were shown | Healthcare 0.1.7: read-only on the page once the visit is checked in, completed, cancelled or missed; the minutes are hidden. A new appointment is for today unless another day is chosen |
| The follow-up's "Covered By" read as the patient's id, because a Fee Validity is headed by its patient, which is a link. Every record headed by a link (29 entities across the apps, such as Lease by its unit and Check-In by its key result) was named by an id where something linked to it, in notifications and in search | Ferrum (this branch): a record headed by a link is named, and found by search, by the linked record's title, at most two steps on, when that entity is open (`store::title_text`). Lists and reads of such entities always carry `_titles`. The kernel's `rules_it` asserts it, including a restricted entity, which is not named through a link |

## What it did not cover

- Practitioners as users: the walk ran as the administrator, so a practitioner seeing only their own patients was not walked. The suite tests it.
- Cancelling, a patient who did not come, a withdrawn consent and a new form revision. The suite tests these.
- Submitting the invoice and taking the payment. That is ERP's billing, walked in the order-to-cash journey.
