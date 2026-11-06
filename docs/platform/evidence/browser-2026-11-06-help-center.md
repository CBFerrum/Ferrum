# A browser on a running server, 6 November 2026: the help center

The help center (FERRUM-UX-003) had not been opened on a running server since it was built. Every
list's and form's Help link goes to `/data/help/{entity}`. On the walk server's earlier build of
Ferrum PR 89 (before 6620da3; the shell from Ferrum-Frontend a76295e), opening one at 20:40 UTC led
to the records frame's first page, "Records: Open a list from an app, or from people". The server read
"help" as the name of an app whose records are shown under `/data/`, found none, and redirected.

## What changed

`shell::DATA_PAGES` (server/src/shell.rs) names the core's own pages under `/data/`: records,
reports, Customize and the help center. Any other first segment is an app's records, as before. A
unit test keeps help among them.

## Checked on the build that has it

On PR 89 at 66e3097, at 20:58 UTC:

| Check | What it showed |
|---|---|
| A record's Help link | `/data/help/Appointment` opened the article: Appointments, a Healthcare master record, its description, how it works ("counts as soon as it is saved", its number APT-{YYYY}-{seq:5}), each field with what it takes and means, the hidden ones left out, and its four actions |
| The index | `/data/help`: every record type the person may read, by module, each with its description |
| Search | "kitchen ticket" found Kitchen Tickets and Orders. The article carried Restaurant 0.2.0's words: "A line voided on the order is struck off it, and a ticket with nothing left to make is Voided", and Voided among the statuses |

## What it found, and what changed

| Finding | Now |
|---|---|
| Links were named by their entity names ("a appointmenttype", "a feevalidity") and fields by their types ("data", "int"), and the article read "A appointment counts…" | Ferrum-Frontend PR 44: the record type's label with its article ("an appointment type"), and words a person uses ("a line of text", "a whole number"). The shell takes it at its next build |
