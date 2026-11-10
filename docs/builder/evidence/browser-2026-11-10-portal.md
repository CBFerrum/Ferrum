# Portal roles, walked in a browser (10 November 2026)

BF-PORTAL-004 and 005. The server built from branch `feo/bf1-portal-roles` (Ferrum) with the interfaces
of `feo/bf1-portal-roles` (Ferrum-Frontend), on this machine, port 5193, a fresh workspace "Riverside
Clinic" (core, studio, healthcare). Practitioners, two patients (Pia, Ned) and three visits (two
Pia's, one Ned's) were made through `/api/v1` as the administrator. Everything below was done in the
desktop app's browser.

**As the administrator** (Administration, Roles & permissions):

1. A new role "Patients"; a rule on Patient, read, **Theirs through** `email`; the row reads "own only,
   through email".
2. A rule on Appointment, read, through `patient.email`, **May run** "Cancel It": "own only, through
   patient.email, may run cancel_appointment" (since shown by label, "may run Cancel It").
3. **For the portal** ticked: "Patients is for the portal". The delete, cancel, export and import boxes
   of its rules went grey, and "Own records only" was ticked and fixed.
4. Users, New user, kind **Portal person**: the roles offered were the portal role alone (not the
   clinic's member roles). Pia Fernandes was created holding "Patients".

**As Pia** (signed in with the code emailed to her, read from the outbox):

5. `/portal/`: "Your records" beside the apps' pages (invoices, orders, visits).
6. "Your records": Appointments and Patients.
7. Appointments: her two visits, and not Ned's.
8. One of them: its fields read-only, the actions "Cancel It" and "Print" only (not Check In, Complete
   or Did Not Come). The Discussion and Activity tabs showed at first, empty, with buttons the server
   refuses; the record page now leaves them out for portal people (Ferrum-Frontend, same branch).
9. "Cancel It": the visit became Cancelled and the button went (its condition no longer held).
10. Ned's visit opened by its address: "Appointment not found".

**A change saved.** Picking the consent form needed a rule the role could not hold, which led to
reference data (decision BD-02, amended): a portal role may read whole an entity holding no personal
data. With "Consent Form, read" (accepted) and "Practitioner, read" (refused: it keeps an email), and a
rule on Patient Consent (read, create, through `patient.email`):

11. "Your records" listed Patient Consents beside Appointments and Patients, and not Consent Forms.
12. New Patient Consent: the patient picker offered Pia's own record only; the form picker the
    "Treatment" form. Saved with "Given By": "Patient Consent created", version 1, given at the
    moment, read-only afterwards (the role creates; it does not change).

Not walked: the explanation screen for a portal person (tested in `crates/kernel/tests/portal_it.rs`),
a portal person's search, and the same on a phone.
