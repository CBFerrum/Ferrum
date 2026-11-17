# 19. Languages and formats

Every word a person reads goes through the platform's catalogs, every date and time is shown on one clock, and every
number and amount is written in the person's locale from an exact value. Country and sector content is data or an
add-on, never code in an engine. This guide says how, with the functions that exist today and the gaps that do not.

Sources: Ferrum-Frontend `src/lib/i18n.ts` and `src/lib/format.ts` (exported by `@ferrum/client`) and
`packages/build/words.mjs`; the platform's `server/src/shell_words.rs`, `docs/api.md` (*Conventions*, *Messages*),
`docs/limits.md`, `docs/plan/localization.md`.

## The languages there are

The interface offers English, Hindi and Arabic (`LANGUAGES` in `src/lib/i18n.ts`). English is the source and the
key: a word without a translation stays English, so a catalog may be partial. The page's language is settled once
(the person, then the workspace, then the browser) and comes in the bootstrap's `locale` (`language`, `dir`,
`timezone`, `display_timezone`, `chosen`, `words`, `names`). Right to left is decided by the language's base subtag
(`ar`, `fa`, `he`, `ur` and others: `RTL` in `i18n.ts`).

Every translation of the interface's own words into Hindi and Arabic so far was made by the implementer, not a native
speaker (Ferrum-Frontend: `docs/design/EVIDENCE.md`, *Not done, and why*). Say so of your app's catalogs too until a
native speaker has read them.

## Every word through `t()`

```tsx
import { t } from "@ferrum/client";

<Button>{t("New contract")}</Button>
toast(t("Moved {n} items to “{folder}”", { n: formatDecimal(moved, 0), folder: name }));
```

- `t(english, vars?)` looks the English up in the workspace's own words, then the app's catalog, then the interface's
  catalog, then the names of models and manifests; it fills `{name}` places from `vars`. A place filled with the
  name of a record type, field, option, action or app is filled with that name in the page's language.
- **The argument is a double-quoted string literal**, always. The build collects the words a page asks for with the
  pattern `t("…")` or `tr("…")` (`packages/build/words.mjs`, `CALL`): `t('…')`, a template literal or a variable is
  not collected, so it never reaches `dist/words.json` (what administrators translate) and a catalog entry for it
  fails `check` with "is not a word its pages ask for".
- **Call it when drawing, not when a module loads.** A `t()` at the top level of a module runs before the catalogs are
  in, and the English sticks. Map an id to words in a function, as Code does (`ferrum-app-code/ui/src/words.ts`,
  `repositorySaid(state)` returns `{ label: t("Ready"), tone: "green" }` per state).
- `tr` is the same function imported under another name where a file has a `t` of its own (Drive imports
  `t as tr`). The collector knows both names.
- Text that is not a word of the interface (a record's name, a code, a file name, a number built in code) is not
  translated: show it through `isolate(text)`, which keeps its own direction in a right-to-left page and shows the
  name of a model or manifest item in the page's language.

## Catalogs and files

An app's own words, by language, in its `ui/`:

| File | Holds | Read by |
|---|---|---|
| `ui/src/locales/<language>.json` | English → the language's words, for the app's pages | the `catalogs` of `mountApp({ route, routes, catalogs: { ar: () => import("./locales/ar.json"), hi: () => import("./locales/hi.json") } })` in `main.tsx` (`addCatalogs` takes the same); fetched only for a page in that language |
| `ui/src/locales/names/<language>.json` | English → the language's words for the names the app's model and manifest give pages (entity, field, option, action, report, module and app labels and descriptions) | copied to `dist/names/` by the build; the server merges them for every page (`/api/shell/names`, the bootstrap's `locale.names`) |
| `ui/dist/words.json` | every word the sources give `t()`, sorted (written by the build) | Administration → Interface words, where a workspace puts them in its own language as `InterfaceWord` records (key `[language, english]`) |

Drive (`ferrum-app-drive/ui/src/locales/ar.json`, `hi.json`, `names/`) is a complete example. `cli.mjs check` fails
when a catalog is not JSON, holds an English key the sources no longer ask for, has an empty translation, or keeps
other `{places}` than the English (`catalogIssues`, `namesIssues` in `words.mjs`). Nothing checks that a catalog is
complete: a missing translation shows English.

The workspace's own words stand over every catalog, the app's included (`applyLocale`: "The workspace's own words in
the language stand over every catalog"). The order is: workspace words, then the app's names, then the interface's
words.

Draw only after `wordsReady()` settles; `mountApp` does ([17-the-interface.md](17-the-interface.md)).

## Places and variables

- Places are `{word}` with letters, digits and underscores. The translation must keep the same places (checked).
- Pass numbers already formatted: `t()` writes a number with `String(n)`, so `{n: 1200}` shows `1200`, not `1,200`
  or `١٬٢٠٠`. Use `formatDecimal(n, 0)` (the page's chosen locale), not `n.toLocaleString()` (the browser's locale,
  which may not be the one the person chose).
- Pass dates already formatted with `formatMoment` or `formatDate` (below).
- A place for a quoted name keeps the quotes in the sentence: `t("Delete “{name}”?", { name })`, so a translator can
  move them.

## Plurals: what exists and what does not

`t()` has no plural forms, and the catalogs are flat English → words, not ICU messages (the plan's "message
catalogues (ICU)", `docs/plan/apps-ui.md` §12.9, is not built). Apps write two keys and choose in code:

```ts
export const items = (n: number) => (n === 1 ? t("1 item") : t("{n} items", { n }));   // ferrum-app-drive/ui/src/bulk.ts
```

That is right for English and Hindi and wrong for Arabic, which has six plural forms. Until the kit has plural
support (`Intl.PluralRules` is not used anywhere in Ferrum-Frontend):

- prefer sentences whose words do not change with the count: `t("Items: {n}")`, `t("{n} selected")`;
- where you must, use the two-key form above, and know it is wrong for Arabic;
- never build the plural in code: `` `${n} ${n === 1 ? "use" : "uses"}` `` is English that can never be translated.

## What never to concatenate

Build whole sentences with places; never glue translated pieces together, because word order differs between
languages. Seen in Drive and to be avoided:

- `ui/src/Edit.tsx` (the editor's list of people) adds `t(" (viewing)")` to a name: a fragment that assumes the name
  comes first. The same file also has the whole-sentence form, `t("{name} (viewing)", { name })`, which is the one to
  use.

Other things not to build in code: a list joined with `", "` and `" and "` (write `t("{a} and {b}")`, or one item
per line); a percent sign appended to a number (`formatValue` does that for `percent` fields, a known limit); an
ordinal ("1st"); a capitalised word made by changing the case of a translated one.

## Direction

- The page's `dir` and `lang` are set from the bootstrap by `applyLocale`; never set them yourself.
- `t()` returns English left untranslated in a right-to-left page wrapped in U+2068 and U+2069, so its full stop stays
  at its end; `isolate()` does the same for text shown as it is; `relativeTime` isolates its phrase.
- `.btn` sets `unicode-bidi: plaintext`, so a Latin label reads correctly in an Arabic page (PATTERNS.md, *Labels and
  direction*).
- Use logical CSS properties (`inline-start`, `inline-end`, `ms-*`, `me-*`, `ps-*`, `pe-*`, `start-*`, `end-*`), never
  `left` and `right` for layout. Left and Right mean "back" and "on" the other way round right to left: the kit's tabs
  and toolbars do this, and a key handler of your own must.
- Figures in a report are isolated so a minus stays in front right to left (PATTERNS.md, *Generic reports*).
- Rich text: each paragraph takes its own direction (`unicode-bidi: plaintext`); `sanitizeRichText` keeps `dir`.

## Dates, times and time zones: one clock

Times on every page a member opens are shown on one clock: the zone the person chose to see times in
(`display_timezone`), else the workspace's (`timezone`), never the browser's unless neither is known. Business days
("today", posting dates, due dates) are always the workspace's (`docs/api.md`, *Conventions*, "Dates").

| Value | Stored and sent as | Shown with | Taken from an input with |
|---|---|---|---|
| A date (`date` field) | `YYYY-MM-DD` | `formatDate(value)`: no zone shift | `<input type="date">` as it is |
| A moment (`datetime` field, `created_at`) | RFC 3339 in UTC | `formatMoment(value, shape)` or `formatDateTime(value)`: on the shown zone, with the zone's short name added when it is not the browser's | `fromZonedInput(s)` from a `datetime-local` value; `toZonedInput(v)` to fill one |
| "Today" | `YYYY-MM-DD` | — | `todayIso()`: the workspace's day |
| How long ago | RFC 3339 | `relativeTime(value)`: in the page's language through `Intl`, never glued words | — |

```tsx
import { formatMoment, formatDate, relativeTime } from "@ferrum/client";
<time dateTime={doc.due_at}>{formatMoment(doc.due_at, { dateStyle: "medium", timeStyle: "short" })}</time>
```

- Never `new Date(x).toLocaleString()`, `toLocaleDateString()` or a date library's own formatter: they use the
  browser's zone and locale, so two members of one workspace see different times for one moment.
- A page that draws times itself (a calendar grid) passes `shownTimeZone()` as `timeZone` to `Intl` and labels the
  zone with `zoneLabel()`.
- `setTimeZone` and `setDisplayZone` are set from the bootstrap by `@ferrum/client`; an app does not call them.
- A due moment of a paused service target is never shown (`TargetCard`), and no countdown moves.
- The server: API date-times are RFC 3339 UTC; a person's zone is the `timezone` preference
  (`PUT /api/prefs/timezone`, an IANA name; `docs/api.md`, *Preferences*). Mail and messages go out in the
  recipient's language and time zone where the `Recipient` has them (`docs/api.md`, *Messages*).

## Numbers and money

- Money and decimals are strings end to end, in the API and in the interface. Never parse them to a JavaScript
  number to add, round or compare. Totals come from the server (`POST /api/v1/{Entity}/_calc`) or an engine built
  for WebAssembly (Point of Sale's `ui/src/lib/pricing.ts` loads ERP's pricing crate) (Ferrum-Frontend: `README.md`,
  *Conventions*).
- Show a decimal with `formatDecimal(value, precision)`: grouping and the decimal mark of the page's locale, rounded
  exactly (half away from zero) from the string. A currency field: `formatValue({ type: "currency" }, value,
  { currencyPrecision })` with the precision of the record's currency when known.
- There is no currency formatter in the client: `formatDecimal` writes no symbol. Show the record's currency code (or
  its symbol, from the Currency record) beside the amount yourself, as data, not as a hard-coded `₹` or `$`.
- `formatValue` writes a `check` field as `t("Yes")` or `t("No")` and a percent with a `%` appended, which is a known
  limit of the client.
- Figures in tables are end-aligned and tabular (`.num`, `tabular-nums`).

## Units, names and addresses

- **Units** are data: a unit of measure is a record (ERP's), with its own name in the workspace's words. There is no
  unit-formatting helper; write the quantity with `formatDecimal` and the unit's name after it.
- **People's names**: store and show the name as the person gave it, in one field. Do not split it into first and last
  names to build a greeting, and do not change its case. `initials(name)` gives an avatar's mark.
- **Addresses**: a country is the name people type and a read-only `country_code` (ISO 3166-1 alpha-2,
  `ferrum_domain::countries`) follows it on every save (`docs/plan/localization.md`, *L0*). Read the code, never the
  name. There is no address-layout helper; show the address's lines as entered.
- **Phone numbers** in messages are in international form (`docs/api.md`, *Messages*).

## Country and sector content is data

An engine holds no country's rules. A country is an add-on on a seam, with its fields, tax determination, checks,
company set-up, e-invoicing, returns and prints (`docs/plan/localization.md`, *How a country add-on is built*; ERP's
`ferrum_app_erp::localization`, India in `addons/india`). The same holds for a sector: generic engines, with the
sector's content as data or an add-on ([01-what-an-app-is.md](01-what-an-app-is.md),
[11-integrations.md](11-integrations.md)). In the interface this means:

- no `if (country === "IN")` in a page: read what the add-on put in the record (its fields, prefixed with its code,
  such as `in_gstin`) or what the server answers;
- no country's names, tax words or document titles in the app's catalog as if they were the interface's: they belong to
  the add-on's package (its labels, translated in its `names/` catalog) or to the workspace's own words;
- prints with legal content are the add-on's print formats, not the app's.

## Words the server says

- The bar, sign-in, sign-up, the portal's home, the consent page and status pages are translated by the server
  (`server/src/shell_words.rs`, `server/shell/locales/<language>.json`).
- Mail and message templates are per language (`docs/api.md`, *Messages*; `messages::render_for`).
- **Refusals and check messages from the server stay as written, in English**: the client shows the `message` of an
  API error as it comes (`parseApiError`), and `docs/limits.md` says the messages of checks and refusals stay as
  written. An app's Rust refusal (`Error::field("due", "must be after the start")`) is read in English on an Arabic
  page. Write those messages in plain words ([20-writing-for-people.md](20-writing-for-people.md)) and, where a screen
  can say it better, map the stable `error` code to a sentence of the interface's own through `t()`.
- The client's own fallback messages for a status (Ferrum-Frontend `packages/client/src/errors.ts`) go through `t()`,
  except "Something went wrong on the server." and "Request failed (…)", and the "row" in a field error's place.

## Mistakes people make

| Mistake | What goes wrong | Instead |
|---|---|---|
| `t(label)` with a variable | not in `words.json`; the catalog key fails `check` | a function mapping ids to `t("…")` literals |
| `const LABELS = { open: t("Open") }` at module top | English forever | call `t()` while drawing |
| `` `${n} files` `` | never translated | `t("{n} files", { n: formatDecimal(n, 0) })` |
| `t("Hello ") + name` | word order fixed in English | `t("Hello, {name}", { name })` |
| `new Date(x).toLocaleString()` | the browser's zone and locale | `formatMoment(x, shape)` |
| `parseFloat(amount) * qty` | rounding errors on money | the server's `_calc`, or an engine in WebAssembly |
| `margin-left: 8px` | wrong side right to left | `margin-inline-start: 8px` |
| `if (country === "DE")` in a page | a country in the app | the add-on's fields and the server's answer |

## Checked by

- `cli.mjs check` (Ferrum-Frontend `packages/build/words.mjs`): catalogs hold only words the sources ask for, none
  empty, places kept; `words.test.ts` tests the collector.
- Ferrum-Frontend's own tests of `i18n.ts` and `format.ts` (`src/lib/*.test.ts`) hold the functions, not your use of
  them.
- Reviewed, not checked: that every word goes through `t()` with a literal, plurals, concatenation, the one clock,
  money as strings, and no country in code. Nothing checks catalog completeness or a translation's quality.

## See also

- [17-the-interface.md](17-the-interface.md), [18-accessibility.md](18-accessibility.md) (direction and language of
  parts), [20-writing-for-people.md](20-writing-for-people.md)
- [11-integrations.md](11-integrations.md) for country and sector rules as add-ons
- `docs/plan/localization.md`, `docs/api.md` (*Conventions*, *Preferences*, *Messages*), `docs/limits.md`
- Ferrum-Frontend: `src/lib/i18n.ts`, `src/lib/format.ts`, `packages/build/words.mjs`
