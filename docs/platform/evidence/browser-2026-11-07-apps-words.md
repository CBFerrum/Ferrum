# A browser on a running server, 7 November 2026, late: apps' own words, a once-shown link, English right to left, a case gone overdue

The walk server was rebuilt on this PR's lock: every app at its main, each app's interface built
with Ferrum-Frontend 1f5981d (PR 49), on Ferrum main with PR 97 and this PR's fix to it.

The machine restarted at 17:45 UTC, and the walk server's secret key went with the scratch folder
it was kept in. The server was started with a new one, so secrets sealed before can no longer be
opened (webhook secrets and the like). None of these walks read one. The workspaces, their records
and their files (`ferrum-otel/storage`) were untouched, and the session signed in before the
restart still held.

Workspace walk, signed in as its administrator, from 18:03 UTC; then workspace walk-intranet
(Asia/Kolkata).

## What was walked

| Journey | What happened |
|---|---|
| Apps' own words (FERRUM-UX-006) | `GET /api/shell/words` listed 30 of the workspace's apps with the words their interfaces give `t()`: Bookings 232, Mail, Chat and Calendar 337, Drive 204, ERP 248, Learn 312, Sign 243 and so on. Admin, Interface words offered them under "Words of". In Arabic, Drive's "Upload" had no translation. The administrator wrote رفع for it, and the button said "Save 1 word". With their own language set to العربية, Drive's Upload button read رفع, and its New button the interface's own جديد. The word was removed again afterwards, and the language set back |
| English left untranslated, right to left | Field Service's dispatch board, in Arabic, read "Every open job has a time." with its full stop at its end, and "Find a job…" with its ellipsis at its end. Before Ferrum-Frontend PR 49 they read ".Every open job has a time" and "...Find a job" |
| A link shown once | Share It Outside on the Harbour hills asset, in Arabic, for print until 30 November, opened a dialog: the server's note ("Shown once: copy it now"), the link in a read-only box under الرابط, laid out left to right, and the buttons نسخ (Copy) and تم (Done). Before PR 49 the page said only "Share It Outside: done", and the link went no further than the request |
| A case gone overdue (FERRUM-INDUSTRY-010) | CASE-2026-00002 in walk-intranet was opened and due on 7 November. At 18:11 UTC (23:41 in Kolkata) it was Open and not overdue. At 18:31 UTC (00:01 on 8 November in Kolkata) it was overdue: the server's upkeep, which runs each minute, had marked it at 18:30:11 UTC, eleven seconds after the workspace's day turned. CASE-2026-00001, resolved, stayed as it was |

## What it found

| Finding | Now |
|---|---|
| `/api/shell/words` answered 503 when one app's bundle was missing from storage: "board", a test app installed days ago from a server started in another checkout, whose storage stayed there. That one app emptied the whole list | Fixed in this PR: an app whose words cannot be read is left out, with a warning in the log, and the rest are listed |
| Sizes and counts an app builds in its own code ("3 items", "28 KB") still reorder right to left: they do not go through `t()` | Recorded |

## What it did not cover

- Hindi, and apps' own words with built-in translations: the apps carry none, so their words are
  English until a workspace writes its own.
- The case was read from the database, in a read-only transaction under the server's own system
  setting, as its upkeep reads it. The person signed in to walk-intranet in this browser is a
  member without a case officer's role, and the passwords of that workspace's other people went
  with the scratch folder.
