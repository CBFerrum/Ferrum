# 20. Writing for people

The words on screens, in API answers, in mail and in documentation are part of the app. This guide is the house style
for them: what to say in each kind of message, and what never to say. It covers the words; how they get translated is
[19-languages-and-formats.md](19-languages-and-formats.md), how they are announced is
[18-accessibility.md](18-accessibility.md).

Sources: the kit's own words (Ferrum-Frontend `src/ui/patterns.tsx`, `src/ui/kit.tsx`,
`packages/client/src/errors.ts`), the server's error answer (`server/src/error.rs`), the kernel's errors
(`crates/kernel/src/error.rs`), `docs/api.md` (*Conventions*, *Documents*), `docs/security.md`, and the apps quoted
below. Nothing checks words automatically: everything here is reviewed, not checked.

## The voice

- **Plain words, short sentences, present tense.** "Could not save", not "An error occurred during the persistence
  operation". "Ask an administrator", not "Contact your system administrator for assistance".
- **Say what happened and what to do next**, in that order. Nothing else.
- **Sentence case** everywhere: titles, buttons, menu items, column headers ("New contract", not "New Contract").
- **Name things as the person does.** "Repositories", "Contracts", "The floor"; not the entity's internal name or a
  table name. A field's label is what a person would call it.
- **No blame, no drama, no jokes in errors.** Not "You entered an invalid date", but "Enter a date after the start
  date". No exclamation marks.
- **No marketing words** in the interface or the docs: not "seamless", "blazing", "powerful", "simply".
- **British spelling** as the platform's documentation uses ("colour", "behaviour", "licence" as a noun); be
  consistent within a repository.
- **Absolute dates** in anything kept (docs, changelogs, release notes): "16 Nov 2026", never "yesterday" or "last
  week".
- **Numbers as figures** formatted for the locale (`formatDecimal`), and a count of zero said quietly or not at all
  ("Nothing waiting", not "(0)").

## Buttons and actions

- A button says what it does, as a verb and, where it helps, an object: "Archive", "Revoke", "New repository",
  "Restore version 3". Not "OK", "Submit" or "Yes" where a verb fits.
- The confirming button of a question repeats the verb of the question: "Revoke {name}?" → **Revoke**.
- One primary action per page and per dialog; the rest are ordinary buttons; rare ones go in "More actions"
  (PATTERNS.md, *Page anatomy*).
- An action that cannot be done now stays and says why (`MenuItem refusal={t("Only the owner can transfer it")}`),
  rather than disappearing and leaving the person to guess.
- **An action says what it did** (`docs/app-contract.md`, *Reviewed*). The server gives an answer without a `message`
  one from what it says: the record's new status and the records it made, such as "Now Admitted; made
  STU-2026-00001" (`docs/api.md`, *Documents*, `actions/{action}`). Return a `message` of your own when that would
  not be clear.

## Confirmations and Undo

Ask before something that cannot be undone, or that others will see at once. Do not ask before something that can be
undone: do it and offer Undo.

```tsx
// ferrum-app-code/ui/src/tabs.tsx
await confirmDialog({
  title: t("Revoke {name}?", { name: label }),
  message: t("Whatever uses it can no longer clone or push."),
  confirmLabel: t("Revoke"),
  danger: true,
});
```

- The **title** is the question, with the thing's name: "Archive web?", "Delete this comment?".
- The **message** says the consequence in one sentence, especially what cannot be got back and who else is affected:
  "It can still be read and cloned, and nobody can push to it until it is restored." (Code,
  `ui/src/Repositories.tsx`).
- `danger: true` for destructive actions: the confirming button is drawn red.
- Something that **can** be undone (move to Trash, archive, file to a folder, remove from a list) is done at once with
  `toast(t("Moved to Trash"), "success", { action: { label: t("Undo"), onSelect: restore } })`. The toast stays at
  least 8 seconds (PATTERNS.md, *Overlays*). An app does not build its own Undo bar.
- For a bulk action, say the scope before and the outcome after: "Archive 240 conversations?"; then
  `announce(t("240 conversations archived"))` or a toast.
- Never ask twice for the same thing. Know that the kit's `confirmDialog` puts focus on Cancel when `danger` is set
  and on the confirming button otherwise (`data-autofocus` in `ConfirmHost`, Ferrum-Frontend `src/ui/kit.tsx`), so Enter
  confirms only a question that is not destructive: the question and the button's verb must still be unmistakable.

## Errors and refusals

### On a screen

An error message says **what went wrong** and **what to do**; it never blames, and never leaks.

| Situation | Say | Do not say |
|---|---|---|
| A field is wrong | "Enter a date after the start date" (at the field) | "Invalid input", "Validation failed" |
| Not allowed | "You don't have access to this. Ask an administrator or its owner to give you access." (the kit's `denied` words) | what the thing is, who owns it, or that it exists |
| Changed by someone else | "Someone else changed this; yours is not saved", with Reload and the draft kept | "Conflict (409)" |
| No connection | "You're offline", and that work is kept only if it is | "Network error" |
| A server error | the kit's "Something went wrong", the reason the server gave, "Try again" | a stack trace, SQL, a file path |
| Too many requests | "Too many requests. Please wait a moment and try again." (`errors.ts`) | the limit's internals |

- Use `LoadFailed` and `StatePanel` for whole regions: their default words are already plain (`StatePanel`
  defaults in `src/ui/patterns.tsx`: "Nothing here yet", "No matches", "You don't have access to this", "You're
  offline", "Something went wrong").
- Use `refusal(e)` for a toast or a one-line place: it gives the field errors' messages, or the server's message
  (`packages/client/src/errors.ts`).
- Keep what the person typed. An error that throws away a form is a second error.
- A toast never carries the only copy of an error that matters: put it at the field or in the region.

### In the API (your Rust code)

The server turns a kernel `Error` into `{"error": code, "message": text, "fields"?: [...]}` with a stable code
(`crates/kernel/src/error.rs`, `server/src/error.rs`; [21-apis-and-docs.md](21-apis-and-docs.md)). The message is what
a person reads, usually in English (the server does not translate refusals; `docs/limits.md`), so:

- `Error::field("due", "must be after the start date")`: the field's name and what is wrong, in the words of a person
  fixing it. Cloud's are good models: `Error::field("note", "say why")`, `Error::field("detail", "say what was
  seen")`, `Error::field("category", format!("one of {}", CATEGORIES.join(", ")))` (`ferrum-app-cloud/src/`).
- `Error::invalid("…")` for a request that cannot be done as asked: say what would make it work. Drive:
  "Idempotency-Key must be 8 to 120 letters, digits or -_.:" (`ferrum-app-drive/src/site.rs`).
- `Error::Conflict("…")` for a clash with what exists, saying which: "this project already has a restriction of that
  kind". Database constraint errors are turned into words for you, but a unique index's default is "duplicate value
  (`<constraint name>`)": give each of yours a sentence with `ferrum_kernel::error::set_constraint_message(name,
  message)` (`crates/kernel/src/error.rs`), or the person reads your index's name. No app repository calls it.
- `Error::NotFound("project")` reads "project not found". Use it, with the same words, both when the thing does not
  exist and when the caller may not know it exists ([05-permissions-and-people.md](05-permissions-and-people.md)):
  Cloud's API answers 404 to someone with no standing on a project, "so that what exists elsewhere looks the same as
  what does not exist at all" (`ferrum-app-cloud/docs/api.md`).
- `Error::Forbidden("…")` only where the caller already knows the thing exists and lacks a right on it: say which
  right or role would allow it, never someone else's data.
- **Never put in a message**: a secret or any part of one, a token, a password, a query string, SQL, a file path on
  the server, another workspace's or another person's data, an internal id the person cannot use, a stack trace.
  `Error::Db` and `Error::Internal` are logged with an id and answered as "An internal error occurred (operation …)"
  (`server/src/error.rs`), so the person quotes the operation and an administrator finds it under Needs a person
  (`docs/events.md`). Do not wrap internal detail into `Invalid` to get it shown.
- Messages of a secret action parameter are redacted by the kernel (`Error::redact`); do not echo a parameter back.

## Empty states and first use

An empty region says **what this is** and **the first step**, with the step as a button:

```tsx
<StatePanel kind="empty" title={t("No repositories yet")} action={<Button variant="primary" icon="plus" onClick={make}>{t("New repository")}</Button>}>
  {t("A repository keeps a project's code and its history.")}
</StatePanel>
```

- First use and "a filter hid everything" are different states with different words: `empty` versus `no-matches`
  ("Nothing matches “{q}”.", with Clear filters).
- A work home with nothing waiting shows one empty state under its head, not a row of zeros (PATTERNS.md, *Work
  home*).
- A queue with nothing in it says "Nothing waiting" (the default) and keeps its header.

## Status words

- A state is a word, not only a colour: "Ready", "Being made", "Could not be made", "Waiting for a runner" (Code,
  `ui/src/words.ts`). The words describe the state for a person; the tone comes from the stored id.
- Never claim more than is known: "Not checked yet" before a merge check ran, never "Mergeable"; a run is "Passed"
  only when it passed; `SaveStatus` says "Saved" only after the server confirmed, "Kept on this device, not saved yet"
  for work held offline, and claims nothing when there was no answer (`unknown`).
- A state the page does not know is shown as it is (the raw id), not guessed (Code's `repositorySaid` default).

## Names of things

- One name per thing across the app and its docs: if the menu says "Repositories", the button says "New repository"
  and the docs say "repository".
- The names an app's model and manifest give pages (entity labels, fields, options, actions, reports) are the names
  people read: write them as words ("Due date", "Parent Account"), with a `description` that says what the thing is
  for (CRM: `label: Parent Account, description: "The group or head office it belongs to"`, `packages/crm/package.yaml`).
- Do not show internal names: `x_gst_number`, `e_drive_file`, `cloud-ops-…`. The client's `humanize()` turns a field
  name into words only as a fallback.

## Mail and messages

Mail an app sends goes through templates (`docs/api.md`, *Messages*; *Moving an app's fixed wording to templates*), so
a workspace can change the words and each recipient gets their language. The same rules hold: the subject says what
happened ("Your invoice INV-0042 is due on 20 Nov 2026"), the body says what to do, and a link opens the record.

## Documentation, READMEs and reports

How a README is built is [22-readme-and-repository.md](22-readme-and-repository.md); the rules for what it says:

- **Say what was run and what was not.** "Tested against PostgreSQL 17 with the suite in `tests/`"; "Not run against a
  real payment provider: the tests use a fake"; "No screen reader pass has been done". A reader must be able to tell
  what was seen working from what was only written.
- **Name the evidence class**: read in the code, a unit or component test, seen in a real browser, reviewed by a
  person (Ferrum-Frontend `docs/design/EVIDENCE.md` keeps these apart). Compiling is not "done".
- **No claims you cannot point at**: no "fully accessible", "GDPR compliant", "production ready", "enterprise grade",
  no counts you did not count.
- **Limits are written down** where a person will look for them: the app's README and the platform's `docs/limits.md`
  style ("Drive: public links outlive their creator's access").
- **Code paths, commands and versions exact**, so a reader can run what you ran.
- No names of people, no internal ticket numbers, no local paths, no mention of the tools that wrote the text in a
  public repository; no attribution lines in commits.

## Checked by

Nothing checks words automatically. The kit's states carry tested default words (`src/ui/patterns.tsx` and its tests);
`cli.mjs check` only checks that catalog entries match words the sources ask for. Everything else here is reviewed,
not checked.

## See also

- [17-the-interface.md](17-the-interface.md) (states and patterns), [18-accessibility.md](18-accessibility.md)
  (names, errors and announcements), [19-languages-and-formats.md](19-languages-and-formats.md)
- [21-apis-and-docs.md](21-apis-and-docs.md) (error codes), [22-readme-and-repository.md](22-readme-and-repository.md)
- [05-permissions-and-people.md](05-permissions-and-people.md) and [13-security-rules.md](13-security-rules.md)
  (never say whether something exists)
- `docs/api.md` (*Conventions*), `docs/security.md`, `docs/events.md` (*Dead events*, operations)
