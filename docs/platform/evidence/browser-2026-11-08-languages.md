# A browser on a running server, 8 November 2026: every app's own words in Arabic and Hindi, a SCORM package, a guest's series, an erasure through a restore

The walk server was rebuilt on Ferrum main 6d65c1f (PR 100). Every app is at its main, and each
app's interface is built with Ferrum-Frontend 5b22e7b (PRs 51 to 53). The server's own pages and
the bar are translated (PR 99). The binary is a debug build of the server crate. The database is
the walk's own (`ferrum_walk`), and object storage is `ferrum-otel/storage`.

Workspace `walk`, signed in as its administrator, from 22:28 UTC on 7 November (03:58 in Kolkata
on the 8th). The administrator's own language was Arabic, then Hindi for the second part, then
Arabic again.

## What was walked

| Journey | What happened |
|---|---|
| The shell in Arabic (FERRUM-UX-006) | Home fetched the Arabic catalog (`ar-*.js`, only for this language) and drew «صباح الخير, Walk» on its first paint. The page set `lang="ar" dir="rtl"`. The entry is one script: since PR 52 there is no top-level await, so nothing is split into preloaded chunks. |
| Drive in Arabic | Drive fetched the Frontend's catalog and its own (`ui/src/locales/ar.json`). Its entry was one file with no preloads (178.7 KB of 200 KB). Navigation, toolbar, columns, sort and «3 عناصر» read Arabic. Dates were written in Arabic («07‏/11‏/2026، 5:04 م UTC»). Sizes are isolated, so «28 KB», «79 B» and «642 B» keep their order. The sidebar and actions sat on the right. |
| CRM in Arabic | The pipeline's title (خط المبيعات), the shell's menus (الرئيسية، الإضافات، الإدارة، تخصيص) and the view's words read Arabic. Entity and report names in its navigation (Leads, Opportunities, Sales Pipeline) come from CRM's models and read English, as do the stage names, which are records. |
| Operations in Arabic | `/ops/` first answered with the server's own page, in Arabic: «لا شيء مثبّت هنا». After the `ops` package was installed in the workspace, Operations loaded its own catalog. Its title (العمليات), tabs (المواقع، يحتاج إلى شخص، الأوامر) and empty state («لا مواقع بعد»، «أضف موقعك الأول») read Arabic. Its entity names (Sites, Devices) come from its model and read English. |
| Hindi | With the administrator's language set to हिन्दी, home read «सुप्रभात, Walk» with `lang="hi"`, and the bar's Apps button read «ऐप». In Drive, the navigation, toolbar, columns, dates («7 नव॰ 2026, 5:04 pm UTC») and «3 आइटम» read Hindi, as did the labels people hear: «दिखाई गई हर चीज़ चुनें», «harbour-hills.jpg चुनें», «harbour-hills.jpg के लिए और». |
| A SCORM 1.2 package in a browser (FERRUM-LMS-002) | The package was written for this walk: an `imsmanifest.xml` (schemaversion 1.2, mastery score 80) and one SCO that finds the API in its parent window. Learn Settings were set to allow packages. The zip was uploaded through `/api/files` and made a SCORM lesson of «Kitchen hygiene basics». Saving the lesson read SCORM 1.2, launch `index.html` and mastery 80 from the package. The administrator enrolled («سجّل») and opened the lesson. The player served the package from `/api/app/lms/lessons/{lesson}/package/{file}/` and gave it the API: `LMSInitialize` answered true and the status was «not attempted». The right answer was submitted, and the SCO set score 90, «passed» and a session time of 00:02:30, then committed and finished (all true). The lesson then showed «مكتمل» and the course 33.3%. Its SCORM Progress record held status Passed, score 90%, 150 seconds, two commits and the package's CMI data (`lesson_status: passed`, `score.raw: 90`, the learner's id and name). |
| A guest's weekly series (FERRUM-BOOKINGS-002) | «Property viewing» (free) was set to let guests book it up to three times in a row. On its public booking page (`/bookings/book/walk`, in English, the browser's language), 11:00 on 13 November was picked. The form offered «Repeat», with «Just this once» to «Every month», and «Times in all» («Up to 3, this one included.»). A test guest chose every week, three times, and confirmed. The page answered «You are booked» and «Also booked, at the same time: Fri 20 Nov, 11:00; Fri 27 Nov, 11:00». Three confirmed bookings, BKG-2026-00001 to 00003, share one series. |
| An erasure through a restore (FERRUM-PRIVACY-006) | A customer, «Erase Rehearsal», was created with an email address. The walk database was dumped as its superuser (6 MB). The customer was then erased through `POST /api/admin/personal-data/erase`: the name became «Erased 23d040ad» and the email was gone. The journal written to object storage (`privacy-journal/{tenant}/1120.json`) names ids and fields, not the address. The server was stopped and the database restored from the dump into a new, empty database: the customer was back, with their name and email. `ferrum task reapply_erasures` answered «2 erasures replayed, 2 records and accounts erased again» (this one and an earlier walk's), and the customer was erased again. The server started on the restored database. |

## What it found

| Finding | Now |
|---|---|
| Sentences an app builds in its own code with template strings read English on a page in another language: Drive's storage meter («29 KB of 5.0 GB»), CRM's «0 deals», Learn's «3 lessons · 5 min» and «0 of 3 lessons». A scan finds about 440 such template strings in 23 apps: counts, «due …», «Overdue by …», validation messages, toasts and what screen readers announce. | Next: the words pass learns them, and the apps are rebuilt |
| Entity, field, action and report names from apps' models read English: the apps' navigation and the records pages. | Recorded (FERRUM-UX-006). A workspace's own words translate them; the apps carry no built-in translations of their models |
| The server's 404 page links «the App Manager» in English inside its Arabic sentence. Home's card region is labelled «Cards». | Next |
| The later dates of a guest's series are recorded with source «Internal» (the occurrences do not copy the first booking's source). | Next (Bookings) |
| `pg_restore --clean` over the walk's live database stopped at a partition's inherited constraint («cannot drop inherited constraint "op_measurement_20261108_pkey"»), after it had already dropped other objects. That is the procedure docs/operations.md gave for restoring in place. | Fixed in this PR: restoring in place now recreates the database first, as this walk did and as `deploy/smoke.sh` does on a new volume |

## What it did not cover

- The Compose smoke test (`deploy/smoke.sh`, with its new erasure step) was not run: this machine has
  no Ferrum image and too little disk to build one. The same steps were run by hand above.
- The translations were made by the model; no native speaker has reviewed them.
- The guest booking page was walked in English only, since it follows the guest's browser.
- The SCORM package is one written for the walk, not a commercial course; SCORM 2004 and xAPI were
  not walked.
