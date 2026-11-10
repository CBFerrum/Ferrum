# Portal roles

How a workspace gives its customers, patients, guardians or suppliers their own records on the portal
by configuration, with no app's code ([decision BD-02](decisions.md#bd-02-portal-access-bf1),
BF-PORTAL-001 to 006).

## What a portal role is

A role marked **For the portal** (Administration, Roles & permissions). Portal people hold only such
roles: a member role given to one counts for nothing, and guests hold none. A rule of a portal role
reaches its holder's own records: it may read, write, create and submit them, and run the actions it
names. Or it reads **reference data** whole, to pick from: an entity no package declares as people's
personal data and with no email or phone field (consent forms, appointment types), read and nothing
more. Never delete, cancel, export or import; never every entity (`*`); its holders are never
administrators. The server refuses anything else when the rule or the mark is saved. Reference data is
not listed on the portal's page; it is there for the records' pickers.

## Whose records are whose

A rule reaches a person's own records by one of:

* **Who made them** or the entity's owner fields, as any own-records rule does (`if_owner`): tickets a
  customer opened.
* **A path to their email** (`via`): from the record, at most two links, then optionally a child table,
  ending at an email or text field. `email` on a patient record; `patient.email` on an appointment;
  `guardians.email` on a student; `student.guardians.email` on a report card. Emails compare trimmed
  and in any case. A path is checked when the rule is saved and every time it is used: one whose field
  has gone reaches nothing.

A path is looked up when the record is read or changed, never kept: a guardian taken off a student
loses the student's records at once. Creating or changing a record that would end outside the person's
reach is refused (a patient booking for someone else, or moving their visit to another patient).

A path goes from the record outwards. A record that only links to theirs from elsewhere (an invoice
for a patient's account, where the patient links to the account) is not reached by one: ERP's own
portal page shows a customer's invoices.

## What they see and do

* Fields in a permission group stay hidden unless the rule names the group, in lists, records,
  searches and prints alike.
* A rule may name the entity's **actions** its holders may run on the records it reaches, whatever
  each action's own permission asks: a patient cancels a visit without being able to edit it. The
  action's condition and its app's checks still apply (Healthcare holds a patient to the clinic's
  cancellation window; the clinic's people are not).
* A **workflow transition** is theirs to take only when it names one of their portal roles: one naming
  no roles is for the workspace's members who may change the record, never for an outside person,
  whatever their rule lets them write (an applicant cannot take "Approve" on her own application).
* A record's comments, files, assignments, tags and history are the workspace members' business:
  portal people never reach them, even on their own records.
* Unscoped reports that sum up every record refuse them; Studio's dashboards compute with their rights.

## The portal's pages

Studio gives the portal a **Your records** page (`/portal/records/`): each entity their portal roles
reach, its list, each record's form, and the actions they may run. It shows only to people holding a
portal role that reaches something. Pages apps contribute stay beside it.

## Checking before giving

**Explain access** (Administration, Roles & permissions) answers for a person and a record: whether
they reach it, through which rule, and by which path.

## Apps that ship portal roles

An app's package may ship portal roles as configuration (`portal: true` on a role, `via` and `actions`
on its rules). Healthcare ships **Patient Portal** and Education **Family**; administrators give them to
portal people and may change them like any role.
