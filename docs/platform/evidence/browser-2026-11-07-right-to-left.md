# A browser on a running server, 7 November 2026: the records pages right to left

FERRUM-UX-006 said that pages had not been checked one by one right to left. This walk took the
walk server's records pages in Arabic, in workspace walk, signed in as its administrator, whose
own language was set to العربية on the Account page (and set back to the workspace's afterwards).
It ran twice:
1. On Ferrum main 1915711 with the shell from Ferrum-Frontend 204b713. The page took `lang="ar"`
   and `dir="rtl"`.
2. On the same server, with the shell from Ferrum-Frontend PR 46 (9d80f4f), which carries what the
   first walk found.

## What was walked

| Page | Before PR 46 | With PR 46 |
|---|---|---|
| The account page | Mirrored: the bar, breadcrumbs, cards and the browsers table read from the right; dates written the Arabic way ("2026/11/05، 11:41 م UTC"). Nearly every word English, and an English sentence's full stop at its start | Its title, sections, their descriptions, the browsers table and its Sign out in Arabic. The sign-in sentence, "Manage your Ferrum account" and the time zone's default stay English |
| Customers, the list | Mirrored: the checkbox column on the right, columns reading from it, the toolbar and the search box's icon flipped. Its count read "records 12", and the toolbar and search were English | "السجلات: 12", the views (قائمة, لوحة), Customize, Export CSV, Import, New, the search box, Columns, Views and Filter in Arabic |
| A customer, the record | Mirrored, but the breadcrumb's chevron and the closed Addresses section's still pointed right, and its tabs and actions were English | The chevrons point left, and the open section's points down. The tabs (التفاصيل, ذات الصلة, النقاش, النشاط) and the actions (حفظ, طباعة, البريد الإلكتروني) in Arabic |

The record and field labels (Customers, Customer Name, Addresses) are each app's own words and
stay as its YAML writes them.

## What it found, and what changed

| Finding | Now |
|---|---|
| Directional icons kept pointing right: the kit's chevron and back arrow, and Lucide's | They point the other way right to left. The mirror uses `scale`, so it composes with an open section's rotation (Ferrum-Frontend PR 46) |
| The stylesheets and components set left and right: margins, paddings, borders, insets, text alignment and corners. Most held, because layouts are flex and grid | They are logical now: 69 lines of CSS and 140 class lists. Centred elements, the sheets that name their side, and safe-area insets stay as they were |
| A tab strip's and a column resize handle's arrow keys went by Left and Right | They step on and back along the line (`arrowStep`, tested both ways) |
| The words on every records page were English | In Arabic and Hindi, with a test that every word the interface asks for is in both catalogs |

## What it did not cover

- Apps' own screens, one by one, right to left: they are built with the same kit and stylesheet,
  but their own words are English and their own layouts were not walked.
- Hindi was not walked: it reads left to right, and the test checks its catalog.
