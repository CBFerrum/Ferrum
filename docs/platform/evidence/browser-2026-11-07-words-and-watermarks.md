# A browser on a running server, 7 November 2026: interface words, duplicates, renditions, watermarks, grades and pivots

The walk server was rebuilt on this PR's lock: every app at its main, each app's interface built
with Ferrum-Frontend 8a1c2e7 (PRs 47 and 48), on Ferrum main with PRs 94 and 95. All of it was in
workspace walk, signed in as its administrator, from 17:00 UTC.

## What was walked

| Journey | What happened |
|---|---|
| A workspace's own words (FERRUM-UX-006) | Admin, Interface words listed every word the interface asks for, with the built-in Hindi and Arabic beside each. In Arabic, "Save" showed the built-in حفظ. The administrator wrote احفظ الآن in its place and saved (one InterfaceWord record), then set their own language to العربية on the Account page. A customer's record in CRM then read احفظ الآن on its Save button. The word was removed again afterwards, and the language set back to the workspace's |
| The account page in Arabic | Every sentence read in Arabic, among them the sign-in sentence, "Manage your Ferrum account" (إدارة حساب Ferrum الخاص بك) and the time zone's default |
| Apps' own screens right to left | In Arabic: CRM's list and record, Drive's files and Field Service's dispatch board. Navigation sits at the line's start, lists read from the right, and search boxes and fields align to their start. The dispatch board keeps its hours running left to right, with its grid lines under them and its technicians' column at the line's start: it places jobs by coordinates, so the conversion left it alone. These apps' own words are English, and an English sentence left on a right-to-left page shows its full stop at its start (".Every open job has a time") and its numbers moved ("items 2") |
| A duplicate value | A new Permit Type named Market Stall, as an existing one is, was refused with "another Permit Type already has this Type Name" in the form's banner. Before Ferrum PR 94 and Ferrum-Frontend PR 47, it read "Changed by someone else" |
| Renditions as pictures (FERRUM-ECOSYSTEM-012) | The Walk flyer asset, a PDF, showed its thumbnail and its preview (its first page, drawn) as pictures above their download links. Before PR 47, each showed only a download link |
| A watermark on a shared link | A 1200 × 800 picture was drawn in the page and given to Drive's own Upload input. It was made an asset, Harbour hills, submitted and approved. It was then shared outside for Harbour Weekly, for the web, until 30 November, five downloads, with the watermark "PREVIEW Harbour Weekly". The link handed out image/jpeg, 1200 × 800 and 132,709 bytes, rather than the stored 28,888-byte file. It came as an attachment named harbour-hills.jpg, `no-store` and `noindex`, with the words repeated on a slant across the whole picture, legible over both sky and hills |
| Scores as letters, key results graded (FERRUM-ECOSYSTEM-011) | Objectives Settings was made with the scale Letters. Closing "Two EU cells by March" at 50% gave it Score 50%, Scored F and Grade Progress Made (the bands at 40 and 70). Its key result "EU cells in service" (1.5 of 2 cells) got the same: Score 50%, Scored F and Progress Made |
| A dataset that pivots over named values (FERRUM-ECOSYSTEM-010) | A new Dataset was built on its page with Studio's builder: Customers, as a pivot, grouped by Customer Group, its columns from Customer Type. The builder offered the type's options under "Only these values", and with Company and Individual ticked, the "Show up to" box went away. The preview's columns became Customer Group, Company, Individual and Total · Count, in that order, for 12 customers. The server kept the definition with `pivot.values` ["Company", "Individual"] and its lineage "Customer: customer_group, customer_type" |

## What it found

| Finding | Now |
|---|---|
| An action answering with a link shown once (Share It Outside) showed only "Share It Outside: done": the link reached the page and went no further, so a person could not copy it | Fixed in the next Ferrum-Frontend change: such an answer (a link, code, token or secret) opens a dialog with it, a copy button and the server's note. On this build the link was read from the request |
| An English sentence on a right-to-left page shows its punctuation at its start and its numbers moved | Fixed in the next Ferrum-Frontend change: a word without a translation, on a right-to-left page, is set apart as a run of its own direction (U+2068 … U+2069) |
| The pivot's value checkboxes had no accessible names | Fixed in the next Ferrum-Frontend change |
| "Save 1 words" on the Interface words page | Recorded: the page counts words in the plural |

## What it did not cover

- Certifying the pivot dataset and loading it: a Data Steward who does not own it certifies it,
  and Metrics' `datasets_it` covers that (four columns kept, the CSV rows "Retail,1,0,1" and
  "Wholesale,3,0,3").
- Hindi was not walked. Forms, Mail and the other apps not named above were not walked right to
  left: Forms is not installed in this workspace, and this administrator has no mailbox.
